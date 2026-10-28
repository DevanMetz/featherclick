//! GlobalShortcuts uses compositor-owned grabs, without keyboard device access.
use super::{Code, HotKey, Modifiers};
use ordered_stream::{OrderedStream, PollResult};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::Poll;
use std::time::Duration;
use zbus::{Connection, OwnedObjectPath, OwnedVariant, Proxy, Variant};

const DEST: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const IFACE: &str = "org.freedesktop.portal.GlobalShortcuts";
type Options = HashMap<String, OwnedVariant>;
type Shortcuts = Vec<(String, Options)>;

pub(super) struct Listener {
    conn: Connection,
    session: Option<OwnedObjectPath>,
    ids: Vec<u32>,
    task: Option<Pin<Box<dyn Future<Output = std::io::Result<()>> + Send>>>,
}

impl Listener {
    pub(super) fn open() -> Result<Self, String> {
        zbus::block_on(async {
            let conn = zbus::connection::Builder::session()
                .method_timeout(Duration::from_secs(10))
                .build()
                .await
                .map_err(error)?;
            let proxy = Proxy::new(&conn, DEST, PATH, IFACE).await.map_err(error)?;
            let _: u32 = proxy.get_property("version").await.map_err(error)?;
            Ok(Self {
                conn,
                session: None,
                ids: Vec::new(),
                task: None,
            })
        })
    }

    fn close(&mut self) {
        self.task = None;
        for id in self.ids.drain(..) {
            super::publish(id, false);
        }
        if let Some(session) = self.session.take() {
            let _ = zbus::block_on(async {
                Proxy::new(&self.conn, DEST, session, "org.freedesktop.portal.Session")
                    .await?
                    .call_method("Close", &())
                    .await
            });
        }
    }

    pub(super) fn set(
        &mut self,
        toggle: Option<HotKey>,
        capture: Option<HotKey>,
    ) -> Result<(), String> {
        let result = self.bind(toggle, capture);
        if result.is_err() {
            self.close();
        }
        result
    }

