//! Versioned snapshot model (schema v1) and field value types.
//!
//! Schema rules:
//! - unknown version on load => SnapshotError::VersionMismatch, file untouched;
//! - serde deny_unknown_fields keeps the on-disk format strict.

use serde::{Deserialize, Serialize};

use crate::store::StoreError;

pub const SCHEMA_VERSION: u32 = 1;

/// Runtime state of the `tzautoupdate` service, carried by the *applied*
/// timezone record (stage 6): the orchestrator captures the pre-apply state
/// and stores it here, so restore can return the service to it. Mirror of
/// `apply::TzAutoUpdateState` — this crate stays dependency-free.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TzServiceState {
    Enabled,
    Disabled,
}

/// Value of one observed system field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum FieldValue {
    /// Timezone: Windows registry key name + whether DST automation was off.
    Timezone {
        key_name: String,
        dst_disabled: bool,
        /// Only set on the applied record: the `tzautoupdate` state captured
        /// before the first apply. Missing (`None`) means "unknown".
        #[serde(default, skip_serializing_if = "Option::is_none")]
        service_state: Option<TzServiceState>,
    },
    /// Ordered keyboard preload list, e.g. ["00000409", "00000419"].
    KbdPreload(Vec<String>),
    /// GeoNation, e.g. "203".
    GeoNation(String),
    /// Locale name, e.g. "ru-RU".
    LocaleName(String),
}

/// Field identity (JSON map key and restore item identity).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldId {
    Timezone,
    LocaleName,
    GeoNation,
    KbdPreload,
}

pub const ALL_FIELDS: [FieldId; 4] = [
    FieldId::Timezone,
    FieldId::LocaleName,
    FieldId::GeoNation,
    FieldId::KbdPreload,
];

impl FieldId {
    pub fn as_str(self) -> &'static str {
        match self {
            FieldId::Timezone => "timezone",
            FieldId::LocaleName => "locale_name",
            FieldId::GeoNation => "geo_nation",
            FieldId::KbdPreload => "kbd_preload",
        }
    }
}

impl std::fmt::Display for FieldId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Original + (optionally) applied value of one field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldSnapshot {
    /// State observed before any change.
    pub original: FieldValue,
    /// Value the applier set after a successful change; None until then.
    pub applied: Option<FieldValue>,
}

/// Root persisted document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub version: u32,
    /// RFC 3339 UTC timestamp of capture.
    pub created_utc: String,
    /// C1 (D3): IPs observed on the native network while nothing was
    /// applied yet. Empty for snapshots written by older versions.
    #[serde(default)]
    pub native_ips: Vec<String>,
    pub fields: SnapshotFields,
}

/// The four known fields; unknown JSON keys are rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SnapshotFields {
    pub timezone: FieldSnapshot,
    pub locale_name: FieldSnapshot,
    pub geo_nation: FieldSnapshot,
    pub kbd_preload: FieldSnapshot,
}

impl SnapshotFields {
    /// GeoNation and LocaleName are both plain JSON strings, so untagged
    /// deserialization always yields the first string variant (GeoNation).
    /// Re-type them by the field they belong to after loading.
    pub fn normalize(&mut self) {
        fn fix(v: &mut FieldValue, expect: FieldId) {
            let string: Option<&str> = match v {
                FieldValue::GeoNation(name) if matches!(expect, FieldId::LocaleName) => Some(name),
                FieldValue::LocaleName(name) if matches!(expect, FieldId::GeoNation) => Some(name),
                _ => None,
            };
            if let Some(name) = string {
                *v = match expect {
                    FieldId::GeoNation => FieldValue::GeoNation(name.to_string()),
                    _ => FieldValue::LocaleName(name.to_string()),
                };
            }
        }
        fix(&mut self.locale_name.original, FieldId::LocaleName);
        if let Some(v) = self.locale_name.applied.as_mut() {
            fix(v, FieldId::LocaleName);
        }
        fix(&mut self.geo_nation.original, FieldId::GeoNation);
        if let Some(v) = self.geo_nation.applied.as_mut() {
            fix(v, FieldId::GeoNation);
        }
    }
}

