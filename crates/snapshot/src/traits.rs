//! OS-facing seams. All production OS access goes through these traits;
//! tests substitute mocks, so business logic stays pure safe Rust.

use crate::model::{FieldId, FieldValue};

/// Reads the current system values for every known field.
///
/// Windows impl lives in crate::sys; it is read-only and needs no admin.
pub trait StateReader {
    fn read_all(&self) -> std::io::Result<Vec<(FieldId, FieldValue)>>;
}

/// Is a process (pid) still alive? Windows: OpenProcess + GetExitCodeProcess.
/// Pid reuse is a known risk; the heartbeat decides, this trait only observes.
pub trait ProcessLiveness: Send + Sync {
    fn is_alive(&self, pid: u32) -> bool;
}

/// Clock seam for heartbeat expiry decisions.
pub trait Clock: Send + Sync {
    /// Seconds since the Unix epoch.
    fn now_unix(&self) -> u64;
}

/// Real system clock.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_unix(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    }
}
