# Linux parity

User-facing Windows behaviour, and where Linux stands. X11 was checked on this tree (clicks, the F8 grab, always-on-top). Wayland notes are from the code paths, since a Wayland session is not available here.

| Feature | Windows | Linux X11 | Linux Wayland | Files |
| --- | --- | --- | --- | --- |
| Left, right, and middle click | working | working | partial — wlroots virtual pointer only; GNOME and KDE are left out because enigo's portal client aborts when no portal is present | `src/engine.rs`, `Cargo.toml` |
| Single, double, and hold | working | working | partial — same limit as clicking | `src/engine.rs` |
| Fixed, random, and human-like intervals | working | working | working — `nanosleep` is already fine-grained, so there is no `timeBeginPeriod` step | `src/engine.rs`, `src/platform.rs` |
| Start delay | working | working | working | `src/engine.rs`, `src/ui.rs` |
| Click at the pointer | working | working | partial — the virtual pointer cannot report a position, so cursor mode, jitter, and stop-on-move have nothing to measure | `src/engine.rs` |
| Click a fixed point | working | working | partial — absolute motion uses the first output, and negative coordinates are rejected | `src/engine.rs` |
| Pick a position | working | working | partial — fails when the pointer position cannot be read | `src/engine.rs`, `src/ui.rs` |
| Jitter (box, ellipse, spread) | working | working | partial — needs a pointer move, same limit as a fixed point | `src/engine.rs` |
| Click limit and time limit | working | working | working | `src/engine.rs` |
| Stop when the pointer moves | working | working | partial — with no pointer position the limit never trips | `src/engine.rs` |
| Global hotkeys | working | working | missing — no global grab; the buttons are the fallback | `src/hotkeys.rs`, `src/ui.rs` |
| Rebind a hotkey | working | working | partial — the window can capture a chord, but it is not registered globally | `src/ui.rs`, `src/hotkeys.rs` |
| Dark and light theme | working | working | working | `src/ui.rs` |
| Always on top | working | working | missing — core Wayland has no client stacking request, and winit's `set_window_level` is a no-op. Layer-shell would cover wlroots only, and it is not a normal window | `src/ui.rs` |
| Window icon | working | working | missing — `set_window_icon` is a no-op. The launcher still uses `featherclick.desktop` | `src/icon.rs`, `assets/` |
| Headless mode | working | working | partial — needs the virtual pointer, or XWayland | `src/cli.rs`, `src/engine.rs` |
| Saved settings | working | working | working | `src/config.rs` |
| Physical pixel coordinates | working — per-monitor DPI v2 | working — X server pixels | partial — the virtual pointer's extent is one output, with no fractional-scale adjustment | `src/platform.rs`, `src/engine.rs` |
| Headless stdout | working — reattach the parent console | working — the process is never detached | working | `src/platform.rs` |
