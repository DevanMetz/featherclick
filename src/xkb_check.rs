//! Check the keyboard libraries before winit initializes its event loop.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Backend {
    Wayland,
    X11,
}

// Match winit's default selection: Wayland takes precedence over X11.
fn backend(wayland_display: &str, wayland_socket: &str, display: &str) -> Option<Backend> {
    if !wayland_display.is_empty() || !wayland_socket.is_empty() {
        Some(Backend::Wayland)
    } else if !display.is_empty() {
        Some(Backend::X11)
    } else {
        None
    }
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

pub fn check() -> Result<(), &'static str> {
    let env = |name| std::env::var(name).unwrap_or_default();
    let selected = backend(
        &env("WAYLAND_DISPLAY"),
        &env("WAYLAND_SOCKET"),
        &env("DISPLAY"),
    );
    check_with(selected, |library| match library {
        Library::Common => {
            xkbcommon_dl::xkbcommon_option().is_some()
                && xkbcommon_dl::xkbcommon_compose_option().is_some()
        }
        Library::X11 => xkbcommon_dl::x11::xkbcommon_x11_option().is_some(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selects_the_same_backend_as_winit() {
        assert_eq!(backend("wayland-0", "", ":0"), Some(Backend::Wayland));
        assert_eq!(backend("", "3", ":0"), Some(Backend::Wayland));
        assert_eq!(backend("", "", ":0"), Some(Backend::X11));
        assert_eq!(backend("", "", ""), None);
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
