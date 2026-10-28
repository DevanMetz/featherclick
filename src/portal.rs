//! Clicks on GNOME and KDE through the RemoteDesktop portal.
//!
//! Those compositors do not offer `zwlr_virtual_pointer_manager_v1`. The
//! portal's `NotifyPointer*` methods move and click in the stream's logical
//! pixels, including a monitor that sits left of the origin. libei stays out:
//! enigo's client aborts the process when the portal is missing.
//!
//! The permission dialog is raised on the first run, not when the window
//! opens. A restore token in `portal-restore` skips the dialog after that.
//! Refusing the portal, or having no portal, falls back to X11.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::thread;
use std::time::Duration;

use enigo::{Button, Direction, Enigo, Mouse, Settings as EnigoSettings};
use ordered_stream::{OrderedStream, PollResult};
use zbus::proxy::SignalStream;
use zbus::{Connection, Message, OwnedObjectPath, OwnedVariant, Proxy, Variant};

const DEST: &str = "org.freedesktop.portal.Desktop";
const DESKTOP_PATH: &str = "/org/freedesktop/portal/desktop";
const REMOTE_DESKTOP: &str = "org.freedesktop.portal.RemoteDesktop";
const REQUEST_IFACE: &str = "org.freedesktop.portal.Request";
const SESSION_IFACE: &str = "org.freedesktop.portal.Session";

const POINTER: u32 = 2;
const PERSIST_UNTIL_REVOKED: u32 = 2;
const BTN_LEFT: i32 = 0x110;
const BTN_RIGHT: i32 = 0x111;
const BTN_MIDDLE: i32 = 0x112;

const SHORT_WAIT: Duration = Duration::from_secs(5);
const START_WAIT: Duration = Duration::from_secs(120);

type Options = HashMap<String, OwnedVariant>;

static TOKEN: AtomicU64 = AtomicU64::new(1);
thread_local! {
    static IN_BLOCK: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// One PipeWire stream in the compositor's logical layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Output {
    pub node: u32,
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Where `NotifyPointerMotionAbsolute` should land, in that stream's pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct Aim {
    pub node: u32,
    pub x: f64,
    pub y: f64,
}

/// The stream that contains `(x, y)`, or the nearest one. The point is the
/// centre of the clamped pixel, and the range is half-open.
pub(crate) fn locate(outputs: &[Output], x: i32, y: i32) -> Option<Aim> {
    let usable: Vec<&Output> = outputs
        .iter()
        .filter(|output| output.width > 0 && output.height > 0)
        .collect();
    let chosen = usable
        .iter()
        .copied()
        .find(|output| contains(output, x, y))
        .or_else(|| {
            usable
                .iter()
                .copied()
                .min_by_key(|output| distance_to(output, x, y))
        })?;
    let local_x = (x - chosen.x).clamp(0, chosen.width - 1);
    let local_y = (y - chosen.y).clamp(0, chosen.height - 1);
    Some(Aim {
        node: chosen.node,
        x: f64::from(local_x) + 0.5,
        y: f64::from(local_y) + 0.5,
    })
}

fn contains(output: &Output, x: i32, y: i32) -> bool {
    x >= output.x
        && y >= output.y
        && x < output.x.saturating_add(output.width)
        && y < output.y.saturating_add(output.height)
}

fn distance_to(output: &Output, x: i32, y: i32) -> i64 {
    let left = i64::from(output.x);
    let right = i64::from(output.x) + i64::from(output.width);
    let top = i64::from(output.y);
    let bottom = i64::from(output.y) + i64::from(output.height);
    let px = i64::from(x);
    let py = i64::from(y);
    let dx = if px < left {
        left - px
    } else if px >= right {
        px - (right - 1)
    } else {
        0
    };
    let dy = if py < top {
        top - py
    } else if py >= bottom {
        py - (bottom - 1)
    } else {
        0
    };
    dx * dx + dy * dy
}

pub(crate) fn require_success(code: u32) -> Result<(), String> {
    match code {
        0 => Ok(()),
        1 => Err("remote desktop permission was denied".to_string()),
        _ => Err("remote desktop portal failed".to_string()),
    }
}

pub(crate) fn require_pointer(devices: Option<u32>) -> Result<(), String> {
    match devices {
        Some(bits) if bits & POINTER == 0 => {
            Err("remote desktop portal did not grant pointer control".to_string())
        }
        _ => Ok(()),
    }
}

