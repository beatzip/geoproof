//! Graceful shutdown coordination: any signal source funnels into one
//! tokio unbounded channel; the orchestration loop performs the restore
//! and exits. A 5 s watchdog (restore side) guarantees the process never
//! hangs.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;

use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

/// Where a shutdown request came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShutdownReason {
    /// Ctrl+C / Ctrl+Break / console close / logoff console event.
    Console,
    /// WM_QUERYENDSESSION / WM_ENDSESSION on the hidden session window.
    SessionEnd,
    /// User picked "Exit (restore first)" in the tray menu.
    TrayExit,
}

static SHUTDOWN_TX: OnceLock<UnboundedSender<ShutdownReason>> = OnceLock::new();
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

/// Create the process-wide shutdown channel; returns the receiver side.
pub fn init() -> UnboundedReceiver<ShutdownReason> {
    let (tx, rx) = mpsc::unbounded_channel();
    SHUTDOWN_TX
        .set(tx)
        .expect("shutdown channel initialized twice");
    rx
}

/// Clone of the sender side, for signal handlers installed after init.
pub fn sender() -> UnboundedSender<ShutdownReason> {
    SHUTDOWN_TX
        .get()
        .expect("shutdown channel not initialized")
        .clone()
}

/// Request shutdown from anywhere (tray thread, signal handlers).
pub fn request(reason: ShutdownReason) -> bool {
    SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
    SHUTDOWN_TX.get().is_some_and(|tx| tx.send(reason).is_ok())
}

/// D5: whether any source requested shutdown. The geo lookup races against
/// this flag so a shutdown during a lookup aborts the apply immediately
/// instead of waiting for the network (worst case was ~23.5 s, longer than
/// the 5 s handler wait). The channel message stays queued for the run
/// loop's own select!.
pub fn shutdown_requested() -> bool {
    SHUTDOWN_REQUESTED.load(Ordering::SeqCst)
}

/// How long the orchestration loop lets a shutdown restore run (H6): the
/// restore body executes on a worker thread, so the tokio timer can fire
/// while the blocking registry/SCM work proceeds.
pub const RESTORE_WATCHDOG: Duration = Duration::from_secs(5);

/// How long a WM_ENDSESSION / console handler waits for the restore
/// confirmation. Strictly greater than [RESTORE_WATCHDOG]: the loop's
/// watchdog must fire and release the handler through
/// [signal_restore_complete] first; this value is only a backstop.
pub const RESTORE_HANDLER_WAIT: Duration = Duration::from_secs(15);

static RESTORE_COMPLETE: OnceLock<Arc<(Mutex<bool>, Condvar)>> = OnceLock::new();
static RESTORE_TIMED_OUT: AtomicBool = AtomicBool::new(false);

fn restore_slot() -> &'static Arc<(Mutex<bool>, Condvar)> {
    RESTORE_COMPLETE.get_or_init(|| Arc::new((Mutex::new(false), Condvar::new())))
}

/// The orchestration loop calls this once its shutdown restore finished, so a
/// blocked wndproc / console handler can stop waiting and let the OS proceed.
pub fn signal_restore_complete() {
    let (done, cv) = restore_slot().as_ref();
    let mut guard = done.lock().unwrap_or_else(|e| e.into_inner());
    *guard = true;
    cv.notify_all();
}

/// Block up to `timeout` waiting for the shutdown restore to be confirmed.
/// Returns true when the restore completed, false on watchdog expiry.
pub fn wait_for_restore(timeout: Duration) -> bool {
    let (done, cv) = restore_slot().as_ref();
    let guard = done.lock().unwrap_or_else(|e| e.into_inner());
    if *guard {
        return true;
    }
    let (guard, _res) = cv
        .wait_timeout_while(guard, timeout, |finished| !*finished)
        .unwrap_or_else(|e| e.into_inner());
    *guard
}

/// Mark that the 5 s restore watchdog expired without a confirmed restore.
pub fn set_restore_timed_out() {
    RESTORE_TIMED_OUT.store(true, Ordering::SeqCst);
}

/// Whether any shutdown restore hit the watchdog (drives the exit code).
pub fn restore_timed_out() -> bool {
    RESTORE_TIMED_OUT.load(Ordering::SeqCst)
}
