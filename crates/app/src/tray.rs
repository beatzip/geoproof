//! Tray icon + menu, driven from a dedicated UI thread that also pumps
//! the hidden session window (so WM_ENDSESSION reaches us). Menu events
//! become [TrayCommand]s on a std mpsc channel the orchestration loop
//! drains.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuId, MenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::orchestrator::{Status, TrayCommand};

pub struct TrayIcons {
    pub native: Icon,
    pub vpn: Icon,
    pub error: Icon,
}

pub struct TrayController {
    tray: TrayIcon,
    icons: TrayIcons,
    status_item: MenuItem,
    auto_item: CheckMenuItem,
    id_restore: MenuId,
    id_logs: MenuId,
    id_exit_restore: MenuId,
    id_exit_no_restore: MenuId,
    status: Arc<Mutex<Status>>,
    _keep_menu: Menu,
}

impl TrayController {
    pub fn new(icons: TrayIcons, status: Arc<Mutex<Status>>) -> Result<Self, String> {
        let menu = Menu::new();
        let status_item = MenuItem::new("Starting...", false, None);
        let auto_item = CheckMenuItem::new("Auto-apply", true, true, None);
        let restore_item = MenuItem::new("Restore now", true, None);
        let logs_item = MenuItem::new("Open logs", true, None);
        let exit_restore = MenuItem::new("Exit (restore first)", true, None);
        let exit_no_restore = MenuItem::new("Exit (no restore)", true, None);
        let _ = menu.append_items(&[&status_item, &auto_item, &restore_item, &logs_item]);
        // Debug-only escape hatch: no restore on exit (not in release).
        #[cfg(debug_assertions)]
        let _ = menu.append(&exit_no_restore);
        let _ = menu.append(&exit_restore);

        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu.clone()))
            .with_tooltip("VPosE")
            .with_icon(icons.native.clone())
            .build()
            .map_err(|e| format!("tray build: {e}"))?;

        Ok(TrayController {
            tray,
            icons,
            status_item,
            auto_item,
            id_restore: restore_item.id().clone(),
            id_logs: logs_item.id().clone(),
            id_exit_restore: exit_restore.id().clone(),
            id_exit_no_restore: exit_no_restore.id().clone(),
            status,
            _keep_menu: menu,
        })
    }

    fn refresh(&mut self) {
        let status = match self.status.lock() {
            Ok(guard) => guard.clone(),
            Err(_) => Status::Error("status lock poisoned".into()),
        };
        let (text, icon) = match &status {
            Status::Native => ("Status: native".to_string(), &self.icons.native),
            Status::Connected { country } => {
                (format!("Status: connected ({country})"), &self.icons.vpn)
            }
            Status::Busy => ("Status: applying...".to_string(), &self.icons.native),
            Status::Error(msg) => (format!("Status: error ({msg})"), &self.icons.error),
        };
        self.status_item.set_text(text);
        let _ = self.tray.set_icon(Some(icon.clone()));
    }

    fn handle_menu_event(&mut self, event: &MenuEvent, tx: &mpsc::Sender<TrayCommand>) {
        let cmd = if event.id() == &self.id_restore {
            Some(TrayCommand::RestoreNow)
        } else if event.id() == &self.id_exit_restore {
            Some(TrayCommand::Exit { restore: true })
        } else if event.id() == &self.id_exit_no_restore {
            Some(TrayCommand::Exit { restore: false })
        } else if event.id() == self.auto_item.id() {
            let on = self.auto_item.is_checked();
            Some(TrayCommand::ToggleAutoApply(on))
        } else if event.id() == &self.id_logs {
            match crate::logging::log_dir() {
                Ok(dir) => {
                    if let Err(e) = crate::sys::shell::open_in_explorer(&dir) {
                        tracing::warn!(error = %e, "open logs failed");
                    }
                }
                Err(e) => tracing::warn!(error = %e, "log dir resolve failed"),
            }
            None
        } else {
            None
        };
        if let Some(cmd) = cmd {
            let _ = tx.send(cmd);
        }
    }

    /// Run the UI loop until `stop` is set: drain menu events, refresh the
    /// status readout, pump Windows messages (hidden window: WM_ENDSESSION).
    pub fn run_loop(&mut self, tx: mpsc::Sender<TrayCommand>, stop: Arc<AtomicBool>) {
        while !stop.load(Ordering::SeqCst) {
            while let Ok(event) = MenuEvent::receiver().try_recv() {
                self.handle_menu_event(&event, &tx);
            }
            self.refresh();
            crate::sys::session_window::pump_once();
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        // One last drain so a click right before shutdown is not lost.
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            self.handle_menu_event(&event, &tx);
        }
    }
}
