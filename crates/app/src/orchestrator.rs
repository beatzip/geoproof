//! Orchestration: watcher events -> geo -> snapshot -> apply, and restore
//! on disconnect / exit. Generic over the apply seams; the snapshot
//! [Engine] stays concrete (the store dir is the test seam).

use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use apply::error::ApplyError;
use apply::locale::{LocaleApplier, LocaleValue};
use apply::timezone::{TimezoneApplier, TimezoneValue};
use apply::traits::{
    AdminCheck, Applier, LocaleRegistry, ServiceController, SettingChangeBroadcast,
    TzAutoUpdateState, TzRegistry, WinTzSetter,
};
use geo::{GeoInfo, GeoProvider};
use shared::IpEvent;
use snapshot::engine::RestoreItem;
use snapshot::traits::StateReader;
use snapshot::{Clock, Engine, FieldId, FieldValue, ProcessLiveness, TzServiceState};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::sync::{Mutex as AsyncMutex, MutexGuard as AsyncMutexGuard};

/// Seconds between lock heartbeats (well under the snapshot's 60 s stale
/// threshold).
pub const HEARTBEAT_INTERVAL_SECS: u64 = 15;

/// Tray-facing orchestration state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// No VPN region applied.
    Native,
    /// VPN region applied; country is the ISO code.
    Connected { country: String },
    /// An apply/restore cycle is running.
    Busy,
    /// The last operation failed.
    Error(String),
}

/// Commands the tray menu sends into the orchestration loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCommand {
    RestoreNow,
    ToggleAutoApply(bool),
    Exit { restore: bool },
}

/// What [Orchestration::startup] decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartupOutcome {
    /// Nothing pending; the loop can start normally.
    Clean,
    /// A crash restore was executed before the watcher started.
    Restored,
    /// Another live instance holds the lock; the caller must exit.
    LockHeld,
}

pub struct Orchestration<G, R, TR, SR, VR, AR, LR, BR>
where
    G: GeoProvider,
    R: StateReader,
    TR: TzRegistry,
    SR: ServiceController,
    VR: WinTzSetter,
    AR: AdminCheck,
    LR: LocaleRegistry,
    BR: SettingChangeBroadcast,
{
    geo: Arc<G>,
    // Appliers are shared with the restore worker thread (H6/D1): the
    // blocking system work (registry, SCM, broadcasts) runs off the
    // single-threaded runtime so the tokio timer can fire.
    tz: Arc<AsyncMutex<TimezoneApplier<TR, SR, VR, AR>>>,
    locale: Arc<AsyncMutex<LocaleApplier<LR, BR>>>,
    engine: Engine,
    reader: Arc<R>,
    clock: Arc<dyn Clock + Send + Sync>,
    status: Arc<Mutex<Status>>,
    auto_apply: bool,
    current_country: Option<String>,
    /// Geo lookup that failed and must be retried with backoff (H9): the IP
    /// changed but the region was not applied; retrying until success or a
    /// newer IP event replaces it.
    geo_retry: Option<GeoRetry>,
    /// This process's pid, set at startup; mid-run restores re-arm the
    /// snapshot lock with it.
    pid: Option<u32>,
    /// Extra shutdown-abort signal for tests (D5): the production sources
    /// set the process-wide flag via shutdown::request(); tests flip this
    /// probe instead so they never touch the global.
    shutdown_probe: Option<Arc<std::sync::atomic::AtomicBool>>,
}

/// A pending geo lookup retry: which IP still needs its region, how many
/// attempts were made, and when the next attempt is due (unix seconds).
#[derive(Debug, Clone, Copy)]
struct GeoRetry {
    ip: IpAddr,
    attempts: u32,
    next_at_unix: u64,
}

/// First retry after 30 s, then doubling, capped at 5 minutes.
const GEO_RETRY_BASE_SECS: u64 = 30;
const GEO_RETRY_MAX_SECS: u64 = 300;

/// What one timezone apply did, for a later rollback if a following step
/// (locale, snapshot record) fails.
struct TzApplyOutcome {
    /// set_dynamic actually wrote a new zone (not a Noop/skip).
    mutated: bool,
    /// Pre-apply zone value (from the tz setter).
    pre: Option<TimezoneValue>,
    /// tzautoupdate state before the apply.
    pre_service: Option<TzAutoUpdateState>,
    /// H10: the applied timezone record BEFORE this apply. A later rollback
    /// must bring the record back to this value: the previous applied state
    /// on a repeated apply, no record at all on the first one.
    prev_record: Option<FieldValue>,
}

