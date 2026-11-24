//! JSON persistence: atomic snapshot writes and the pid+heartbeat lock file.
//!
//! Storage layout (base dir injectable; production: %LocalAppData%\\vpose):
//!   snapshot.json  - the snapshot document
//!   snapshot.json.tmp - temp file, renamed over snapshot.json on success
//!   snapshot.lock  - pid + last heartbeat unix seconds (JSON)

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::model::{Snapshot, SnapshotError, SCHEMA_VERSION};

pub const SNAPSHOT_FILE: &str = "snapshot.json";
pub const LOCK_FILE: &str = "snapshot.lock";
/// Default heartbeat staleness threshold (seconds).
pub const DEFAULT_HEARTBEAT_TIMEOUT_SECS: u64 = 60;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("snapshot schema version {found} is not supported (expected {expected})")]
    VersionMismatch { found: u32, expected: u32 },
}

impl From<StoreError> for SnapshotError {
    fn from(e: StoreError) -> Self {
        match e {
            StoreError::VersionMismatch { found, expected } => {
                SnapshotError::VersionMismatch { found, expected }
            }
            other => SnapshotError::Store(other),
        }
    }
}

/// Lock file payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LockInfo {
    pub pid: u32,
    /// Last heartbeat, seconds since Unix epoch.
    pub heartbeat_unix: u64,
}

/// File-backed store rooted at an injectable base directory.
#[derive(Debug, Clone)]
pub struct Store {
    base: PathBuf,
}

impl Store {
    pub fn new(base: impl Into<PathBuf>) -> Self {
        Store { base: base.into() }
    }

    pub fn base_dir(&self) -> &Path {
        &self.base
    }

    fn snapshot_path(&self) -> PathBuf {
        self.base.join(SNAPSHOT_FILE)
    }

    fn temp_path(&self) -> PathBuf {
        self.base.join(format!("{SNAPSHOT_FILE}.tmp"))
    }

    pub fn lock_path(&self) -> PathBuf {
        self.base.join(LOCK_FILE)
    }

    /// Full path of the snapshot file (tests and diagnostics).
    pub fn snapshot_path_full(&self) -> PathBuf {
        self.snapshot_path()
    }

    /// Atomically persist the snapshot: serialize to a temp file in the same
    /// directory, then rename over the target. A serialization or write
    /// failure leaves the previous file untouched and removes the temp file.
    pub fn save_snapshot(&self, snapshot: &Snapshot) -> Result<(), StoreError> {
        let bytes = serde_json::to_vec_pretty(snapshot).map_err(|e| {
            let _ = fs::remove_file(self.temp_path());
            StoreError::Json(e)
        })?;
        // First-run safety: the base directory may not exist yet.
        fs::create_dir_all(&self.base)?;
        let temp = self.temp_path();
        fs::write(&temp, &bytes)?;
        match fs::rename(&temp, self.snapshot_path()) {
            Ok(()) => Ok(()),
            Err(e) => {
                let _ = fs::remove_file(&temp);
                Err(StoreError::Io(e))
            }
        }
    }

    /// Load the snapshot, validating the schema version. A missing file is None.
    pub fn load_snapshot(&self) -> Result<Option<Snapshot>, StoreError> {
        let path = self.snapshot_path();
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read(&path)?;
        let mut snapshot: Snapshot = serde_json::from_slice(&raw)?;
        snapshot.validate_version()?;
        snapshot.fields.normalize();
        Ok(Some(snapshot))
    }

    /// Create/overwrite the lock file with the given pid and heartbeat.
    pub fn write_lock(&self, pid: u32, heartbeat_unix: u64) -> Result<(), StoreError> {
        fs::create_dir_all(&self.base)?;
        let info = LockInfo {
            pid,
            heartbeat_unix,
        };
        let bytes = serde_json::to_vec_pretty(&info)?;
        fs::write(self.lock_path(), bytes)?;
        Ok(())
    }

