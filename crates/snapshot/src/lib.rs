//! System state snapshot, persistence and restore planning.
//!
//! Stage 3 scope: capture, persist, plan. Never touches the system;
//! restore *executors* arrive in stages 4-5 and consume [engine]'s plan.
//!
//! Storage path: `%LocalAppData%\vpose\snapshot.json` + `snapshot.lock`
//! (single-process assumption until stage 7).
//!
//! unsafe rule: allowed ONLY in `src/sys/` (thin Windows FFI wrappers, each
//! block commented `// SAFETY:`); every other module forbids unsafe.

pub mod engine;
pub mod model;
pub mod store;
pub mod sys;
pub mod traits;

pub use engine::{Engine, RestoreDecision, RestoreItem, RestorePlan, SkipReason};
pub use model::{FieldId, FieldSnapshot, FieldValue, Snapshot, SnapshotFields, TzServiceState};
pub use store::{Store, StoreError, DEFAULT_HEARTBEAT_TIMEOUT_SECS};
pub use traits::{Clock, ProcessLiveness, StateReader, SystemClock};

#[cfg(windows)]
pub use sys::{WindowsLiveness, WindowsStateReader};
