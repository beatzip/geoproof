//! WM_SETTINGCHANGE broadcast via SendMessageTimeoutW(HWND_BROADCAST).
//! A timeout (LRESULT 0) is tolerable: some windows may be hung; we log a
//! warning and keep going instead of failing the apply.

use windows::Win32::Foundation::LPARAM;
use windows::Win32::UI::WindowsAndMessaging::{
    SendMessageTimeoutW, HWND_BROADCAST, SMTO_ABORTIFHUNG, WM_SETTINGCHANGE,
};

use crate::error::ApplyError;

// 1 s: the broadcast runs inside applier calls that the restore watchdog
// (5 s) must be able to observe; a longer per-call budget could eat it.
const TIMEOUT_MS: u32 = 1000;

/// Production impl of [crate::traits::SettingChangeBroadcast].
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsSettingChangeBroadcast;

impl WindowsSettingChangeBroadcast {
    pub fn new() -> Self {
        Self
    }

    /// Returns Ok even when the broadcast times out (warning logged):
    /// per-window hangs must not abort an already-committed change.
    fn broadcast(setting: &str) -> Result<(), ApplyError> {
        let mut wide: Vec<u16> = setting.encode_utf16().collect();
        wide.push(0);
        let mut result: usize = 0;
        // SAFETY: wide is NUL-terminated and outlives the call; result is an
        // owned out-param. HWND_BROADCAST delivers to all top-level windows.
        let lresult = unsafe {
            SendMessageTimeoutW(
                HWND_BROADCAST,
                WM_SETTINGCHANGE,
                windows::Win32::Foundation::WPARAM(0),
                LPARAM(wide.as_ptr() as isize),
                SMTO_ABORTIFHUNG,
                TIMEOUT_MS,
                Some(&mut result),
            )
        };
        if lresult.0 == 0 {
            tracing::warn!("WM_SETTINGCHANGE({setting}) broadcast timed out; continuing");
        }
        Ok(())
    }
}

impl crate::traits::SettingChangeBroadcast for WindowsSettingChangeBroadcast {
    fn broadcast_locale(&self) -> Result<(), ApplyError> {
        Self::broadcast("Locale")
    }

    fn broadcast_keyboard(&self) -> Result<(), ApplyError> {
        Self::broadcast("Keyboard Layout")
    }
}
