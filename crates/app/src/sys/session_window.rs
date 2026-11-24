//! Hidden top-level window for WM_QUERYENDSESSION / WM_ENDSESSION (logoff,
//! shutdown, restart) plus a non-blocking message pump. A message-only
//! (HWND_MESSAGE) window would NOT receive these broadcasts, so this is an
//! invisible top-level window instead.

use std::sync::OnceLock;

use tokio::sync::mpsc::UnboundedSender;
use windows::core::w;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, PeekMessageW, RegisterClassW,
    TranslateMessage, PM_REMOVE, WINDOW_EX_STYLE, WINDOW_STYLE, WM_DESTROY, WM_ENDSESSION,
    WM_QUERYENDSESSION, WNDCLASSW,
};

use crate::shutdown::ShutdownReason;

static SESSION_END_TX: OnceLock<UnboundedSender<ShutdownReason>> = OnceLock::new();

/// Store the sender the window procedure forwards session-end events to.
pub fn install(tx: UnboundedSender<ShutdownReason>) {
    if SESSION_END_TX.set(tx).is_err() {
        tracing::warn!("session-end handler already installed");
    }
}

/// Create the invisible top-level window that receives WM_ENDSESSION.
/// Must be called on the thread that will pump its messages.
pub fn create_hidden_window() -> Result<HWND, String> {
    let class_name = w!("VPosESessionWindow");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: instance_handle(),
        lpszClassName: class_name,
        ..Default::default()
    };
    // SAFETY: `wc` points to static strings; RegisterClassW copies what it
    // needs before returning.
    let atom = unsafe { RegisterClassW(&wc) };
    if atom == 0 {
        return Err("RegisterClassW failed".to_string());
    }
    // SAFETY: class_name is a NUL-terminated static wide string; the window
    // is created with an empty title and is never shown (no WS_VISIBLE).
    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name,
            w!(""),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            None,
            None,
            Some(instance_handle()),
            None,
        )
    };
    hwnd.map_err(|e| format!("CreateWindowExW failed: {e}"))
}

fn instance_handle() -> windows::Win32::Foundation::HINSTANCE {
    // SAFETY: plain Win32 call with a None module name (own exe image).
    let module = unsafe { GetModuleHandleW(None) }.unwrap_or_default();
    windows::Win32::Foundation::HINSTANCE(module.0)
}

/// Drain and dispatch all currently pending messages once, non-blocking.
/// The tray thread calls this every iteration of its UI loop.
pub fn pump_once() {
    let mut msg = windows::Win32::UI::WindowsAndMessaging::MSG::default();
    // SAFETY: `msg` is a caller-allocated MSG; PeekMessageW removes and
    // dispatches pending messages non-blockingly; TranslateMessage and
    // DispatchMessageW only touch the message we just retrieved.
    unsafe {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        // Query phase: another app may still veto the logoff; do nothing and
        // let the default handling proceed.
        WM_QUERYENDSESSION => LRESULT(1),
        WM_ENDSESSION => {
            if wparam.0 != 0 {
                // D5: request() sets the shutdown flag at the SOURCE so the
                // geo lookup in a running apply aborts immediately.
                let _ = crate::shutdown::request(ShutdownReason::SessionEnd);
                // After we return TRUE the OS is free to kill the process, so
                // block until the orchestration loop confirms the shutdown
                // restore. The handler wait is a backstop: the loop's own
                // 5 s watchdog fires first (H6).
                if !crate::shutdown::wait_for_restore(crate::shutdown::RESTORE_HANDLER_WAIT) {
                    crate::shutdown::set_restore_timed_out();
                }
            }
            // End-session results are ignored by the OS; 0 is the convention.
            LRESULT(0)
        }
        WM_DESTROY => LRESULT(0),
        _ => {
            // SAFETY: default window procedure for unhandled messages.
            DefWindowProcW(hwnd, msg, wparam, lparam)
        }
    }
}
