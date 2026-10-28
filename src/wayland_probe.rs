//! Sees whether the compositor offers `zwlr_virtual_pointer_manager_v1`.
//!
//! The probe only reads the registry. It never creates a pointer, so a check
//! cannot move the cursor or ask for a portal permission.

use std::io::{self, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

const VIRTUAL_POINTER: &str = "zwlr_virtual_pointer_manager_v1";
const PROBE_TIMEOUT: Duration = Duration::from_millis(400);

/// A Wayland session is up. Cached for the process: the compositor is already
/// running when FeatherClick starts.
pub(crate) fn wayland_session() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE
        .get_or_init(|| std::env::var_os("WAYLAND_DISPLAY").is_some_and(|value| !value.is_empty()))
}

/// The compositor advertised a virtual pointer. A missing socket or a timeout
/// counts as absent, so the caller can try the portal.
pub(crate) fn virtual_pointer_present() -> bool {
    static CACHE: OnceLock<bool> = OnceLock::new();
    *CACHE.get_or_init(|| {
        let Ok(display) = std::env::var("WAYLAND_DISPLAY") else {
            return false;
        };
        if display.is_empty() {
            return false;
        }
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
        let path = socket_path(display.as_ref(), runtime.as_deref());
        probe_socket(&path).unwrap_or(false)
    })
}

/// Portal clicks are for a Wayland session that has no virtual pointer.
/// wlroots keeps the enigo path, and a plain X11 session never asks.
pub(crate) fn prefer_portal() -> bool {
    prefer_portal_when(wayland_session(), virtual_pointer_present())
}

pub(crate) fn prefer_portal_when(wayland: bool, virtual_pointer: bool) -> bool {
    wayland && !virtual_pointer
}

pub(crate) fn socket_path(display: &str, runtime_dir: Option<&Path>) -> PathBuf {
    let path = Path::new(display);
    if path.is_absolute() {
        return path.to_path_buf();
    }
    runtime_dir.unwrap_or_else(|| Path::new("/tmp")).join(path)
}

/// Connects to one compositor socket and reports whether the virtual-pointer
/// global is in the registry. `Ok(false)` means the registry was read and the
/// global was not there.
pub(crate) fn probe_socket(path: &Path) -> io::Result<bool> {
    let mut sock = UnixStream::connect(path)?;
    sock.set_read_timeout(Some(PROBE_TIMEOUT))?;
    sock.set_write_timeout(Some(PROBE_TIMEOUT))?;

    // wl_display.get_registry (opcode 1) binds the registry as object 2.
    // wl_display.sync (opcode 0) binds a callback as object 3; its `done`
    // event means every global has arrived.
    write_request(&mut sock, 1, 1, &2u32.to_le_bytes())?;
    write_request(&mut sock, 1, 0, &3u32.to_le_bytes())?;

    let mut found = false;
    loop {
        let (object, opcode, payload) = read_event(&mut sock)?;
        if object == 3 && opcode == 0 {
            return Ok(found);
        }
        if object == 1 && opcode == 0 {
            return Ok(false);
        }
        if object == 2 && opcode == 0 && global_name(&payload) == Some(VIRTUAL_POINTER) {
            found = true;
        }
    }
}

fn write_request(
    sock: &mut UnixStream,
    object: u32,
    opcode: u16,
    payload: &[u8],
) -> io::Result<()> {
    let size = u32::try_from(8 + payload.len()).unwrap_or(u32::MAX);
    let mut header = [0u8; 8];
    header[..4].copy_from_slice(&object.to_le_bytes());
    header[4..].copy_from_slice(&(size << 16 | u32::from(opcode)).to_le_bytes());
    sock.write_all(&header)?;
    sock.write_all(payload)?;
    Ok(())
}

fn read_event(sock: &mut UnixStream) -> io::Result<(u32, u16, Vec<u8>)> {
    let mut header = [0u8; 8];
    read_exact(sock, &mut header)?;
    let object = u32::from_le_bytes(header[..4].try_into().unwrap_or([0; 4]));
    let packed = u32::from_le_bytes(header[4..].try_into().unwrap_or([0; 4]));
    let size = (packed >> 16) as usize;
    let opcode = (packed & 0xffff) as u16;
    if !(8..=8192).contains(&size) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "wayland event size",
        ));
    }
    let mut payload = vec![0u8; size - 8];
    read_exact(sock, &mut payload)?;
    Ok((object, opcode, payload))
}

