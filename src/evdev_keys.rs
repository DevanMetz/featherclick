//! Global hotkeys on Wayland.
//!
//! Wayland has no X11-style grab, so a chord is recognised by reading the
//! keyboard's evdev node. Only the configured chords are reported. The device
//! is not grabbed, because an exclusive grab would swallow every key: the
//! focused window still receives the key. Reading `/dev/input/event*` needs
//! the `input` group on most distributions.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::io::FromRawFd;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use global_hotkey::hotkey::{Code, HotKey, Modifiers};

use super::pretty;

pub(super) const NO_KEYBOARD: &str = "global hotkeys unavailable: no keyboard device";
pub(super) const NEED_ACCESS: &str = "global hotkeys need read access to /dev/input";

const EVENT_BYTES: usize = 24;
const EV_KEY: u16 = 0x01;
/// `KEY_CNT / 8` from `linux/input-event-codes.h`.
const KEY_BITMAP_BYTES: usize = 96;

const KEY_LEFTCTRL: u16 = 29;
const KEY_RIGHTCTRL: u16 = 97;
const KEY_LEFTSHIFT: u16 = 42;
const KEY_RIGHTSHIFT: u16 = 54;
const KEY_LEFTALT: u16 = 56;
const KEY_RIGHTALT: u16 = 100;
const KEY_LEFTMETA: u16 = 125;
const KEY_RIGHTMETA: u16 = 126;

const O_NONBLOCK: i32 = 0x800;
const O_CLOEXEC: i32 = 0x80000;
const POLLIN: i16 = 0x1;
const POLLERR: i16 = 0x8;
const POLLHUP: i16 = 0x10;
const POLLNVAL: i16 = 0x20;

