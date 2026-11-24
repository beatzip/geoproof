//! Snapshot v1 backward compatibility: TimezoneValue without
//! service_state round-trips (spec test 10).

#![allow(clippy::unwrap_used)] // tests: unwrap on controlled fixtures is fine

use apply::{TimezoneValue, TzAutoUpdateState};

const V1_JSON: &str = r#"{"key_name":"Russian Standard Time","dst_disabled":false}"#;

// ---- 10. Snapshot v1 roundtrip ----

#[test]
fn snapshot_v1_without_service_state_roundtrips() {
    let v: TimezoneValue = serde_json::from_str(V1_JSON).expect("v1 JSON must parse");
    assert_eq!(v.key_name, "Russian Standard Time");
    assert!(!v.dst_disabled);
    assert_eq!(v.service_state, None, "v1 JSON has no service_state");

    // Serializing back must NOT emit the optional field.
    let back = serde_json::to_string(&v).unwrap();
    assert_eq!(back, V1_JSON, "round-trip must stay byte-identical to v1");
}

#[test]
fn v2_json_with_service_state_parses() {
    let json =
        r#"{"key_name":"Tokyo Standard Time","dst_disabled":true,"service_state":"Enabled"}"#;
    let v: TimezoneValue = serde_json::from_str(json).unwrap();
    assert_eq!(v.service_state, Some(TzAutoUpdateState::Enabled));

    let json2 = json.replace("Enabled", "Disabled");
    let v2: TimezoneValue = serde_json::from_str(&json2).unwrap();
    assert_eq!(v2.service_state, Some(TzAutoUpdateState::Disabled));
}

#[test]
fn equality_ignores_nothing_service_state_participates() {
    let mut a = TimezoneValue::new("X", false);
    let b = TimezoneValue::new("X", false);
    assert_eq!(a, b);
    a.service_state = Some(TzAutoUpdateState::Disabled);
    assert_ne!(a, b, "service_state is part of the value identity");
}

#[test]
fn field_snapshot_style_wrapper_roundtrips() {
    // Mirrors the snapshot crate's FieldSnapshot envelope shape without
    // depending on the snapshot crate: original/applied of TimezoneValue.
    let original = TimezoneValue::new("Russian Standard Time", false);
    let json = serde_json::to_string(&original).unwrap();
    let parsed: TimezoneValue = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed, original);
    assert!(parsed.service_state.is_none());
}
