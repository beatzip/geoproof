//! Reversible system-state appliers. Stage 4 scope: the Windows timezone
//! plus the `tzautoupdate` service state.
//!
//! Design:
//! - all OS access goes through [traits] seams; the applier logic is safe
//!   Rust testable offline with mocks;
//! - [crate::sys] is the only module with `unsafe` (thin FFI wrappers,
//!   every block commented `// SAFETY:`);
//! - no dependency on the `snapshot` crate: appliers stay decoupled, the
//!   orchestrator (stage 6) owns snapshot integration;
//! - snapshot integration follows **variant A** (`capture_service_state()`
//!   called by the orchestrator before `apply`), fixed in README.

// unsafe_code = deny comes from [workspace.lints] (opted in via
// [lints] workspace = true in Cargo.toml): every module is denied by
// default and src/sys/* opts out with an explicit allow at module level.

pub mod error;
pub mod locale;
#[cfg(windows)]
pub mod sys;
pub mod timezone;
pub mod traits;

pub use error::{AppliedChange, ApplyError};
pub use locale::{LocaleApplier, LocaleValue};
pub use timezone::tz_registry::WindowsTzRegistry;
pub use timezone::{TimezoneApplier, TimezoneValue, SERVICE_NAME};
pub use traits::{
    AdminCheck, Applier, LocaleRegistry, ServiceController, ServiceStartType,
    SettingChangeBroadcast, TzAutoUpdateState, TzNames, TzRegistry, TzRules, WinTzSetter, ZoneData,
};

#[cfg(windows)]
pub use sys::{
    WindowsAdminCheck, WindowsLocaleRegistry, WindowsRegistry, WindowsServiceController,
    WindowsSettingChangeBroadcast, WindowsTzSetter,
};

/// Minimal single-threaded block_on for manual bins and tests.
pub fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("tokio runtime")
        .block_on(fut)
}