/// `<config_dir>/featherclick/portal-restore`. Kept beside the settings file
/// so a settings rewrite cannot drop the portal grant.
pub(crate) fn restore_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("featherclick")
        .join("portal-restore")
}

pub(crate) fn load_token(path: &Path) -> Option<String> {
    let text = std::fs::read_to_string(path).ok()?;
    let token = text.trim();
    if token.is_empty() || token.chars().any(|ch| matches!(ch, '\n' | '\r' | '\0')) {
        None
    } else {
        Some(token.to_string())
    }
}

pub(crate) fn store_token(path: &Path, token: &str) -> std::io::Result<()> {
    if token.is_empty() || token.chars().any(|ch| matches!(ch, '\n' | '\r' | '\0')) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "restore token",
        ));
    }
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("portal-restore");
    let tmp = path.with_file_name(format!("{name}.tmp"));
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(token.as_bytes())?;
        file.write_all(b"\n")?;
    }
    std::fs::rename(&tmp, path)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

/// Pointer position from XWayland, when `DISPLAY` is set. Never clicks.
pub(crate) fn read_cursor() -> Result<(i32, i32), String> {
    let reader = x11_reader().ok_or_else(|| "cannot read the pointer".to_string())?;
    reader.location().map_err(|err| err.to_string())
}

fn x11_reader() -> Option<Enigo> {
    let display = std::env::var_os("DISPLAY")?;
    if display.is_empty() {
        return None;
    }
    let settings = EnigoSettings {
        linux_delay: 0,
        wayland_display: Some("featherclick-no-wayland".into()),
        ..EnigoSettings::default()
    };
    Enigo::new(&settings).ok()
}

pub(crate) struct PortalPointer {
    conn: Connection,
    session: OwnedObjectPath,
    streams: Vec<Output>,
    reader: Option<Enigo>,
}

impl PortalPointer {
    /// Opens a session on the user bus. The portal may show a dialog here.
    pub(crate) fn open() -> Result<Self, String> {
        Self::open_with(None, restore_path(), SHORT_WAIT, START_WAIT)
    }

    fn open_with(
        address: Option<&str>,
        token_path: PathBuf,
        short_wait: Duration,
        start_wait: Duration,
    ) -> Result<Self, String> {
        let opened = block(open_session(address, &token_path, short_wait, start_wait))?;
        Ok(Self {
            conn: opened.conn,
            session: opened.session,
            streams: opened.streams,
            reader: x11_reader(),
        })
    }

    pub(crate) fn button(&mut self, button: Button, direction: Direction) -> Result<(), String> {
        let code = match button {
            Button::Left => BTN_LEFT,
            Button::Right => BTN_RIGHT,
            Button::Middle => BTN_MIDDLE,
            _ => {
                return Err(
                    "remote desktop portal only has the left, right, and middle buttons"
                        .to_string(),
                );
            }
        };
        match direction {
            Direction::Press => self.pointer_button(code, 1),
            Direction::Release => self.pointer_button(code, 0),
            Direction::Click => {
                self.pointer_button(code, 1)?;
                self.pointer_button(code, 0)
            }
        }
    }

    pub(crate) fn move_abs(&mut self, x: i32, y: i32) -> Result<(), String> {
        let aim = locate(&self.streams, x, y)
            .ok_or_else(|| "no screen from the remote desktop portal".to_string())?;
        let conn = self.conn.clone();
        let session = self.session.clone();
        block(async move { notify_motion(&conn, &session, aim.node, aim.x, aim.y).await })
    }

    pub(crate) fn location(&self) -> Result<(i32, i32), String> {
        self.reader
            .as_ref()
            .ok_or_else(|| "cannot read the pointer".to_string())?
            .location()
            .map_err(|err| err.to_string())
    }

    fn pointer_button(&self, button: i32, state: u32) -> Result<(), String> {
        let conn = self.conn.clone();
        let session = self.session.clone();
        block(async move { notify_button(&conn, &session, button, state).await })
    }
}

impl Drop for PortalPointer {
    fn drop(&mut self) {
        if IN_BLOCK.with(std::cell::Cell::get) {
            return;
        }
        let conn = self.conn.clone();
        let session = self.session.clone();
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            block(async move {
                let proxy = Proxy::new(&conn, DEST, &session, SESSION_IFACE).await?;
                proxy.call_method("Close", &()).await?;
                Ok::<(), zbus::Error>(())
            })
        }));
    }
}

