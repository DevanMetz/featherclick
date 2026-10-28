//! Wayland window icon.
//!
//! winit sets `_NET_WM_ICON` on X11 and leaves `set_window_icon` empty on
//! Wayland. When the compositor offers `xdg-toplevel-icon-v1`, the icon is
//! attached to the `xdg_toplevel` winit already created, on a guest queue of
//! winit's `wl_display`. No second Wayland connection is opened.
//!
//! The name is `featherclick`, matching `assets/featherclick.desktop`. If the
//! protocol is missing, that desktop file remains the icon the launcher shows.

use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::ptr::NonNull;

use wayland_client::protocol::{
    wl_buffer::WlBuffer,
    wl_registry::{self, WlRegistry},
    wl_shm::{self, WlShm},
    wl_shm_pool::WlShmPool,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::xdg::shell::client::xdg_toplevel::XdgToplevel;
use wayland_protocols::xdg::toplevel_icon::v1::client::{
    xdg_toplevel_icon_manager_v1::{self, XdgToplevelIconManagerV1},
    xdg_toplevel_icon_v1::XdgToplevelIconV1,
};

/// Icon name looked up in the theme, and the name of the desktop file.
const ICON_NAME: &str = "featherclick";

/// Why the icon was not set. Callers stay quiet: the desktop entry still applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The compositor has no `xdg_toplevel_icon_manager_v1`.
    Unsupported,
    /// The display, the toplevel, or shared memory could not be used.
    Failed,
}

/// Icon objects that must outlive `set_icon`.
///
/// The protocol keeps the `wl_buffer` referenced until the icon object is
/// destroyed. Drop sends that destroy, then destroys the buffers.
pub struct Session {
    icon: XdgToplevelIconV1,
    buffers: Vec<WlBuffer>,
    pool: WlShmPool,
    conn: Connection,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.icon.destroy();
        for buffer in &self.buffers {
            buffer.destroy();
        }
        self.pool.destroy();
        let _ = self.conn.flush();
    }
}

#[derive(Default)]
struct State {
    shm: Option<WlShm>,
    manager: Option<XdgToplevelIconManagerV1>,
    sizes: Vec<i32>,
    formats: Vec<wl_shm::Format>,
}

impl Dispatch<WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };
        match interface.as_str() {
            "wl_shm" => {
                state.shm = Some(registry.bind(name, version.min(1), qh, ()));
            }
            "xdg_toplevel_icon_manager_v1" => {
                state.manager = Some(registry.bind(name, version.min(1), qh, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<WlShm, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlShm,
        event: wl_shm::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let wl_shm::Event::Format { format } = event
            && let WEnum::Value(format) = format
        {
            state.formats.push(format);
        }
    }
}

impl Dispatch<XdgToplevelIconManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &XdgToplevelIconManagerV1,
        event: xdg_toplevel_icon_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_toplevel_icon_manager_v1::Event::IconSize { size } = event {
            state.sizes.push(size);
        }
    }
}

delegate_noop!(State: ignore WlShmPool);
delegate_noop!(State: ignore WlBuffer);
delegate_noop!(State: ignore XdgToplevelIconV1);

/// Publish `rgba` (top-down, straight alpha) as the toplevel's window icon.
///
/// `display` and `toplevel` are touched only after the byte length is checked,
/// so a rejected image never dereferences them. The returned session owns the
/// shared-memory buffers and must be kept until the window closes.
pub fn publish(
    display: NonNull<std::ffi::c_void>,
    toplevel: NonNull<std::ffi::c_void>,
    rgba: &[u8],
    width: u32,
    height: u32,
) -> Result<Session, Error> {
    if !icon_ok(rgba, width, height) {
        return Err(Error::Failed);
    }

    let conn = guest_connection(display);
    let mut state = State::default();
    let mut queue = conn.new_event_queue::<State>();
    let qh = queue.handle();
    let _registry = conn.display().get_registry(&qh, ());
    // The first roundtrip delivers globals. Binds made while dispatching them
    // stay queued until the next flush, so the second roundtrip is what
    // receives `icon_size` and `wl_shm.format`.
    queue.roundtrip(&mut state).map_err(|_| Error::Failed)?;
    queue.roundtrip(&mut state).map_err(|_| Error::Failed)?;

    let Some(manager) = state.manager.take() else {
        return Err(Error::Unsupported);
    };
    let Some(shm) = state.shm.take() else {
        return Err(Error::Failed);
    };
    if !argb_available(&state.formats) {
        return Err(Error::Failed);
    }

    // Resolve the foreign toplevel before creating buffers, so a bad pointer
    // does not leave icon objects on the compositor.
    let foreign = foreign_toplevel(&conn, toplevel)?;
    let edges = icon_edges(&state.sizes, width);
    let (pool, buffers, _memory) = create_buffers(&shm, &qh, rgba, width, &edges)?;
    let icon = manager.create_icon(&qh, ());
    icon.set_name(ICON_NAME.to_string());
    for buffer in &buffers {
        icon.add_buffer(buffer, 1);
    }
    manager.set_icon(&foreign, Some(&icon));
    if queue.roundtrip(&mut state).is_err() {
        icon.destroy();
        for buffer in &buffers {
            buffer.destroy();
        }
        pool.destroy();
        let _ = conn.flush();
        return Err(Error::Failed);
    }

    Ok(Session {
        icon,
        buffers,
        pool,
        conn,
    })
}

