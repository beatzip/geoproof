//! vpose_autostart: standalone helper to create/remove/query the Task
//! Scheduler entry manually (no tray, no orchestration). Same operations
//! as `vpose.exe --autostart ...`.

fn main() -> std::process::ExitCode {
    let action = match std::env::args().nth(1).as_deref() {
        Some("install") => install(),
        Some("uninstall") => uninstall(),
        Some("status") => status(),
        _ => {
            eprintln!("usage: vpose_autostart <install|uninstall|status>");
            return std::process::ExitCode::from(2);
        }
    };
    match action {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn install() -> Result<(), String> {
    let res = app::autostart::install_elevated();
    if res.is_ok() {
        println!("autostart task installed");
    }
    res
}

fn uninstall() -> Result<(), String> {
    let res = app::autostart::uninstall();
    if res.is_ok() {
        println!("autostart task removed");
    }
    res
}

fn status() -> Result<(), String> {
    match app::autostart::status() {
        Ok(true) => {
            println!("autostart task installed");
            Ok(())
        }
        Ok(false) => {
            println!("autostart task not installed");
            Err("exit 1: task missing".to_string())
        }
        Err(e) => Err(e),
    }
}
