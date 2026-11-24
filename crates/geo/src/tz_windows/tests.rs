//! Offline mapping tests. Snapshot numbers (TOTAL_IANA, TABLE.len(),
//! conflicts) are pinned to the committed CLDR windowsZones.xml
//! (otherVersion 7e11800, typeVersion 2021a); update them deliberately when
//! bumping CLDR, never mechanically.

use super::*;

fn assert_mapping(iana: &str, win: &str) {
    assert_eq!(iana_to_windows(iana), Some(win), "mapping of {iana}");
}

#[test]
fn paris_maps_to_romance() {
    assert_mapping("Europe/Paris", "Romance Standard Time");
}

#[test]
fn new_york_maps_to_eastern() {
    assert_mapping("America/New_York", "Eastern Standard Time");
}

#[test]
fn phoenix_maps_to_us_mountain() {
    assert_mapping("America/Phoenix", "US Mountain Standard Time");
}

#[test]
fn utc_maps_via_alias() {
    // XML maps only Etc/UTC; bare UTC comes through the alias table.
    assert_mapping("UTC", "UTC");
    assert_mapping("Etc/UTC", "UTC");
}

#[test]
fn kathmandu_both_spellings() {
    assert_mapping("Asia/Kathmandu", "Nepal Standard Time");
    assert_mapping("Asia/Katmandu", "Nepal Standard Time");
}

#[test]
fn kyiv_maps_via_alias_to_fle() {
    // The pinned CLDR (typeVersion 2021a) has no Europe/Kyiv; only the legacy
    // Europe/Kiev, whose 001 mapping is FLE Standard Time (not W. Europe —
    // the W. Europe 001 zone is Europe/Berlin in this file). Spec asked to
    // verify against the XML; recorded deviation from the spec's example.
    assert_mapping("Europe/Kyiv", "FLE Standard Time");
    assert_mapping("Europe/Kiev", "FLE Standard Time");
}

#[test]
fn every_alias_resolves() {
    for (modern, legacy) in ALIASES {
        let direct = lookup_exact(modern);
        let via = iana_to_windows(modern);
        assert_eq!(
            via,
            if direct.is_some() {
                direct
            } else {
                lookup_exact(legacy)
            },
            "alias {modern} -> {legacy} must resolve"
        );
        // Both spellings must resolve to the same Windows name.
        assert_eq!(
            iana_to_windows(modern),
            iana_to_windows(legacy),
            "both spellings of {modern} must agree"
        );
    }
}

#[test]
fn unknown_zone_is_none() {
    assert_eq!(iana_to_windows("Mars/Olympus_Mons"), None);
    assert_eq!(iana_to_windows(""), None);
}

#[test]
fn coverage_at_least_90_percent() {
    let total = total_iana_zones();
    let mapped = mapped_iana_zones();
    assert!(
        mapped * 10 >= total * 9,
        "coverage {mapped}/{total} below 0.90"
    );
}

#[test]
fn snapshot_statistics_match_committed_cldr() {
    // Pinned to CLDR release-46-1 windowsZones.xml
    // (otherVersion 7e11800, typeVersion 2021a). Update deliberately on a
    // CLDR bump: rerun the build, read the new numbers, replace these three.
    assert_eq!(total_iana_zones(), 444, "total unique IANA tokens");
    assert_eq!(mapped_iana_zones(), 444, "mapped IANA zones");
    assert!(
        mapping_conflicts().is_empty(),
        "unexpected conflicts: {:?}",
        mapping_conflicts()
    );
}

#[test]
fn table_is_sorted_for_binary_search() {
    for pair in TABLE.windows(2) {
        assert!(
            pair[0].0 < pair[1].0,
            "table not strictly sorted at {} < {}",
            pair[0].0,
            pair[1].0
        );
    }
}
