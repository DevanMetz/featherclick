//! Platform shims that are too small to justify a dependency.

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