fn read_exact(sock: &mut UnixStream, buf: &mut [u8]) -> io::Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        match sock.read(&mut buf[filled..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "wayland socket closed",
                ));
            }
            Ok(n) => filled += n,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

/// `wl_registry.global`: u32 name, string interface, u32 version.
fn global_name(payload: &[u8]) -> Option<&str> {
    if payload.len() < 4 {
        return None;
    }
    let mut offset = 4;
    read_string(payload, &mut offset)
}

fn read_string<'a>(payload: &'a [u8], offset: &mut usize) -> Option<&'a str> {
    if *offset + 4 > payload.len() {
        return None;
    }
    let len = u32::from_le_bytes(payload[*offset..*offset + 4].try_into().ok()?) as usize;
    *offset += 4;
    if *offset + len > payload.len() {
        return None;
    }
    let bytes = &payload[*offset..*offset + len];
    *offset += len + (4 - (len % 4)) % 4;
    let bytes = bytes.strip_suffix(&[0]).unwrap_or(bytes);
    std::str::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;
    use std::os::unix::net::UnixListener;
    use std::thread;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn portal_is_only_for_wayland_without_a_virtual_pointer() {
        assert!(!prefer_portal_when(false, false));
        assert!(!prefer_portal_when(false, true));
        assert!(!prefer_portal_when(true, true));
        assert!(prefer_portal_when(true, false));
    }

    #[test]
    fn socket_path_keeps_an_absolute_display_and_joins_a_relative_one() {
        assert_eq!(
            socket_path("/tmp/wayland-9", None),
            PathBuf::from("/tmp/wayland-9")
        );
        assert_eq!(
            socket_path("wayland-1", Some(Path::new("/run/user/1000"))),
            PathBuf::from("/run/user/1000/wayland-1")
        );
        assert_eq!(
            socket_path("wayland-0", None),
            PathBuf::from("/tmp/wayland-0")
        );
    }

    #[test]
    fn a_missing_socket_is_an_error() {
        let err = probe_socket(Path::new("/tmp/featherclick-no-such-wayland-socket")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn the_probe_sees_only_the_virtual_pointer_global() {
        let (present, present_server) =
            serve(&["wl_shm", "zwlr_virtual_pointer_v1", VIRTUAL_POINTER]);
        assert!(probe_socket(&present).unwrap());
        present_server.join().unwrap();

        let (absent, absent_server) = serve(&["wl_shm", "zwlr_virtual_pointer_manager_v1_extra"]);
        assert!(!probe_socket(&absent).unwrap());
        absent_server.join().unwrap();
    }

    fn serve(globals: &[&str]) -> (PathBuf, thread::JoinHandle<()>) {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("featherclick-wl-{}-{nanos}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        let interfaces: Vec<String> = globals.iter().map(|name| (*name).to_string()).collect();
        let server = thread::spawn(move || {
            let (mut sock, _) = listener.accept().unwrap();
            let mut request = [0u8; 24];
            sock.read_exact(&mut request).unwrap();
            let mut bytes = Vec::new();
            for (index, interface) in interfaces.iter().enumerate() {
                bytes.extend(global(index as u32 + 1, interface));
            }
            bytes.extend(callback_done());
            sock.write_all(&bytes).unwrap();
        });
        (path, server)
    }

    fn global(name: u32, interface: &str) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&name.to_le_bytes());
        payload.extend(wayland_string(interface));
        payload.extend_from_slice(&1u32.to_le_bytes());
        message(2, 0, &payload)
    }

    fn callback_done() -> Vec<u8> {
        message(3, 0, &0u32.to_le_bytes())
    }

    fn wayland_string(text: &str) -> Vec<u8> {
        let mut bytes = text.as_bytes().to_vec();
        bytes.push(0);
        let pad = (4 - (bytes.len() % 4)) % 4;
        let len = bytes.len() as u32;
        bytes.extend(std::iter::repeat_n(0, pad));
        let mut out = Vec::new();
        out.extend_from_slice(&len.to_le_bytes());
        out.extend_from_slice(&bytes);
        out
    }

    fn message(object: u32, opcode: u16, payload: &[u8]) -> Vec<u8> {
        let size = (8 + payload.len()) as u32;
        let mut out = Vec::new();
        out.extend_from_slice(&object.to_le_bytes());
        out.extend_from_slice(&(size << 16 | u32::from(opcode)).to_le_bytes());
        out.extend_from_slice(payload);
        out
    }
}
