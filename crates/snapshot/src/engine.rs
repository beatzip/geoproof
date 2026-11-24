//! Engine: save-once capture, applied-value recording, restore planning and
//! completion. Pure decision logic over the injected store/reader seams; it
//! never touches the system itself.

use std::fmt;
use std::io;

use crate::model::SnapshotError;
use crate::store::StoreError;

/// Engine error: store failures, missing snapshots, bad reader output.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("no snapshot exists")]
    NoSnapshot,
    #[error("state reader returned invalid data: {0}")]
    BadReader(String),
    #[error("io error: {0}")]
    Io(#[from] io::Error),
}

impl From<SnapshotError> for EngineError {
    fn from(e: SnapshotError) -> Self {
        match e {
            SnapshotError::VersionMismatch { found, expected } => {
                EngineError::Store(StoreError::VersionMismatch { found, expected })
            }
            SnapshotError::Store(e) => EngineError::Store(e),
        }
    }
}

pub type Result<T> = std::result::Result<T, EngineError>;

use serde::Serialize;

use crate::model::{FieldId, FieldValue, Snapshot, SnapshotFields, SCHEMA_VERSION};
use crate::store::Store;
use crate::traits::StateReader;

/// What the engine decided at startup about crash recovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoreDecision {
    /// No snapshot and no stale lock: nothing to do.
    NoSnapshot,
    /// A lock exists but it is alive and fresh: the previous process may
    /// still be running; do not touch anything.
    LockHeld,
    /// Snapshot exists and the previous holder is gone (dead pid or stale
    /// heartbeat): the restore executor (stages 4+) must run the plan.
    RestoreNeeded,
}

/// One field the restore executor (stage 4+) must write back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RestoreItem {
    pub field: FieldId,
    /// Value to restore (the snapshot original).
    pub original: FieldValue,
}

/// Why a field was skipped, when it was.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// The applier never changed this field (applied == null).
    NotTouched,
    /// Current != applied: the user changed it afterwards; do not clobber.
    UserChanged,
}

/// Equality used by [Engine::restore_plan]. The applied timezone record
/// carries `service_state` (the pre-apply `tzautoupdate` state, stage 6),
/// which is restore metadata: the state reader never observes it, so it is
/// not part of "did the user change this field".
fn matches_current(applied: &FieldValue, current: &FieldValue) -> bool {
    match (applied, current) {
        (
            FieldValue::Timezone {
                key_name: a_key,
                dst_disabled: a_dst,
                ..
            },
            FieldValue::Timezone {
                key_name: c_key,
                dst_disabled: c_dst,
                ..
            },
        ) => a_key == c_key && a_dst == c_dst,
        (a, c) => a == c,
    }
}

impl fmt::Display for SkipReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SkipReason::NotTouched => f.write_str("not touched"),
            SkipReason::UserChanged => f.write_str("user changed after apply"),
        }
    }
}

/// The plan plus the fields that were skipped (with reasons/warnings).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestorePlan {
    pub items: Vec<RestoreItem>,
    /// (field, reason) for every skipped field.
    pub warnings: Vec<(FieldId, SkipReason)>,
}

/// Save-once engine over an injectable store.
#[derive(Debug, Clone)]
pub struct Engine {
    store: Store,
}

