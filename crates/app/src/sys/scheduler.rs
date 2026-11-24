//! schtasks CLI wrapper (spec decision: CLI, not COM). All commands run
//! hidden (CREATE_NO_WINDOW) since the process runs without a console
//! window when started from Task Scheduler.

use std::os::windows::process::CommandExt;

pub const TASK_NAME: &str = "VPosE";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Run schtasks with `args`, returning the raw output.
pub fn schtasks(args: &[&str]) -> std::io::Result<std::process::Output> {
    std::process::Command::new("schtasks")
        .args(args)
        .creation_flags(CREATE_NO_WINDOW)
        .output()
}
