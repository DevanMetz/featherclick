//! The one connection a run uses to move and click.
//!
//! On Wayland, a compositor that offers a virtual pointer is used directly, and
//! the point is mapped across the whole layout. enigo is the fallback: X11,
//! Windows, macOS, and a Wayland session whose compositor has no virtual
//! pointer (GNOME and KDE). enigo is not opened as well as the virtual
//! pointer, because it would deliver the same click a second time through
//! XWayland.

use enigo::{Button, Coordinate, Direction, Enigo, Mouse, Settings as EnigoSettings};

#[cfg(target_os = "linux")]
use crate::wayland_pointer::WaylandPointer;

pub struct Input {
    inner: Inner,
}

enum Inner {
    Enigo(Box<Enigo>),
    #[cfg(target_os = "linux")]
    Wayland(Box<WaylandPointer>),
    #[cfg(target_os = "linux")]
    Portal(Box<crate::portal::PortalPointer>),
}

pub fn connect() -> Result<Input, String> {
    #[cfg(target_os = "linux")]
    if crate::wayland_probe::prefer_portal() {
        return match crate::portal::PortalPointer::open() {
            Ok(portal) => Ok(Input {
                inner: Inner::Portal(Box::new(portal)),
            }),
            Err(err) => enigo().map_err(|_| err),
        };
    }
    #[cfg(target_os = "linux")]
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        match WaylandPointer::connect() {
            Ok(pointer) => {
                return Ok(Input {
                    inner: Inner::Wayland(Box::new(pointer)),
                });
            }
            Err(wayland_error) => {
                return match enigo() {
                    Ok(input) => Ok(input),
                    Err(enigo_error) => Err(format!("{wayland_error}; {enigo_error}")),
                };
            }
        }
    }
    enigo()
}

/// Headless runs need no window handle: the portal accepts an empty parent
/// and reuses the same private restore token as GUI runs.
pub(crate) fn connect_headless() -> Result<Input, String> {
    #[cfg(target_os = "linux")]
    {
        connect_headless_with(
            crate::wayland_probe::wayland_session(),
            |backend| match backend {
                HeadlessBackend::VirtualPointer => WaylandPointer::connect().map(|pointer| Input {
                    inner: Inner::Wayland(Box::new(pointer)),
                }),
                HeadlessBackend::Portal => {
                    crate::portal::PortalPointer::open().map(|pointer| Input {
                        inner: Inner::Portal(Box::new(pointer)),
                    })
                }
                HeadlessBackend::X11 => enigo(),
            },
        )
    }
    #[cfg(not(target_os = "linux"))]
    connect()
}

#[cfg(target_os = "linux")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HeadlessBackend {
    VirtualPointer,
    Portal,
    X11,
}

#[cfg(target_os = "linux")]
fn connect_headless_with<T>(
    wayland: bool,
    mut open: impl FnMut(HeadlessBackend) -> Result<T, String>,
) -> Result<T, String> {
    let mut errors = Vec::new();
    let backends: &[HeadlessBackend] = if wayland {
        &[
            HeadlessBackend::VirtualPointer,
            HeadlessBackend::Portal,
            HeadlessBackend::X11,
        ]
    } else {
        &[HeadlessBackend::X11]
    };
    for &backend in backends {
        match open(backend) {
            Ok(input) => return Ok(input),
            Err(error) => errors.push(error),
        }
    }
    Err(format!("no input backend works: {}", errors.join("; ")))
}

fn enigo() -> Result<Input, String> {
    let settings = EnigoSettings {
        // Every extra millisecond of per-event delay is a millisecond off the
        // achievable click rate.
        linux_delay: 0,
        wayland_display: Some("featherclick-no-display".into()),
        ..EnigoSettings::default()
    };
    Enigo::new(&settings)
        .map(|enigo| Input {
            inner: Inner::Enigo(Box::new(enigo)),
        })
        .map_err(|err| format!("cannot simulate input: {err}"))
}

impl Input {
    pub fn location(&self) -> Result<(i32, i32), String> {
        match &self.inner {
            Inner::Enigo(enigo) => enigo.location().map_err(|err| err.to_string()),
            #[cfg(target_os = "linux")]
            Inner::Portal(pointer) => pointer.location(),
            #[cfg(target_os = "linux")]
            Inner::Wayland(_) => {
                Err("the wayland virtual pointer does not report a position".to_string())
            }
        }
    }

