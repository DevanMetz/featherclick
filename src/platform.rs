//! Platform shims that are too small to justify a dependency.

use std::sync::Once;
use std::sync::atomic::{AtomicU8, Ordering};

/// How many times headless Ctrl+C / SIGINT has been delivered.
///
/// The handler only touches this atomic: a signal handler must not lock or
/// allocate. The session polls it from the click loop.
static INTERRUPTS: AtomicU8 = AtomicU8::new(0);

/// Reattaches stdout/stderr to the launching console.
///
/// Release Windows builds use the GUI subsystem so double-clicking the exe does
/// not flash a console window; that also detaches the standard streams, which
/// `--headless` needs back.
#[cfg(windows)]
pub fn attach_console() {
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    // Fails harmlessly when there is no parent console (launched from Explorer).
    unsafe {
        AttachConsole(ATTACH_PARENT_PROCESS);
    }
}

#[cfg(not(windows))]
pub fn attach_console() {}

/// Declares per-monitor-v2 DPI awareness, so screen coordinates are real pixels.
///
/// Without this a process is DPI-unaware and Windows virtualises every
/// coordinate: `GetCursorPos` reports scaled values that round differently from
/// call to call, and an "at 1000,500" click lands somewhere else entirely on a
/// scaled display. The GUI would get the same level from winit; headless runs
/// have nothing to set it for them, and setting it first is harmless because
/// both ask for v2.
#[cfg(windows)]
pub fn init_dpi() {
    use windows_sys::Win32::UI::HiDpi::{
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, SetProcessDpiAwarenessContext,
    };
    // Fails harmlessly on Windows versions without the v2 context.
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
}

#[cfg(not(windows))]
pub fn init_dpi() {}

/// Raises the system timer resolution for as long as it is held.
///
/// Without this, `Sleep` granularity on Windows is the ~15.6 ms scheduler tick,
/// which caps click-timing accuracy. Everywhere else this is a no-op.
pub struct TimerResolution {
    #[cfg(windows)]
    active: bool,
}

impl TimerResolution {
    pub fn acquire() -> Self {
        #[cfg(windows)]
        {
            use windows_sys::Win32::Media::timeBeginPeriod;
            Self {
                active: unsafe { timeBeginPeriod(1) } == 0,
            }
        }
        #[cfg(not(windows))]
        {
            Self {}
        }
    }
}

impl Drop for TimerResolution {
    fn drop(&mut self) {
        #[cfg(windows)]
        if self.active {
            use windows_sys::Win32::Media::timeEndPeriod;
            unsafe {
                timeEndPeriod(1);
            }
        }
    }
}

/// Moves the pointer to a physical pixel on the virtual desktop.
///
/// `SendInput` with `MOUSEEVENTF_ABSOLUTE` (what enigo uses for an absolute
/// move) maps 0..65535 onto the primary monitor only. A second monitor, and
/// any negative coordinate, never receives the point, and `GetCursorPos`
/// would then disagree with the coordinate the click was aimed at.
/// `SetCursorPos` is the same space `GetCursorPos` reports.
#[cfg(windows)]
pub(crate) fn set_cursor_pos(x: i32, y: i32) -> Result<(), String> {
    use windows_sys::Win32::Foundation::POINT;
    use windows_sys::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};

    fn put(x: i32, y: i32) -> Result<(), String> {
        if unsafe { SetCursorPos(x, y) } == 0 {
            Err(std::io::Error::last_os_error().to_string())
        } else {
            Ok(())
        }
    }

    fn read() -> Result<(i32, i32), String> {
        let mut point = POINT { x: 0, y: 0 };
        if unsafe { GetCursorPos(&mut point) } == 0 {
            Err(std::io::Error::last_os_error().to_string())
        } else {
            Ok((point.x, point.y))
        }
    }

    put(x, y)?;
    if read()? == (x, y) {
        return Ok(());
    }
    // The cursor can be nudged between the write and the read. One retry, then
    // refuse to click somewhere other than the requested pixel.
    put(x, y)?;
    let now = read()?;
    if now == (x, y) {
        Ok(())
    } else {
        Err(format!(
            "pointer went to {},{} instead of {x},{y}",
            now.0, now.1
        ))
    }
}
/// Arranges for the first Ctrl+C to ask the headless session to stop, and for
/// a second one to terminate the process. Installed only for `--headless`:
/// the windowed process should keep the default handler so a terminal Ctrl+C
/// still quits it.
pub fn arm_headless_interrupt() {
    static ONCE: Once = Once::new();
    ONCE.call_once(install_interrupt_handler);
}

/// `true` after the first headless Ctrl+C, until the process exits.
pub fn interrupt_requested() -> bool {
    INTERRUPTS.load(Ordering::SeqCst) > 0
}

#[cfg(unix)]
fn install_interrupt_handler() {
    unsafe {
        libc::signal(
            libc::SIGINT,
            handle_sigint as *const () as libc::sighandler_t,
        );
    }
}

/// First delivery sets the flag. A second restores the default action and
/// re-raises, which is async-signal-safe and exits with the usual status.
#[cfg(unix)]
unsafe extern "C" fn handle_sigint(_signal: libc::c_int) {
    if INTERRUPTS.fetch_add(1, Ordering::SeqCst) > 0 {
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
            libc::raise(libc::SIGINT);
        }
    }
}

#[cfg(windows)]
fn install_interrupt_handler() {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    unsafe {
        SetConsoleCtrlHandler(Some(handle_console), 1);
    }
}

/// Console-control callback. Returning 1 marks the event handled; returning 0
/// lets Windows terminate the process, which is what a second Ctrl+C wants.
#[cfg(windows)]
unsafe extern "system" fn handle_console(ctrl: u32) -> i32 {
    use windows_sys::Win32::System::Console::{CTRL_BREAK_EVENT, CTRL_C_EVENT};
    if (ctrl == CTRL_C_EVENT || ctrl == CTRL_BREAK_EVENT)
        && INTERRUPTS.fetch_add(1, Ordering::SeqCst) == 0
    {
        return 1;
    }
    0
}

#[cfg(not(any(unix, windows)))]
fn install_interrupt_handler() {}
