<div align="center">

# FeatherClick

A tiny cross-platform autoclicker with human-like timing.

Single binary, no runtime dependencies, no background services.

</div>

## Features

- **Human-like timing** — fixed interval, uniform random between a min and max, or
  Gaussian (mean and sigma), all in milliseconds.
- **Random pixels in a range** — every click can land anywhere in a box, an
  ellipse, or a Gaussian spread around the target.
- **Limits that stop it for you** — stop after *N* clicks, after *S* seconds, or
  as soon as you move the pointer away from the click point.
- **Click styles** — left / right / middle, single, double, or press-and-hold.
- **Global hotkeys** — start/stop and pick-the-position bindings, rebindable in
  the app, with the current binding always shown next to the button.
- **Minimal UI** — one window, dark or light, optionally always-on-top, and it
  repaints only when something changes.
- **Headless mode** — the same engine driven from the command line, for scripts
  and machines without a display server.

## Download

Grab the archive for your platform from
[Releases](https://github.com/DevanMetz/featherclick/releases):

| Platform | Archive | Notes |
| --- | --- | --- |
| Windows 10/11 | `featherclick-*-windows-x86_64.zip` | Unzip and run `featherclick.exe`. |
| macOS 11+ | `featherclick-*-macos-universal.zip` | Universal (Apple silicon + Intel). |
| Linux x86_64 | `featherclick-*-linux-x86_64.tar.gz` | Needs an X11 or Wayland session. |

### First run on macOS

The build is not notarised. Right-click the app and choose **Open** the first
time, or clear the quarantine flag:

```sh
xattr -dr com.apple.quarantine /Applications/FeatherClick.app
```

macOS also requires permission to send synthetic clicks: **System Settings →
Privacy & Security → Accessibility**, and enable FeatherClick. Without it the
clicks are silently dropped by the system.

### First run on Linux

```sh
tar -xzf featherclick-*-linux-x86_64.tar.gz
cd featherclick-*-linux-x86_64
install -Dm755 featherclick ~/.local/bin/featherclick
# optional desktop entry + icon
install -Dm644 featherclick.desktop ~/.local/share/applications/featherclick.desktop
install -Dm644 featherclick.png ~/.local/share/icons/hicolor/256x256/apps/featherclick.png
```

## Platform support

| | Windows 10/11 | macOS 11+ | Linux (X11) | Linux (Wayland) |
| --- | --- | --- | --- | --- |
| Clicking | ✅ | ✅ (Accessibility permission) | ✅ | ✅ wlroots only |
| Global hotkeys | ✅ | ✅ | ✅ | ❌ — use the buttons |
| Picking a position | ✅ | ✅ | ✅ | ✅ |

On Wayland, clicking uses the compositor's virtual-pointer protocol, which
wlroots-based compositors (Sway, Hyprland) provide. GNOME and KDE are not
covered: their only input path is the RemoteDesktop portal, and the upstream
portal client aborts the process when no portal is present, so it is
deliberately not compiled in — a crash on launch is worse than a documented
gap. On compositors without virtual-pointer support, clicks still reach
XWayland applications. Wayland has no equivalent of X11's global grabs, so
hotkeys are unavailable there and the on-screen buttons are the way to drive
it.

## Using the window

1. Pick a **Button** and **Style**.
2. Choose a **Timing** mode. `Random` (uniform) is the safest default for
   anything watching for automation; `Human-like` gives a Gaussian spread.
3. Choose a **Target**:
   - **Pointer** clicks wherever the pointer already is and never moves it, so
     you can keep repositioning while it runs.
   - **Fixed** returns to a point before every click. Type coordinates, or press
     **Pick** and the position is taken after a 3 second countdown, or press the
     pick hotkey (`F7` by default) while hovering the target.
4. Optionally add **Jitter** — the spread around the target, in pixels.
5. Set **Limits** if you want the run to end by itself.
6. Press **Start** (or the toggle hotkey).

Editing settings while it runs takes effect on the next start; the snapshot is
taken once so a run stays consistent.

Default hotkeys: **F6** start/stop, **F7** pick the pointer position. Click a
binding and press a new combination to change it; **Esc** cancels, **×** clears
it.

## Headless mode

Any of the run options below also opens no window:

```sh
# 20 clicks, one every 100 ms, at a fixed point with a 25 px spread
featherclick --headless --clicks 20 --interval 100 --at 640,360 --jitter 25

# click at the pointer for 30 seconds
featherclick --headless --seconds 30 --button right --quiet

# run with whatever the saved settings say, 100 clicks
featherclick --headless --clicks 100
```

| Option | Meaning |
| --- | --- |
| `--headless` | Do not open a window. |
| `--clicks <N>` | Stop after N clicks (0 = unlimited). |
| `--seconds <S>` | Stop after S seconds (0 = unlimited). |
| `--interval <MS>` | Fixed interval, overriding the saved timing mode. |
| `--at <X,Y>` | Click a fixed screen point. |
| `--jitter <PX>` | Randomise the point by up to ±PX pixels. |
| `--button <left\|right\|middle>` | Which button to click. |
| `--delay <MS>` | Wait before the first click. |
| `--quiet` | Print only the final summary. |
| `--help`, `--version` | Usage and version. |

Everything else (style, jitter shape, limits) comes from the saved settings.
Exit status is `0` for a normal stop — including limits — and `1` if the input
backend could not be reached.

Coordinates are physical screen pixels, measured from the top-left of the
primary display.

## Configuration

Settings are saved automatically, and the file is plain JSON you can edit
directly:

| Platform | Path |
| --- | --- |
| Windows | `%APPDATA%\featherclick\config.json` |
| macOS | `~/Library/Application Support/featherclick/config.json` |
| Linux | `~/.config/featherclick/config.json` |

```jsonc
{
  "button": "left",          // left | right | middle
  "style": "single",         // single | double | hold
  "hold_ms": 40,

  "timing": "uniform",       // fixed | uniform | normal
  "interval_ms": 100,        // fixed
  "interval_min_ms": 40,     // uniform
  "interval_max_ms": 120,
  "interval_mean_ms": 100,   // normal (human-like)
  "interval_std_ms": 25,
  "start_delay_ms": 0,

  "position": "cursor",      // cursor | fixed
  "point": [0, 0],           // fixed target, screen pixels
  "jitter": "off",           // off | rectangle | ellipse | normal
  "jitter_x": 4,
  "jitter_y": 4,

  "max_clicks": 0,           // 0 = unlimited
  "max_seconds": 0,          // 0 = unlimited
  "stop_on_move_px": 0,      // 0 = off

  "hotkey_toggle": "F6",
  "hotkey_capture": "F7",

  "theme": "dark",           // dark | light
  "always_on_top": false
}
```

Missing or unknown fields are ignored, so a partial file still loads. Values are
clamped into usable ranges on load — for example an inverted min/max pair is
swapped rather than rejected.

## How it works

**Timing.** The click loop sleeps in short slices and only spins for the last
fraction of a millisecond, using the overshoot it measured on the previous
sleep to size that spin. On Windows it also raises the system timer resolution
to 1 ms for the duration of a run, because the default scheduler tick is
~15.6 ms. Measured on Windows 11 against an OS-level mouse hook, a 100 ms fixed
interval held to 96–109 ms per click, and a 40–120 ms uniform setting produced
40.3–120.3 ms with a flat distribution and mean 79.0 ms.

**The rate ceiling.** Waking a sleeping thread costs the OS about a millisecond
of granularity, so intervals below a few milliseconds do not run at the
requested rate: asking for 1 ms measured 193 clicks/s. At that rate the loop
used 3.1% of one core, and at 10 clicks/s the processor time was too small to
measure at all.

**Precision features are not decoration.** `stop-on-move`, the click counter and
the elapsed timer are all sampled on the same loop as the clicks, so the limits
stop the run within one interval, and a stop is never late by more than the
25 ms slice length.

**Lightness.** One process, one worker thread, an egui window that repaints only
while a run is active or a countdown is on screen: 10 seconds of idle time
measured 0.0000 s of processor time, and 100 clicks over 10 seconds also
measured 0.0000 s. A release build is a single ~5.8 MB binary that links no
system GUI toolkit, no web view and no scripting engine. Accessibility tree
support is compiled out to keep the binary small, which means the UI is not
exposed to screen readers.

**State.** The engine snapshots your settings when a run starts, so a run cannot
change shape halfway through. The UI writes settings to disk 400 ms after the
last edit, atomically, so an interrupted save cannot truncate the file.

## Building

```sh
cargo build --release
cargo test
```

Linux needs the usual winit build dependencies:

```sh
sudo apt install pkg-config libxkbcommon-dev libwayland-dev libx11-dev \
  libxcursor-dev libxrandr-dev libxi-dev libgl1-mesa-dev
```

The icon is generated rather than checked in by hand:

```sh
python tools/make_icon.py   # rewrites assets/icon.ico, icon.rgba and icon.png
```

CI builds and tests all three platforms on every push, and runs the engine on a
virtual X display to prove the input backend actually connects.

## Limitations

- Global hotkeys are implemented with OS-level registration: X11 only on Linux,
  and the hotkey never reaches the focused application.
- No macro recording, sequences, or keyboard keys — FeatherClick clicks, and
  nothing else.
- Windows and macOS builds are unsigned. Windows SmartScreen may warn on first
  run; choose *More info → Run anyway*.

## Responsible use

Automating input may violate the terms of service of games, websites, and
online services, and using it to gain an unfair advantage can get an account
banned. Some places also prohibit unattended click generation outright. Check
the rules that apply to you before using this, and prefer the built-in limits —
click caps, time caps, and stop-on-move — over leaving it running unattended.

## License

MIT — see [LICENSE](LICENSE).