struct Opened {
    conn: Connection,
    session: OwnedObjectPath,
    streams: Vec<Output>,
}

async fn open_session(
    address: Option<&str>,
    token_path: &Path,
    short_wait: Duration,
    start_wait: Duration,
) -> Result<Opened, String> {
    let conn = connect_bus(address).await?;
    let restore = load_token(token_path);

    let create_token = next_token();
    let session_token = next_token();
    let mut create = Options::new();
    create.insert("handle_token".into(), string_variant(&create_token));
    create.insert(
        "session_handle_token".into(),
        string_variant(&session_token),
    );
    let created = call_request(&conn, "CreateSession", &create, &create_token, short_wait).await?;
    let session = created
        .get("session_handle")
        .and_then(variant_text)
        .ok_or_else(|| "remote desktop portal did not return a session".to_string())?;
    let session = OwnedObjectPath::try_from(session)
        .map_err(|_| "remote desktop portal did not return a session".to_string())?;

    let select_token = next_token();
    let mut select = Options::new();
    select.insert("handle_token".into(), string_variant(&select_token));
    select.insert("types".into(), u32_variant(POINTER));
    select.insert("persist_mode".into(), u32_variant(PERSIST_UNTIL_REVOKED));
    if let Some(token) = restore {
        select.insert("restore_token".into(), string_variant(&token));
    }
    call_request(
        &conn,
        "SelectDevices",
        &(session.clone(), select),
        &select_token,
        short_wait,
    )
    .await?;

    let start_token = next_token();
    let mut start = Options::new();
    start.insert("handle_token".into(), string_variant(&start_token));
    let started = call_request(
        &conn,
        "Start",
        &(session.clone(), "", start),
        &start_token,
        start_wait,
    )
    .await?;
    require_pointer(started.get("devices").and_then(as_u32))?;
    if let Some(token) = started.get("restore_token").and_then(variant_text) {
        let _ = store_token(token_path, &token);
    }
    let streams = started
        .get("streams")
        .map(|value| parse_streams(value))
        .unwrap_or_default();

    Ok(Opened {
        conn,
        session,
        streams,
    })
}

async fn connect_bus(address: Option<&str>) -> Result<Connection, String> {
    let builder = match address {
        Some(address) => zbus::connection::Builder::address(address),
        None => zbus::connection::Builder::session(),
    };
    builder
        .method_timeout(Duration::from_secs(15))
        .build()
        .await
        .map_err(explain)
}

async fn call_request<B>(
    conn: &Connection,
    method: &str,
    body: &B,
    token: &str,
    timeout: Duration,
) -> Result<Options, String>
where
    B: serde::Serialize + zbus::DynamicType + Sync,
{
    let unique = conn
        .unique_name()
        .map(|name| name.as_str().to_string())
        .ok_or_else(|| "remote desktop portal is not available".to_string())?;
    let path = object_path("request", &unique, token);
    let mut signals = subscribe(conn, &path).await?;
    let desktop = Proxy::new(conn, DEST, DESKTOP_PATH, REMOTE_DESKTOP)
        .await
        .map_err(explain)?;
    let returned: OwnedObjectPath = desktop.call(method, body).await.map_err(explain)?;
    if returned.as_str() != path {
        signals = subscribe(conn, returned.as_str()).await?;
    }
    let message = wait_signal(signals.as_mut(), timeout).await?;
    let (code, results): (u32, HashMap<String, OwnedVariant>) = message
        .body()
        .deserialize()
        .map_err(|err| format!("remote desktop portal failed: {err}"))?;
    require_success(code)?;
    Ok(results)
}

async fn subscribe<'a>(
    conn: &Connection,
    path: &str,
) -> Result<Pin<Box<SignalStream<'a>>>, String> {
    let proxy = Proxy::new(conn, DEST, path, REQUEST_IFACE)
        .await
        .map_err(explain)?;
    let stream = proxy.receive_signal("Response").await.map_err(explain)?;
    Ok(Box::pin(stream))
}