impl<G, R, TR, SR, VR, AR, LR, BR> Orchestration<G, R, TR, SR, VR, AR, LR, BR>
where
    G: GeoProvider,
    // Send + Sync + 'static on the system seams: restore runs on a
    // dedicated worker thread (H6), so the appliers and reader must be
    // movable into it.
    R: StateReader + Send + Sync + 'static,
    TR: TzRegistry + Send + Sync + 'static,
    SR: ServiceController + Send + Sync + 'static,
    VR: WinTzSetter + Send + Sync + 'static,
    AR: AdminCheck + Send + Sync + 'static,
    LR: LocaleRegistry + Send + Sync + 'static,
    BR: SettingChangeBroadcast + Send + Sync + 'static,
{
    pub fn new(
        geo: Arc<G>,
        tz: TimezoneApplier<TR, SR, VR, AR>,
        locale: LocaleApplier<LR, BR>,
        engine: Engine,
        reader: R,
        clock: Arc<dyn Clock + Send + Sync>,
    ) -> Self {
        Orchestration {
            geo,
            tz: Arc::new(AsyncMutex::new(tz)),
            locale: Arc::new(AsyncMutex::new(locale)),
            engine,
            reader: Arc::new(reader),
            clock,
            status: Arc::new(Mutex::new(Status::Native)),
            auto_apply: true,
            current_country: None,
            geo_retry: None,
            pid: None,
            shutdown_probe: None,
        }
    }

    /// Shared status handle for the tray.
    pub fn status_handle(&self) -> Arc<Mutex<Status>> {
        Arc::clone(&self.status)
    }

    /// Test hook (D5): extra shutdown-abort signal checked alongside the
    /// global flag during the geo lookup.
    pub fn set_shutdown_probe(&mut self, probe: Arc<std::sync::atomic::AtomicBool>) {
        self.shutdown_probe = Some(probe);
    }

    pub fn auto_apply(&self) -> bool {
        self.auto_apply
    }

    fn set_status(&self, status: Status) {
        set_shared_status(&self.status, status);
    }

    /// Startup sequence: crash restore first (before the watcher can emit
    /// an event and overwrite originals), then save-once + process lock.
    pub async fn startup(
        &mut self,
        liveness: &dyn ProcessLiveness,
        heartbeat_timeout_secs: u64,
        pid: u32,
    ) -> StartupOutcome {
        self.pid = Some(pid);
        match self
            .engine
            .startup_decision(liveness, &*self.clock, heartbeat_timeout_secs)
        {
            Ok(snapshot::RestoreDecision::LockHeld) => return StartupOutcome::LockHeld,
            Ok(snapshot::RestoreDecision::RestoreNeeded) => {
                tracing::info!("crash restore needed; running before the watcher starts");
                self.restore_to_native().await;
                // Re-arm ONLY on a complete restore: it consumed the old
                // snapshot and lock, so later applies must be tracked
                // again. A failed restore keeps the old snapshot for the
                // next-start retry; overwriting it would lose the
                // originals.
                self.rearm_after_restore();
                return StartupOutcome::Restored;
            }
            Ok(snapshot::RestoreDecision::NoSnapshot) => {}
            Err(e) => {
                // No readable decision: treat as clean start but log loudly;
                // save-once below will fail the same way if storage is broken.
                tracing::error!(error = %e, "startup_decision failed; continuing as clean start");
            }
        }
        self.arm(pid);
        StartupOutcome::Clean
    }

    /// Save originals once and take the process lock. Failures are logged;
    /// the loop still runs, but restore protection is degraded.
    fn arm(&self, pid: u32) {
        let now = self.clock.now_unix();
        let created = rfc3339(now);
        if let Err(e) = self.engine.save(&*self.reader, created) {
            tracing::error!(error = %e, "snapshot save failed");
        }
        if let Err(e) = self.engine.store().write_lock(pid, now) {
            tracing::error!(error = %e, "lock write failed");
        }
    }

    /// Refresh the heartbeat of the existing lock. Fails if there is no lock.
    /// Re-arm after a mid-run restore: the restore consumed the snapshot and
    /// the lock, so later applies must be tracked again (and the heartbeat
    /// needs its lock file). A failed restore keeps the old snapshot and is
    /// NOT re-armed — the originals stay for the next-start retry.
    fn rearm_after_restore(&self) {
        let restored = matches!(self.status.lock().map(|g| g.clone()), Ok(Status::Native));
        if restored {
            if let Some(pid) = self.pid {
                self.arm(pid);
                tracing::info!("re-armed fresh snapshot after restore");
            }
        }
    }

    pub fn touch_heartbeat(&self) {
        if let Err(e) = self.engine.store().touch_heartbeat(self.clock.now_unix()) {
            tracing::warn!(error = %e, "heartbeat touch failed");
        }
    }

    /// Main loop: IP events, tray commands, shutdown signal, heartbeats.
    /// Returns when the watcher channel closes or shutdown/exit arrives.
    pub async fn run(
        &mut self,
        mut events: broadcast::Receiver<IpEvent>,
        mut shutdown: mpsc::UnboundedReceiver<crate::shutdown::ShutdownReason>,
        commands: Arc<Mutex<std::sync::mpsc::Receiver<TrayCommand>>>,
    ) {
        let mut heartbeat = tokio::time::interval(Duration::from_secs(HEARTBEAT_INTERVAL_SECS));
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut cmd_tick = tokio::time::interval(Duration::from_millis(200));
        cmd_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut geo_tick = tokio::time::interval(Duration::from_secs(1));
        geo_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                biased;
                reason = shutdown.recv() => {
                    tracing::info!(?reason, "shutdown signal");
                    self.set_status(Status::Busy);
                    // Always restore on graceful shutdown (spec: no
                    // "restore only if applied" flag; the snapshot makes a
                    // no-op restore cheap and safe). Capped with the 5 s
                    // watchdog (H6): the restore runs on a worker thread, so
                    // the tokio timer can actually fire while the blocking
                    // registry/SCM work proceeds; after WM_ENDSESSION the OS
                    // may kill the process at any moment.
                    self.restore_with_watchdog().await;
                    crate::shutdown::signal_restore_complete();
                    break;
                }
                ev = events.recv() => match ev {
                    Ok(event) => self.on_event(event).await,
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!(skipped = n, "ip event lag");
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        tracing::warn!("ip event channel closed");
                        break;
                    }
                },
                _ = geo_tick.tick() => self.poll_geo_retry().await,
                _ = heartbeat.tick() => self.touch_heartbeat(),
                _ = cmd_tick.tick() => {
                    if self.drain_commands(&commands).await {
                        break;
                    }
                }
            }
        }
    }

    async fn on_event(&mut self, event: IpEvent) {
        match event {
            IpEvent::Connected(ip) => {
                if !self.auto_apply {
                    tracing::info!(%ip, "auto-apply disabled; ip event ignored");
                    return;
                }
                // C1 (D3): native detection combines the remembered native
                // IPs with the geo-vs-originals match — see apply_for_ip.
                // Connected goes through the same dispatch as Changed; the
                // first IP seen while nothing is applied is recorded as
                // native there.
                self.apply_for_ip(ip).await;
            }
            IpEvent::Changed(_, ip) => {
                if !self.auto_apply {
                    tracing::info!(%ip, "auto-apply disabled; ip event ignored");
                    return;
                }
                self.apply_for_ip(ip).await;
            }
            IpEvent::Disconnected => {
                self.geo_retry = None;
                if self.auto_apply {
                    tracing::info!("vpn disconnected; restoring native state");
                    self.set_status(Status::Busy);
                    self.restore_to_native().await;
                    self.rearm_after_restore();
                } else {
                    tracing::info!("auto-apply disabled; disconnect ignored");
                }
            }
        }
    }

    /// Process one watcher event through the same dispatch as the run
    /// loop (public entry for integration tests).
    pub async fn handle_ip_event(&mut self, event: IpEvent) {
        self.on_event(event).await;
    }

    /// Drain pending tray commands. Returns true when the loop must exit.
    async fn drain_commands(
        &mut self,
        commands: &Arc<Mutex<std::sync::mpsc::Receiver<TrayCommand>>>,
    ) -> bool {
        let mut exit = false;
        loop {
            let cmd = match commands.lock() {
                Ok(rx) => rx.try_recv(),
                Err(_) => break,
            };
            match cmd {
                Ok(TrayCommand::RestoreNow) => {
                    self.set_status(Status::Busy);
                    self.restore_to_native().await;
                    self.rearm_after_restore();
                }
                Ok(TrayCommand::ToggleAutoApply(on)) => {
                    self.auto_apply = on;
                    tracing::info!(enabled = on, "auto-apply toggled");
                }
                Ok(TrayCommand::Exit { restore }) => {
                    if restore {
                        self.set_status(Status::Busy);
                        // Tray Exit is the path the review requires to be
                        // watchdog-capped (review Table 6 #1).
                        self.restore_with_watchdog().await;
                        // Release a possibly waiting console/wndproc waiter.
                        crate::shutdown::signal_restore_complete();
                    }
                    exit = true;
                    break;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    tracing::warn!("tray command channel closed");
                    break;
                }
            }
        }
        exit
    }

    /// Apply region settings for `ip`. Fail-closed (H2): without a working
    /// snapshot store nothing is applied and the status goes to Error.
    pub async fn apply_for_ip(&mut self, ip: IpAddr) {
        tracing::info!(%ip, "applying region for ip");
        self.set_status(Status::Busy);

        if let Err(e) = self.ensure_snapshot_tracked() {
            tracing::error!(%ip, error = %e, "snapshot store unavailable; nothing applied");
            self.arm_geo_retry(ip);
            self.set_status(Status::Error("snapshot store unavailable".into()));
            return;
        }
        // H3: originals captured at startup can be stale by the first apply;
        // refresh them while nothing is applied yet.
        if let Err(e) = self.recapture_originals_if_unapplied() {
            tracing::error!(%ip, error = %e, "originals recapture failed; nothing applied");
            self.arm_geo_retry(ip);
            self.set_status(Status::Error("snapshot recapture failed".into()));
            return;
        }

        // D5: the geo lookup races against the shutdown flag — a shutdown
        // during a lookup aborts the apply immediately; the loop then takes
        // its own shutdown branch without waiting for the network (the old
        // worst case was ~23.5 s, longer than the 5 s handler wait).
        let lookup = tokio::select! {
            biased;
            _ = wait_shutdown_requested(self.shutdown_probe.as_ref()) => None,
            result = self.geo.lookup(ip) => Some(result),
        };
        let info = match lookup {
            None => {
                tracing::info!(%ip, "shutdown during geo lookup; apply aborted");
                return;
            }
            Some(Ok(info)) => info,
            Some(Err(e)) => {
                // H9: a single failure must not leave the region stale until
                // the next IP change; arm the backoff retry and show Error.
                tracing::warn!(%ip, error = %e, "geo lookup failed; will retry with backoff");
                self.arm_geo_retry(ip);
                self.set_status(Status::Error("geo lookup failed".into()));
                return;
            }
        };
        self.geo_retry = None;
        tracing::info!(
            %ip,
            country = %info.country_code,
            tz = %info.timezone,
            "geo lookup ok"
        );

        // C1 (D3): two native signals, combined. An IP remembered as native
        // is sufficient on its own — a geo database can be wrong for a home
        // network (e.g. Kaliningrad reported as Europe/Moscow, p07). The
        // geo-vs-originals match is the fallback for a home IP the list
        // does not know (new DHCP lease, same network).
        let ip_native = self.ip_is_native(ip);
        if self.anything_applied() && (ip_native || self.geo_matches_originals(&info)) {
            tracing::info!(
                %ip,
                native_ip = ip_native,
                "ip is in the native region; restoring originals"
            );
            self.set_status(Status::Busy);
            self.restore_to_native().await;
            self.rearm_after_restore();
            return;
        }
        // First ever observation (HEAD heuristic): the first IP seen while
        // nothing is applied yet is the home network — record it and show
        // Native instead of applying its geo (p06, expat p08). An IP already
        // in the list means the user is home again with nothing to restore.
        if !self.anything_applied() && (ip_native || self.native_ips().is_empty()) {
            if !ip_native {
                self.record_native_ip(ip);
            }
            self.set_status(Status::Native);
            return;
        }

        let tz_out = match self.apply_timezone(&info).await {
            Ok(out) => out,
            Err(e) => {
                // H10: apply errors must be visible, not a silent blue icon.
                // The timezone applier already rolled its own service state
                // back; nothing locale-side was touched yet.
                tracing::error!(%ip, error = %e, "timezone apply failed");
                self.set_status(Status::Error(e));
                return;
            }
        };
        if let Err(e) = self.apply_locale(&info).await {
            tracing::error!(%ip, error = %e, "locale apply failed; rolling back timezone");
            // H10 (D2): on a repeated apply the pre-apply timezone is the
            // PREVIOUS applied region, not native. After a successful
            // rollback the applied record must be rewritten to that state,
            // or restore_plan later sees current != applied, skips the zone
            // as UserChanged and deletes the snapshot — leaving the wrong
            // zone and a disabled service behind.
            if tz_out.mutated && self.rollback_timezone(&tz_out).await {
                self.reapply_applied_tz_record(&tz_out);
            }
            self.set_status(Status::Error(e));
            return;
        }
        self.current_country = Some(info.country_code.clone());
        self.set_status(Status::Connected {
            country: info.country_code.clone(),
        });
    }

    /// Retry a pending geo lookup when its backoff elapsed (H9). Called from
    /// the run loop tick and directly from tests.
    pub async fn poll_geo_retry(&mut self) {
        let Some(retry) = self.geo_retry else {
            return;
        };
        if self.clock.now_unix() < retry.next_at_unix {
            return;
        }
        tracing::info!(ip = %retry.ip, attempts = retry.attempts, "retrying geo lookup");
        self.apply_for_ip(retry.ip).await;
    }

    fn arm_geo_retry(&mut self, ip: IpAddr) {
        let attempts = match &self.geo_retry {
            Some(retry) if retry.ip == ip => retry.attempts + 1,
            _ => 1,
        };
        let backoff = GEO_RETRY_BASE_SECS << (attempts - 1).min(4);
        let backoff = backoff.min(GEO_RETRY_MAX_SECS);
        let next_at = self.clock.now_unix() + backoff;
        tracing::debug!(%ip, attempts, backoff_secs = backoff, "geo retry armed");
        self.geo_retry = Some(GeoRetry {
            ip,
            attempts,
            next_at_unix: next_at,
        });
    }

    /// H2: make sure a readable snapshot exists (creating it if absent) and
    /// that the store can actually persist it. Any failure here means the
    /// system would be modified without a recovery path.
    fn ensure_snapshot_tracked(&self) -> Result<(), String> {
        match self.engine.store().load_snapshot() {
            Ok(Some(_)) => Ok(()),
            Ok(None) => {
                let created = rfc3339(self.clock.now_unix());
                self.engine
                    .save(&*self.reader, created)
                    .map_err(|e| e.to_string())?;
                // Verify the save actually landed and is readable.
                match self.engine.store().load_snapshot() {
                    Ok(Some(_)) => Ok(()),
                    Ok(None) => Err("snapshot still missing after save".to_string()),
                    Err(e) => Err(e.to_string()),
                }
            }
            Err(e) => Err(e.to_string()),
        }
    }

    fn recapture_originals_if_unapplied(&self) -> Result<(), String> {
        let created = rfc3339(self.clock.now_unix());
        self.engine
            .recapture_originals(&*self.reader, created)
            .map_err(|e| e.to_string())
    }

    fn anything_applied(&self) -> bool {
        self.engine
            .store()
            .load_snapshot()
            .ok()
            .flatten()
            .is_some_and(|snap| {
                [
                    snap.fields.timezone.applied.is_some(),
                    snap.fields.locale_name.applied.is_some(),
                    snap.fields.geo_nation.applied.is_some(),
                    snap.fields.kbd_preload.applied.is_some(),
                ]
                .iter()
                .any(|&a| a)
            })
    }

    /// True when the geo answer for an IP corresponds to the region recorded
    /// as the system's original (i.e. this is the native region).
    fn geo_matches_originals(&self, info: &GeoInfo) -> bool {
        let Some(key) = geo::tz_windows::iana_to_windows(&info.timezone) else {
            return false;
        };
        self.engine
            .store()
            .load_snapshot()
            .ok()
            .flatten()
            .is_some_and(|snap| match &snap.fields.timezone.original {
                FieldValue::Timezone { key_name, .. } => *key_name == key,
                _ => false,
            })
    }

    /// C1 (D3): IPs remembered as belonging to the native (home) network.
    fn native_ips(&self) -> Vec<String> {
        self.engine
            .store()
            .load_snapshot()
            .ok()
            .flatten()
            .map(|snap| snap.native_ips)
            .unwrap_or_default()
    }

    fn ip_is_native(&self, ip: IpAddr) -> bool {
        let s = ip.to_string();
        self.native_ips().iter().any(|n| n == &s)
    }

    /// Best-effort: on a store failure the geo-vs-originals fallback still
    /// recognises the native region.
    fn record_native_ip(&self, ip: IpAddr) {
        if let Err(e) = self.engine.record_native_ip(ip.to_string()) {
            tracing::error!(%ip, error = %e, "native ip record failed");
        }
    }

    /// H4: the FIRST pre-apply tzautoupdate state is the one restore needs.
    /// A repeated apply must not overwrite it with the already-disabled
    /// state, or the service would stay off forever. Returns the whole
    /// applied record (H10): a later rollback must rewrite it verbatim.
    fn existing_applied_tz_record(&self) -> Option<FieldValue> {
        self.engine
            .store()
            .load_snapshot()
            .ok()
            .flatten()
            .and_then(|snap| snap.fields.timezone.applied)
    }

    /// H10: the applied records for the three locale fields before an apply.
    fn existing_locale_records(&self) -> [Option<FieldValue>; 3] {
        match self.engine.store().load_snapshot() {
            Ok(Some(snap)) => [
                snap.fields.locale_name.applied,
                snap.fields.geo_nation.applied,
                snap.fields.kbd_preload.applied,
            ],
            _ => [None, None, None],
        }
    }

    /// H10: bring the locale applied records back to `prev` — the state the
    /// applier just rolled the system back to. Records that were absent are
    /// cleared, present ones rewritten verbatim. Best-effort.
    fn rewrite_locale_records(&self, prev: &[Option<FieldValue>; 3]) {
        let ids = [FieldId::LocaleName, FieldId::GeoNation, FieldId::KbdPreload];
        for (id, record) in ids.iter().zip(prev) {
            let res = match record {
                Some(v) => self.engine.record_applied(*id, v.clone()),
                None => self.engine.clear_applied(*id),
            };
            if let Err(e) = res {
                tracing::error!(field = ?id, error = %e, "locale record rewrite failed");
            }
        }
    }

    /// H10 (D2): after a successful timezone rollback the live system holds
    /// the pre-apply state; the applied record must say the same. With a
    /// previous applied record it is rewritten verbatim (so a later restore
    /// reverts to it instead of skipping the field as UserChanged and
    /// deleting the snapshot); without one (first apply) the record is
    /// cleared. Best-effort: the outer Error status already reports the
    /// failure.
    fn reapply_applied_tz_record(&self, out: &TzApplyOutcome) {
        let res = match &out.prev_record {
            Some(record) => self
                .engine
                .record_applied(FieldId::Timezone, record.clone()),
            None => self.engine.clear_applied(FieldId::Timezone),
        };
        if let Err(e) = res {
            tracing::error!(error = %e, "applied timezone record rewrite failed");
        }
    }

    /// Returns whether the timezone was actually mutated, plus the pre-apply
    /// values needed for a later rollback. Err means the applier failed and
    /// the record could not be trusted (nothing locale-side applied yet).
    async fn apply_timezone(&mut self, info: &GeoInfo) -> Result<TzApplyOutcome, String> {
        let key = match geo::tz_windows::iana_to_windows(&info.timezone) {
            Some(key) => key.to_string(),
            None => {
                tracing::warn!(
                    iana = %info.timezone,
                    "unknown Windows timezone; timezone not applied"
                );
                return Ok(TzApplyOutcome {
                    mutated: false,
                    pre: None,
                    pre_service: None,
                    prev_record: None,
                });
            }
        };
        let pre = Applier::current(&*lock_applier(&self.tz).await).await.ok();
        // Pre-apply service state goes into the snapshot's applied record so
        // restore can return tzautoupdate to it (stage 6 decision).
        let pre_service = lock_applier(&self.tz)
            .await
            .capture_service_state()
            .unwrap_or_else(|e| {
                tracing::warn!(error = %e, "capture_service_state failed; service restore will not touch it");
                None
            });
        // H1: DynamicDaylightTimeDisabled must stay FALSE. Setting it to true
        // disables DST transitions, so in DST-observing zones the system
        // offset diverges from the VPN country's real offset.
        let value = TimezoneValue::new(key, false);
        // H10: the applied record before this apply decides what a later
        // rollback restores to.
        let prev_record = self.existing_applied_tz_record();
        // The lock is held only across the apply call itself; the arms below
        // take other self borrows (snapshot engine), so the guard must be
        // dropped first.
        let applied = {
            let mut guard = lock_applier(&self.tz).await;
            Applier::apply(&mut *guard, &value).await
        };
        match applied {
            Ok(change) => {
                if change.is_noop() {
                    tracing::debug!("timezone already up to date");
                    return Ok(TzApplyOutcome {
                        mutated: false,
                        pre,
                        pre_service,
                        prev_record,
                    });
                }
                // H4 (D4): the applied record's pre-apply service state is
                // kept VERBATIM. A record without service_state (capture
                // failed at the first apply, or an old-version snapshot)
                // must stay None: the live state read here is already the
                // disabled one — fabricating it would make every future
                // restore reproduce the disabled service.
                let service_state = match &prev_record {
                    Some(FieldValue::Timezone { service_state, .. }) => *service_state,
                    _ => pre_service.map(tz_state_to_snapshot),
                };
                let record = FieldValue::Timezone {
                    key_name: value.key_name.clone(),
                    dst_disabled: false,
                    service_state,
                };
                // Record only after a successful apply; if the record itself
                // fails (H2) the system changed without tracking — roll back.
                if let Err(e) = self.engine.record_applied(FieldId::Timezone, record) {
                    tracing::error!(error = %e, "record_applied(timezone) failed; rolling back");
                    let out = TzApplyOutcome {
                        mutated: true,
                        pre,
                        pre_service,
                        prev_record,
                    };
                    self.rollback_timezone(&out).await;
                    return Err("snapshot record failed; timezone rolled back".to_string());
                }
                tracing::info!(tz = %value.key_name, "timezone applied");
                Ok(TzApplyOutcome {
                    mutated: true,
                    pre,
                    pre_service,
                    prev_record,
                })
            }
            Err(ApplyError::UnknownZone(zone)) => {
                tracing::warn!(zone = %zone, "zone data missing; timezone skipped");
                Ok(TzApplyOutcome {
                    mutated: false,
                    pre,
                    pre_service,
                    prev_record,
                })
            }
            Err(e) => {
                tracing::error!(tz = %value.key_name, error = %e, "timezone apply failed");
                Err(e.to_string())
            }
        }
    }

    /// Write the timezone back to its pre-apply value (H2/H10 rollback).
    /// Returns true only when the revert actually succeeded.
    async fn rollback_timezone(&mut self, out: &TzApplyOutcome) -> bool {
        let Some(pre) = &out.pre else {
            tracing::warn!("no pre-apply timezone captured; rollback skipped");
            return false;
        };
        let mut revert_value = pre.clone();
        revert_value.service_state = out.pre_service;
        if let Err(e) = Applier::revert(&mut *lock_applier(&self.tz).await, &revert_value).await {
            tracing::error!(error = %e, "timezone rollback failed");
            return false;
        }
        tracing::info!("timezone rolled back to pre-apply value");
        true
    }

    async fn apply_locale(&mut self, info: &GeoInfo) -> Result<(), String> {
        // H10: pre-apply records for every locale field. If the record write
        // below fails part-way, the rollback must leave records describing
        // the rolled-back system — restore_plan would otherwise compare live
        // state against stale records and skip the fields as UserChanged.
        let prev_records = self.existing_locale_records();
        let current = match Applier::current(&*lock_applier(&self.locale).await).await {
            Ok(current) => current,
            Err(e) => {
                tracing::warn!(error = %e, "locale current read failed; locale not applied");
                return Err(format!("locale current read failed: {e}"));
            }
        };
        let locale_name = match info.languages.first() {
            Some(name) => name.clone(),
            None => {
                tracing::warn!("geo returned no languages; locale not applied");
                return Err("geo returned no languages".to_string());
            }
        };
        // Unknown country: keep the current geo nation rather than guessing.
        let geo_nation = match crate::country::geoid_for(&info.country_code) {
            Some(numeric) => numeric.to_string(),
            None => {
                tracing::warn!(
                    country = %info.country_code,
                    "no ISO numeric code; geo nation kept as-is"
                );
                current.geo_nation.clone()
            }
        };
        // Spec (stage 6): keyboard preload is not changed (stage 8 TODO).
        let value = LocaleValue {
            locale_name,
            geo_nation,
            kbd_preload: current.kbd_preload.clone(),
        };
        // The lock is held only across the apply call itself; the arms below
        // re-lock the same mutex (rollback), so the guard must be dropped
        // first.
        let applied = {
            let mut guard = lock_applier(&self.locale).await;
            Applier::apply(&mut *guard, &value).await
        };
        match applied {
            Ok(apply::error::AppliedChange::LocaleChanged { changed_fields, .. }) => {
                if let Err(e) = self.record_locale_changes(&value, &changed_fields) {
                    // H2: the system changed but the record failed — put the
                    // locale fields back to their pre-apply values.
                    tracing::error!(error = %e, "record_applied(locale) failed; rolling back");
                    if let Err(re) =
                        Applier::revert(&mut *lock_applier(&self.locale).await, &current).await
                    {
                        // H10: the system keeps the NEW values — describe
                        // them, so a later restore still reverts every field
                        // to the originals instead of skipping as
                        // UserChanged.
                        tracing::error!(error = %re, "locale rollback failed");
                        if let Err(we) = self.record_locale_changes(&value, &changed_fields) {
                            tracing::error!(error = %we, "locale record rewrite failed");
                        }
                    } else {
                        // System back to the pre-apply state: restore the
                        // pre-apply records.
                        self.rewrite_locale_records(&prev_records);
                    }
                    return Err(e);
                }
                tracing::info!(fields = ?changed_fields, "locale applied");
                Ok(())
            }
            Ok(apply::error::AppliedChange::Noop) => {
                tracing::debug!("locale already up to date");
                Ok(())
            }
            Ok(other @ apply::error::AppliedChange::Changed { .. }) => {
                tracing::warn!(?other, "unexpected tz-style change from locale applier");
                Ok(())
            }
            Err(e) => {
                tracing::error!(error = %e, "locale apply failed");
                Err(e.to_string())
            }
        }
    }

    fn record_locale_changes(
        &self,
        value: &LocaleValue,
        changed_fields: &[&'static str],
    ) -> Result<(), String> {
        let mut first_err = None;
        for field in changed_fields {
            let (id, record) = match *field {
                apply::locale::FIELD_LOCALE_NAME => (
                    FieldId::LocaleName,
                    FieldValue::LocaleName(value.locale_name.clone()),
                ),
                apply::locale::FIELD_GEO_NATION => (
                    FieldId::GeoNation,
                    FieldValue::GeoNation(value.geo_nation.clone()),
                ),
                apply::locale::FIELD_KBD_PRELOAD => (
                    FieldId::KbdPreload,
                    FieldValue::KbdPreload(value.kbd_preload.clone()),
                ),
                other => {
                    tracing::warn!(field = other, "unknown locale field; not recorded");
                    continue;
                }
            };
            if let Err(e) = self.engine.record_applied(id, record) {
                tracing::error!(field, error = %e, "record_applied(locale) failed");
                if first_err.is_none() {
                    first_err = Some(e.to_string());
                }
            }
        }
        first_err.map_or(Ok(()), Err)
    }

    /// Restore everything the snapshot knows about. The snapshot is cleared
    /// only when every planned revert succeeded; otherwise it stays and the
    /// next start retries (stage 6 decision).
    ///
    /// The blocking system work (registry, SCM, file I/O, WM broadcasts)
    /// runs on a dedicated worker thread (H6/D1): inline it would starve the
    /// single-threaded runtime and make any `tokio::time::timeout` around
    /// the restore dead code.
    pub async fn restore_to_native(&mut self) {
        match self.spawn_restore_worker().await {
            Ok(true) => {
                self.current_country = None;
                self.set_status(Status::Native);
                tracing::info!("restore to native complete");
            }
            Ok(false) => {
                // The worker already logged the per-field failures and kept
                // the snapshot for the next-start retry.
                self.set_status(Status::Error("restore incomplete".into()));
            }
            Err(e) => {
                tracing::error!(error = %e, "restore_plan failed");
                self.set_status(Status::Error("restore plan failed".into()));
            }
        }
    }

    /// Restore capped by the 5 s watchdog (H6). Used on the shutdown and
    /// tray-exit paths, where the OS may kill the process: on timeout the
    /// loop abandons the (still running, best-effort) worker and raises the
    /// timeout flag; the snapshot, if `complete_restore` did not run, is
    /// retried on the next start.
    async fn restore_with_watchdog(&mut self) {
        match tokio::time::timeout(
            crate::shutdown::RESTORE_WATCHDOG,
            self.spawn_restore_worker(),
        )
        .await
        {
            Ok(Ok(true)) => {
                self.current_country = None;
                self.set_status(Status::Native);
                tracing::info!("restore to native complete");
            }
            Ok(Ok(false)) => {
                self.set_status(Status::Error("restore incomplete".into()));
            }
            Ok(Err(e)) => {
                tracing::error!(error = %e, "restore_plan failed");
                self.set_status(Status::Error("restore plan failed".into()));
            }
            Err(_) => {
                tracing::error!(
                    "restore exceeded the 5 s watchdog; abandoning it (retry on next start)"
                );
                crate::shutdown::set_restore_timed_out();
            }
        }
    }

    /// Run the restore body on a worker thread and await its result.
    async fn spawn_restore_worker(&mut self) -> Result<bool, String> {
        let (tx, rx) = oneshot::channel();
        let engine = self.engine.clone();
        let reader = Arc::clone(&self.reader);
        let tz = Arc::clone(&self.tz);
        let locale = Arc::clone(&self.locale);
        let status = Arc::clone(&self.status);
        std::thread::spawn(move || {
            let result = apply::block_on(restore_worker_body(engine, reader, tz, locale, status));
            // The receiver may be gone after a watchdog timeout — fine.
            let _ = tx.send(result);
        });
        let result = rx
            .await
            .map_err(|e| format!("restore worker dropped: {e}"))?;
        result
    }
}

