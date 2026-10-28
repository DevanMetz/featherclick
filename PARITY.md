# Linux parity

Windows is the reference. Each row is a user-facing feature or setting, and
the Linux columns are for the code on this branch. X11 and Wayland are split
where they differ.

`working` means the feature behaves as it does on Windows. `partial` means
some sessions or inputs work. `missing` means the control does nothing useful.
`not possible` means a client cannot provide the Windows behavior; the closest
equivalent is named in the note.

| Feature | Windows | X11 | Wayland | Files |
| --- | --- | --- | --- | --- |
| Left, right, and middle click | working | working | partial | `src/engine.rs`, `Cargo.toml` |
| Single, double, and hold | working | working | partial | `src/engine.rs` |
| Fixed, uniform, and human-like intervals | working | working | working | `src/engine.rs`, `src/config.rs` |
| Start delay | working | working | working | `src/engine.rs` |
| Click at the pointer | working | working | partial | `src/engine.rs` |
| Click a fixed point | working | working | partial | `src/engine.rs` |
| Pick the pointer position | working | working | partial | `src/engine.rs`, `src/ui.rs` |
| Jitter (off, box, ellipse, spread) | working | working | partial | `src/engine.rs` |
| Click limit | working | working | working | `src/engine.rs` |
| Time limit | working | working | working | `src/engine.rs` |
| Stop when the pointer moves | working | working | partial | `src/engine.rs` |
| Global hotkeys (toggle and pick) | working | working | partial — readable `/dev/input/event*` required; keys are not swallowed | `src/hotkeys.rs`, `src/evdev_keys.rs` |
| Rebind a hotkey | working | working | partial — requires keyboard access | `src/ui.rs`, `src/hotkeys.rs`, `src/evdev_keys.rs` |
| Dark / light theme | working | working | working | `src/ui.rs` |
| Always on top | working | working | not possible | `src/ui.rs` |
| Window icon | working | working | partial | `src/icon.rs`, `src/main.rs`, `src/wayland_icon.rs`, `assets/featherclick.desktop` |
| Saved settings | working | working | working | `src/config.rs` |
| Headless mode | working | working | partial | `src/cli.rs`, `src/engine.rs` |
| Screen coordinates | working | working | not possible | `src/platform.rs` |

## Clicking and fixed points

Wayland clicks go through wlroots' virtual pointer (Sway, Hyprland). GNOME and
KDE do not offer that protocol. enigo's libei portal client is not compiled
in: it aborts when `org.freedesktop.portal.Desktop` is absent (`Cargo.toml`).

On a session where both an X11 display and a Wayland socket are reachable,
enigo opens both and sends each button and each move to both.

A fixed Wayland point uses the first output's size only, and a negative
coordinate is rejected. Jitter, hold, and double click share that path, so
they are partial on Wayland for the same reason. Headless mode uses the same
engine.

## Pointer location

enigo's Wayland backend does not implement `location()`. The Linux backend
asks Wayland first and does not fall through to X11, so on a Wayland
connection Pick does not update the point, cursor mode keeps the saved point,
and stop-on-move never sees movement. X11 reads the pointer.

The stop-on-move check itself runs after each click on every platform.

## Hotkeys

`global-hotkey` on X11 uses a grab. On Wayland the listener reads keyboard
devices without grabbing them, so the focused window also receives the key.
Access to `/dev/input/event*` is required, typically via the `input` group.
The on-screen buttons still work without access.

## Always on top

Not possible on Wayland. No standard protocol lets an ordinary client keep a
window above others. KDE's plasma-window-management protocol has
`keep_above`, and it says regular clients must not bind it and that only one
client may. Layer-shell would replace the xdg toplevel winit already created.
Sway's sticky flag is a different behavior. The checkbox still sends
`WindowLevel::AlwaysOnTop`; winit ignores it on Wayland. On X11, winit sets
`_NET_WM_STATE_ABOVE`.

## Window icon

Windows embeds `assets/icon.ico` and winit sets the window icons. X11 sets
`_NET_WM_ICON` from the same RGBA image (`src/main.rs`).

On Wayland, winit's `set_window_icon` does nothing. This branch attaches the
icon with `xdg-toplevel-icon-v1` when the compositor offers it, on the
`wl_display` and `xdg_toplevel` winit already owns (`src/wayland_icon.rs`).
The buffer is premultiplied ARGB8888. The icon name is `featherclick`. The
64px image is always sent, plus any edge from 1 to 256 that the compositor
asks for. If the protocol is absent, taskbars keep the desktop-file icon
(`assets/featherclick.desktop`). The window does not say so.

## Screen coordinates

Windows asks for per-monitor DPI v2 so coordinates are physical pixels.
X11 coordinates are pixels. A Wayland client only sees the compositor's
logical layout and cannot ask for physical pixels. The closest equivalent is
that logical point.
