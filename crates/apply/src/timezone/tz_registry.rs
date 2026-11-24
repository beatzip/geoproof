//! Windows implementation of [TzRegistry]: reads zone definitions from
//! `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Time Zones\<key>`.
//! Pure decoding logic is split out so it is testable without the registry.

use std::io;

use crate::error::ApplyError;
use crate::sys::registry::WindowsRegistry;
use crate::sys::time::SystemDate;
use crate::traits::TzNames;
use crate::traits::{TzRegistry, ZoneData};

const TZ_KEY_PREFIX: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion\Time Zones\";

/// Reads zones through the low-level registry reader.
///
/// Owns its [WindowsRegistry] so the applier chain is `'static` and can be
/// moved to a dedicated restore worker thread (H6).
pub struct WindowsTzRegistry {
    pub registry: WindowsRegistry,
}

impl WindowsTzRegistry {
    pub fn new() -> Self {
        Self {
            registry: WindowsRegistry,
        }
    }
}

impl Default for WindowsTzRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl TzRegistry for WindowsTzRegistry {
    fn read_zone(&self, key_name: &str) -> Result<Option<ZoneData>, ApplyError> {
        let key_path = format!("{TZ_KEY_PREFIX}{key_name}");
        if !self.registry.key_exists(&key_path)? {
            return Ok(None);
        }
        read_zone_data(&self.registry, &key_path).map(Some)
    }
}

/// Decode one zone key: names + root TZI, overridden by the current-year
/// entry when a `Dynamic DST` subkey exists.
fn read_zone_data(registry: &WindowsRegistry, key_path: &str) -> Result<ZoneData, ApplyError> {
    let std = registry.read_sz(key_path, "Std")?;
    let dlt = registry.read_sz(key_path, "Dlt")?;
    let tzi = registry
        .read_binary(key_path, "TZI")?
        .ok_or_else(|| reg_err(format!("{key_path}: TZI value missing")))?;

    let mut rules = decode_tzi(&tzi)
        .ok_or_else(|| reg_err(format!("{key_path}: TZI blob too short ({})", tzi.len())))?;

    if let Some(dynamic) = pick_dynamic_tzi(registry, key_path)? {
        rules = dynamic;
    }

    Ok(ZoneData {
        names: TzNames { std, dlt },
        rules,
    })
}

/// When a `Dynamic DST` subkey exists, return the TZI for the current year,
/// clamped to `FirstEntry`..=`LastEntry` (before the first year use the
/// first entry, after the last use the last).
fn pick_dynamic_tzi(
    registry: &WindowsRegistry,
    key_path: &str,
) -> Result<Option<crate::traits::TzRules>, ApplyError> {
    let dyn_path = format!("{key_path}\\Dynamic DST");
    if !registry.key_exists(&dyn_path)? {
        return Ok(None);
    }
    let first = read_u32_le(registry, &dyn_path, "FirstEntry")?;
    let last = read_u32_le(registry, &dyn_path, "LastEntry")?;
    let year = u32::from(current_year()).clamp(first, last);

    let blob = registry
        .read_binary(&dyn_path, &year.to_string())?
        .ok_or_else(|| reg_err(format!("{dyn_path}: entry {year} missing")))?;
    decode_tzi(&blob)
        .map(Some)
        .ok_or_else(|| reg_err(format!("{dyn_path}: entry {year} TZI too short")))
}

fn read_u32_le(registry: &WindowsRegistry, path: &str, value: &str) -> Result<u32, ApplyError> {
    registry
        .read_dword(path, value)?
        .ok_or_else(|| reg_err(format!("{path}: {value} missing")))
}

/// Year of the current wall-clock date; obtained from the OS, never a
/// hardcoded constant.
fn current_year() -> u16 {
    crate::sys::time::current_system_date().year
}

fn reg_err(msg: String) -> ApplyError {
    let err = io::Error::new(io::ErrorKind::InvalidData, msg.clone());
    ApplyError::RegistryRead(msg, err)
}

/// Decode the `TZI` REG_BINARY blob (REG_TZI layout, little-endian):
/// LONG Bias, LONG StandardBias, LONG DaylightBias,
/// SYSTEMTIME StandardDate, SYSTEMTIME DaylightDate.
pub fn decode_tzi(blob: &[u8]) -> Option<crate::traits::TzRules> {
    if blob.len() < 44 {
        return None;
    }
    fn i32_at(blob: &[u8], off: usize) -> i32 {
        let b: [u8; 4] = blob[off..off + 4]
            .try_into()
            .expect("44-byte blob: 4-byte slice");
        i32::from_le_bytes(b)
    }
    fn date_at(blob: &[u8], off: usize) -> Option<SystemDate> {
        let year = u16::from_le_bytes(blob[off..off + 2].try_into().ok()?);
        let month = u16::from_le_bytes(blob[off + 2..off + 4].try_into().ok()?);
        let day_of_week = u16::from_le_bytes(blob[off + 4..off + 6].try_into().ok()?);
        let day = u16::from_le_bytes(blob[off + 6..off + 8].try_into().ok()?);
        let hour = u16::from_le_bytes(blob[off + 8..off + 10].try_into().ok()?);
        let minute = u16::from_le_bytes(blob[off + 10..off + 12].try_into().ok()?);
        let second = u16::from_le_bytes(blob[off + 12..off + 14].try_into().ok()?);
        let millis = u16::from_le_bytes(blob[off + 14..off + 16].try_into().ok()?);
        Some(SystemDate {
            year,
            month,
            day_of_week,
            day,
            hour,
            minute,
            second,
            millis,
        })
    }
    Some(crate::traits::TzRules {
        bias_minutes: i32_at(blob, 0),
        standard_bias_minutes: i32_at(blob, 4),
        daylight_bias_minutes: i32_at(blob, 8),
        standard_date: date_at(blob, 12)?,
        daylight_date: date_at(blob, 28)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(bias: i32, std_bias: i32, dlt_bias: i32) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&bias.to_le_bytes());
        v.extend_from_slice(&std_bias.to_le_bytes());
        v.extend_from_slice(&dlt_bias.to_le_bytes());
        // StandardDate
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&10u16.to_le_bytes());
        v.extend_from_slice(&7u16.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&3u16.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        // DaylightDate
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&3u16.to_le_bytes());
        v.extend_from_slice(&7u16.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&2u16.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        v.extend_from_slice(&0u16.to_le_bytes());
        assert_eq!(v.len(), 44);
        v
    }

    #[test]
    fn decode_tzi_full_blob() {
        let rules = decode_tzi(&blob(-180, 0, -60)).expect("44-byte blob decodes");
        assert_eq!(rules.bias_minutes, -180);
        assert_eq!(rules.standard_bias_minutes, 0);
        assert_eq!(rules.daylight_bias_minutes, -60);
        assert_eq!(rules.standard_date.month, 10);
        assert_eq!(rules.daylight_date.month, 3);
    }

    #[test]
    fn decode_tzi_rejects_short_blob() {
        assert!(decode_tzi(&[0u8; 43]).is_none());
    }
}
