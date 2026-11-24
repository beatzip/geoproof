//! vpose.exe entrypoint: CLI (--autostart install|uninstall|status) and
//! the tray application (--run or no args).

use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use app::orchestrator::{Orchestration, StartupOutcome, TrayCommand};
use apply::traits::AdminCheck;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--autostart") => autostart_cli(&args),
        Some("--run") => run_app(),
        Some("--help") | Some("-h") => {
            print_usage();
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("unknown argument: {other}");
            print_usage();
            ExitCode::from(2)
        }
        // No args and --run both mean "run the tray application".
        _ => run_app(),
    }
}

fn print_usage() {
    println!(
        "VPosE — region follow for VPN (stage 6)\n\
         Usage:\n\
         \x20 vpose.exe                     run the tray application\n\
         \x20 vpose.exe --run               run the tray application (from Task Scheduler)\n\
         \x20 vpose.exe --autostart install    create the logon task (UAC if needed)\n\
         \x20 vpose.exe --autostart uninstall  remove the logon task\n\
         \x20 vpose.exe --autostart status     exit 0 when the task exists, 1 otherwise"
    );
}

fn autostart_cli(args: &[String]) -> ExitCode {
    match args.get(1).map(String::as_str) {
        Some("install") => match app::autostart::install_elevated() {
            Ok(()) => {
                println!("autostart task installed");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("install failed: {e}");
                ExitCode::FAILURE
            }
        },
        Some("uninstall") => match app::autostart::uninstall() {
            Ok(()) => {
                println!("autostart task removed");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("uninstall failed: {e}");
                ExitCode::FAILURE
            }
        },
        Some("status") => match app::autostart::status() {
            Ok(true) => {
                println!("autostart task installed");
                ExitCode::SUCCESS
            }
            Ok(false) => {
                println!("autostart task not installed");
                ExitCode::from(1)
            }
            Err(e) => {
                eprintln!("status query failed: {e}");
                ExitCode::FAILURE
            }
        },
        _ => {
            eprintln!("--autostart needs: install | uninstall | status");
            ExitCode::from(2)
        }
    }
}

