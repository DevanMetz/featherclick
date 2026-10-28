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
| Click a fixed point | working | working | working | `src/engine.rs` |
| Pick the pointer position | working | working | working | `src/engine.rs`, `src/ui.rs`, `src/pick_overlay.rs` |
| Jitter (off, box, ellipse, spread) | working | working | partial | `src/engine.rs` |
| Click limit | working | working | working | `src/engine.rs` |
| Time limit | working | working | working | `src/engine.rs` |
| Stop when the pointer moves | working | working | partial | `src/engine.rs` |
| Global hotkeys (toggle and pick) | working | working | working with GlobalShortcuts portal; evdev fallback requires readable `/dev/input/event*` and does not swallow keys | `src/hotkeys.rs`, `src/portal_hotkeys.rs`, `src/evdev_keys.rs` |
| Rebind a hotkey | working | working | working with GlobalShortcuts portal; evdev fallback requires keyboard access | `src/ui.rs`, `src/hotkeys.rs`, `src/portal_hotkeys.rs`, `src/evdev_keys.rs` |
| Dark / light theme | working | working | working | `src/ui.rs` |
| Always on top | working | working | not possible | `src/ui.rs` |
| Window icon | working | working | partial | `src/icon.rs`, `src/main.rs`, `src/wayland_icon.rs`, `assets/featherclick.desktop` |
| Window startup | working | working — checks X11 keyboard libraries after Wayland fallback | working — probes the connection before selecting Wayland | `src/main.rs`, `src/xkb_check.rs` |
| Saved settings | working — atomic saves with unique temp files, corrupt-file backup and defaults | working — same recovery and value clamping | working — same recovery and value clamping | `src/config.rs` |
| Headless mode | working | working | partial — desktop session and compositor permission required; pointer reads may require XWayland | `src/cli.rs`, `src/input.rs`, `src/portal.rs`, `src/engine.rs` |
| Screen coordinates | working | working | not possible | `src/platform.rs` |

## Clicking and fixed points

Wayland clicks use wlroots' virtual pointer (Sway, Hyprland), or the RemoteDesktop portal on GNOME and KDE. enigo's libei portal client is not compiled in because it aborts when `org.freedesktop.portal.Desktop` is absent; the separate portal client reports errors and falls back to X11.

When both X11 and Wayland are available, clicks use one backend;
a separate X11 connection can read the pointer position.

A fixed Wayland point maps across the entire xdg-output logical layout,
including negative coordinates and fractional scaling. The virtual pointer is
unbound and is the only connection used for clicks. The mapping is unit-tested;
it has not been tested on a live Wayland compositor. Jitter, hold, double click,
and headless mode use the same engine.

## Pointer location

The Wayland virtual pointer does not report its position. An X11-only connection
reads it when available; otherwise Pick opens a dimmed fullscreen window on
each output. The next click selects a logical point; Esc cancels. The picker
uses xdg-output origins and logical sizes, including negative origins and
fractional scaling, without requesting portal access. It requires xdg-shell
and xdg-output and has not been tested on a live compositor. Cursor jitter
and stop-on-move still report that the position is unavailable. X11 and
Windows read the pointer directly.

The stop-on-move check itself runs after each click on every platform.

## Hotkeys

`global-hotkey` on X11 uses a grab. Wayland prefers the
[GlobalShortcuts portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.GlobalShortcuts.html)
for toggle and pick, without `/dev/input` access. The desktop controls approval,
key consumption and the final shortcuts; configured keys are preferred triggers
and the desktop may override them. Rebinding closes the old session and registers
the new triggers in a fresh session, which may show another approval dialog.

When the portal is missing, refuses registration, or does not bind both requested
actions, FeatherClick falls back to the existing keyboard device listener.
That fallback requires readable `/dev/input/event*` (typically via the `input`
group) and the focused window also receives the key. XWayland grabs cannot see
native Wayland windows, so grabs are used only in X11 sessions. The on-screen
buttons still work when neither Wayland backend is available.

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


GNOME and KDE use the RemoteDesktop portal when no virtual pointer is available. Permission is requested when a run starts and the restore token is saved with mode 0600. A missing or refused portal falls back to XWayland. Portal clicks use the stream’s logical layout, including negative monitor origins. Picking reads through X11 when available, otherwise uses the native Wayland overlay without opening a portal session.

## Headless mode

CLI runs open no FeatherClick window. On Wayland they try the virtual pointer,
then the RemoteDesktop portal with an empty parent window, then X11/XWayland,
keeping only the first successful input backend. The portal shares the GUI's
private restore token; the compositor decides whether permission can be restored
without a prompt. `--quiet` cannot hide a compositor permission dialog.
Fixed points (including jitter) use logical desktop coordinates without pointer
reads when stop-on-move is off (`--stop-on-move 0` overrides saved tracking). Cursor jitter and stop-on-move still need
readable pointer coordinates, usually through XWayland; unavailable reads fail
the run rather than silently dropping the setting. No working backend or a
runtime failure exits 1; invalid arguments exit 2 with a short error. Live
Wayland compositor behavior has not been verified, so parity remains partial.

