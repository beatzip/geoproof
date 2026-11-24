//! IANA -> Windows timezone name mapping, generated at build time from the
//! pinned CLDR windowsZones.xml (see build.rs), plus modern->legacy IANA
//! aliases (the CLDR file uses legacy tzdb spellings).
include!(concat!(env!("OUT_DIR"), "/tz_windows_generated.rs"));

/// Modern -> legacy IANA aliases. The CLDR windowsZones.xml (typeVersion
/// 2021a) spells zones the legacy way; a modern spelling from a provider
/// (e.g. Europe/Kyiv) is first looked up directly and then through this
/// table. Extend only together with a test.
pub static ALIASES: &[(&str, &str)] = &[
    ("Asia/Kathmandu", "Asia/Katmandu"),
    ("Europe/Kyiv", "Europe/Kiev"),
    ("America/Nuuk", "America/Godthab"),
    ("Asia/Kolkata", "Asia/Calcutta"),
    ("Asia/Yangon", "Asia/Rangoon"),
    ("Asia/Ho_Chi_Minh", "Asia/Saigon"),
    ("Africa/Asmara", "Africa/Asmera"),
    ("Pacific/Chuuk", "Pacific/Truk"),
    ("Pacific/Pohnpei", "Pacific/Ponape"),
    ("Pacific/Kanton", "Pacific/Enderbury"),
    ("America/Indiana/Indianapolis", "America/Indianapolis"),
    ("America/Argentina/Buenos_Aires", "America/Buenos_Aires"),
    ("America/Kentucky/Louisville", "America/Louisville"),
    // The XML maps only Etc/UTC / Etc/GMT; the bare "UTC" spelling providers
    // emit maps through this alias.
    ("UTC", "Etc/UTC"),
];

/// Map an IANA timezone name to its Windows timezone name.
///
/// Exact match first; on a miss, the modern->legacy alias table is consulted
/// and the lookup retried. Unknown zones return None.
pub fn iana_to_windows(iana: &str) -> Option<&'static str> {
    if let Some(hit) = lookup_exact(iana) {
        return Some(hit);
    }
    lookup_exact(ALIASES.iter().find(|(modern, _)| *modern == iana)?.1)
}

/// Binary search over the sorted generated table.
fn lookup_exact(iana: &str) -> Option<&'static str> {
    TABLE
        .binary_search_by(|(name, _)| (*name).cmp(iana))
        .ok()
        .map(|idx| TABLE[idx].1)
}

/// Total number of unique IANA zone tokens in the XML (build statistic).
pub fn total_iana_zones() -> usize {
    TOTAL_IANA
}

/// Mapped IANA zone count (build statistic).
pub fn mapped_iana_zones() -> usize {
    TABLE.len()
}

/// Recorded mapping conflicts (001 priority kept), if any.
pub fn mapping_conflicts() -> &'static [&'static str] {
    CONFLICTS
}

#[cfg(test)]
mod tests;