/// The restore body, run on a worker thread (H6). Returns Ok(true) when
/// every planned revert succeeded and the snapshot was cleared; Ok(false)
/// when some reverts failed (snapshot kept for the next-start retry); Err
/// when the plan itself could not be computed.
async fn restore_worker_body<R, TR, SR, VR, AR, LR, BR>(
    engine: Engine,
    reader: Arc<R>,
    tz: Arc<AsyncMutex<TimezoneApplier<TR, SR, VR, AR>>>,
    locale: Arc<AsyncMutex<LocaleApplier<LR, BR>>>,
    status: Arc<Mutex<Status>>,
) -> Result<bool, String>
where
    R: StateReader,
    TR: TzRegistry,
    SR: ServiceController,
    VR: WinTzSetter,
    AR: AdminCheck,
    LR: LocaleRegistry,
    BR: SettingChangeBroadcast,
{
    let plan = match engine.restore_plan(&*reader) {
        Ok(plan) => plan,
        Err(e) => return Err(e.to_string()),
    };
    for (field, reason) in &plan.warnings {
        tracing::warn!(field = %field, reason = %reason, "restore skipped");
    }
    // The applied record is the only place the pre-apply tzautoupdate
    // service state lives (the original itself is stored with
    // service_state: None).
    let applied_tz_service_state = engine
        .store()
        .load_snapshot()
        .ok()
        .flatten()
        .and_then(|snap| snap.fields.timezone.applied)
        .and_then(|applied| match applied {
            FieldValue::Timezone { service_state, .. } => service_state,
            _ => None,
        });
    let mut all_ok = true;
    for item in &plan.items {
        let res = revert_item(&tz, &locale, item, applied_tz_service_state).await;
        match res {
            Ok(()) => tracing::info!(field = %item.field, "field restored"),
            Err(e) => {
                all_ok = false;
                tracing::error!(field = %item.field, error = %e, "restore revert failed");
            }
        }
    }
    if !all_ok {
        tracing::error!("restore incomplete; snapshot kept, retry on next start");
        set_shared_status(&status, Status::Error("restore incomplete".into()));
        return Ok(false);
    }
    if let Err(e) = engine.complete_restore() {
        tracing::error!(error = %e, "complete_restore failed");
        set_shared_status(&status, Status::Error("snapshot cleanup failed".into()));
        return Ok(false);
    }
    Ok(true)
}