async fn wait_signal(
    mut stream: Pin<&mut SignalStream<'_>>,
    timeout: Duration,
) -> Result<Message, String> {
    let mut timer = std::pin::pin!(thread_timeout(timeout));
    std::future::poll_fn(|cx| {
        if timer.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err("remote desktop portal timed out".to_string()));
        }
        match stream.as_mut().poll_next_before(cx, None) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(PollResult::Item { data, .. }) => Poll::Ready(Ok(data)),
            Poll::Ready(PollResult::Terminated) => {
                Poll::Ready(Err("remote desktop portal closed the request".to_string()))
            }
            Poll::Ready(PollResult::NoneBefore) => Poll::Pending,
        }
    })
    .await
}

async fn notify_motion(
    conn: &Connection,
    session: &OwnedObjectPath,
    node: u32,
    x: f64,
    y: f64,
) -> Result<(), String> {
    let proxy = Proxy::new(conn, DEST, DESKTOP_PATH, REMOTE_DESKTOP)
        .await
        .map_err(explain)?;
    let options = Options::new();
    proxy
        .call_method(
            "NotifyPointerMotionAbsolute",
            &(session.clone(), options, node, x, y),
        )
        .await
        .map_err(explain)?;
    Ok(())
}

async fn notify_button(
    conn: &Connection,
    session: &OwnedObjectPath,
    button: i32,
    state: u32,
) -> Result<(), String> {
    let proxy = Proxy::new(conn, DEST, DESKTOP_PATH, REMOTE_DESKTOP)
        .await
        .map_err(explain)?;
    let options = Options::new();
    proxy
        .call_method(
            "NotifyPointerButton",
            &(session.clone(), options, button, state),
        )
        .await
        .map_err(explain)?;
    Ok(())
}

fn explain(err: zbus::Error) -> String {
    match &err {
        zbus::Error::FDO(fdo) => match fdo.as_ref() {
            zbus::fdo::Error::NameHasNoOwner(_) | zbus::fdo::Error::ServiceUnknown(_) => {
                return "remote desktop portal is not available".to_string();
            }
            _ => {}
        },
        zbus::Error::Address(_) | zbus::Error::Handshake(_) => {
            return "remote desktop portal is not available".to_string();
        }
        _ => {}
    }
    let text = err.to_string();
    if text.contains("No such file")
        || text.contains("Connection refused")
        || text.contains("Failed to connect")
    {
        return "remote desktop portal is not available".to_string();
    }
    format!("remote desktop portal failed: {err}")
}

fn next_token() -> String {
    let n = TOKEN.fetch_add(1, Ordering::Relaxed);
    format!("featherclick{n}")
}

fn bus_token(unique_name: &str) -> String {
    unique_name.trim_start_matches(':').replace('.', "_")
}

fn object_path(kind: &str, unique_name: &str, token: &str) -> String {
    format!(
        "/org/freedesktop/portal/desktop/{kind}/{}/{}",
        bus_token(unique_name),
        token
    )
}

fn string_variant(text: &str) -> OwnedVariant {
    OwnedVariant::try_from(Variant::new(text.to_owned())).expect("string variant")
}

fn u32_variant(value: u32) -> OwnedVariant {
    OwnedVariant::try_from(Variant::new(value)).expect("u32 variant")
}

fn variant_text(value: &OwnedVariant) -> Option<String> {
    text_of(value)
}

fn text_of(value: &Variant) -> Option<String> {
    match value {
        Variant::Str(text) => Some(text.to_string()),
        Variant::ObjectPath(path) => Some(path.to_string()),
        Variant::Variant(inner) => text_of(inner),
        _ => None,
    }
}

fn as_u32(value: &OwnedVariant) -> Option<u32> {
    u32_of(value)
}

fn u32_of(value: &Variant) -> Option<u32> {
    match value {
        Variant::U32(n) => Some(*n),
        Variant::U16(n) => Some(u32::from(*n)),
        Variant::U8(n) => Some(u32::from(*n)),
        Variant::I32(n) if *n >= 0 => Some(*n as u32),
        Variant::Variant(inner) => u32_of(inner),
        _ => None,
    }
}

fn as_i32(value: &Variant) -> Option<i32> {
    match value {
        Variant::I32(n) => Some(*n),
        Variant::U32(n) if *n <= i32::MAX as u32 => Some(*n as i32),
        Variant::I16(n) => Some(i32::from(*n)),
        Variant::U16(n) => Some(i32::from(*n)),
        Variant::Variant(inner) => as_i32(inner),
        _ => None,
    }
}