/// `true` when this process is in a Wayland session, including one that also
/// has `DISPLAY` set for XWayland. An X11 grab does not see keys there.
pub(super) fn session_is_wayland(
    wayland_display: Option<&str>,
    session_type: Option<&str>,
) -> bool {
    wayland_display.is_some_and(|value| !value.is_empty())
        || session_type.is_some_and(|value| value.eq_ignore_ascii_case("wayland"))
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Binding {
    id: u32,
    code: u16,
    mods: Modifiers,
}

#[derive(Clone, Copy, Default)]
pub(super) struct Pair {
    toggle: Option<HotKey>,
    capture: Option<HotKey>,
}

/// Drops a chord this backend cannot hear, and refuses to arm one key as both
/// actions. Start/stop keeps the key.
pub(super) fn normalize(toggle: Option<HotKey>, capture: Option<HotKey>) -> (Pair, Option<String>) {
    let mut problem = None;
    let toggle = acceptable(toggle, &mut problem);
    let mut capture = acceptable(capture, &mut problem);
    if let (Some(toggle), Some(capture_key)) = (toggle, capture)
        && toggle.id() == capture_key.id()
    {
        problem.get_or_insert_with(|| {
            format!(
                "{} is already used by start / stop",
                pretty(&toggle.to_string())
            )
        });
        capture = None;
    }
    (Pair { toggle, capture }, problem)
}

fn acceptable(hotkey: Option<HotKey>, problem: &mut Option<String>) -> Option<HotKey> {
    let hotkey = hotkey?;
    if evdev_code(hotkey.key).is_none() {
        problem.get_or_insert_with(|| format!("cannot listen for {}", pretty(&hotkey.to_string())));
        None
    } else {
        Some(hotkey)
    }
}

fn bindings_of(pair: Pair) -> Vec<Binding> {
    [pair.toggle, pair.capture]
        .into_iter()
        .flatten()
        .filter_map(|hotkey| {
            evdev_code(hotkey.key).map(|code| Binding {
                id: hotkey.id(),
                code,
                mods: held_mods(hotkey.mods),
            })
        })
        .collect()
}

/// The four modifiers a binding can require. Lock keys are ignored, matching
/// an X11 grab that is also registered with NumLock and CapsLock down.
fn held_mods(mods: Modifiers) -> Modifiers {
    mods & (Modifiers::CONTROL | Modifiers::SHIFT | Modifiers::ALT | Modifiers::SUPER)
}

#[derive(Default)]
struct KeyWatch {
    down: HashSet<u16>,
    /// Chord ids a press was emitted for and whose key is still down.
    armed: Vec<u32>,
    bindings: Vec<Binding>,
}

impl KeyWatch {
    /// Releases chords whose binding went away. A chord that is still
    /// configured stays armed, so editing the other key does not invent a
    /// release for a key the user is holding.
    fn set_bindings(&mut self, bindings: Vec<Binding>) -> Vec<(u32, bool)> {
        if self.bindings == bindings {
            return Vec::new();
        }
        let mut removed = Vec::new();
        let mut still = Vec::new();
        for id in self.armed.drain(..) {
            if bindings.iter().any(|binding| binding.id == id) {
                still.push(id);
            } else {
                removed.push((id, false));
            }
        }
        self.armed = still;
        self.bindings = bindings;
        removed
    }

    /// `value` is the evdev key value: 0 up, 1 down, 2 repeat.
    fn feed(&mut self, code: u16, value: i32) -> Vec<(u32, bool)> {
        match value {
            0 => self.release(code),
            1 => self.press(code),
            2 => {
                // A repeat means the key is down. It must not fire again, and
                // a repeat seen before the initial press (we started mid-hold)
                // must not fire either.
                self.down.insert(code);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    fn press(&mut self, code: u16) -> Vec<(u32, bool)> {
        if !self.down.insert(code) {
            return Vec::new();
        }
        let mods = modifiers_down(&self.down);
        let mut fired = Vec::new();
        for binding in &self.bindings {
            if binding.code == code && binding.mods == mods && !self.armed.contains(&binding.id) {
                self.armed.push(binding.id);
                fired.push((binding.id, true));
            }
        }
        fired
    }

    fn release(&mut self, code: u16) -> Vec<(u32, bool)> {
        if !self.down.remove(&code) {
            return Vec::new();
        }
        let mut fired = Vec::new();
        let mut still = Vec::new();
        for id in self.armed.drain(..) {
            let released = self
                .bindings
                .iter()
                .any(|binding| binding.id == id && binding.code == code);
            if released {
                fired.push((id, false));
            } else {
                still.push(id);
            }
        }
        self.armed = still;
        fired
    }
}

fn modifiers_down(down: &HashSet<u16>) -> Modifiers {
    let mut mods = Modifiers::empty();
    if down.contains(&KEY_LEFTCTRL) || down.contains(&KEY_RIGHTCTRL) {
        mods.insert(Modifiers::CONTROL);
    }
    if down.contains(&KEY_LEFTSHIFT) || down.contains(&KEY_RIGHTSHIFT) {
        mods.insert(Modifiers::SHIFT);
    }
    if down.contains(&KEY_LEFTALT) || down.contains(&KEY_RIGHTALT) {
        mods.insert(Modifiers::ALT);
    }
    if down.contains(&KEY_LEFTMETA) || down.contains(&KEY_RIGHTMETA) {
        mods.insert(Modifiers::SUPER);
    }
    mods
}

/// Linux evdev code for a physical key, or `None` when this backend cannot
/// hear it. Values are from `linux/input-event-codes.h`.
fn evdev_code(code: Code) -> Option<u16> {
    Some(match code {
        Code::KeyA => 30,
        Code::KeyB => 48,
        Code::KeyC => 46,
        Code::KeyD => 32,
        Code::KeyE => 18,
        Code::KeyF => 33,
        Code::KeyG => 34,
        Code::KeyH => 35,
        Code::KeyI => 23,
        Code::KeyJ => 36,
        Code::KeyK => 37,
        Code::KeyL => 38,
        Code::KeyM => 50,
        Code::KeyN => 49,
        Code::KeyO => 24,
        Code::KeyP => 25,
        Code::KeyQ => 16,
        Code::KeyR => 19,
        Code::KeyS => 31,
        Code::KeyT => 20,
        Code::KeyU => 22,
        Code::KeyV => 47,
        Code::KeyW => 17,
        Code::KeyX => 45,
        Code::KeyY => 21,
        Code::KeyZ => 44,
        Code::Digit1 => 2,
        Code::Digit2 => 3,
        Code::Digit3 => 4,
        Code::Digit4 => 5,
        Code::Digit5 => 6,
        Code::Digit6 => 7,
        Code::Digit7 => 8,
        Code::Digit8 => 9,
        Code::Digit9 => 10,
        Code::Digit0 => 11,
        Code::Minus => 12,
        Code::Equal => 13,
        Code::Backspace => 14,
        Code::Tab => 15,
        Code::Enter => 28,
        Code::Space => 57,
        Code::CapsLock => 58,
        Code::Escape => 1,
        Code::F1 => 59,
        Code::F2 => 60,
        Code::F3 => 61,
        Code::F4 => 62,
        Code::F5 => 63,
        Code::F6 => 64,
        Code::F7 => 65,
        Code::F8 => 66,
        Code::F9 => 67,
        Code::F10 => 68,
        Code::F11 => 87,
        Code::F12 => 88,
        other => return evdev_code_more(other),
    })
}

fn evdev_code_more(code: Code) -> Option<u16> {
    Some(match code {
        Code::F13 => 183,
        Code::F14 => 184,
        Code::F15 => 185,
        Code::F16 => 186,
        Code::F17 => 187,
        Code::F18 => 188,
        Code::F19 => 189,
        Code::F20 => 190,
        Code::F21 => 191,
        Code::F22 => 192,
        Code::F23 => 193,
        Code::F24 => 194,
        Code::Backquote => 41,
        Code::BracketLeft => 26,
        Code::BracketRight => 27,
        Code::Semicolon => 39,
        Code::Quote => 40,
        Code::Backslash => 43,
        Code::IntlBackslash => 86,
        Code::Comma => 51,
        Code::Period => 52,
        Code::Slash => 53,
        Code::NumLock => 69,
        Code::ScrollLock => 70,
        Code::Numpad7 => 71,
        Code::Numpad8 => 72,
        Code::Numpad9 => 73,
        Code::NumpadSubtract => 74,
        Code::Numpad4 => 75,
        Code::Numpad5 => 76,
        Code::Numpad6 => 77,
        Code::NumpadAdd => 78,
        Code::Numpad1 => 79,
        Code::Numpad2 => 80,
        Code::Numpad3 => 81,
        Code::Numpad0 => 82,
        Code::NumpadDecimal => 83,
        Code::NumpadEnter => 96,
        Code::NumpadDivide => 98,
        Code::NumpadMultiply => 55,
        Code::NumpadEqual => 117,
        Code::PrintScreen => 99,
        Code::Pause => 119,
        Code::Home => 102,
        Code::ArrowUp => 103,
        Code::PageUp => 104,
        Code::ArrowLeft => 105,
        Code::ArrowRight => 106,
        Code::End => 107,
        Code::ArrowDown => 108,
        Code::PageDown => 109,
        Code::Insert => 110,
        Code::Delete => 111,
        Code::AudioVolumeMute => 113,
        Code::AudioVolumeDown => 114,
        Code::AudioVolumeUp => 115,
        Code::MediaTrackNext => 163,
        Code::MediaPlayPause => 164,
        Code::MediaTrackPrevious => 165,
        Code::MediaStop => 166,
        Code::MediaPause => 201,
        Code::MediaPlay => 207,
        _ => return None,
    })
}

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}

#[derive(Debug)]
struct OpenDev {
    file: File,
    id: (u64, u64),
    dead: bool,
}

pub(super) struct Listener {
    pair: Arc<Mutex<Pair>>,
    stop: Arc<AtomicBool>,
    wake: File,
    thread: Option<JoinHandle<()>>,
}

impl Listener {
    pub(super) fn start(on_fire: impl Fn(u32, bool) + Send + 'static) -> Result<Self, String> {
        // `input_event` is 24 bytes on 64-bit. A 32-bit kernel uses 16, and
        // misreading it would invent keystrokes.
        if usize::BITS != 64 {
            return Err("global hotkeys unavailable on this build".to_string());
        }
        let devices = open_keyboards(Path::new("/dev/input"))?;
        let wake = event_fd().map_err(|err| format!("global hotkeys unavailable: {err}"))?;
        let wake_for_thread = wake
            .try_clone()
            .map_err(|err| format!("global hotkeys unavailable: {err}"))?;
        let pair = Arc::new(Mutex::new(Pair::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let pair_thread = Arc::clone(&pair);
        let stop_thread = Arc::clone(&stop);
        let thread = thread::Builder::new()
            .name("featherclick-evdev".to_string())
            .spawn(move || listen(devices, wake_for_thread, pair_thread, stop_thread, on_fire))
            .map_err(|err| format!("global hotkeys unavailable: {err}"))?;
        Ok(Self {
            pair,
            stop,
            wake,
            thread: Some(thread),
        })
    }

    pub(super) fn set(&self, toggle: Option<HotKey>, capture: Option<HotKey>) -> Option<String> {
        let (pair, problem) = normalize(toggle, capture);
        *self.pair.lock().unwrap_or_else(|err| err.into_inner()) = pair;
        // The listener may be blocked in poll. Wake it so the new chord is
        // armed before the next key, not up to two seconds later.
        poke(&self.wake);
        problem
    }
}

impl Drop for Listener {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.wake.write_all(&1u64.to_ne_bytes());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn listen(
    mut devices: Vec<OpenDev>,
    mut wake: File,
    pair: Arc<Mutex<Pair>>,
    stop: Arc<AtomicBool>,
    on_fire: impl Fn(u32, bool),
) {
    let mut watch = KeyWatch::default();
    let mut next_scan = Instant::now() + Duration::from_secs(2);
    loop {
        if stop.load(Ordering::Acquire) {
            break;
        }

        let mut polls = Vec::with_capacity(devices.len() + 1);
        polls.push(PollFd {
            fd: wake.as_raw_fd(),
            events: POLLIN,
            revents: 0,
        });
        for device in &devices {
            polls.push(PollFd {
                fd: device.file.as_raw_fd(),
                events: POLLIN,
                revents: 0,
            });
        }
        if poll_wait(&mut polls, 2_000).is_err() {
            break;
        }
        if polls[0].revents & (POLLIN | POLLERR | POLLHUP) != 0 {
            let mut buf = [0u8; 16];
            let _ = wake.read(&mut buf);
            if stop.load(Ordering::Acquire) {
                break;
            }
        }
        // Apply after poll returns so a binding change that woke us is in
        // effect for keys that arrived in the same wake.
        let current = *pair.lock().unwrap_or_else(|err| err.into_inner());
        for (id, pressed) in watch.set_bindings(bindings_of(current)) {
            on_fire(id, pressed);
        }
        for (index, device) in devices.iter_mut().enumerate() {
            let flags = polls[index + 1].revents;
            if flags & (POLLIN | POLLERR | POLLHUP | POLLNVAL) == 0 {
                continue;
            }
            if flags & POLLIN == 0 || !drain(&mut device.file, &mut watch, &on_fire) {
                device.dead = true;
            }
        }
        devices.retain(|device| !device.dead);
        if Instant::now() >= next_scan {
            let have: HashSet<(u64, u64)> = devices.iter().map(|device| device.id).collect();
            if let Ok(found) = scan(Path::new("/dev/input")) {
                devices.extend(
                    found
                        .files
                        .into_iter()
                        .filter(|device| !have.contains(&device.id)),
                );
            }
            next_scan = Instant::now() + Duration::from_secs(2);
        }
    }
}

/// Reads every pending key event. `false` means the device is gone.
fn drain(file: &mut File, watch: &mut KeyWatch, on_fire: &impl Fn(u32, bool)) -> bool {
    let mut buf = [0u8; EVENT_BYTES * 32];
    loop {
        match file.read(&mut buf) {
            Ok(0) => return false,
            Ok(size) => {
                for (code, value) in decode_keys(&buf[..size]) {
                    for (id, pressed) in watch.feed(code, value) {
                        on_fire(id, pressed);
                    }
                }
            }
            Err(err) if err.kind() == ErrorKind::Interrupted => continue,
            Err(err) if err.kind() == ErrorKind::WouldBlock => return true,
            Err(_) => return false,
        }
    }
}

fn decode_keys(buf: &[u8]) -> Vec<(u16, i32)> {
    let mut keys = Vec::new();
    let (chunks, _) = buf.as_chunks::<EVENT_BYTES>();
    for chunk in chunks {
        let type_ = u16::from_ne_bytes([chunk[16], chunk[17]]);
        if type_ != EV_KEY {
            continue;
        }
        let code = u16::from_ne_bytes([chunk[18], chunk[19]]);
        let value = i32::from_ne_bytes([chunk[20], chunk[21], chunk[22], chunk[23]]);
        keys.push((code, value));
    }
    keys
}

fn open_keyboards(dir: &Path) -> Result<Vec<OpenDev>, String> {
    let scan = match scan(dir) {
        Ok(scan) => scan,
        Err(err) if err.kind() == ErrorKind::NotFound => return Err(NO_KEYBOARD.to_string()),
        Err(err) if err.kind() == ErrorKind::PermissionDenied => {
            return Err(NEED_ACCESS.to_string());
        }
        Err(err) => return Err(format!("global hotkeys unavailable: {err}")),
    };
    if scan.files.is_empty() {
        return Err(if scan.denied {
            NEED_ACCESS.to_string()
        } else {
            NO_KEYBOARD.to_string()
        });
    }
    Ok(scan.files)
}

struct Scan {
    files: Vec<OpenDev>,
    denied: bool,
}

fn scan(dir: &Path) -> io::Result<Scan> {
    let mut files = Vec::new();
    let mut denied = false;
    for entry in std::fs::read_dir(dir)? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !is_event_node(name) {
            continue;
        }
        match OpenOptions::new()
            .read(true)
            .custom_flags(O_NONBLOCK | O_CLOEXEC)
            .open(entry.path())
        {
            Ok(file) => {
                if !keyboard_bitmap(&file) {
                    continue;
                }
                if let Ok(id) = file_id(&file) {
                    files.push(OpenDev {
                        file,
                        id,
                        dead: false,
                    });
                }
            }
            Err(err) if err.kind() == ErrorKind::PermissionDenied => denied = true,
            Err(_) => {}
        }
    }
    Ok(Scan { files, denied })
}

fn is_event_node(name: &str) -> bool {
    name.strip_prefix("event")
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|byte| byte.is_ascii_digit()))
}

fn file_id(file: &File) -> io::Result<(u64, u64)> {
    let meta = file.metadata()?;
    Ok((meta.dev(), meta.ino()))
}

fn keyboard_bitmap(file: &File) -> bool {
    let mut bits = [0u8; KEY_BITMAP_BYTES];
    let request = eviocgbit(EV_KEY as u32, KEY_BITMAP_BYTES as u32);
    let rc = unsafe { ioctl(file.as_raw_fd(), request, bits.as_mut_ptr()) };
    rc >= 0 && bitmap_is_keyboard(&bits)
}

/// A keyboard, rather than a mouse, reports a letter or a space key.
fn bitmap_is_keyboard(bits: &[u8]) -> bool {
    // KEY_ESC, KEY_ENTER, KEY_SPACE, KEY_A.
    bitmap_has(bits, 1) || bitmap_has(bits, 28) || bitmap_has(bits, 57) || bitmap_has(bits, 30)
}

fn bitmap_has(bits: &[u8], code: u16) -> bool {
    let index = code as usize / 8;
    let bit = code % 8;
    bits.get(index).is_some_and(|byte| byte & (1 << bit) != 0)
}

/// `_IOC(_IOC_READ, 'E', 0x20 + ev, len)` from `linux/ioctl.h`.
fn eviocgbit(ev: u32, len: u32) -> u64 {
    const IOC_READ: u64 = 2;
    (IOC_READ << 30) | ((len as u64) << 16) | ((b'E' as u64) << 8) | (0x20 + ev as u64)
}

fn poke(wake: &File) {
    let one = 1u64.to_ne_bytes();
    unsafe {
        // eventfd wants exactly 8 bytes. A full counter returns EAGAIN, which
        // still means the listener is already awake.
        let _ = write(wake.as_raw_fd(), one.as_ptr().cast(), one.len());
    }
}

fn event_fd() -> io::Result<File> {
    let fd = unsafe { eventfd(0, O_NONBLOCK | O_CLOEXEC) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // Safety: eventfd just returned this fd, and it is not owned elsewhere.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn poll_wait(fds: &mut [PollFd], timeout_ms: i32) -> io::Result<i32> {
    loop {
        let rc = unsafe { poll(fds.as_mut_ptr(), fds.len(), timeout_ms) };
        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        return Ok(rc);
    }
}

unsafe extern "C" {
    fn poll(fds: *mut PollFd, nfds: usize, timeout: i32) -> i32;
    fn eventfd(init: u32, flags: i32) -> i32;
    fn ioctl(fd: i32, request: u64, arg: *mut u8) -> i32;
    // Same signature as the libc `write` std already links, so this stays a
    // declaration of that symbol rather than a second definition.
    fn write(fd: i32, buf: *const std::ffi::c_void, count: usize) -> isize;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn bind(text: &str) -> Binding {
        let hotkey = HotKey::from_str(text).unwrap();
        Binding {
            id: hotkey.id(),
            code: evdev_code(hotkey.key).unwrap(),
            mods: held_mods(hotkey.mods),
        }
    }

    fn press_values(watch: &mut KeyWatch, code: u16, value: i32) -> Vec<bool> {
        watch
            .feed(code, value)
            .into_iter()
            .map(|(_, down)| down)
            .collect()
    }

    #[test]
    fn wayland_session_ignores_an_empty_display_name() {
        assert!(!session_is_wayland(None, None));
        assert!(!session_is_wayland(Some(""), Some("x11")));
        assert!(session_is_wayland(Some("wayland-0"), Some("x11")));
        assert!(session_is_wayland(None, Some("Wayland")));
        assert!(!session_is_wayland(None, Some("tty")));
    }

    #[test]
    fn function_keys_use_the_kernel_numbering() {
        // F11 is not F1 + 10. The kernel leaves a gap after F10.
        assert_eq!(evdev_code(Code::F6), Some(64));
        assert_eq!(evdev_code(Code::F10), Some(68));
        assert_eq!(evdev_code(Code::F11), Some(87));
        assert_eq!(evdev_code(Code::F12), Some(88));
        assert_eq!(evdev_code(Code::KeyK), Some(37));
        assert_eq!(evdev_code(Code::Digit1), Some(2));
        assert_eq!(evdev_code(Code::Digit0), Some(11));
        assert_eq!(evdev_code(Code::ArrowUp), Some(103));
        assert_eq!(evdev_code(Code::Fn), None);
    }

    #[test]
    fn ioctl_number_matches_the_linux_macro() {
        // _IOC(_IOC_READ, 'E', 0x20 + EV_KEY, 96) == 0x80604521
        assert_eq!(eviocgbit(1, 96), 0x8060_4521);
    }

    #[test]
    fn a_letter_key_marks_a_keyboard_and_a_mouse_button_does_not() {
        let mut bits = [0u8; KEY_BITMAP_BYTES];
        // KEY_A is 30: byte 3, bit 6.
        bits[3] |= 1 << 6;
        assert!(bitmap_has(&bits, 30));
        assert!(bitmap_is_keyboard(&bits));

        let mut mouse = [0u8; KEY_BITMAP_BYTES];
        // BTN_LEFT is 0x110 = 272.
        assert!(272 / 8 < mouse.len());
        mouse[272 / 8] |= 1 << (272 % 8);
        assert!(!bitmap_is_keyboard(&mouse));
    }

    #[test]
    fn bare_key_fires_once_and_ignores_repeat() {
        let mut watch = KeyWatch::default();
        watch.set_bindings(vec![bind("F6")]);
        let f6 = 64;
        assert_eq!(press_values(&mut watch, f6, 1), vec![true]);
        assert!(press_values(&mut watch, f6, 2).is_empty());
        assert!(press_values(&mut watch, f6, 1).is_empty(), "still held");
        assert_eq!(press_values(&mut watch, f6, 0), vec![false]);
        assert_eq!(press_values(&mut watch, f6, 1), vec![true]);
    }

    #[test]
    fn a_repeat_seen_before_the_press_does_not_fire() {
        let mut watch = KeyWatch::default();
        watch.set_bindings(vec![bind("F8")]);
        assert!(press_values(&mut watch, 66, 2).is_empty());
        assert!(press_values(&mut watch, 66, 0).is_empty());
        assert_eq!(press_values(&mut watch, 66, 1), vec![true]);
    }

    #[test]
    fn chord_requires_exactly_its_modifiers() {
        let mut watch = KeyWatch::default();
        let binding = bind("Ctrl+K");
        let id = binding.id;
        watch.set_bindings(vec![binding]);
        assert!(press_values(&mut watch, 37, 1).is_empty(), "K alone");
        assert!(press_values(&mut watch, 37, 0).is_empty());

        assert!(press_values(&mut watch, KEY_LEFTCTRL, 1).is_empty());
        assert_eq!(watch.feed(37, 1), vec![(id, true)]);
        assert_eq!(watch.feed(37, 0), vec![(id, false)]);

        // Ctrl+Shift+K is a different chord.
        assert!(press_values(&mut watch, KEY_LEFTSHIFT, 1).is_empty());
        assert!(press_values(&mut watch, 37, 1).is_empty());
        assert!(press_values(&mut watch, 37, 0).is_empty());
    }

    #[test]
    fn lock_keys_do_not_change_the_chord() {
        let mut watch = KeyWatch::default();
        watch.set_bindings(vec![bind("F6")]);
        assert!(press_values(&mut watch, 69, 1).is_empty(), "num lock");
        assert!(press_values(&mut watch, 58, 1).is_empty(), "caps lock");
        assert_eq!(press_values(&mut watch, 64, 1), vec![true]);
    }

    #[test]
    fn either_side_of_a_modifier_counts() {
        let mut watch = KeyWatch::default();
        watch.set_bindings(vec![bind("Ctrl+Shift+K")]);
        press_values(&mut watch, KEY_RIGHTCTRL, 1);
        press_values(&mut watch, KEY_LEFTSHIFT, 1);
        assert_eq!(press_values(&mut watch, 37, 1), vec![true]);
    }

    #[test]
    fn releasing_one_chord_leaves_the_other_armed() {
        let mut watch = KeyWatch::default();
        let f6 = bind("F6");
        let f7 = bind("F7");
        watch.set_bindings(vec![f6, f7]);
        assert_eq!(watch.feed(f6.code, 1), vec![(f6.id, true)]);
        assert_eq!(watch.feed(f7.code, 1), vec![(f7.id, true)]);
        assert_eq!(watch.feed(f6.code, 0), vec![(f6.id, false)]);
        assert_eq!(watch.feed(f7.code, 0), vec![(f7.id, false)]);
    }

    #[test]
    fn changing_bindings_releases_a_held_chord() {
        let mut watch = KeyWatch::default();
        let f6 = bind("F6");
        watch.set_bindings(vec![f6]);
        assert_eq!(watch.feed(f6.code, 1), vec![(f6.id, true)]);
        let released = watch.set_bindings(vec![bind("F7")]);
        assert_eq!(released, vec![(f6.id, false)]);
        assert!(watch.feed(f6.code, 0).is_empty());
    }

    #[test]
    fn changing_the_other_binding_keeps_a_held_chord() {
        let mut watch = KeyWatch::default();
        let f6 = bind("F6");
        let f7 = bind("F7");
        watch.set_bindings(vec![f6, f7]);
        assert_eq!(watch.feed(f6.code, 1), vec![(f6.id, true)]);
        assert_eq!(watch.feed(f7.code, 1), vec![(f7.id, true)]);
        let released = watch.set_bindings(vec![f6, bind("F8")]);
        assert_eq!(released, vec![(f7.id, false)]);
        assert_eq!(watch.feed(f6.code, 0), vec![(f6.id, false)]);
    }

    #[test]
    fn decoded_bytes_drive_the_same_chord() {
        let mut bytes = Vec::new();
        bytes.extend(pack(EV_KEY, KEY_LEFTCTRL, 1));
        bytes.extend(pack(2, 0, 1)); // EV_REL, ignored
        bytes.extend(pack(EV_KEY, 37, 1));
        bytes.extend(pack(EV_KEY, 37, 0));
        bytes.extend(pack(EV_KEY, KEY_LEFTCTRL, 0));
        bytes.extend([0u8; 10]); // trailing partial event

        let binding = bind("Ctrl+K");
        let mut watch = KeyWatch::default();
        watch.set_bindings(vec![binding]);
        let mut fired = Vec::new();
        for (code, value) in decode_keys(&bytes) {
            fired.extend(watch.feed(code, value));
        }
        assert_eq!(fired, vec![(binding.id, true), (binding.id, false)]);
    }

    fn pack(type_: u16, code: u16, value: i32) -> [u8; EVENT_BYTES] {
        let mut bytes = [0u8; EVENT_BYTES];
        bytes[16..18].copy_from_slice(&type_.to_ne_bytes());
        bytes[18..20].copy_from_slice(&code.to_ne_bytes());
        bytes[20..24].copy_from_slice(&value.to_ne_bytes());
        bytes
    }

    #[test]
    fn duplicate_and_unheard_chords_are_refused() {
        let f6 = HotKey::from_str("F6").unwrap();
        let (pair, problem) = normalize(Some(f6), Some(f6));
        assert!(pair.toggle.is_some());
        assert!(pair.capture.is_none());
        assert_eq!(
            problem.as_deref(),
            Some("F6 is already used by start / stop")
        );

        let chord = HotKey::from_str("Ctrl+K").unwrap();
        let (_, problem) = normalize(Some(chord), Some(chord));
        assert_eq!(
            problem.as_deref(),
            Some("Ctrl+K is already used by start / stop")
        );

        let unheard = HotKey::new(None, Code::Fn);
        let (pair, problem) = normalize(Some(unheard), None);
        assert!(pair.toggle.is_none());
        assert_eq!(problem.as_deref(), Some("cannot listen for Fn"));
    }

    #[test]
    fn a_directory_without_a_keyboard_is_a_clear_error() {
        let dir = std::env::temp_dir().join(format!("featherclick-hotkeys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).unwrap();
        assert_eq!(open_keyboards(&dir).unwrap_err(), NO_KEYBOARD);

        std::fs::write(dir.join("event0"), b"regular file").unwrap();
        std::fs::write(dir.join("mice"), b"ignored").unwrap();
        assert_eq!(open_keyboards(&dir).unwrap_err(), NO_KEYBOARD);

        let missing = dir.join("missing");
        assert_eq!(open_keyboards(&missing).unwrap_err(), NO_KEYBOARD);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