    pub fn move_abs(&mut self, x: i32, y: i32) -> Result<(), String> {
        match &mut self.inner {
            Inner::Enigo(enigo) => enigo
                .move_mouse(x, y, Coordinate::Abs)
                .map_err(|err| err.to_string()),
            #[cfg(target_os = "linux")]
            Inner::Wayland(pointer) => pointer.move_abs(x, y),
            #[cfg(target_os = "linux")]
            Inner::Portal(pointer) => pointer.move_abs(x, y),
        }
    }

    pub fn button(&mut self, button: Button, direction: Direction) -> Result<(), String> {
        match &mut self.inner {
            Inner::Enigo(enigo) => enigo
                .button(button, direction)
                .map_err(|err| err.to_string()),
            #[cfg(target_os = "linux")]
            Inner::Wayland(pointer) => wayland_button(pointer, button, direction),
            #[cfg(target_os = "linux")]
            Inner::Portal(pointer) => pointer.button(button, direction),
        }
    }
}

#[cfg(target_os = "linux")]
fn wayland_button(
    pointer: &mut WaylandPointer,
    button: Button,
    direction: Direction,
) -> Result<(), String> {
    // linux/input-event-codes.h
    let code = match button {
        Button::Left => 0x110,
        Button::Right => 0x111,
        Button::Middle => 0x112,
        _ => return Err(format!("button {button:?} is not supported on wayland")),
    };
    match direction {
        Direction::Press => pointer.button(code, true),
        Direction::Release => pointer.button(code, false),
        Direction::Click => {
            pointer.button(code, true)?;
            pointer.button(code, false)
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    /// Moves the pointer. Run under Xvfb with `WAYLAND_DISPLAY` unset:
    /// `xvfb-run -a cargo test --bin featherclick -- --ignored x11_move`.
    #[test]
    #[ignore = "moves the pointer; run under xvfb"]
    fn x11_move_round_trips() {
        let mut input = connect().expect("Xvfb should accept an X11 connection");
        for point in [(0, 0), (40, 30), (100, 80)] {
            input
                .move_abs(point.0, point.1)
                .unwrap_or_else(|err| panic!("move to {point:?}: {err}"));
            let seen = input.location().unwrap_or_else(|err| panic!("read: {err}"));
            assert_eq!(
                seen, point,
                "the pointer did not land on the requested pixel"
            );
        }
    }
}

impl enigo::Mouse for Input {
    fn button(&mut self, button: Button, direction: Direction) -> enigo::InputResult<()> {
        Input::button(self, button, direction).map_err(enigo::InputError::Mapping)
    }
    fn move_mouse(&mut self, x: i32, y: i32, coordinate: Coordinate) -> enigo::InputResult<()> {
        if coordinate != Coordinate::Abs {
            return Err(enigo::InputError::InvalidInput("relative move unsupported"));
        }
        self.move_abs(x, y).map_err(enigo::InputError::Mapping)
    }
    fn scroll(&mut self, _: i32, _: enigo::Axis) -> enigo::InputResult<()> {
        Err(enigo::InputError::InvalidInput("scroll unsupported"))
    }
    fn main_display(&self) -> enigo::InputResult<(i32, i32)> {
        Err(enigo::InputError::InvalidInput("display size unavailable"))
    }
    fn location(&self) -> enigo::InputResult<(i32, i32)> {
        Input::location(self).map_err(enigo::InputError::Mapping)
    }
}

#[cfg(all(test, target_os = "linux"))]
mod headless_tests {
    use super::*;

    #[test]
    fn wayland_falls_back_in_order_and_stops_at_the_first_connection() {
        use HeadlessBackend::*;
        for (winner, expected) in [
            (VirtualPointer, vec![VirtualPointer]),
            (Portal, vec![VirtualPointer, Portal]),
            (X11, vec![VirtualPointer, Portal, X11]),
        ] {
            let mut attempts = Vec::new();
            let result = connect_headless_with(true, |backend| {
                attempts.push(backend);
                if backend == winner {
                    Ok(backend)
                } else {
                    Err("unavailable".into())
                }
            });
            assert_eq!(result.unwrap(), winner);
            assert_eq!(attempts, expected);
        }
    }

    #[test]
    fn x11_never_requests_portal_permission() {
        let mut attempts = Vec::new();
        let result = connect_headless_with(false, |backend| {
            attempts.push(backend);
            Ok(())
        });
        assert!(result.is_ok());
        assert_eq!(attempts, [HeadlessBackend::X11]);
    }

    #[test]
    fn total_failure_preserves_backend_errors() {
        let error =
            connect_headless_with::<()>(true, |backend| Err(format!("{backend:?} unavailable")))
                .unwrap_err();
        assert!(error.starts_with("no input backend works:"));
        for name in ["VirtualPointer", "Portal", "X11"] {
            assert!(error.contains(name));
        }
    }
}