fn parse_streams(value: &Variant) -> Vec<Output> {
    let array = match peel(value) {
        Variant::Array(array) => array,
        _ => return Vec::new(),
    };
    array
        .inner()
        .iter()
        .filter_map(|element| {
            let structure = match peel(element) {
                Variant::Structure(structure) => structure,
                _ => return None,
            };
            let fields = structure.fields();
            if fields.len() < 2 {
                return None;
            }
            let node = u32_of(&fields[0])?;
            output_from_props(node, &fields[1])
        })
        .collect()
}

fn output_from_props(node: u32, props: &Variant) -> Option<Output> {
    let (x, y) = dict_pair(props, "position").unwrap_or((0, 0));
    let size = dict_pair(props, "size")?;
    let (width, height) = dict_pair(props, "logical_size").unwrap_or(size);
    if width <= 0 || height <= 0 {
        return None;
    }
    Some(Output {
        node,
        x,
        y,
        width,
        height,
    })
}

fn dict_pair(props: &Variant, key: &str) -> Option<(i32, i32)> {
    let value = dict_get(props, key)?;
    let structure = match peel(value) {
        Variant::Structure(structure) => structure,
        _ => return None,
    };
    let fields = structure.fields();
    if fields.len() != 2 {
        return None;
    }
    Some((as_i32(&fields[0])?, as_i32(&fields[1])?))
}

fn dict_get<'a>(props: &'a Variant<'a>, key: &str) -> Option<&'a Variant<'a>> {
    match peel(props) {
        Variant::Dict(dict) => dict.iter().find_map(|(entry_key, value)| {
            (text_of(entry_key).as_deref() == Some(key)).then_some(value)
        }),
        Variant::Array(entries) => entries.inner().iter().find_map(|entry| {
            let Variant::Structure(structure) = peel(entry) else {
                return None;
            };
            let fields = structure.fields();
            if fields.len() == 2 && text_of(&fields[0]).as_deref() == Some(key) {
                Some(&fields[1])
            } else {
                None
            }
        }),
        _ => None,
    }
}

fn peel<'a>(value: &'a Variant<'_>) -> &'a Variant<'a> {
    match value {
        Variant::Variant(inner) => peel(inner),
        other => other,
    }
}

struct InBlock;

impl Drop for InBlock {
    fn drop(&mut self) {
        IN_BLOCK.with(|flag| flag.set(false));
    }
}

fn block<T>(future: impl Future<Output = T>) -> T {
    IN_BLOCK.with(|flag| flag.set(true));
    let _guard = InBlock;
    zbus::block_on(future)
}

struct Timeout {
    waker: Arc<Mutex<Option<Waker>>>,
    fired: Arc<AtomicBool>,
    started: bool,
    duration: Duration,
}

fn thread_timeout(duration: Duration) -> Timeout {
    Timeout {
        waker: Arc::new(Mutex::new(None)),
        fired: Arc::new(AtomicBool::new(false)),
        started: false,
        duration,
    }
}

impl Future for Timeout {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.fired.load(Ordering::Acquire) {
            return Poll::Ready(());
        }
        if let Ok(mut slot) = self.waker.lock() {
            *slot = Some(cx.waker().clone());
        }
        if self.fired.load(Ordering::Acquire) {
            return Poll::Ready(());
        }
        if !self.started {
            self.started = true;
            let waker = Arc::clone(&self.waker);
            let fired = Arc::clone(&self.fired);
            let duration = self.duration;
            thread::spawn(move || {
                thread::sleep(duration);
                fired.store(true, Ordering::Release);
                if let Ok(mut slot) = waker.lock()
                    && let Some(waker) = slot.take()
                {
                    waker.wake();
                }
            });
            if self.fired.load(Ordering::Acquire) {
                return Poll::Ready(());
            }
        }
        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::{Child, Command, Stdio};
    use std::time::{SystemTime, UNIX_EPOCH};
    use zbus::interface;
    use zbus::message::Header;
    use zbus::object_server::{ObjectServer, ResponseDispatchNotifier};
    use zbus::runtime::traits::TaskHandle;

