// SPDX-License-Identifier: GPL-2.0-only
//! The resolution of the timer, which the delays between the control
//! messages of a bridge need finer than Windows gives by default: its timer
//! ticks every 15.6 ms, making each delay of 1 ms as long, and tuning take
//! seconds.

/// Keeps the timer of the process at 1 ms while alive, on Windows.
pub struct FineTimer(());

impl FineTimer {
    pub fn new() -> Self {
        #[cfg(windows)]
        // SAFETY: no pointer is passed; it is undone once by the drop.
        unsafe {
            windows_sys::Win32::Media::timeBeginPeriod(1);
        }
        Self(())
    }
}

impl Drop for FineTimer {
    fn drop(&mut self) {
        #[cfg(windows)]
        // SAFETY: matches the timeBeginPeriod of `new`.
        unsafe {
            windows_sys::Win32::Media::timeEndPeriod(1);
        }
    }
}
