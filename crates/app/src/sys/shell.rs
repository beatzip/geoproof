//! Shell helpers: open a folder in Explorer, and self-elevation via
//! ShellExecuteW `runas` (autostart install only — the scheduled task
//! itself needs no elevation at logon).

use windows::core::PCWSTR;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Open `path` in Explorer ("explore" verb).
pub fn open_in_explorer(path: &std::path::Path) -> Result<(), String> {
    let verb = wide("explore");
    let file = wide(&path.to_string_lossy());
    // SAFETY: all string buffers are NUL-terminated and outlive the call;
    // the result handle is inspected and not stored.
    let res = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            None,
            None,
            SW_SHOWNORMAL,
        )
    };
    if res.0 as isize <= 32 {
        return Err(format!("ShellExecuteW explore failed: {res:?}"));
    }
    Ok(())
}

/// Re-run the current executable with `args` under the UAC prompt
/// ("runas"). Fire-and-forget: the elevated child does its own reporting.
pub fn runas_self(args: &[&str]) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let params = args
        .iter()
        .map(|a| format!("\"{a}\""))
        .collect::<Vec<_>>()
        .join(" ");
    let verb = wide("runas");
    let file = wide(&exe.to_string_lossy());
    let params = wide(&params);
    // SAFETY: all string buffers are NUL-terminated and outlive the call;
    // the result handle is inspected and not stored.
    let res = unsafe {
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR(params.as_ptr()),
            None,
            SW_SHOWNORMAL,
        )
    };
    if res.0 as isize <= 32 {
        return Err(format!("UAC elevation was declined or failed: {res:?}"));
    }
    Ok(())
}