    #[test]
    fn locate_uses_the_stream_that_contains_the_point() {
        let outputs = [
            Output {
                node: 7,
                x: 0,
                y: 0,
                width: 800,
                height: 600,
            },
            Output {
                node: 8,
                x: -1920,
                y: 0,
                width: 1920,
                height: 1080,
            },
        ];
        let aim = locate(&outputs, -10, 20).unwrap();
        assert_eq!(aim.node, 8);
        assert_eq!(aim.x, 1910.5);
        assert_eq!(aim.y, 20.5);

        // The left edge of the right-hand screen belongs to node 7.
        let edge = locate(&outputs, 0, 0).unwrap();
        assert_eq!(edge.node, 7);
        assert_eq!(edge.x, 0.5);
        assert_eq!(edge.y, 0.5);

        // Half-open: x == origin + width is outside, so it clamps onto node 8.
        let boundary = locate(&outputs, -1920, 0).unwrap();
        assert_eq!(boundary.node, 8);
        assert_eq!(boundary.x, 0.5);

        let past = locate(&[outputs[1]], -1, 0).unwrap();
        assert_eq!(past.node, 8);
        assert_eq!(past.x, 1919.5);
    }

    #[test]
    fn locate_skips_empty_streams_and_clamps_to_the_nearest() {
        assert!(locate(&[], 1, 1).is_none());
        assert!(
            locate(
                &[Output {
                    node: 1,
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 10
                }],
                1,
                1
            )
            .is_none()
        );

        let aim = locate(
            &[Output {
                node: 3,
                x: 10,
                y: 10,
                width: 5,
                height: 5,
            }],
            0,
            0,
        )
        .unwrap();
        assert_eq!(aim.node, 3);
        assert_eq!(aim.x, 0.5);
        assert_eq!(aim.y, 0.5);

        // Overlap: the first containing stream wins.
        let overlap = locate(
            &[
                Output {
                    node: 1,
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                },
                Output {
                    node: 2,
                    x: 0,
                    y: 0,
                    width: 100,
                    height: 100,
                },
            ],
            40,
            40,
        )
        .unwrap();
        assert_eq!(overlap.node, 1);
    }

    #[test]
    fn portal_responses_distinguish_denial_from_a_missing_pointer() {
        assert!(require_success(0).is_ok());
        assert_eq!(
            require_success(1).unwrap_err(),
            "remote desktop permission was denied"
        );
        assert_eq!(
            require_success(2).unwrap_err(),
            "remote desktop portal failed"
        );
        assert!(require_pointer(None).is_ok());
        assert!(require_pointer(Some(POINTER)).is_ok());
        assert!(require_pointer(Some(POINTER | 1)).is_ok());
        assert_eq!(
            require_pointer(Some(1)).unwrap_err(),
            "remote desktop portal did not grant pointer control"
        );
    }

