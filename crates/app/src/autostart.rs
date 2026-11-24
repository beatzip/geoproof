//! Task Scheduler autostart via schtasks CLI (spec decision: CLI, not
//! COM). The task runs the elevated monolith at logon; UAC is requested
//! once at install time via a `runas` self-restart.

use apply::traits::AdminCheck;
#[cfg(windows)]
use apply::WindowsAdminCheck;

use crate::sys::scheduler::{schtasks, TASK_NAME};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutostartAction {
    Install,
    Uninstall,
    Status,
}

/// `schtasks /Create /TN VPosE /TR "\"<exe>\" --run" /SC ONLOGON /DELAY
/// 0000:30 /RL HIGHEST /F`. The delay lets the elevated token become
/// available: without it the logon trigger can start the task with the
/// filtered (non-elevated) token.
pub fn install() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let tr = format!("\"{}\" --run", exe.display());
    let out = schtasks(&[
        "/Create", "/TN", TASK_NAME, "/TR", &tr, "/SC", "ONLOGON", "/DELAY", "0000:30", "/RL",
        "HIGHEST", "/F",
    ])
    .map_err(|e| format!("schtasks spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "schtasks /Create failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    tracing::info!(task = TASK_NAME, "autostart task installed");
    Ok(())
}

pub fn uninstall() -> Result<(), String> {
    let out = schtasks(&["/Delete", "/TN", TASK_NAME, "/F"])
        .map_err(|e| format!("schtasks spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "schtasks /Delete failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    tracing::info!(task = TASK_NAME, "autostart task removed");
    Ok(())
}

/// `Ok(true)` when the task exists, `Ok(false)` when not.
pub fn status() -> Result<bool, String> {
    status_for_task(TASK_NAME)
}

/// Query an arbitrary task by name (test seam: a guaranteed-absent task
/// name gives a deterministic `Ok(false)` without touching the real one).
pub fn status_for_task(task_name: &str) -> Result<bool, String> {
    let out = schtasks(&["/Query", "/TN", task_name, "/FO", "CSV"])
        .map_err(|e| format!("schtasks spawn: {e}"))?;
    Ok(out.status.success())
}

/// Install, elevating via UAC when the current token is not admin: the
/// process re-runs itself with the same `--autostart install` arguments
/// through ShellExecuteW "runas" and this invocation returns.
#[cfg(windows)]
pub fn install_elevated() -> Result<(), String> {
    if WindowsAdminCheck.is_admin() {
        return install();
    }
    tracing::info!("not elevated; requesting UAC for autostart install");
    crate::sys::shell::runas_self(&["--autostart", "install"])
}