fn run_app() -> ExitCode {
    // Hide the console window the Task Scheduler leaves visible; CLI
    // subcommands keep their output because they return before this.
    app::sys::console::hide_window();

    // File logging first: everything below is recorded.
    let _log_guard = match app::logging::init() {
        Ok(guard) => guard,
        Err(e) => {
            eprintln!("logging init failed: {e}");
            return ExitCode::FAILURE;
        }
    };

    // Single instance across all user sessions.
    let _mutex = match app::sys::single_instance::SingleInstance::acquire(
        app::sys::single_instance::SINGLE_INSTANCE_MUTEX,
    ) {
        Some(guard) => guard,
        None => {
            eprintln!("VPosE is already running (instance mutex is held)");
            return ExitCode::from(1);
        }
    };

    match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt.block_on(app_main()),
        Err(e) => {
            eprintln!("runtime init failed: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn app_main() -> ExitCode {
    let shutdown_rx = app::shutdown::init();
    app::sys::console::install(app::shutdown::sender());
    app::sys::session_window::install(app::shutdown::sender());

    let data_dir = match std::env::var("LOCALAPPDATA") {
        Ok(base) => std::path::Path::new(&base).join("vpose"),
        Err(_) => {
            eprintln!("LOCALAPPDATA is not set");
            return ExitCode::FAILURE;
        }
    };
    let engine = snapshot::Engine::new(snapshot::Store::new(data_dir.join("snapshot")));
    let reader = snapshot::WindowsStateReader;
    let liveness = snapshot::WindowsLiveness;
    let clock: Arc<dyn snapshot::Clock + Send + Sync> = Arc::new(snapshot::SystemClock);

    let geo_client = Arc::new(geo::GeoClient::with_reqwest(geo::GeoClientConfig::default()));

    // The tz registry owns its WindowsRegistry handle so the applier chain
    // is 'static and can be moved to the restore worker thread (H6).
    let tz_registry = apply::timezone::tz_registry::WindowsTzRegistry::new();
    let tz_applier = apply::TimezoneApplier::new(
        tz_registry,
        apply::WindowsServiceController,
        apply::WindowsTzSetter,
        apply::WindowsAdminCheck,
    );
    let locale_applier = apply::LocaleApplier::new(
        apply::WindowsLocaleRegistry::new(),
        apply::WindowsSettingChangeBroadcast::new(),
    );

    let mut orch = Orchestration::new(
        geo_client,
        tz_applier,
        locale_applier,
        engine,
        reader,
        clock,
    );

    if !apply::WindowsAdminCheck.is_admin() {
        tracing::warn!("running without admin rights; timezone apply will be refused");
    }

    // Crash restore happens strictly BEFORE the watcher starts: otherwise
    // the first ip event could capture VPN state as the new originals.
    let outcome = orch
        .startup(
            &liveness,
            snapshot::DEFAULT_HEARTBEAT_TIMEOUT_SECS,
            std::process::id(),
        )
        .await;
    if outcome == StartupOutcome::LockHeld {
        eprintln!("another live VPosE instance holds the lock; exiting");
        return ExitCode::from(1);
    }

    // Public-IP providers: plain-text endpoints. All requests are pinned to
    // IPv4 (local_address) so a dual-stack host cannot make providers
    // disagree about the address family. Consensus is a strict majority
    // among the providers that ANSWERED, so one permanently blocked
    // endpoint does not blind the watcher.
    let providers: Vec<Arc<dyn watcher::IpProvider>> = vec![
        Arc::new(watcher::HttpProvider::new(
            "https://api.ipify.org",
            Duration::from_secs(5),
        )),
        Arc::new(watcher::HttpProvider::new(
            "https://ifconfig.me/ip",
            Duration::from_secs(5),
        )),
        Arc::new(watcher::HttpProvider::new(
            "https://icanhazip.com",
            Duration::from_secs(5),
        )),
    ];
    let (ip_watcher, events_rx) =
        match watcher::IpWatcher::new(watcher::IpWatcherConfig::default(), providers) {
            Ok(pair) => pair,
            Err(e) => {
                tracing::error!(error = %e, "watcher init failed");
                return ExitCode::FAILURE;
            }
        };
    tokio::spawn(async move {
        ip_watcher.run().await;
    });

    // Tray UI on its own thread (it owns the hidden session window).
    let (cmd_tx, cmd_rx) = mpsc::channel::<TrayCommand>();
    let cmd_rx = Arc::new(Mutex::new(cmd_rx));
    let stop_ui = Arc::new(AtomicBool::new(false));
    let icons = match load_icons() {
        Ok(icons) => icons,
        Err(e) => {
            tracing::error!(error = %e, "icon load failed");
            return ExitCode::FAILURE;
        }
    };
    let status = orch.status_handle();
    let stop_for_ui = Arc::clone(&stop_ui);
    let ui_thread = std::thread::spawn(move || {
        if let Err(e) = app::sys::session_window::create_hidden_window() {
            tracing::warn!(error = %e, "session window failed; logoff will not be graceful");
        }
        match app::tray::TrayController::new(icons, status) {
            Ok(mut tray) => tray.run_loop(cmd_tx, stop_for_ui),
            Err(e) => {
                tracing::error!(error = %e, "tray init failed; shutting down");
                app::shutdown::request(app::shutdown::ShutdownReason::Console);
            }
        }
    });

    orch.run(events_rx, shutdown_rx, cmd_rx).await;

    stop_ui.store(true, Ordering::SeqCst);
    let _ = ui_thread.join();
    if app::shutdown::restore_timed_out() {
        tracing::error!("restore did not finish within the watchdog; exiting nonzero");
        return ExitCode::from(1);
    }
    tracing::info!("VPosE exited");
    ExitCode::SUCCESS
}

fn load_icons() -> Result<app::tray::TrayIcons, String> {
    Ok(app::tray::TrayIcons {
        native: icon_from_ico(include_bytes!("../../assets/icon_native.ico"))?,
        vpn: icon_from_ico(include_bytes!("../../assets/icon_vpn.ico"))?,
        error: icon_from_ico(include_bytes!("../../assets/icon_error.ico"))?,
    })
}

fn icon_from_ico(bytes: &[u8]) -> Result<tray_icon::Icon, String> {
    let img = image::load_from_memory(bytes).map_err(|e| format!("ico decode: {e}"))?;
    let rgba = img.to_rgba8();
    let (w, h) = rgba.dimensions();
    tray_icon::Icon::from_rgba(rgba.into_raw(), w, h).map_err(|e| format!("icon rgba: {e}"))
}