    #[test]
    fn restore_token_roundtrip_is_private_to_the_given_path() {
        let dir = temp_dir("token");
        let path = dir.join("portal-restore");
        assert!(load_token(&path).is_none());
        store_token(&path, "granted-1").unwrap();
        assert_eq!(load_token(&path).as_deref(), Some("granted-1"));
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(store_token(&path, "has\nnewline").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_click_hits_the_monitor_that_contains_the_point() {
        let daemon = Daemon::start();
        let state = Arc::new(Mutex::new(Vec::new()));
        let server = serve(&daemon.address, Arc::clone(&state));
        let dir = temp_dir("click");
        let token_path = dir.join("portal-restore");

        let mut pointer = PortalPointer::open_with(
            Some(&daemon.address),
            token_path.clone(),
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .unwrap();
        pointer.move_abs(-10, 20).unwrap();
        pointer.move_abs(100, 20).unwrap();
        pointer.button(Button::Left, Direction::Press).unwrap();
        pointer.button(Button::Left, Direction::Release).unwrap();
        assert_eq!(load_token(&token_path).as_deref(), Some("granted-1"));
        drop(pointer);

        let again = PortalPointer::open_with(
            Some(&daemon.address),
            token_path,
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .unwrap();
        drop(again);

        let events = state.lock().unwrap().clone();
        assert!(
            events.contains(&Recorded::Select {
                types: POINTER,
                persist: PERSIST_UNTIL_REVOKED,
                restore: None,
            }),
            "{events:?}"
        );
        assert!(
            events.contains(&Recorded::Select {
                types: POINTER,
                persist: PERSIST_UNTIL_REVOKED,
                restore: Some("granted-1".to_string()),
            }),
            "{events:?}"
        );
        assert!(
            events.contains(&Recorded::Move {
                node: 8,
                x: 1910.5,
                y: 20.5
            }),
            "{events:?}"
        );
        assert!(
            events.contains(&Recorded::Move {
                node: 7,
                x: 100.5,
                y: 20.5
            }),
            "{events:?}"
        );
        assert!(events.contains(&Recorded::Button {
            button: BTN_LEFT,
            state: 1
        }));
        assert!(events.contains(&Recorded::Button {
            button: BTN_LEFT,
            state: 0
        }));
        assert!(events.contains(&Recorded::Close));
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn temp_dir(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "featherclick-portal-{label}-{}-{nanos}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    struct Daemon {
        child: Child,
        address: String,
        dir: PathBuf,
    }

    impl Daemon {
        fn start() -> Self {
            let daemon = ["/usr/bin/dbus-daemon", "/bin/dbus-daemon"]
                .into_iter()
                .map(PathBuf::from)
                .find(|path| path.exists())
                .expect("dbus-daemon is required to test the remote desktop portal");
            let dir = temp_dir("bus");
            let socket = dir.join("bus");
            let address = format!("unix:path={}", socket.display());
            let child = Command::new(daemon)
                .args([
                    "--session",
                    "--nofork",
                    "--nopidfile",
                    &format!("--address={address}"),
                ])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            for _ in 0..50 {
                if socket.exists() {
                    break;
                }
                thread::sleep(Duration::from_millis(20));
            }
            assert!(socket.exists(), "dbus-daemon did not create its socket");
            Self {
                child,
                address,
                dir,
            }
        }
    }

    impl Drop for Daemon {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[derive(Clone, Debug, PartialEq)]
    enum Recorded {
        Select {
            types: u32,
            persist: u32,
            restore: Option<String>,
        },
        Move {
            node: u32,
            x: f64,
            y: f64,
        },
        Button {
            button: i32,
            state: u32,
        },
        Close,
    }

    struct PortalMock {
        state: Arc<Mutex<Vec<Recorded>>>,
    }

    struct SessionObj {
        state: Arc<Mutex<Vec<Recorded>>>,
    }

    fn serve(address: &str, state: Arc<Mutex<Vec<Recorded>>>) -> Connection {
        let mock = PortalMock { state };
        block(async move {
            zbus::connection::Builder::address(address)
                .name(DEST)
                .serve_at(DESKTOP_PATH, mock)
                .method_timeout(Duration::from_secs(15))
                .build()
                .await
                .unwrap()
        })
    }

    #[interface(interface = "org.freedesktop.portal.RemoteDesktop")]
    impl PortalMock {
        async fn create_session(
            &self,
            options: HashMap<String, OwnedVariant>,
            #[zbus(header)] header: Header<'_>,
            #[zbus(connection)] conn: &Connection,
            #[zbus(object_server)] server: &ObjectServer,
        ) -> zbus::fdo::Result<ResponseDispatchNotifier<OwnedObjectPath>> {
            let request = request_path(&header, &options)?;
            let session = session_path(&header, &options)?;
            server
                .at(
                    session.clone(),
                    SessionObj {
                        state: Arc::clone(&self.state),
                    },
                )
                .await
                .map_err(|err| zbus::fdo::Error::Failed(err.to_string()))?;
            let mut results = Options::new();
            results.insert("session_handle".into(), path_variant(&session));
            Ok(emit_later(conn, request, 0, results))
        }

        fn select_devices(
            &self,
            _session: OwnedObjectPath,
            options: HashMap<String, OwnedVariant>,
            #[zbus(header)] header: Header<'_>,
            #[zbus(connection)] conn: &Connection,
        ) -> zbus::fdo::Result<ResponseDispatchNotifier<OwnedObjectPath>> {
            self.state.lock().unwrap().push(Recorded::Select {
                types: options.get("types").and_then(as_u32).unwrap_or(0),
                persist: options.get("persist_mode").and_then(as_u32).unwrap_or(0),
                restore: options.get("restore_token").and_then(variant_text),
            });
            let request = request_path(&header, &options)?;
            Ok(emit_later(conn, request, 0, Options::new()))
        }

        fn start(
            &self,
            _session: OwnedObjectPath,
            _parent: String,
            options: HashMap<String, OwnedVariant>,
            #[zbus(header)] header: Header<'_>,
            #[zbus(connection)] conn: &Connection,
        ) -> zbus::fdo::Result<ResponseDispatchNotifier<OwnedObjectPath>> {
            let request = request_path(&header, &options)?;
            let mut results = Options::new();
            results.insert("devices".into(), u32_variant(POINTER));
            results.insert("restore_token".into(), string_variant("granted-1"));
            results.insert("streams".into(), streams_variant());
            Ok(emit_later(conn, request, 0, results))
        }

        fn notify_pointer_motion_absolute(
            &self,
            _session: OwnedObjectPath,
            _options: HashMap<String, OwnedVariant>,
            stream: u32,
            x: f64,
            y: f64,
        ) {
            self.state
                .lock()
                .unwrap()
                .push(Recorded::Move { node: stream, x, y });
        }

        fn notify_pointer_button(
            &self,
            _session: OwnedObjectPath,
            _options: HashMap<String, OwnedVariant>,
            button: i32,
            state: u32,
        ) {
            self.state
                .lock()
                .unwrap()
                .push(Recorded::Button { button, state });
        }
    }

    #[interface(interface = "org.freedesktop.portal.Session")]
    impl SessionObj {
        fn close(&self) {
            self.state.lock().unwrap().push(Recorded::Close);
        }
    }

    fn request_path(
        header: &Header<'_>,
        options: &HashMap<String, OwnedVariant>,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        portal_path(header, options, "request", "handle_token")
    }

    fn session_path(
        header: &Header<'_>,
        options: &HashMap<String, OwnedVariant>,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        portal_path(header, options, "session", "session_handle_token")
    }

    fn portal_path(
        header: &Header<'_>,
        options: &HashMap<String, OwnedVariant>,
        kind: &str,
        key: &str,
    ) -> zbus::fdo::Result<OwnedObjectPath> {
        let sender = header
            .sender()
            .ok_or_else(|| zbus::fdo::Error::Failed("missing sender".into()))?;
        let token = options
            .get(key)
            .and_then(variant_text)
            .unwrap_or_else(next_token);
        OwnedObjectPath::try_from(object_path(kind, sender.as_str(), &token))
            .map_err(|err| zbus::fdo::Error::Failed(err.to_string()))
    }

    fn emit_later(
        conn: &Connection,
        path: OwnedObjectPath,
        code: u32,
        results: HashMap<String, OwnedVariant>,
    ) -> ResponseDispatchNotifier<OwnedObjectPath> {
        let (notifier, listener) = ResponseDispatchNotifier::new(path.clone());
        let conn = conn.clone();
        let task_conn = conn.clone();
        conn.spawn("featherclick-portal-response", async move {
            listener.await;
            if let Ok(message) =
                Message::signal(path.as_str(), REQUEST_IFACE, "Response").build(&(code, results))
            {
                let _ = task_conn.send(&message).await;
            }
        })
        .detach();
        notifier
    }

    fn path_variant(path: &OwnedObjectPath) -> OwnedVariant {
        OwnedVariant::try_from(Variant::new(path.clone())).expect("object path")
    }

    fn pair_variant(x: i32, y: i32) -> OwnedVariant {
        OwnedVariant::try_from(Variant::new((x, y))).expect("pair")
    }

    fn stream_props(
        x: i32,
        y: i32,
        physical: (i32, i32),
        logical: (i32, i32),
    ) -> HashMap<String, OwnedVariant> {
        let mut props = Options::new();
        props.insert("position".into(), pair_variant(x, y));
        props.insert("size".into(), pair_variant(physical.0, physical.1));
        props.insert("logical_size".into(), pair_variant(logical.0, logical.1));
        props
    }

    fn streams_variant() -> OwnedVariant {
        // Node 8 is listed first and its buffer is twice the logical size.
        // A point on the right-hand screen must still land on node 7, which
        // only happens when logical_size is the coordinate space.
        let streams = vec![
            (8u32, stream_props(-1920, 0, (3840, 2160), (1920, 1080))),
            (7u32, stream_props(0, 0, (800, 600), (800, 600))),
        ];
        OwnedVariant::try_from(Variant::new(streams)).expect("streams")
    }
}
