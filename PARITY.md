# Linux parity

User-facing Windows behaviour, and where Linux stands on this branch. X11 clicks were checked on a virtual display. The portal client was checked against a local bus and a stand-in for `org.freedesktop.portal.RemoteDesktop`. This tree has no Wayland compositor, so the wlroots path is the existing enigo client.

| Feature | Windows | Linux X11 | Linux Wayland | Files |
| --- | --- | --- | --- | --- |
| Left, right, and middle click | working | working | partial — wlroots uses the virtual pointer through enigo. GNOME and KDE, which do not offer that protocol, are clicked through the RemoteDesktop portal: one permission dialog, then the grant in `portal-restore`. A refused or missing portal falls back to XWayland windows. If both fail, the portal error is shown | src/engine.rs, src/portal.rs, src/wayland_probe.rs |
| Single, double, and hold | working | working | partial — same path as clicking | src/engine.rs, src/portal.rs |
| Fixed, random, and human-like intervals | working | working | working — nanosleep is already fine-grained; there is no `timeBeginPeriod` equivalent to set | src/engine.rs, src/platform.rs |
| Start delay | working | working | working | src/engine.rs, src/ui.rs |
| Click at the pointer | working | working | partial — a portal click with no move lands on the seat cursor. The position is read from XWayland when `DISPLAY` is set. A pure Wayland session cannot report the pointer | src/engine.rs, src/portal.rs |
| Click a fixed point | working | working | partial — the portal aims at the stream that contains the point, in logical pixels, including a monitor left of the origin. wlroots still uses enigo's first output and rejects a negative coordinate | src/engine.rs, src/portal.rs |
| Pick a position | working | working | partial — read through X11 when `DISPLAY` is set, including before the portal session exists. With only a virtual pointer, or with no XWayland, the position is not reported | src/engine.rs, src/portal.rs |
| Jitter | working | working | partial — a portal move uses the stream layout. Cursor-mode jitter needs the X11 pointer read; without it the click stays on the seat cursor | src/engine.rs, src/portal.rs |
| Click limit and time limit | working | working | working | src/engine.rs |
| Stop when the pointer moves | working | working | partial — needs a pointer read, so it trips only when XWayland can report the position | src/engine.rs, src/portal.rs |
| Global hotkeys | working | working | missing — the hotkey crate only grabs on X11 | src/hotkeys.rs |
| Rebind a hotkey | working | working | missing — same limit as the grab | src/ui.rs, src/hotkeys.rs |
| Dark and light theme | working | working | working | src/ui.rs |
| Always on top | working | working | missing — core Wayland has no client keep-above, and winit's `set_window_level` is a no-op. Layer-shell would change the window type | src/ui.rs |
| Window icon | working | working | missing — `set_window_icon` is a no-op. The launcher icon comes from `featherclick.desktop` | src/icon.rs, assets/ |
| Headless mode | working | working | partial — same click path as the window. The portal dialog, when one is required, is raised on the first run | src/cli.rs, src/engine.rs, src/portal.rs |
| Saved settings | working | working | working | src/config.rs |
| Physical pixel coordinates | working — per-monitor DPI v2 | working — X server pixels | not possible — the portal and the virtual pointer both speak the compositor's logical layout, not the framebuffer | src/platform.rs, src/portal.rs |
| Headless stdout | working — reattach the parent console | working — the process is never detached | working | src/platform.rs |

X11 absolute positions go through XTest as 16-bit root coordinates. A point outside ±32767 cannot be addressed. Ordinary layouts fit, so this stays working.

Pointer position on a pure Wayland session, with no XWayland, is not possible: the virtual pointer and the RemoteDesktop portal do not report where the seat cursor is. The closest equivalent is the X11 query used beside the portal.
