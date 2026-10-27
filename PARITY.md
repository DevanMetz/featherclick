# Linux parity

User-facing Windows behaviour, and where Linux stands. X11 clicks were checked on a virtual display. Wayland notes are from the code paths and the layout tests: this tree has no Wayland compositor.

| Feature | Windows | Linux X11 | Linux Wayland | Files |
| --- | --- | --- | --- | --- |
| Left, right, and middle click | working | working | partial — wlroots virtual pointer only; GNOME and KDE are left out because enigo's portal client aborts | src/engine.rs, src/input.rs, Cargo.toml |
| Single, double, and hold | working | working | partial — same limit as clicking | src/engine.rs |
| Fixed, random, and human-like intervals | working | working | working — nanosleep is already fine-grained; there is no `timeBeginPeriod` equivalent to set | src/engine.rs, src/platform.rs |
| Start delay | working | working | working | src/engine.rs, src/ui.rs |
| Click at the pointer | working | working | partial — a click with no move is delivered at the seat cursor, but the position cannot be read | src/engine.rs, src/input.rs |
| Click a fixed point | working | working | partial — on wlroots the point is mapped across the whole xdg-output logical layout, including a monitor left of or above the origin. GNOME and KDE still have no virtual pointer. The mapping is unit-tested only | src/pointer_layout.rs, src/wayland_pointer.rs, src/input.rs |
| Pick a position | working | working | partial — with a virtual pointer the position is not reported, so Pick does nothing. Without that protocol, the X11 connection can still read it | src/engine.rs, src/ui.rs, src/input.rs |
| Jitter | working | working | partial — needs a pointer read in cursor mode, and a move in every mode, with the same limit as a fixed point | src/engine.rs |
| Click limit and time limit | working | working | working | src/engine.rs |
| Stop when the pointer moves | working | working | partial — with no pointer position the limit never trips | src/engine.rs |
| Global hotkeys | working | working | missing — the hotkey crate only grabs on X11 | src/hotkeys.rs |
| Rebind a hotkey | working | working | missing — same limit as the grab | src/ui.rs, src/hotkeys.rs |
| Dark and light theme | working | working | working | src/ui.rs |
| Always on top | working | working | missing — core Wayland has no client stacking request, and winit's `set_window_level` is a no-op. Layer-shell would change the window type, and the other protocols are compositor-specific | src/ui.rs |
| Window icon | working | working | missing — `set_window_icon` is a no-op, and xdg-toplevel-icon needs the same connection winit owns. The launcher icon comes from `featherclick.desktop` | src/icon.rs, assets/ |
| Headless mode | working | working | partial — a fixed point needs the virtual pointer, or XWayland when the compositor has none | src/cli.rs, src/engine.rs |
| Saved settings | working | working | working | src/config.rs |
| Physical pixel coordinates | working — per-monitor DPI v2 | working — X server pixels | not possible — Wayland's pointer space is the logical layout, not the framebuffer. A fixed point uses that layout, and xdg-output already includes fractional scale | src/platform.rs, src/pointer_layout.rs |
| Headless stdout | working — reattach the parent console | working — the process is never detached | working | src/platform.rs |

X11 absolute positions go through XTest as 16-bit root coordinates. A point outside ±32767 cannot be addressed. Ordinary layouts fit, so this stays working.