    fn bind(&mut self, toggle: Option<HotKey>, capture: Option<HotKey>) -> Result<(), String> {
        self.close();
        let mut shortcuts = Shortcuts::new();
        let mut ids = HashMap::new();
        for (name, description, key) in [
            ("toggle", "Start / stop", toggle),
            ("pick", "Pick position", capture),
        ] {
            if let Some(key) = key {
                let trigger = trigger(key)
                    .ok_or_else(|| format!("cannot register {key} with the shortcuts portal"))?;
                shortcuts.push((
                    name.into(),
                    HashMap::from([
                        ("description".into(), text(description)),
                        ("preferred_trigger".into(), text(&trigger)),
                    ]),
                ));
                ids.insert(name.to_string(), key.id());
            }
        }
        if shortcuts.is_empty() {
            return Ok(());
        }
        zbus::block_on(async {
            let (token, mut options) = request_options();
            options.insert(
                "session_handle_token".into(),
                text(&format!("session{token}")),
            );
            let result = request(&self.conn, "CreateSession", &options, &token).await?;
            let session = match result.get("session_handle").map(|v| &**v) {
                Some(Variant::Str(s)) => OwnedObjectPath::try_from(s.as_str()).map_err(error)?,
                Some(Variant::ObjectPath(p)) => p.to_owned().into(),
                _ => return Err("shortcuts portal did not return a session".into()),
            };
            self.session = Some(session.clone());
            let proxy = Proxy::new(&self.conn, DEST, PATH, IFACE)
                .await
                .map_err(error)?;
            // Subscribe before binding: an activation can arrive with the response.
            let mut signals = Box::pin(proxy.receive_all_signals().await.map_err(error)?);
            let (token, options) = request_options();
            let result = request(
                &self.conn,
                "BindShortcuts",
                &(session.clone(), shortcuts, "", options),
                &token,
            )
            .await?;
            let bound: Shortcuts = result
                .get("shortcuts")
                .ok_or("shortcuts portal returned no shortcuts")?
                .try_clone()
                .map_err(error)?
                .try_into()
                .map_err(error)?;
            if ids
                .keys()
                .any(|id| !bound.iter().any(|(name, _)| name == id))
            {
                return Err("shortcuts portal refused a shortcut".into());
            }
            self.ids = ids.values().copied().collect();
            self.task = Some(Box::pin(self.conn.spawn(
                "featherclick-shortcuts",
                async move {
                    loop {
                        let message = std::future::poll_fn(|cx| {
                            match signals.as_mut().poll_next_before(cx, None) {
                                Poll::Ready(PollResult::Item { data, .. }) => {
                                    Poll::Ready(Some(data))
                                }
                                Poll::Ready(PollResult::Terminated) => Poll::Ready(None),
                                _ => Poll::Pending,
                            }
                        })
                        .await;
                        let Some(message) = message else { break };
                        let pressed = match message.header().member().map(|m| m.as_str()) {
                            Some("Activated") => true,
                            Some("Deactivated") => false,
                            _ => continue,
                        };
                        if let Ok((path, name, _, _)) =
                            message
                                .body()
                                .deserialize::<(OwnedObjectPath, String, u64, Options)>()
                            && path == session
                            && let Some(id) = ids.get(&name)
                        {
                            super::publish(*id, pressed);
                        }
                    }
                },
            )));
            Ok(())
        })
    }
}
impl Drop for Listener {
    fn drop(&mut self) {
        self.close();
    }
}
fn error(e: impl std::fmt::Display) -> String {
    format!("shortcuts portal: {e}")
}
fn text(s: &str) -> OwnedVariant {
    OwnedVariant::try_from(Variant::new(s.to_string())).expect("string variant")
}
fn request_options() -> (String, Options) {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let token = format!("featherclickkeys{}", NEXT.fetch_add(1, Ordering::Relaxed));
    (
        token.clone(),
        HashMap::from([("handle_token".into(), text(&token))]),
    )
}
async fn request<B: serde::Serialize + zbus::DynamicType + Sync>(
    conn: &Connection,
    method: &str,
    body: &B,
    token: &str,
) -> Result<Options, String> {
    let unique = conn
        .unique_name()
        .ok_or("shortcuts portal has no bus name")?
        .as_str()
        .trim_start_matches(':')
        .replace('.', "_");
    let path = format!("/org/freedesktop/portal/desktop/request/{unique}/{token}");
    let proxy = Proxy::new(conn, DEST, path, "org.freedesktop.portal.Request")
        .await
        .map_err(error)?;
    let mut signals = Box::pin(proxy.receive_signal("Response").await.map_err(error)?);
    let desktop = Proxy::new(conn, DEST, PATH, IFACE).await.map_err(error)?;
    let returned: OwnedObjectPath = desktop.call(method, body).await.map_err(error)?;
    // The portal must honor the supplied handle token.
    if returned.as_str() != proxy.path().as_str() {
        return Err("shortcuts portal returned an unexpected request".into());
    }
    let mut timer = Box::pin(thread_timeout(Duration::from_secs(120)));
    let message = std::future::poll_fn(|cx| {
        if timer.as_mut().poll(cx).is_ready() {
            return Poll::Ready(Err("shortcuts portal timed out".into()));
        }
        match signals.as_mut().poll_next_before(cx, None) {
            Poll::Ready(PollResult::Item { data, .. }) => Poll::Ready(Ok(data)),
            Poll::Ready(PollResult::Terminated) => {
                Poll::Ready(Err("shortcuts portal disconnected".into()))
            }
            _ => Poll::Pending,
        }
    })
    .await;
    let message = match message {
        Ok(message) => message,
        Err(err) => {
            let _ = proxy.call_method("Close", &()).await;
            return Err(err);
        }
    };
    let (code, results): (u32, Options) = message.body().deserialize().map_err(error)?;
    if code != 0 {
        return Err("shortcuts portal refused registration".into());
    }
    Ok(results)
}