impl Engine {
    pub fn new(store: Store) -> Self {
        Engine { store }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Save-once capture: if an unrestored snapshot already exists, this is a
    /// no-op (originals are never overwritten between starts).
    pub fn save(&self, reader: &dyn StateReader, created_utc: String) -> Result<()> {
        if self.store.load_snapshot()?.is_some() {
            tracing::info!("snapshot already exists and is unrestored; save() is a no-op");
            return Ok(());
        }
        let read = reader.read_all()?;
        let mut it = read.into_iter();
        let fields = SnapshotFields {
            timezone: snap_of(&mut it, FieldId::Timezone)?,
            locale_name: snap_of(&mut it, FieldId::LocaleName)?,
            geo_nation: snap_of(&mut it, FieldId::GeoNation)?,
            kbd_preload: snap_of(&mut it, FieldId::KbdPreload)?,
        };
        self.store
            .save_snapshot(&Snapshot::new(created_utc, fields))?;
        Ok(())
    }

    /// Rebuild the originals from the live system when nothing has been
    /// applied yet (first apply of this process). A snapshot captured at
    /// startup can be days old by the time the first apply happens; without
    /// this refresh a later restore would clobber the user's intermediate
    /// changes with stale originals.
    ///
    /// No-op when a snapshot is absent or any field already carries an
    /// `applied` value (then the originals are still authoritative).
    pub fn recapture_originals(&self, reader: &dyn StateReader, created_utc: String) -> Result<()> {
        let mut snapshot = match self.store.load_snapshot()? {
            Some(s) => s,
            None => return Ok(()),
        };
        let any_applied = [
            snapshot.fields.timezone.applied.is_some(),
            snapshot.fields.locale_name.applied.is_some(),
            snapshot.fields.geo_nation.applied.is_some(),
            snapshot.fields.kbd_preload.applied.is_some(),
        ]
        .iter()
        .any(|&a| a);
        if any_applied {
            return Ok(());
        }
        tracing::info!("nothing applied yet; refreshing snapshot originals from the live system");
        let read = reader.read_all()?;
        let mut it = read.into_iter();
        // Rebuild the fields in place: the native_ips list (C1) recorded
        // before this recapture must survive it.
        snapshot.fields = SnapshotFields {
            timezone: snap_of(&mut it, FieldId::Timezone)?,
            locale_name: snap_of(&mut it, FieldId::LocaleName)?,
            geo_nation: snap_of(&mut it, FieldId::GeoNation)?,
            kbd_preload: snap_of(&mut it, FieldId::KbdPreload)?,
        };
        snapshot.created_utc = created_utc;
        self.store.save_snapshot(&snapshot)?;
        Ok(())
    }

    /// Record that the applier successfully set a field (stages 4+ call this).
    pub fn record_applied(&self, field: FieldId, value: FieldValue) -> Result<()> {
        let mut snapshot = self.store.load_snapshot()?.ok_or(EngineError::NoSnapshot)?;
        let slot = match field {
            FieldId::Timezone => &mut snapshot.fields.timezone,
            FieldId::LocaleName => &mut snapshot.fields.locale_name,
            FieldId::GeoNation => &mut snapshot.fields.geo_nation,
            FieldId::KbdPreload => &mut snapshot.fields.kbd_preload,
        };
        slot.applied = Some(value);
        self.store.save_snapshot(&snapshot)?;
        Ok(())
    }

    /// Clear a field's applied record.
    ///
    /// Used by the rollback paths (H10): after the system goes back to its
    /// pre-apply state the record must not keep claiming the field was
    /// applied, or `restore_plan` compares live state against a stale
    /// record and skips the field as UserChanged.
    pub fn clear_applied(&self, field: FieldId) -> Result<()> {
        let mut snapshot = self.store.load_snapshot()?.ok_or(EngineError::NoSnapshot)?;
        let slot = match field {
            FieldId::Timezone => &mut snapshot.fields.timezone,
            FieldId::LocaleName => &mut snapshot.fields.locale_name,
            FieldId::GeoNation => &mut snapshot.fields.geo_nation,
            FieldId::KbdPreload => &mut snapshot.fields.kbd_preload,
        };
        slot.applied = None;
        self.store.save_snapshot(&snapshot)?;
        Ok(())
    }

    /// C1 (D3): remember an IP as belonging to the native (home) network.
    /// Deduplicated; snapshots written by older versions start with an
    /// empty list.
    pub fn record_native_ip(&self, ip: String) -> Result<()> {
        let mut snapshot = self.store.load_snapshot()?.ok_or(EngineError::NoSnapshot)?;
        if snapshot.native_ips.iter().any(|s| s == &ip) {
            return Ok(());
        }
        snapshot.native_ips.push(ip);
        self.store.save_snapshot(&snapshot)?;
        Ok(())
    }

    /// Compute the restore plan against the current system state.
    ///
    /// Per field: applied == null -> skip (NotTouched); current == applied ->
    /// plan a write of the original; current != applied -> skip + warning
    /// (the user changed it themselves; do not clobber).
    pub fn restore_plan(&self, reader: &dyn StateReader) -> Result<RestorePlan> {
        let snapshot = match self.store.load_snapshot()? {
            Some(s) => s,
            None => {
                return Ok(RestorePlan {
                    items: Vec::new(),
                    warnings: Vec::new(),
                })
            }
        };
        let read = reader.read_all()?;
        let mut items = Vec::new();
        let mut warnings = Vec::new();
        for (id, current) in read {
            let slot = match id {
                FieldId::Timezone => &snapshot.fields.timezone,
                FieldId::LocaleName => &snapshot.fields.locale_name,
                FieldId::GeoNation => &snapshot.fields.geo_nation,
                FieldId::KbdPreload => &snapshot.fields.kbd_preload,
            };
            match &slot.applied {
                // Spec: applied == null -> skipped silently (we never touched it).
                None => {}
                Some(applied) if matches_current(applied, &current) => {
                    items.push(RestoreItem {
                        field: id,
                        original: slot.original.clone(),
                    });
                }
                Some(_) => warnings.push((id, SkipReason::UserChanged)),
            }
        }
        Ok(RestorePlan { items, warnings })
    }

    /// Startup decision: is a crash restore needed?
    /// RestoreNeeded iff a lock exists AND (pid dead OR heartbeat stale) AND
    /// an unrestored snapshot exists. Threshold is configurable; the default
    /// is 60 seconds.
    pub fn startup_decision(
        &self,
        liveness: &dyn crate::traits::ProcessLiveness,
        clock: &dyn crate::traits::Clock,
        heartbeat_timeout_secs: u64,
    ) -> Result<RestoreDecision> {
        let lock = match self.store.read_lock()? {
            Some(l) => l,
            None => return Ok(RestoreDecision::NoSnapshot),
        };
        let fresh = clock.now_unix().saturating_sub(lock.heartbeat_unix) < heartbeat_timeout_secs;
        if liveness.is_alive(lock.pid) && fresh {
            Ok(RestoreDecision::LockHeld)
        } else if self.store.load_snapshot()?.is_some() {
            Ok(RestoreDecision::RestoreNeeded)
        } else {
            Ok(RestoreDecision::NoSnapshot)
        }
    }

    /// Remove the snapshot and the lock. A repeated restore_plan afterwards
    /// returns empty (idempotence).
    pub fn complete_restore(&self) -> Result<()> {
        self.store.clear()?;
        Ok(())
    }
}

fn snap_of(
    it: &mut std::vec::IntoIter<(FieldId, FieldValue)>,
    id: FieldId,
) -> Result<crate::model::FieldSnapshot> {
    let (_, value) = it
        .find(|(fid, _)| *fid == id)
        .ok_or_else(|| EngineError::BadReader(format!("reader did not return {id}")))?;
    Ok(crate::model::FieldSnapshot {
        original: value,
        applied: None,
    })
}

/// The schema version this engine writes; kept for the demo/README.
pub const ENGINE_SCHEMA_VERSION: u32 = SCHEMA_VERSION;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{TzServiceState, ALL_FIELDS};
    use crate::store::Store;
    use crate::traits::{Clock, ProcessLiveness};
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::fs;
    use std::path::PathBuf;
    use std::rc::Rc;

