//! Ctrl+C / Ctrl+Break / window-close console handlers, forwarded into a
//! tokio unbounded channel through a process-wide static.

use std::sync::OnceLock;

use tokio::sync::mpsc::UnboundedSender;
use windows::core::BOOL;
use windows::Win32::System::Console::{
    GetConsoleWindow, SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_CLOSE_EVENT, CTRL_C_EVENT,
    CTRL_LOGOFF_EVENT, CTRL_SHUTDOWN_EVENT,
};
use windows::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE};

use crate::shutdown::ShutdownReason;

static CONSOLE_TX: OnceLock<UnboundedSender<ShutdownReason>> = OnceLock::new();

/// Install the console ctrl handler. Events are forwarded to `tx`; the
/// handler returns TRUE so Windows gives the process time to clean up.
pub fn install(tx: UnboundedSender<ShutdownReason>) {
    if CONSOLE_TX.set(tx).is_err() {
        tracing::warn!("console handler already installed");
        return;
    }
    // SAFETY: SetConsoleCtrlHandler is a plain Win32 call; `handler` is a
    // `extern "system"` fn with no captures and never panics.
    unsafe {
        if let Err(e) = SetConsoleCtrlHandler(Some(handler), true) {
            tracing::warn!("SetConsoleCtrlHandler failed: {e}");
        }
    }
}

/// Hide the console window when the binary is started as the tray app
/// (`--run`, e.g. by the Task Scheduler): the exe stays console-subsystem
/// so CLI subcommands keep their output, but the tray run must not leave a
/// visible console window. A brief flash before this call is unavoidable.
pub fn hide_window() {
    // SAFETY: plain Win32 calls; GetConsoleWindow returns NULL when the
    // process has no console (nothing to hide then).
    unsafe {
        let hwnd = GetConsoleWindow();
        if !hwnd.0.is_null() {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
}

/// Console ctrl handler: forwarded events only, no panics, no allocation
/// beyond the channel send.
unsafe extern "system" fn handler(ctrl_type: u32) -> BOOL {
    let reason = match ctrl_type {
        CTRL_C_EVENT | CTRL_BREAK_EVENT | CTRL_CLOSE_EVENT | CTRL_LOGOFF_EVENT
        | CTRL_SHUTDOWN_EVENT => ShutdownReason::Console,
        _ => return BOOL(0),
    };
    // D5: request() sets the shutdown flag at the SOURCE, so the geo lookup
    // in a running apply aborts immediately instead of blocking the loop
    // until the network call returns.
    let _ = crate::shutdown::request(reason);
    // After TRUE returns from CTRL_CLOSE/LOGOFF/SHUTDOWN the OS may terminate
    // the process, so block until the orchestration loop confirms its
    // restore instead of returning immediately. The handler wait is a
    // backstop: the loop's own 5 s watchdog fires first (H6).
    if !crate::shutdown::wait_for_restore(crate::shutdown::RESTORE_HANDLER_WAIT) {
        crate::shutdown::set_restore_timed_out();
    }
    BOOL(1)
}