/// XDG shortcuts syntax uses XKB keysyms and uppercase modifier names.
fn trigger(key: HotKey) -> Option<String> {
    let name = key.key.to_string();
    let symbol = if let Some(letter) = name.strip_prefix("Key") {
        letter.to_ascii_lowercase()
    } else if let Some(digit) = name.strip_prefix("Digit") {
        digit.into()
    } else if (1..=24).any(|n| name == format!("F{n}")) {
        name
    } else {
        match key.key {
            Code::Numpad0 => "KP_0",
            Code::Numpad1 => "KP_1",
            Code::Numpad2 => "KP_2",
            Code::Numpad3 => "KP_3",
            Code::Numpad4 => "KP_4",
            Code::Numpad5 => "KP_5",
            Code::Numpad6 => "KP_6",
            Code::Numpad7 => "KP_7",
            Code::Numpad8 => "KP_8",
            Code::Numpad9 => "KP_9",
            Code::NumpadEnter => "KP_Enter",
            Code::NumpadAdd => "KP_Add",
            Code::NumpadSubtract => "KP_Subtract",
            Code::NumpadMultiply => "KP_Multiply",
            Code::NumpadDivide => "KP_Divide",
            Code::NumpadDecimal => "KP_Decimal",
            Code::NumpadEqual => "KP_Equal",
            Code::PrintScreen => "Print",
            Code::Pause => "Pause",
            Code::NumLock => "Num_Lock",
            Code::ScrollLock => "Scroll_Lock",
            Code::AudioVolumeMute => "XF86AudioMute",
            Code::AudioVolumeDown => "XF86AudioLowerVolume",
            Code::AudioVolumeUp => "XF86AudioRaiseVolume",
            Code::MediaTrackNext => "XF86AudioNext",
            Code::MediaTrackPrevious => "XF86AudioPrev",
            Code::MediaPlayPause | Code::MediaPlay => "XF86AudioPlay",
            Code::MediaPause => "XF86AudioPause",
            Code::MediaStop => "XF86AudioStop",
            Code::Space => "space",
            Code::Enter => "Return",
            Code::Escape => "Escape",
            Code::Tab => "Tab",
            Code::Backspace => "BackSpace",
            Code::Delete => "Delete",
            Code::Insert => "Insert",
            Code::Home => "Home",
            Code::End => "End",
            Code::PageUp => "Prior",
            Code::PageDown => "Next",
            Code::ArrowUp => "Up",
            Code::ArrowDown => "Down",
            Code::ArrowLeft => "Left",
            Code::ArrowRight => "Right",
            Code::Minus => "minus",
            Code::Equal => "equal",
            Code::BracketLeft => "bracketleft",
            Code::BracketRight => "bracketright",
            Code::Backslash => "backslash",
            Code::Semicolon => "semicolon",
            Code::Quote => "apostrophe",
            Code::Backquote => "grave",
            Code::Comma => "comma",
            Code::Period => "period",
            Code::Slash => "slash",
            _ => return None,
        }
        .into()
    };
    let mut parts = Vec::new();
    for (modifier, name) in [
        (Modifiers::CONTROL, "CTRL"),
        (Modifiers::ALT, "ALT"),
        (Modifiers::SHIFT, "SHIFT"),
        (Modifiers::SUPER, "LOGO"),
    ] {
        if key.mods.contains(modifier) {
            parts.push(name.to_string());
        }
    }
    parts.push(symbol);
    Some(parts.join("+"))
}

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::task::{Context, Waker};
use std::thread;
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
    #[test]
    fn triggers_use_xdg_modifiers_and_xkb_keysyms() {
        for (binding, expected) in [
            ("F6", "F6"),
            ("Ctrl+Shift+K", "CTRL+SHIFT+k"),
            ("Super+Alt+1", "ALT+LOGO+1"),
            ("Ctrl+Enter", "CTRL+Return"),
            ("Space", "space"),
            ("ArrowUp", "Up"),
            ("PageDown", "Next"),
            ("Slash", "slash"),
        ] {
            assert_eq!(
                trigger(super::super::parse(binding).unwrap()).as_deref(),
                Some(expected)
            );
        }
        assert_eq!(trigger(HotKey::new(None, Code::Fn)), None);
    }
}

