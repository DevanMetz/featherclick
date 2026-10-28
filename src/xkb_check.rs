//! Check the keyboard libraries before winit initializes its event loop.

use std::os::fd::FromRawFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Backend {
    Wayland,
    X11,
}

// Keep selection pure so stale environment variables can be tested without
// mutating the process environment. The probe follows WAYLAND_SOCKET precedence.
fn backend(
    wayland_requested: bool,
    wayland_reachable: bool,
    display: &str,
) -> Result<Backend, &'static str> {
    if wayland_requested && wayland_reachable {
        Ok(Backend::Wayland)
    } else if !display.is_empty() {
        Ok(Backend::X11)
    } else {
        Err("no reachable display; start a desktop session or use --headless")
    }
}

fn display_path(display: &str, runtime: Option<&Path>) -> Option<PathBuf> {
    let path = Path::new(display);
    if path.is_absolute() {
        Some(path.to_path_buf())
    } else {
        runtime
            .filter(|path| path.is_absolute())
            .map(|dir| dir.join(path))
    }
}

fn wayland_reachable(socket: Option<&str>, display: &str, runtime: Option<&Path>) -> bool {
    if let Some(socket) = socket {
        let Ok(fd) = socket.parse::<i32>() else {
            return false;
        };
        if fd < 0 {
            return false;
        }
        // Duplicate rather than consume the inherited connection: winit still
        // needs the original descriptor and WAYLAND_SOCKET environment variable.
        let duplicate = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
        if duplicate < 0 {
            return false;
        }
        // SAFETY: fcntl returned a new descriptor owned by this stream.
        return unsafe { UnixStream::from_raw_fd(duplicate) }
            .peer_addr()
            .is_ok();
    }
    display_path(display, runtime).is_some_and(|path| UnixStream::connect(path).is_ok())
}

#[derive(Clone, Copy)]
enum Library {
    Common,
    X11,
}

fn check_with(
    backend: Option<Backend>,
    load: impl Fn(Library) -> bool,
) -> Result<(), &'static str> {
    if backend.is_none() {
        return Ok(());
    }
    if !load(Library::Common) {
        return Err(
            "cannot load libxkbcommon; install libxkbcommon0 (Debian/Ubuntu) or libxkbcommon (Fedora/Arch)",
        );
    }
    if backend == Some(Backend::X11) && !load(Library::X11) {
        return Err(
            "cannot load libxkbcommon-x11; install libxkbcommon-x11-0 (Debian/Ubuntu) or libxkbcommon-x11 (Fedora/Arch)",
        );
    }
    Ok(())
}

pub fn check() -> Result<Backend, &'static str> {
    let env = |name| std::env::var(name).unwrap_or_default();
    let display = env("WAYLAND_DISPLAY");
    let socket = std::env::var("WAYLAND_SOCKET").ok();
    let requested = !display.is_empty() || socket.as_ref().is_some_and(|value| !value.is_empty());
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from);
    let selected = backend(
        requested,
        requested && wayland_reachable(socket.as_deref(), &display, runtime.as_deref()),
        &env("DISPLAY"),
    )?;
    check_with(Some(selected), |library| match library {
        Library::Common => {
            xkbcommon_dl::xkbcommon_option().is_some()
                && xkbcommon_dl::xkbcommon_compose_option().is_some()
        }
        Library::X11 => xkbcommon_dl::x11::xkbcommon_x11_option().is_some(),
    })?;
    Ok(selected)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_only_reachable_wayland_and_falls_back_to_x11() {
        assert_eq!(backend(true, true, ":0"), Ok(Backend::Wayland));
        assert_eq!(backend(true, false, ":0"), Ok(Backend::X11));
        assert_eq!(backend(false, false, ":0"), Ok(Backend::X11));
        assert_eq!(backend(false, true, ":0"), Ok(Backend::X11));
        assert!(backend(true, false, "").is_err());
        assert!(backend(false, false, "").is_err());
        assert_eq!(backend(true, true, ""), Ok(Backend::Wayland));
    }

    #[test]
    fn fallback_checks_x11_libraries() {
        let selected = backend(true, false, ":0").unwrap();
        assert!(check_with(Some(selected), |lib| matches!(lib, Library::Common)).is_err());
    }

    #[test]
    fn socket_paths_follow_wayland_connection_rules() {
        assert_eq!(
            display_path("/run/wayland-1", None),
            Some(PathBuf::from("/run/wayland-1"))
        );
        assert_eq!(
            display_path("wayland-1", Some(Path::new("/run/user/1"))),
            Some(PathBuf::from("/run/user/1/wayland-1"))
        );
        assert_eq!(display_path("wayland-1", None), None);
        assert_eq!(display_path("wayland-1", Some(Path::new("relative"))), None);
    }

    #[test]
    fn probes_live_and_stale_display_sockets() {
        use std::os::unix::net::UnixListener;
        let path = std::env::current_dir()
            .unwrap()
            .join("target")
            .join(format!("startup-wayland-{}", std::process::id()));
        let listener = UnixListener::bind(&path).unwrap();
        assert!(wayland_reachable(None, path.to_str().unwrap(), None));
        assert!(!wayland_reachable(
            Some("invalid"),
            path.to_str().unwrap(),
            None
        ));
        drop(listener);
        assert!(!wayland_reachable(None, path.to_str().unwrap(), None));
        std::fs::remove_file(&path).unwrap();
        assert!(!wayland_reachable(None, path.to_str().unwrap(), None));
    }

    #[test]
    fn inherited_socket_probe_preserves_the_connection() {
        use std::os::fd::AsRawFd;
        let (socket, _peer) = UnixStream::pair().unwrap();
        let fd = socket.as_raw_fd().to_string();
        assert!(wayland_reachable(Some(&fd), "", None));
        assert!(socket.peer_addr().is_ok());
        assert!(!wayland_reachable(Some("invalid"), "/ignored", None));
        assert!(!wayland_reachable(Some("-1"), "", None));
        assert!(!wayland_reachable(Some(""), "", None));
    }

    #[test]
    fn reports_missing_libraries_for_the_selected_backend() {
        let missing_x11 = |library| matches!(library, Library::Common);
        assert_eq!(
            check_with(Some(Backend::X11), missing_x11),
            Err(
                "cannot load libxkbcommon-x11; install libxkbcommon-x11-0 (Debian/Ubuntu) or libxkbcommon-x11 (Fedora/Arch)"
            )
        );
        assert_eq!(check_with(Some(Backend::Wayland), missing_x11), Ok(()));
        assert_eq!(
            check_with(Some(Backend::Wayland), |_| false),
            Err(
                "cannot load libxkbcommon; install libxkbcommon0 (Debian/Ubuntu) or libxkbcommon (Fedora/Arch)"
            )
        );
        assert_eq!(check_with(None, |_| false), Ok(()));
    }
}
