//! File logging: %LocalAppData%\vpose\logs\vpose.log, daily rotation,
//! 7 files kept. Level: info by default, RUST_LOG overrides.

use tracing_subscriber::EnvFilter;

pub fn init() -> Result<tracing_appender::non_blocking::WorkerGuard, String> {
    let dir = log_dir()?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("create log dir {dir:?}: {e}"))?;
    let appender = tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("vpose")
        .filename_suffix("log")
        .max_log_files(7)
        .build(&dir)
        .map_err(|e| format!("log appender: {e}"))?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_ansi(false)
        .with_writer(writer)
        .try_init()
        .map_err(|e| format!("tracing init: {e}"))?;
    Ok(guard)
}

pub fn log_dir() -> Result<std::path::PathBuf, String> {
    let base = std::env::var("LOCALAPPDATA").map_err(|_| "LOCALAPPDATA is not set".to_string())?;
    Ok(std::path::Path::new(&base).join("vpose").join("logs"))
}
