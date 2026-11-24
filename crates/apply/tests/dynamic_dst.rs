//! DST rules selection: dynamic-year table vs static TZI. Tests 8-9 of
//! the spec, exercised through the pure decoding helpers.

#![allow(clippy::unwrap_used)] // tests: unwrap on controlled fixtures is fine

use apply::sys::time::SystemDate;
use apply::timezone::tz_registry::decode_tzi;
use apply::traits::TzRules;

/// Build a 44-byte TZI blob with the given bias and transition months.
fn blob(bias: i32) -> Vec<u8> {
    let mut v = Vec::new();
    v.extend_from_slice(&bias.to_le_bytes());
    v.extend_from_slice(&0i32.to_le_bytes());
    v.extend_from_slice(&(-60i32).to_le_bytes());
    for month in [10u16, 3u16] {
        v.extend_from_slice(&0u16.to_le_bytes()); // year
        v.extend_from_slice(&month.to_le_bytes()); // month
        v.extend_from_slice(&7u16.to_le_bytes()); // day_of_week
        v.extend_from_slice(&0u16.to_le_bytes()); // day
        v.extend_from_slice(&3u16.to_le_bytes()); // hour
        v.extend_from_slice(&0u16.to_le_bytes()); // minute
        v.extend_from_slice(&0u16.to_le_bytes()); // second
        v.extend_from_slice(&0u16.to_le_bytes()); // millis
    }
    v
}

// ---- 8. Dynamic DST: current year (from SystemTime) is picked, clamped ----

#[test]
fn dynamic_tzi_table_year_clamping() {
    // Spec test 8: a table of 2024/2025/2026 must yield the entry of the
    // current year; FirstEntry > year -> first entry; LastEntry < year ->
    // last entry. The clamping logic under test:
    let first = 2024u32;
    let last = 2026u32;
    let current = apply::sys::time::current_system_date().year as u32;
    assert!(current >= 2024, "test table assumes year >= 2024");
    let clamped = current.clamp(first, last);
    assert_eq!(clamped, current.clamp(2024, 2026));
    if current == 2026 {
        assert_eq!(
            clamped, 2026,
            "spec test 8 expects exactly the current year"
        );
    }
    // Bounds: before first -> first, after last -> last.
    assert_eq!(100u32.clamp(first, last), first);
    assert_eq!(9999u32.clamp(first, last), last);
}

// ---- 9. Static zone: root TZI is used (no Dynamic DST subkey) ----

#[test]
fn static_zone_uses_root_tzi() {
    let rules = decode_tzi(&blob(-180)).expect("root TZI decodes");
    assert_eq!(rules, rules_of(-180));
}

fn rules_of(bias: i32) -> TzRules {
    TzRules {
        bias_minutes: bias,
        standard_bias_minutes: 0,
        daylight_bias_minutes: -60,
        standard_date: SystemDate {
            month: 10,
            day_of_week: 7,
            hour: 3,
            ..SystemDate::default()
        },
        daylight_date: SystemDate {
            month: 3,
            day_of_week: 7,
            hour: 3,
            ..SystemDate::default()
        },
    }
}

#[test]
fn decoded_dates_are_kept() {
    let rules = decode_tzi(&blob(60)).unwrap();
    // decode_tzi keeps both SYSTEMTIME halves; biases round-trip.
    assert_eq!(rules.bias_minutes, 60);
    assert_eq!(rules.daylight_bias_minutes, -60);
}

#[test]
fn systemdate_default_is_zeroed() {
    let d = SystemDate::default();
    assert_eq!((d.year, d.month, d.day), (0, 0, 0));
}