fn icon_ok(rgba: &[u8], width: u32, height: u32) -> bool {
    if width == 0 || height == 0 || width != height || width > 256 {
        return false;
    }
    match (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
    {
        Some(len) => rgba.len() == len,
        None => false,
    }
}

/// Preferred square edges, plus the source edge. `0`, negatives, and anything
/// above 256 are ignored. The source is 64, so it always qualifies.
fn icon_edges(requested: &[i32], native: u32) -> Vec<u32> {
    let mut edges: Vec<u32> = requested
        .iter()
        .copied()
        .filter(|size| (1..=256).contains(size))
        .map(|size| size as u32)
        .collect();
    if (1..=256).contains(&native) {
        edges.push(native);
    }
    edges.sort_unstable();
    edges.dedup();
    edges
}

/// An empty advertisement still tries ARGB8888: every compositor that offers
/// `wl_shm` has to support it, and some do not bother to say so.
fn argb_available(formats: &[wl_shm::Format]) -> bool {
    formats.is_empty() || formats.contains(&wl_shm::Format::Argb8888)
}

/// Nearest-neighbour scale into premultiplied little-endian ARGB8888.
///
/// `wl_shm` stores alpha already multiplied into the color channels. Straight
/// RGBA bytes would composite dark.
fn scale_argb(rgba: &[u8], src_edge: u32, dst_edge: u32) -> Vec<u8> {
    let src_edge = src_edge as usize;
    let dst_edge = dst_edge as usize;
    let mut out = vec![0u8; dst_edge * dst_edge * 4];
    for y in 0..dst_edge {
        let sy = y * src_edge / dst_edge;
        for x in 0..dst_edge {
            let sx = x * src_edge / dst_edge;
            let i = (sy * src_edge + sx) * 4;
            let o = (y * dst_edge + x) * 4;
            let r = u32::from(rgba[i]);
            let g = u32::from(rgba[i + 1]);
            let b = u32::from(rgba[i + 2]);
            let a = u32::from(rgba[i + 3]);
            out[o] = ((b * a + 127) / 255) as u8;
            out[o + 1] = ((g * a + 127) / 255) as u8;
            out[o + 2] = ((r * a + 127) / 255) as u8;
            out[o + 3] = a as u8;
        }
    }
    out
}

fn guest_connection(display: NonNull<std::ffi::c_void>) -> Connection {
    // Safety: `display` is the `wl_display` winit owns for this window. The
    // guest backend does not disconnect it. The caller keeps the session for
    // the life of the window, and this runs on the UI thread inside a frame
    // callback, when winit is not blocked in a Wayland read.
    let backend =
        unsafe { wayland_backend::client::Backend::from_foreign_display(display.as_ptr().cast()) };
    Connection::from_backend(backend)
}

fn foreign_toplevel(
    conn: &Connection,
    toplevel: NonNull<std::ffi::c_void>,
) -> Result<XdgToplevel, Error> {
    // Safety: `toplevel` is winit's `xdg_toplevel`. It stays valid for this
    // call. The id is passed to `set_icon` and is never destroyed here.
    let id = unsafe {
        wayland_backend::client::ObjectId::from_ptr(
            XdgToplevel::interface(),
            toplevel.as_ptr().cast(),
        )
    }
    .map_err(|_| Error::Failed)?;
    XdgToplevel::from_id(conn, id).map_err(|_| Error::Failed)
}

fn create_buffers(
    shm: &WlShm,
    qh: &QueueHandle<State>,
    rgba: &[u8],
    src_edge: u32,
    edges: &[u32],
) -> Result<(WlShmPool, Vec<WlBuffer>, OwnedFd), Error> {
    if edges.is_empty() {
        return Err(Error::Failed);
    }
    let mut bytes = Vec::new();
    let mut layouts = Vec::with_capacity(edges.len());
    for &edge in edges {
        let pixels = scale_argb(rgba, src_edge, edge);
        let offset = bytes.len();
        bytes.extend_from_slice(&pixels);
        layouts.push((edge, offset));
    }
    let memory = memfd_with(&bytes)?;
    let pool = shm.create_pool(memory.as_fd(), bytes.len() as i32, qh, ());
    let mut buffers = Vec::with_capacity(layouts.len());
    for (edge, offset) in layouts {
        let edge = edge as i32;
        buffers.push(pool.create_buffer(
            offset as i32,
            edge,
            edge,
            edge * 4,
            wl_shm::Format::Argb8888,
            qh,
            (),
        ));
    }
    Ok((pool, buffers, memory))
}

fn memfd_with(bytes: &[u8]) -> Result<OwnedFd, Error> {
    if bytes.is_empty() || bytes.len() > i32::MAX as usize {
        return Err(Error::Failed);
    }
    let raw = unsafe {
        libc::memfd_create(
            c"featherclick-icon".as_ptr(),
            libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING,
        )
    };
    if raw < 0 {
        return Err(Error::Failed);
    }
    // Safety: `memfd_create` returned a fresh descriptor, and it is not used
    // anywhere else. `OwnedFd` closes it.
    let file = unsafe { OwnedFd::from_raw_fd(raw) };
    if unsafe { libc::ftruncate(file.as_raw_fd(), bytes.len() as i64) } != 0 {
        return Err(Error::Failed);
    }
    // Safety: the mapping is a shared map of this memfd, `bytes.len()` long.
    // It is written and unmapped before the descriptor is sealed or returned.
    let map = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            bytes.len(),
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            file.as_raw_fd(),
            0,
        )
    };
    if map == libc::MAP_FAILED {
        return Err(Error::Failed);
    }
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), map.cast::<u8>(), bytes.len());
        libc::munmap(map, bytes.len());
        // Sealing is optional. A writable mapping has to be gone first, which
        // it is. Failure leaves the bytes in the memfd either way.
        let _ = libc::fcntl(
            file.as_raw_fd(),
            libc::F_ADD_SEALS,
            libc::F_SEAL_SEAL | libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_WRITE,
        );
    }
    Ok(file)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_a_non_square_icon_without_touching_the_display() {
        let display = NonNull::dangling();
        let toplevel = NonNull::dangling();
        let err = publish(display, toplevel, &[1, 2, 3, 4, 5, 6, 7, 8], 2, 1);
        assert_eq!(err.err(), Some(Error::Failed));
        let err = publish(display, toplevel, &[0; 4], 2, 2);
        assert_eq!(err.err(), Some(Error::Failed));
        let err = publish(display, toplevel, &[], 0, 0);
        assert_eq!(err.err(), Some(Error::Failed));
    }

    #[test]
    fn premultiplies_and_scales_from_the_source_grid() {
        let mut rgba = vec![0u8; 8 * 8 * 4];
        rgba[0] = 10;
        rgba[1] = 20;
        rgba[2] = 30;
        rgba[3] = 128;
        let i = (2 * 8 + 2) * 4;
        rgba[i] = 255;
        rgba[i + 3] = 255;

        let native = scale_argb(&rgba, 8, 8);
        assert_eq!(&native[0..4], &[15, 10, 5, 128]);
        assert_eq!(&native[i..i + 4], &[0, 0, 255, 255]);

        // 1 * 8 / 4 = 2, so the middle of the 4px image is source (2, 2).
        let small = scale_argb(&rgba, 8, 4);
        assert_eq!(&small[0..4], &[15, 10, 5, 128]);
        let (x, y, edge) = (1, 1, 4);
        let mid = (y * edge + x) * 4;
        assert_eq!(&small[mid..mid + 4], &[0, 0, 255, 255]);
    }

    #[test]
    fn keeps_requested_edges_inside_1_to_256_and_the_native_edge_once() {
        assert_eq!(icon_edges(&[0, 4, 8, 1000, -3, 8], 8), vec![4, 8]);
        assert_eq!(icon_edges(&[], 64), vec![64]);
        assert_eq!(icon_edges(&[64], 64), vec![64]);
        assert!(argb_available(&[]));
        assert!(argb_available(&[wl_shm::Format::Argb8888]));
        assert!(!argb_available(&[wl_shm::Format::Xrgb8888]));
    }

    #[test]
    fn sets_the_icon_on_the_toplevel_that_was_created() {
        let rgba = sample_rgba();
        // Drop the session before the stand-in disconnects the display it borrowed.
        let (result, seen) = stand_in(true, |display, toplevel| {
            publish(display, toplevel, &rgba, 8, 8).map(drop)
        });
        if let Err(err) = result {
            panic!("{err:?}\n{seen:#?}");
        }
        assert_eq!(seen.error, None, "{seen:#?}");
        assert!(seen.toplevel.is_some(), "{seen:#?}");
        assert_eq!(seen.icon_toplevel, seen.toplevel, "{seen:#?}");
        assert_eq!(seen.name.as_deref(), Some("featherclick"), "{seen:#?}");
        assert_eq!(seen.buffers.len(), 2, "{seen:#?}");

        let small = buffer(&seen, 4);
        let big = buffer(&seen, 8);
        assert_eq!(small.scale, 1);
        assert_eq!(big.scale, 1);
        assert_eq!(small.height, small.width);
        assert_eq!(big.height, big.width);
        assert_eq!(&small.pixels[0..4], &[15, 10, 5, 128]);
        assert_eq!(&big.pixels[0..4], &[15, 10, 5, 128]);
        let (x, y, edge) = (1, 1, 4);
        let mid = (y * edge + x) * 4;
        let source = (2 * 8 + 2) * 4;
        assert_eq!(&small.pixels[mid..mid + 4], &[0, 0, 255, 255]);
        assert_eq!(&big.pixels[source..source + 4], &[0, 0, 255, 255]);
    }

    #[test]
    fn reports_unsupported_when_the_compositor_has_no_icon_protocol() {
        let rgba = sample_rgba();
        let (result, seen) = stand_in(false, |display, toplevel| {
            publish(display, toplevel, &rgba, 8, 8)
        });
        assert_eq!(result.err(), Some(Error::Unsupported), "{seen:#?}");
        assert_eq!(seen.icon_toplevel, None, "{seen:#?}");
    }

    fn sample_rgba() -> Vec<u8> {
        let mut rgba = vec![0u8; 8 * 8 * 4];
        rgba[0] = 10;
        rgba[1] = 20;
        rgba[2] = 30;
        rgba[3] = 128;
        let i = (2 * 8 + 2) * 4;
        rgba[i] = 255;
        rgba[i + 3] = 255;
        rgba
    }

    fn buffer(seen: &Seen, width: i32) -> &Captured {
        seen.buffers
            .iter()
            .find(|buffer| buffer.width == width)
            .unwrap_or_else(|| panic!("no {width}px buffer in {seen:#?}"))
    }

    fn stand_in<T>(
        with_icon: bool,
        body: impl FnOnce(NonNull<std::ffi::c_void>, NonNull<std::ffi::c_void>) -> T,
    ) -> (T, Seen) {
        use std::os::unix::net::UnixStream;
        use std::sync::{Arc, Mutex};
        use std::thread;

        let (client, server) = UnixStream::pair().expect("socketpair");
        let seen = Arc::new(Mutex::new(Seen::default()));
        let seen_thread = Arc::clone(&seen);
        let server = thread::spawn(move || serve(server, seen_thread, with_icon));
        let setup = TestClient::start(client);
        let result = body(setup.display_ptr(), setup.toplevel_ptr());
        drop(setup);
        server.join().expect("stand-in thread");
        let seen = seen.lock().expect("seen").clone();
        (result, seen)
    }

    use std::os::unix::net::UnixStream;
    use std::sync::{Arc, Mutex};

    use wayland_client::protocol::{wl_compositor::WlCompositor, wl_surface::WlSurface};
    use wayland_protocols::xdg::shell::client::{xdg_surface::XdgSurface, xdg_wm_base::XdgWmBase};

    struct TestClient {
        conn: Connection,
        state: TestState,
    }

    #[derive(Default)]
    struct TestState {
        compositor: Option<WlCompositor>,
        wm: Option<XdgWmBase>,
        surface: Option<WlSurface>,
        xdg_surface: Option<XdgSurface>,
        toplevel: Option<XdgToplevel>,
    }

    impl TestClient {
        fn start(stream: UnixStream) -> Self {
            let conn = Connection::from_socket(stream).expect("wayland client");
            let mut queue = conn.new_event_queue::<TestState>();
            let qh = queue.handle();
            let mut state = TestState::default();
            let _registry = conn.display().get_registry(&qh, ());
            queue.roundtrip(&mut state).expect("globals");
            let surface = state
                .compositor
                .as_ref()
                .expect("compositor")
                .create_surface(&qh, ());
            let xdg_surface =
                state
                    .wm
                    .as_ref()
                    .expect("wm_base")
                    .get_xdg_surface(&surface, &qh, ());
            let toplevel = xdg_surface.get_toplevel(&qh, ());
            state.surface = Some(surface);
            state.xdg_surface = Some(xdg_surface);
            state.toplevel = Some(toplevel);
            queue.roundtrip(&mut state).expect("toplevel");
            Self { conn, state }
        }

        fn display_ptr(&self) -> NonNull<std::ffi::c_void> {
            NonNull::new(self.conn.backend().display_ptr().cast()).expect("display pointer")
        }

        fn toplevel_ptr(&self) -> NonNull<std::ffi::c_void> {
            let ptr = self
                .state
                .toplevel
                .as_ref()
                .expect("toplevel")
                .id()
                .as_ptr();
            NonNull::new(ptr.cast()).expect("toplevel pointer")
        }
    }

    impl Dispatch<WlRegistry, ()> for TestState {
        fn event(
            state: &mut Self,
            registry: &WlRegistry,
            event: wl_registry::Event,
            _: &(),
            _: &Connection,
            qh: &QueueHandle<Self>,
        ) {
            let wl_registry::Event::Global {
                name,
                interface,
                version,
            } = event
            else {
                return;
            };
            match interface.as_str() {
                "wl_compositor" => {
                    state.compositor = Some(registry.bind(name, version.min(1), qh, ()));
                }
                "xdg_wm_base" => {
                    state.wm = Some(registry.bind(name, version.min(1), qh, ()));
                }
                _ => {}
            }
        }
    }

    delegate_noop!(TestState: ignore WlCompositor);
    delegate_noop!(TestState: ignore WlSurface);
    delegate_noop!(TestState: ignore XdgWmBase);
    delegate_noop!(TestState: ignore XdgSurface);
    delegate_noop!(TestState: ignore XdgToplevel);

    #[derive(Clone, Debug, Default)]
    struct Seen {
        toplevel: Option<u32>,
        icon_toplevel: Option<u32>,
        name: Option<String>,
        buffers: Vec<Captured>,
        error: Option<String>,
        trace: Vec<String>,
    }

    #[derive(Clone, Debug)]
    struct Captured {
        width: i32,
        height: i32,
        scale: i32,
        pixels: Vec<u8>,
    }

    #[derive(Clone, Debug)]
    enum Obj {
        Display,
        Registry,
        Shm,
        Pool(Vec<u8>),
        Buffer {
            pool: u32,
            offset: i32,
            width: i32,
            height: i32,
            stride: i32,
        },
        Compositor,
        Surface,
        WmBase,
        XdgSurface,
        Toplevel,
        IconManager,
        Icon,
    }

    struct Server {
        stream: UnixStream,
        seen: Arc<Mutex<Seen>>,
        with_icon: bool,
        objs: std::collections::HashMap<u32, Obj>,
        fds: std::collections::VecDeque<OwnedFd>,
        buf: Vec<u8>,
    }

    fn serve(stream: UnixStream, seen: Arc<Mutex<Seen>>, with_icon: bool) {
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(3)));
        let mut server = Server {
            stream,
            seen,
            with_icon,
            objs: std::collections::HashMap::new(),
            fds: std::collections::VecDeque::new(),
            buf: Vec::new(),
        };
        server.objs.insert(1, Obj::Display);
        server.run();
    }

    impl Server {
        fn run(&mut self) {
            loop {
                if let Err(err) = self.drain() {
                    self.fail(err);
                    break;
                }
                match self.recv() {
                    Recv::Data => {}
                    Recv::Eof | Recv::Timeout => break,
                    Recv::Err(err) => {
                        self.fail(err);
                        break;
                    }
                }
            }
        }

        fn drain(&mut self) -> Result<(), String> {
            while self.buf.len() >= 8 {
                let object = u32::from_ne_bytes(self.buf[0..4].try_into().unwrap());
                let word = u32::from_ne_bytes(self.buf[4..8].try_into().unwrap());
                let size = (word >> 16) as usize;
                let opcode = (word & 0xffff) as u16;
                if !(8..=1_048_576).contains(&size) {
                    return Err(format!("bad message size {size}"));
                }
                if self.buf.len() < size {
                    break;
                }
                let body = self.buf[8..size].to_vec();
                self.buf.drain(..size);
                self.dispatch(object, opcode, &body)?;
            }
            Ok(())
        }

        fn dispatch(&mut self, id: u32, opcode: u16, body: &[u8]) -> Result<(), String> {
            let kind = self
                .objs
                .get(&id)
                .cloned()
                .ok_or_else(|| format!("unknown object {id}"))?;
            self.note(format!("{kind:?}@{id} op {opcode}"));
            match (&kind, opcode) {
                (Obj::Display, 0) => {
                    let callback = read_u32(body)?;
                    self.objs.insert(callback, Obj::Display);
                    self.send(callback, 0, &u32_bytes(0))?;
                }
                (Obj::Display, 1) => {
                    let registry = read_u32(body)?;
                    self.objs.insert(registry, Obj::Registry);
                    self.advertise(registry)?;
                }
                (Obj::Registry, 0) => self.bind(body)?,
                (Obj::Shm, 0) => self.create_pool(body)?,
                (Obj::Pool(_), 0) => self.create_buffer(id, body)?,
                (Obj::Pool(_), 1) => {
                    self.objs.remove(&id);
                }
                (Obj::Buffer { .. }, 0) => {
                    self.objs.remove(&id);
                }
                (Obj::Compositor, 0) => {
                    let surface = read_u32(body)?;
                    self.objs.insert(surface, Obj::Surface);
                }
                (Obj::WmBase, 2) => {
                    let mut at = 0;
                    let xdg_surface = read_at(body, &mut at)?;
                    let _surface = read_at(body, &mut at)?;
                    self.objs.insert(xdg_surface, Obj::XdgSurface);
                }
                (Obj::XdgSurface, 1) => {
                    let toplevel = read_u32(body)?;
                    self.objs.insert(toplevel, Obj::Toplevel);
                    let mut seen = self.seen.lock().expect("seen");
                    if seen.toplevel.is_none() {
                        seen.toplevel = Some(toplevel);
                    }
                }
                (Obj::IconManager, 1) => {
                    let icon = read_u32(body)?;
                    self.objs.insert(icon, Obj::Icon);
                }
                (Obj::IconManager, 2) => self.set_icon(body)?,
                (Obj::Icon, 0) => {
                    self.objs.remove(&id);
                }
                (Obj::Icon, 1) => {
                    let name = read_string(body)?.0;
                    self.seen.lock().expect("seen").name = Some(name);
                }
                (Obj::Icon, 2) => self.add_buffer(body)?,
                _ => {
                    return Err(format!(
                        "unhandled {kind:?}@{id} opcode {opcode} ({} bytes)",
                        body.len()
                    ));
                }
            }
            Ok(())
        }

        fn advertise(&mut self, registry: u32) -> Result<(), String> {
            let mut globals = vec![
                (1, "wl_shm", 1u32),
                (2, "wl_compositor", 4),
                (3, "xdg_wm_base", 1),
            ];
            if self.with_icon {
                globals.push((4, "xdg_toplevel_icon_manager_v1", 1));
            }
            for (name, interface, version) in globals {
                let mut body = Vec::new();
                push_u32(&mut body, name);
                push_str(&mut body, interface);
                push_u32(&mut body, version);
                self.send(registry, 0, &body)?;
            }
            Ok(())
        }

        fn bind(&mut self, body: &[u8]) -> Result<(), String> {
            let mut at = 0;
            let _name = read_at(body, &mut at)?;
            let (interface, at_after) = {
                let (interface, next) = read_string(&body[at..])?;
                (interface, at + next)
            };
            at = at_after;
            let _version = read_at(body, &mut at)?;
            let id = read_at(body, &mut at)?;
            self.note(format!("bind {interface} -> {id}"));
            match interface.as_str() {
                "wl_shm" => {
                    self.objs.insert(id, Obj::Shm);
                    self.send(id, 0, &u32_bytes(0))?;
                }
                "wl_compositor" => {
                    self.objs.insert(id, Obj::Compositor);
                }
                "xdg_wm_base" => {
                    self.objs.insert(id, Obj::WmBase);
                }
                "xdg_toplevel_icon_manager_v1" => {
                    self.objs.insert(id, Obj::IconManager);
                    for size in [0, 4, 8, 1000] {
                        self.send(id, 0, &i32_bytes(size))?;
                    }
                    self.send(id, 1, &[])?;
                }
                other => return Err(format!("unexpected bind {other}")),
            }
            Ok(())
        }

        fn create_pool(&mut self, body: &[u8]) -> Result<(), String> {
            let mut at = 0;
            let id = read_at(body, &mut at)?;
            let size = read_at(body, &mut at)? as i32;
            if !(1..=16 * 1024 * 1024).contains(&size) {
                return Err(format!("pool size {size}"));
            }
            let fd = self
                .fds
                .pop_front()
                .ok_or_else(|| "create_pool had no fd".to_string())?;
            let data = read_pool(&fd, size as usize)?;
            self.objs.insert(id, Obj::Pool(data));
            Ok(())
        }

        fn create_buffer(&mut self, pool_id: u32, body: &[u8]) -> Result<(), String> {
            let mut at = 0;
            let id = read_at(body, &mut at)?;
            let offset = read_at(body, &mut at)? as i32;
            let width = read_at(body, &mut at)? as i32;
            let height = read_at(body, &mut at)? as i32;
            let stride = read_at(body, &mut at)? as i32;
            let format = read_at(body, &mut at)?;
            if format != 0 {
                return Err(format!("buffer format {format}"));
            }
            self.objs.insert(
                id,
                Obj::Buffer {
                    pool: pool_id,
                    offset,
                    width,
                    height,
                    stride,
                },
            );
            Ok(())
        }

        fn add_buffer(&mut self, body: &[u8]) -> Result<(), String> {
            let mut at = 0;
            let buffer_id = read_at(body, &mut at)?;
            let scale = read_at(body, &mut at)? as i32;
            let Obj::Buffer {
                pool,
                offset,
                width,
                height,
                stride,
            } = self
                .objs
                .get(&buffer_id)
                .cloned()
                .ok_or_else(|| format!("add_buffer {buffer_id}"))?
            else {
                return Err(format!("add_buffer {buffer_id} is not a buffer"));
            };
            let Some(Obj::Pool(data)) = self.objs.get(&pool) else {
                return Err(format!("buffer {buffer_id} has no pool"));
            };
            let pixels = copy_buffer(data, offset, width, height, stride)?;
            self.seen.lock().expect("seen").buffers.push(Captured {
                width,
                height,
                scale,
                pixels,
            });
            Ok(())
        }

        fn set_icon(&mut self, body: &[u8]) -> Result<(), String> {
            let mut at = 0;
            let toplevel = read_at(body, &mut at)?;
            let icon = read_at(body, &mut at)?;
            if !matches!(self.objs.get(&toplevel), Some(Obj::Toplevel)) {
                return Err(format!("set_icon toplevel {toplevel}"));
            }
            let mut seen = self.seen.lock().expect("seen");
            seen.icon_toplevel = Some(toplevel);
            seen.trace
                .push(format!("set_icon toplevel {toplevel} icon {icon}"));
            Ok(())
        }

        fn send(&mut self, object: u32, opcode: u16, body: &[u8]) -> Result<(), String> {
            if !body.len().is_multiple_of(4) {
                return Err("unaligned event".to_string());
            }
            let size = body.len() + 8;
            let mut out = Vec::with_capacity(size);
            out.extend_from_slice(&object.to_ne_bytes());
            let word = ((size as u32) << 16) | u32::from(opcode);
            out.extend_from_slice(&word.to_ne_bytes());
            out.extend_from_slice(body);
            send_bytes(self.stream.as_raw_fd(), &out)
        }

        fn note(&self, line: String) {
            self.seen.lock().expect("seen").trace.push(line);
        }

        fn fail(&self, err: String) {
            let mut seen = self.seen.lock().expect("seen");
            if seen.error.is_none() {
                seen.error = Some(err);
            }
        }

        fn recv(&mut self) -> Recv {
            let mut bytes = [0u8; 4096];
            let mut control = Aligned([0u8; 256]);
            let mut iov = libc::iovec {
                iov_base: bytes.as_mut_ptr().cast(),
                iov_len: bytes.len(),
            };
            let mut msg = libc::msghdr {
                msg_name: std::ptr::null_mut(),
                msg_namelen: 0,
                msg_iov: &mut iov,
                msg_iovlen: 1,
                msg_control: control.0.as_mut_ptr().cast(),
                msg_controllen: control.0.len(),
                msg_flags: 0,
            };
            let n =
                unsafe { libc::recvmsg(self.stream.as_raw_fd(), &mut msg, libc::MSG_CMSG_CLOEXEC) };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                return match err.kind() {
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut => Recv::Timeout,
                    std::io::ErrorKind::Interrupted => Recv::Data,
                    _ => Recv::Err(err.to_string()),
                };
            }
            if n == 0 {
                return Recv::Eof;
            }
            if msg.msg_flags & libc::MSG_CTRUNC != 0 {
                return Recv::Err("control message truncated".to_string());
            }
            if let Err(err) = self.take_fds(&msg) {
                return Recv::Err(err);
            }
            self.buf.extend_from_slice(&bytes[..n as usize]);
            Recv::Data
        }

        fn take_fds(&mut self, msg: &libc::msghdr) -> Result<(), String> {
            unsafe {
                let mut cmsg = libc::CMSG_FIRSTHDR(msg);
                while !cmsg.is_null() {
                    if (*cmsg).cmsg_level == libc::SOL_SOCKET
                        && (*cmsg).cmsg_type == libc::SCM_RIGHTS
                    {
                        let data = libc::CMSG_DATA(cmsg);
                        let header = libc::CMSG_LEN(0) as usize;
                        let len = (*cmsg).cmsg_len;
                        if len < header {
                            return Err("short cmsg".to_string());
                        }
                        let count = (len - header) / std::mem::size_of::<std::os::fd::RawFd>();
                        let fds = data.cast::<std::os::fd::RawFd>();
                        for i in 0..count {
                            let fd = std::ptr::read_unaligned(fds.add(i));
                            if fd >= 0 {
                                self.fds.push_back(OwnedFd::from_raw_fd(fd));
                            }
                        }
                    }
                    cmsg = cmsg_next(msg, cmsg);
                }
            }
            Ok(())
        }
    }

    enum Recv {
        Data,
        Eof,
        Timeout,
        Err(String),
    }

    #[repr(align(8))]
    struct Aligned([u8; 256]);

    fn cmsg_next(msg: &libc::msghdr, cmsg: *const libc::cmsghdr) -> *mut libc::cmsghdr {
        unsafe {
            let len = (*cmsg).cmsg_len;
            if len < std::mem::size_of::<libc::cmsghdr>() {
                return std::ptr::null_mut();
            }
            let align = std::mem::size_of::<usize>();
            let aligned = (len + align - 1) & !(align - 1);
            let next = (cmsg.cast::<u8>())
                .add(aligned)
                .cast_mut()
                .cast::<libc::cmsghdr>();
            let end = msg.msg_control.cast::<u8>().add(msg.msg_controllen);
            if next.cast::<u8>().add(std::mem::size_of::<libc::cmsghdr>()) > end {
                std::ptr::null_mut()
            } else {
                next
            }
        }
    }

    fn read_pool(fd: &OwnedFd, len: usize) -> Result<Vec<u8>, String> {
        let mut buf = vec![0u8; len];
        let mut got = 0;
        while got < len {
            let n = unsafe {
                libc::pread(
                    fd.as_raw_fd(),
                    buf[got..].as_mut_ptr().cast(),
                    len - got,
                    got as i64,
                )
            };
            if n < 0 {
                return Err(format!("pread: {}", std::io::Error::last_os_error()));
            }
            if n == 0 {
                break;
            }
            got += n as usize;
        }
        if got != len {
            return Err(format!("short pool {got}/{len}"));
        }
        Ok(buf)
    }

    fn copy_buffer(
        pool: &[u8],
        offset: i32,
        width: i32,
        height: i32,
        stride: i32,
    ) -> Result<Vec<u8>, String> {
        if width <= 0 || height <= 0 || stride < width * 4 || offset < 0 {
            return Err(format!("bad buffer {width}x{height} stride {stride}"));
        }
        let mut out = Vec::with_capacity((width * height * 4) as usize);
        for y in 0..height {
            let start = offset as usize + y as usize * stride as usize;
            let end = start + width as usize * 4;
            let row = pool
                .get(start..end)
                .ok_or_else(|| format!("buffer row {y} outside pool"))?;
            out.extend_from_slice(row);
        }
        Ok(out)
    }

    fn read_u32(body: &[u8]) -> Result<u32, String> {
        let mut at = 0;
        let value = read_at(body, &mut at)?;
        if at != body.len() {
            return Err("trailing bytes".to_string());
        }
        Ok(value)
    }

    fn read_at(body: &[u8], at: &mut usize) -> Result<u32, String> {
        let next = at.checked_add(4).ok_or("overflow")?;
        if next > body.len() {
            return Err("short body".to_string());
        }
        let value = u32::from_ne_bytes(body[*at..next].try_into().unwrap());
        *at = next;
        Ok(value)
    }

    fn read_string(body: &[u8]) -> Result<(String, usize), String> {
        if body.len() < 4 {
            return Err("short string".to_string());
        }
        let len = u32::from_ne_bytes(body[0..4].try_into().unwrap()) as usize;
        let pad = (4 - (len % 4)) % 4;
        let end = 4 + len + pad;
        if len == 0 || end > body.len() {
            return Err("bad string".to_string());
        }
        let bytes = &body[4..4 + len];
        let text_end = bytes.iter().position(|byte| *byte == 0).unwrap_or(len);
        Ok((
            String::from_utf8_lossy(&bytes[..text_end]).into_owned(),
            end,
        ))
    }

    fn push_u32(body: &mut Vec<u8>, value: u32) {
        body.extend_from_slice(&value.to_ne_bytes());
    }

    fn push_str(body: &mut Vec<u8>, text: &str) {
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(0);
        push_u32(body, bytes.len() as u32);
        body.extend_from_slice(&bytes);
        while !body.len().is_multiple_of(4) {
            body.push(0);
        }
    }

    fn u32_bytes(value: u32) -> Vec<u8> {
        value.to_ne_bytes().to_vec()
    }

    fn i32_bytes(value: i32) -> Vec<u8> {
        value.to_ne_bytes().to_vec()
    }

    fn send_bytes(fd: std::os::fd::RawFd, bytes: &[u8]) -> Result<(), String> {
        let mut sent = 0;
        while sent < bytes.len() {
            let n = unsafe {
                libc::send(
                    fd,
                    bytes[sent..].as_ptr().cast(),
                    bytes.len() - sent,
                    libc::MSG_NOSIGNAL,
                )
            };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(err.to_string());
            }
            if n == 0 {
                return Err("socket closed".to_string());
            }
            sent += n as usize;
        }
        Ok(())
    }
}