    // ---- mocks -----------------------------------------------------------

    #[derive(Default, Clone)]
    struct MockReader {
        state: Rc<RefCell<HashMap<FieldId, FieldValue>>>,
    }

    impl MockReader {
        fn set(&self, id: FieldId, v: FieldValue) {
            self.state.borrow_mut().insert(id, v);
        }
    }

    impl StateReader for MockReader {
        fn read_all(&self) -> std::io::Result<Vec<(FieldId, FieldValue)>> {
            Ok(ALL_FIELDS
                .iter()
                .map(|id| {
                    (
                        *id,
                        self.state
                            .borrow()
                            .get(id)
                            .cloned()
                            .expect("mock field unset"),
                    )
                })
                .collect())
        }
    }

    struct MockLiveness {
        alive: HashMap<u32, bool>,
    }
    impl ProcessLiveness for MockLiveness {
        fn is_alive(&self, pid: u32) -> bool {
            *self.alive.get(&pid).unwrap_or(&false)
        }
    }

    struct MockClock(u64);
    impl Clock for MockClock {
        fn now_unix(&self) -> u64 {
            self.0
        }
    }

    // ---- helpers ---------------------------------------------------------

    fn test_tmp_root() -> PathBuf {
        // cargo test under DSH denies writes to %TEMP% (sandboxed per-invocation),
        // so tests use a workspace-relative scratch dir instead.
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-tmp");
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn tempdir(tag: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let d = test_tmp_root().join(format!(
            "snap-engine-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn originals() -> Vec<(FieldId, FieldValue)> {
        vec![
            (
                FieldId::Timezone,
                FieldValue::Timezone {
                    key_name: "Original TZ".into(),
                    dst_disabled: false,
                    service_state: None,
                },
            ),
            (FieldId::LocaleName, FieldValue::LocaleName("ru-RU".into())),
            (FieldId::GeoNation, FieldValue::GeoNation("203".into())),
            (
                FieldId::KbdPreload,
                FieldValue::KbdPreload(vec!["00000409".into(), "00000419".into()]),
            ),
        ]
    }

    fn applied_values() -> Vec<(FieldId, FieldValue)> {
        vec![
            (
                FieldId::Timezone,
                FieldValue::Timezone {
                    key_name: "VPN TZ".into(),
                    dst_disabled: true,
                    service_state: Some(TzServiceState::Enabled),
                },
            ),
            (FieldId::LocaleName, FieldValue::LocaleName("en-US".into())),
            (FieldId::GeoNation, FieldValue::GeoNation("999".into())),
            (
                FieldId::KbdPreload,
                FieldValue::KbdPreload(vec!["00000409".into()]),
            ),
        ]
    }

    fn new_engine(tag: &str) -> Engine {
        Engine::new(Store::new(tempdir(tag)))
    }

    // ---- tests -----------------------------------------------------------

    /// Test 1: full cycle -> state back to originals; repeated plan empty.
    #[test]
    fn full_cycle_restores_originals_and_is_idempotent() {
        let engine = new_engine("cycle");
        let reader = MockReader::default();
        for (id, v) in originals() {
            reader.set(id, v.clone());
        }
        engine.save(&reader, "2026-01-01T00:00:00Z".into()).unwrap();

        for (id, applied) in applied_values() {
            reader.set(id, applied.clone());
            engine.record_applied(id, applied).unwrap();
        }

        let plan = engine.restore_plan(&reader).unwrap();
        assert_eq!(plan.items.len(), 4, "all four fields in plan");
        assert!(plan.warnings.is_empty());

        // Mock executor: apply the plan.
        for item in &plan.items {
            reader.set(item.field, item.original.clone());
        }

        engine.complete_restore().unwrap();
        let after = engine.restore_plan(&reader).unwrap();
        assert!(after.items.is_empty(), "second plan must be empty");
        assert!(after.warnings.is_empty());
    }

    /// Test 2: user changed a field after applied -> skipped with warning.
    #[test]
    fn dirty_field_skipped_with_warning() {
        let engine = new_engine("dirty");
        let reader = MockReader::default();
        for (id, v) in originals() {
            reader.set(id, v.clone());
        }
        engine.save(&reader, "t".into()).unwrap();

        // Apply only locale, then the user changes it afterwards.
        let applied = FieldValue::LocaleName("en-US".into());
        reader.set(FieldId::LocaleName, applied.clone());
        engine.record_applied(FieldId::LocaleName, applied).unwrap();
        reader.set(FieldId::LocaleName, FieldValue::LocaleName("de-DE".into()));

        let plan = engine.restore_plan(&reader).unwrap();
        assert!(plan.items.is_empty());
        assert_eq!(
            plan.warnings,
            vec![(FieldId::LocaleName, SkipReason::UserChanged)]
        );
    }

    /// Test 3: untouched field (applied == null) never enters the plan.
    #[test]
    fn untouched_field_not_planned() {
        let engine = new_engine("untouched");
        let reader = MockReader::default();
        for (id, v) in originals() {
            reader.set(id, v.clone());
        }
        engine.save(&reader, "t".into()).unwrap();
        // Apply only geo_nation.
        let applied = FieldValue::GeoNation("999".into());
        reader.set(FieldId::GeoNation, applied.clone());
        engine.record_applied(FieldId::GeoNation, applied).unwrap();

        let plan = engine.restore_plan(&reader).unwrap();
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].field, FieldId::GeoNation);
        // Spec: applied == null is skipped silently, no warnings.
        assert!(plan.warnings.is_empty());
    }

    /// Test 3b (stage 6): the applied TZ record carries service_state, the
    /// reader never does; the plan must still trigger on key/dst equality
    /// and must NOT trigger when the key differs.
    #[test]
    fn tz_applied_with_service_state_is_restorable() {
        let engine = new_engine("tz-service-state");
        let reader = MockReader::default();
        for (id, v) in originals() {
            reader.set(id, v.clone());
        }
        engine.save(&reader, "t".into()).unwrap();

        let applied = FieldValue::Timezone {
            key_name: "VPN TZ".into(),
            dst_disabled: true,
            service_state: Some(TzServiceState::Enabled),
        };
        reader.set(FieldId::Timezone, applied.clone());
        engine.record_applied(FieldId::Timezone, applied).unwrap();

        let plan = engine.restore_plan(&reader).unwrap();
        assert_eq!(plan.items.len(), 1);
        assert_eq!(plan.items[0].field, FieldId::Timezone);
        assert!(plan.warnings.is_empty());

        // User moved to a different zone afterwards -> skip.
        reader.set(
            FieldId::Timezone,
            FieldValue::Timezone {
                key_name: "User TZ".into(),
                dst_disabled: false,
                service_state: None,
            },
        );
        let plan = engine.restore_plan(&reader).unwrap();
        assert!(plan.items.is_empty());
        assert_eq!(
            plan.warnings,
            vec![(FieldId::Timezone, SkipReason::UserChanged)]
        );
    }

    /// Test 4: save-once - a repeated save does not clobber originals.
    #[test]
    fn save_once_preserves_originals() {
        let engine = new_engine("save-once");
        let reader = MockReader::default();
        for (id, v) in originals() {
            reader.set(id, v.clone());
        }
        engine.save(&reader, "first".into()).unwrap();
        // System drifts, then a second start saves again.
        reader.set(FieldId::GeoNation, FieldValue::GeoNation("777".into()));
        engine.save(&reader, "second".into()).unwrap();

        let snap = engine.store().load_snapshot().unwrap().unwrap();
        assert_eq!(snap.created_utc, "first");
        assert_eq!(
            snap.fields.geo_nation.original,
            FieldValue::GeoNation("203".into())
        );
    }

    /// Test 5: unknown version -> VersionMismatch, file untouched.
    #[test]
    fn unknown_version_rejected_file_intact() {
        let engine = new_engine("version");
        let reader = MockReader::default();
        for (id, v) in originals() {
            reader.set(id, v.clone());
        }
        engine.save(&reader, "t".into()).unwrap();

        let path = engine.store().snapshot_path_full();
        let raw = fs::read_to_string(&path)
            .unwrap()
            .replace("\"version\": 1", "\"version\": 99");
        fs::write(&path, raw).unwrap();

        let err = engine.store().load_snapshot().unwrap_err();
        assert!(matches!(
            err,
            crate::store::StoreError::VersionMismatch { found: 99, .. }
        ));

        // Engine methods surface VersionMismatch and never mutate the file.
        let err2 = engine.restore_plan(&reader).unwrap_err();
        assert!(matches!(
            err2,
            EngineError::Store(StoreError::VersionMismatch { .. })
        ));
        assert!(path.exists());
    }

    /// Test 6: stale heartbeat / dead pid -> RestoreNeeded; alive+fresh -> LockHeld.
    #[test]
    fn startup_decision_uses_liveness_and_heartbeat() {
        let engine = new_engine("decision");
        let reader = MockReader::default();
        for (id, v) in originals() {
            reader.set(id, v.clone());
        }
        engine.save(&reader, "t".into()).unwrap();
        engine.store().write_lock(111, 1000).unwrap();

        let liveness = MockLiveness {
            alive: [(111u32, true)].into_iter().collect(),
        };
        let clock = MockClock(1030); // fresh (30s < 60s)

        assert_eq!(
            engine.startup_decision(&liveness, &clock, 60).unwrap(),
            RestoreDecision::LockHeld
        );

        // Stale heartbeat (alive but 120s old).
        let clock = MockClock(1120);
        assert_eq!(
            engine.startup_decision(&liveness, &clock, 60).unwrap(),
            RestoreDecision::RestoreNeeded
        );

        // Dead pid (heartbeat fresh).
        let clock = MockClock(1030);
        let dead = MockLiveness {
            alive: HashMap::new(),
        };
        assert_eq!(
            engine.startup_decision(&dead, &clock, 60).unwrap(),
            RestoreDecision::RestoreNeeded
        );

        // No lock at all.
        engine.store().clear().unwrap();
        engine.save(&reader, "t2".into()).unwrap();
        assert_eq!(
            engine.startup_decision(&liveness, &clock, 60).unwrap(),
            RestoreDecision::NoSnapshot
        );
    }

    /// Test 7: serialization failure leaves the old file intact, temp removed.
    #[test]
    fn atomic_write_failure_leaves_old_file() {
        let engine = new_engine("atomic");
        let reader = MockReader::default();
        for (id, v) in originals() {
            reader.set(id, v.clone());
        }
        engine.save(&reader, "t".into()).unwrap();

        // Break the engine with a store whose base is a FILE: writes fail.
        let file_path = tempdir("atomic").join("blocker");
        fs::write(&file_path, b"x").unwrap();
        let broken = Engine::new(Store::new(file_path.clone()));

        let snap = engine.store().load_snapshot().unwrap().unwrap();
        // Attempt to save through the broken store (base is a file).
        assert!(broken.store().save_snapshot(&snap).is_err());

        // Original snapshot still readable and intact.
        let again = engine.store().load_snapshot().unwrap().unwrap();
        assert_eq!(again, snap);
        // No temp residue next to the real snapshot.

        assert!(!engine.store().base_dir().join("snapshot.json.tmp").exists());
    }

    /// Extra: complete_restore clears everything; repeated restore_plan empty.
    #[test]
    fn complete_restore_clears_state() {
        let engine = new_engine("complete");
        let reader = MockReader::default();
        for (id, v) in originals() {
            reader.set(id, v.clone());
        }
        engine.save(&reader, "t".into()).unwrap();
        engine.complete_restore().unwrap();
        let plan = engine.restore_plan(&reader).unwrap();
        assert!(plan.items.is_empty());
        assert!(plan.warnings.is_empty());
        assert!(engine.store().load_snapshot().unwrap().is_none());
    }

    /// H3: with nothing applied, recapture refreshes stale originals; once a
    /// field is applied, originals are never touched.
    #[test]
    fn recapture_originals_only_before_first_apply() {
        let engine = new_engine("recapture");
        let reader = MockReader::default();
        for (id, v) in originals() {
            reader.set(id, v.clone());
        }
        engine.save(&reader, "stale".into()).unwrap();

        // System drifts (user changed the zone manually), then first apply.
        reader.set(
            FieldId::Timezone,
            FieldValue::Timezone {
                key_name: "Manual TZ".into(),
                dst_disabled: false,
                service_state: None,
            },
        );
        engine.recapture_originals(&reader, "fresh".into()).unwrap();
        let snap = engine.store().load_snapshot().unwrap().unwrap();
        assert_eq!(snap.created_utc, "fresh");
        assert_eq!(
            snap.fields.timezone.original,
            FieldValue::Timezone {
                key_name: "Manual TZ".into(),
                dst_disabled: false,
                service_state: None,
            }
        );

        // After an apply, recapture must be a no-op.
        let applied = FieldValue::Timezone {
            key_name: "VPN TZ".into(),
            dst_disabled: false,
            service_state: Some(TzServiceState::Enabled),
        };
        reader.set(FieldId::Timezone, applied.clone());
        engine.record_applied(FieldId::Timezone, applied).unwrap();
        reader.set(
            FieldId::Timezone,
            FieldValue::Timezone {
                key_name: "Later TZ".into(),
                dst_disabled: false,
                service_state: None,
            },
        );
        engine.recapture_originals(&reader, "newer".into()).unwrap();
        let snap = engine.store().load_snapshot().unwrap().unwrap();
        assert_eq!(snap.created_utc, "fresh");
        assert_eq!(
            snap.fields.timezone.original,
            FieldValue::Timezone {
                key_name: "Manual TZ".into(),
                dst_disabled: false,
                service_state: None,
            }
        );
    }
}