async fn revert_item<TR, SR, VR, AR, LR, BR>(
    tz: &Arc<AsyncMutex<TimezoneApplier<TR, SR, VR, AR>>>,
    locale: &Arc<AsyncMutex<LocaleApplier<LR, BR>>>,
    item: &RestoreItem,
    applied_tz_service_state: Option<TzServiceState>,
) -> Result<(), ApplyError>
where
    TR: TzRegistry,
    SR: ServiceController,
    VR: WinTzSetter,
    AR: AdminCheck,
    LR: LocaleRegistry,
    BR: SettingChangeBroadcast,
{
    match item.field {
        FieldId::Timezone => {
            let FieldValue::Timezone {
                key_name,
                dst_disabled,
                service_state: _,
            } = &item.original
            else {
                return Err(ApplyError::RegistryRead(
                    "timezone original has wrong type".into(),
                    std::io::Error::new(std::io::ErrorKind::InvalidData, "type mismatch"),
                ));
            };
            // service_state comes from the APPLIED record (the original
            // is stored with None): it decides whether the tzautoupdate
            // service is touched on revert.
            let value = TimezoneValue {
                key_name: key_name.clone(),
                dst_disabled: *dst_disabled,
                service_state: applied_tz_service_state.map(tz_state_to_apply),
            };
            Applier::revert(&mut *lock_applier(tz).await, &value).await
        }
        FieldId::LocaleName | FieldId::GeoNation | FieldId::KbdPreload => {
            // Revert compares per field; set the target field from the
            // original and keep the others at their current values.
            let mut target = Applier::current(&*lock_applier(locale).await).await?;
            match &item.original {
                FieldValue::LocaleName(name) => target.locale_name = name.clone(),
                FieldValue::GeoNation(nation) => target.geo_nation = nation.clone(),
                FieldValue::KbdPreload(preload) => target.kbd_preload = preload.clone(),
                _ => {
                    return Err(ApplyError::RegistryRead(
                        format!("{} original has wrong type", item.field),
                        std::io::Error::new(std::io::ErrorKind::InvalidData, "type mismatch"),
                    ))
                }
            }
            Applier::revert(&mut *lock_applier(locale).await, &target).await
        }
    }
}