    /// Refresh the heartbeat of the existing lock. Fails if there is no lock.
    pub fn touch_heartbeat(&self, now_unix: u64) -> Result<(), StoreError> {
        let mut info = self.read_lock()?.ok_or_else(|| {
            StoreError::Io(io::Error::new(io::ErrorKind::NotFound, "no lock file"))
        })?;
        info.heartbeat_unix = now_unix;
        let bytes = serde_json::to_vec_pretty(&info)?;
        fs::write(self.lock_path(), bytes)?;
        Ok(())
    }

    pub fn read_lock(&self) -> Result<Option<LockInfo>, StoreError> {
        let path = self.lock_path();
        if !path.exists() {
            return Ok(None);
        }
        let raw = fs::read(&path)?;
        Ok(Some(serde_json::from_slice(&raw)?))
    }

    /// Remove the snapshot and the lock. Missing files are fine.
    pub fn clear(&self) -> Result<(), StoreError> {
        let _ = fs::remove_file(self.snapshot_path());
        let _ = fs::remove_file(self.lock_path());
        Ok(())
    }
}

/// SCHEMA_VERSION is re-exported here for callers of the store API.
pub fn schema_version() -> u32 {
    SCHEMA_VERSION
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FieldSnapshot, FieldValue, SnapshotFields};

    fn sample() -> Snapshot {
        Snapshot::new(
            "2026-01-01T00:00:00Z".into(),
            SnapshotFields {
                timezone: FieldSnapshot {
                    original: FieldValue::Timezone {
                        key_name: "TZ".into(),
                        dst_disabled: false,
                        service_state: None,
                    },
                    applied: None,
                },
                locale_name: FieldSnapshot {
                    original: FieldValue::LocaleName("ru-RU".into()),
                    applied: None,
                },
                geo_nation: FieldSnapshot {
                    original: FieldValue::GeoNation("203".into()),
                    applied: None,
                },
                kbd_preload: FieldSnapshot {
                    original: FieldValue::KbdPreload(vec!["00000409".into()]),
                    applied: None,
                },
            },
        )
    }

    fn test_tmp_root() -> PathBuf {
        // cargo test under DSH denies writes to %TEMP% (sandboxed per-invocation),
        // so tests use a workspace-relative scratch dir instead.
        let d = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-tmp");
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn tempdir() -> PathBuf {
        use std::sync::atomic::{AtomicU32, Ordering};
        static N: AtomicU32 = AtomicU32::new(0);
        let d = test_tmp_root().join(format!(
            "snap-store-test-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn save_load_roundtrip() {
        let dir = tempdir();
        let store = Store::new(&dir);
        store.save_snapshot(&sample()).unwrap();
        let loaded = store.load_snapshot().unwrap().unwrap();
        assert_eq!(loaded, sample());
    }

    #[test]
    fn version_mismatch_leaves_file() {
        let dir = tempdir();
        let store = Store::new(&dir);
        store.save_snapshot(&sample()).unwrap();
        // Corrupt the version to 99 directly on disk.
        let path = store.snapshot_path();
        let raw = fs::read_to_string(&path)
            .unwrap()
            .replace("\"version\": 1", "\"version\": 99");
        fs::write(&path, raw).unwrap();
        let err = store.load_snapshot().unwrap_err();
        assert!(matches!(
            err,
            StoreError::VersionMismatch {
                found: 99,
                expected: 1
            }
        ));
        // File must be untouched.
        assert!(path.exists());
        assert!(fs::read_to_string(&path)
            .unwrap()
            .contains("\"version\": 99"));
    }

    #[test]
    fn missing_file_is_none() {
        let dir = tempdir();
        let store = Store::new(&dir);
        assert!(store.load_snapshot().unwrap().is_none());
    }

    #[test]
    fn lock_write_touch_read() {
        let dir = tempdir();
        let store = Store::new(&dir);
        assert!(store.read_lock().unwrap().is_none());
        store.write_lock(4242, 1000).unwrap();
        store.touch_heartbeat(1500).unwrap();
        let lock = store.read_lock().unwrap().unwrap();
        assert_eq!(
            lock,
            LockInfo {
                pid: 4242,
                heartbeat_unix: 1500
            }
        );
    }
}