impl Snapshot {
    pub fn new(created_utc: String, fields: SnapshotFields) -> Self {
        Snapshot {
            version: SCHEMA_VERSION,
            created_utc,
            native_ips: Vec::new(),
            fields,
        }
    }

    /// Validate on load: only the current schema version is accepted.
    pub fn validate_version(&self) -> Result<(), StoreError> {
        if self.version == SCHEMA_VERSION {
            Ok(())
        } else {
            Err(StoreError::VersionMismatch {
                found: self.version,
                expected: SCHEMA_VERSION,
            })
        }
    }
}

/// A snapshot de/serialization error.
#[derive(Debug, thiserror::Error)]
pub enum SnapshotError {
    #[error("snapshot schema version {found} is not supported (expected {expected})")]
    VersionMismatch { found: u32, expected: u32 },
    #[error(transparent)]
    Store(crate::store::StoreError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_matches_spec_example() {
        let json = r#"{
            "version": 1,
            "created_utc": "2026-01-01T00:00:00Z",
            "fields": {
                "timezone": { "original": { "key_name": "Russian Standard Time", "dst_disabled": false }, "applied": null },
                "locale_name": { "original": "ru-RU", "applied": null },
                "geo_nation": { "original": "203", "applied": null },
                "kbd_preload": { "original": ["00000409", "00000419"], "applied": null }
            }
        }"#;
        let snap: Snapshot = serde_json::from_str(json).expect("spec example must parse");
        assert_eq!(
            snap.fields.timezone.original,
            FieldValue::Timezone {
                key_name: "Russian Standard Time".into(),
                dst_disabled: false,
                service_state: None,
            }
        );
        let back = serde_json::to_string(&snap).unwrap();
        let reparsed: Snapshot = serde_json::from_str(&back).unwrap();
        assert_eq!(reparsed, snap);
    }

    #[test]
    fn legacy_applied_timezone_without_service_state() {
        // Stage 3 files have no service_state on the timezone value: must
        // still parse, with service_state = None.
        let json = r#"{
            "version": 1,
            "created_utc": "2026-01-01T00:00:00Z",
            "fields": {
                "timezone": { "original": { "key_name": "Russian Standard Time", "dst_disabled": false }, "applied": { "key_name": "FLE Standard Time", "dst_disabled": true } },
                "locale_name": { "original": "ru-RU", "applied": "en-US" },
                "geo_nation": { "original": "203", "applied": "250" },
                "kbd_preload": { "original": ["00000409"], "applied": ["00000409"] }
            }
        }"#;
        let snap: Snapshot = serde_json::from_str(json).expect("legacy file must parse");
        match &snap.fields.timezone.applied {
            Some(FieldValue::Timezone {
                key_name,
                dst_disabled,
                service_state,
            }) => {
                assert_eq!(key_name, "FLE Standard Time");
                assert!(dst_disabled);
                assert_eq!(service_state, &None);
            }
            other => panic!("unexpected applied value: {other:?}"),
        }
        // None is not serialized: the emitted JSON stays identical to stage 3.
        let back = serde_json::to_string(&snap).unwrap();
        assert!(!back.contains("service_state"));
    }

    #[test]
    fn unknown_field_rejected() {
        let json = r#"{
            "version": 1, "created_utc": "2026-01-01T00:00:00Z",
            "fields": {
                "timezone": { "original": 1, "applied": null },
                "locale_name": { "original": "ru-RU", "applied": null },
                "geo_nation": { "original": "203", "applied": null },
                "kbd_preload": { "original": [], "applied": null },
                "bogus": { "original": 1, "applied": null }
            }
        }"#;
        assert!(serde_json::from_str::<Snapshot>(json).is_err());
    }
}