fn tz_state_to_snapshot(state: TzAutoUpdateState) -> TzServiceState {
    match state {
        TzAutoUpdateState::Enabled => TzServiceState::Enabled,
        TzAutoUpdateState::Disabled => TzServiceState::Disabled,
    }
}

/// Lock an applier mutex, reviving a poisoned lock. Guards are held only
/// over synchronous applier work (those futures never return Pending), so a
/// panic must not wedge the loop; the same helper serves the restore worker
/// thread.
async fn lock_applier<T>(mutex: &AsyncMutex<T>) -> AsyncMutexGuard<'_, T> {
    // tokio::sync::Mutex::lock never fails (no poisoning); the guard is Send,
    // so holding it across the synchronous applier futures keeps `run()`
    // spawnable.
    mutex.lock().await
}

/// Shared status write, callable from the restore worker thread too.
fn set_shared_status(status: &Mutex<Status>, new: Status) {
    if let Ok(mut guard) = status.lock() {
        *guard = new;
    }
}

/// D5: resolves as soon as a shutdown is requested from any source (the
/// global flag) or the test probe fired. Polled in 50 ms steps — prompt
/// against the 5 s watchdog — and it does not consume the shutdown channel
/// message that the run loop's own select! must still see.
async fn wait_shutdown_requested(probe: Option<&Arc<std::sync::atomic::AtomicBool>>) {
    loop {
        if crate::shutdown::shutdown_requested() {
            return;
        }
        if let Some(p) = probe {
            if p.load(std::sync::atomic::Ordering::SeqCst) {
                return;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn tz_state_to_apply(state: TzServiceState) -> TzAutoUpdateState {
    match state {
        TzServiceState::Enabled => TzAutoUpdateState::Enabled,
        TzServiceState::Disabled => TzAutoUpdateState::Disabled,
    }
}

fn rfc3339(unix: u64) -> String {
    // RFC 3339 without external crates: seconds since epoch, always UTC.
    // (Snapshot consumers only treat this as an opaque timestamp.)
    let secs = unix as i64;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (h, m, s) = (tod / 3600, (tod % 3600) / 60, tod % 60);
    let (y, mo, d) = civil_from_days(days);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Howard Hinnant's civil_from_days algorithm.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}
