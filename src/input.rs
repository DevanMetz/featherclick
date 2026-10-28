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