#[cfg(test)]
mod portal_tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::{Child, Command, Stdio};
    use zbus::message::Header;
    use zbus::object_server::ResponseDispatchNotifier;
    use zbus::runtime::traits::TaskHandle;
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
            let dir = std::env::temp_dir().join(format!(
                "featherclick-shortcuts-test-{}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).unwrap();
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

    type Records = Arc<Mutex<Vec<(OwnedObjectPath, Shortcuts)>>>;
    struct Mock {
        records: Records,
    }
    fn path(header: &Header<'_>, options: &Options, kind: &str, key: &str) -> OwnedObjectPath {
        let sender = header
            .sender()
            .unwrap()
            .as_str()
            .trim_start_matches(':')
            .replace('.', "_");
        let Variant::Str(token) = &**options.get(key).unwrap() else {
            panic!("token is a string")
        };
        OwnedObjectPath::try_from(format!(
            "/org/freedesktop/portal/desktop/{kind}/{sender}/{token}"
        ))
        .unwrap()
    }
    fn respond(
        conn: &Connection,
        path: OwnedObjectPath,
        code: u32,
        results: Options,
    ) -> ResponseDispatchNotifier<OwnedObjectPath> {
        let (notifier, ready) = ResponseDispatchNotifier::new(path.clone());
        let bus = conn.clone();
        conn.spawn("shortcuts-test-response", async move {
            ready.await;
            bus.emit_signal(
                None::<&str>,
                path,
                "org.freedesktop.portal.Request",
                "Response",
                &(code, results),
            )
            .await
            .unwrap();
        })
        .detach();
        notifier
    }
    #[zbus::interface(interface = "org.freedesktop.portal.GlobalShortcuts")]
    impl Mock {
        async fn create_session(
            &self,
            options: Options,
            #[zbus(header)] header: Header<'_>,
            #[zbus(connection)] conn: &Connection,
        ) -> ResponseDispatchNotifier<OwnedObjectPath> {
            let session = path(&header, &options, "session", "session_handle_token");
            respond(
                conn,
                path(&header, &options, "request", "handle_token"),
                0,
                HashMap::from([("session_handle".into(), text(session.as_str()))]),
            )
        }
        async fn bind_shortcuts(
            &self,
            session: OwnedObjectPath,
            shortcuts: Shortcuts,
            _parent: String,
            options: Options,
            #[zbus(header)] header: Header<'_>,
            #[zbus(connection)] conn: &Connection,
        ) -> ResponseDispatchNotifier<OwnedObjectPath> {
            let mut records = self.records.lock().unwrap();
            records.push((session, shortcuts));
            let shortcuts = &records.last().unwrap().1;
            // Refuse the third registration, after verifying rebinding.
            let code = if records.len() == 3 { 1 } else { 0 };
            respond(
                conn,
                path(&header, &options, "request", "handle_token"),
                code,
                HashMap::from([(
                    "shortcuts".into(),
                    OwnedVariant::try_from(Variant::new(shortcuts.clone())).unwrap(),
                )]),
            )
        }
    }
    #[test]
    fn binding_recreates_sessions_and_refusal_cleans_up() {
        let daemon = Daemon::start();
        let records = Records::default();
        let _server = zbus::block_on(
            zbus::connection::Builder::address(daemon.address.as_str())
                .name(DEST)
                .serve_at(
                    PATH,
                    Mock {
                        records: records.clone(),
                    },
                )
                .build(),
        )
        .unwrap();
        let conn = zbus::block_on(
            zbus::connection::Builder::address(daemon.address.as_str())
                .method_timeout(Duration::from_secs(2))
                .build(),
        )
        .unwrap();
        let mut listener = Listener {
            conn,
            session: None,
            ids: Vec::new(),
            task: None,
        };
        listener
            .set(super::super::parse("F6"), super::super::parse("F7"))
            .unwrap();
        listener.set(super::super::parse("Ctrl+K"), None).unwrap();
        {
            let records = records.lock().unwrap();
            assert_ne!(records[0].0, records[1].0);
            assert_eq!(records[0].1.len(), 2);
            assert_eq!(records[1].1.len(), 1);
            assert_eq!(
                &*records[1].1[0].1["preferred_trigger"],
                &Variant::new("CTRL+k")
            );
        }
        assert!(listener.set(super::super::parse("F8"), None).is_err());
        assert!(listener.session.is_none());
        assert!(listener.task.is_none());
    }
}
