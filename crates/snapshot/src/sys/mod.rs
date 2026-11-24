//! Thin Windows FFI wrappers - the ONLY place where unsafe is allowed in
//! this crate. Every unsafe block carries a // SAFETY: comment. No logic
//! here: read values, convert, return. Read-only; no admin required.

#![cfg(windows)]
#![allow(unsafe_code)]

use windows::core::PWSTR;
use windows::Win32::Foundation::{ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS};
use windows::Win32::System::Registry::{RegGetValueW, HKEY, HKEY_CURRENT_USER, RRF_RT_REG_SZ};
use windows::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::System::Time::{GetDynamicTimeZoneInformation, DYNAMIC_TIME_ZONE_INFORMATION};

use crate::model::{FieldId, FieldValue};
use crate::traits::{ProcessLiveness, StateReader};

fn widestring(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn from_wide(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// Read a REG_SZ value from HKCU by subkey path and value name.
fn reg_read_sz(subkey: &str, value_name: &str) -> std::io::Result<String> {
    let subkey_w = widestring(subkey);
    let value_w = widestring(value_name);
    let mut buf = [0u16; 512];
    let mut data_len = (buf.len() * 2) as u32;

    // SAFETY: RegGetValueW with a fixed-size caller buffer; subkey_w and
    // value_w are NUL-terminated; data_len correctly sizes buf. The call only
    // reads HKCU values and returns the length actually written.
    let code = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(subkey_w.as_ptr()),
            windows::core::PCWSTR(value_w.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut _),
            Some(&mut data_len),
        )
    };
    if code != ERROR_SUCCESS {
        return Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("RegGetValueW({subkey}, {value_name}) failed: win32 error {code:?}"),
        ));
    }
    let chars = (data_len as usize) / 2;
    Ok(from_wide(&buf[..chars.min(buf.len())]))
}

/// Windows state reader: timezone via GetDynamicTimeZoneInformation, the rest
/// via HKCU registry reads. Read-only; no admin required.
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsStateReader;

impl StateReader for WindowsStateReader {
    fn read_all(&self) -> std::io::Result<Vec<(FieldId, FieldValue)>> {
        // Timezone.
        let mut dtzi = DYNAMIC_TIME_ZONE_INFORMATION::default();
        // SAFETY: dtzi is a caller-allocated struct of the exact expected
        // type; the API only fills it in.
        let tz_result = unsafe { GetDynamicTimeZoneInformation(&mut dtzi) };
        if tz_result == 0xffffffffu32 {
            return Err(std::io::Error::other(
                "GetDynamicTimeZoneInformation failed",
            ));
        }
        let key_name = from_wide(&dtzi.TimeZoneKeyName);
        let dst_disabled = dtzi.DynamicDaylightTimeDisabled;

        // Locale and geo (REG_SZ under HKCU).
        let locale_name = reg_read_sz(r"Control Panel\International", "LocaleName")?;
        let geo_nation = reg_read_sz(r"Control Panel\International\Geo", "Nation")?;

        // Keyboard preload: ordered value names under HKCU\Keyboard Layout\Preload.
        let preload = read_preload()?;

        Ok(vec![
            (
                FieldId::Timezone,
                FieldValue::Timezone {
                    key_name,
                    dst_disabled,
                    service_state: None,
                },
            ),
            (FieldId::LocaleName, FieldValue::LocaleName(locale_name)),
            (FieldId::GeoNation, FieldValue::GeoNation(geo_nation)),
            (FieldId::KbdPreload, FieldValue::KbdPreload(preload)),
        ])
    }
}

/// Ordered list of the "Preload" subkey values, sorted by value name (1, 2, ...).
fn read_preload() -> std::io::Result<Vec<String>> {
    use windows::Win32::System::Registry::{RegCloseKey, RegOpenKeyExW, KEY_READ};

    let subkey_w = widestring(r"Keyboard Layout\Preload");
    let mut hkey = HKEY::default();
    // SAFETY: subkey_w is NUL-terminated; hkey is an out-param we close below.
    let code = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            windows::core::PCWSTR(subkey_w.as_ptr()),
            None,
            KEY_READ,
            &mut hkey,
        )
    };
    if code != ERROR_SUCCESS {
        // A missing Preload subkey is a normal state on some systems (the
        // applier reads an absent key as an empty list too); treat it as
        // empty rather than failing the whole snapshot read.
        return Ok(Vec::new());
    }

    let result = enum_preload_values(hkey);

    // SAFETY: hkey was opened above with RegOpenKeyExW; closing exactly once.
    unsafe {
        let _ = RegCloseKey(hkey);
    }
    result
}

/// Enumerate the "Preload" values into an ordered list. hkey must be open.
fn enum_preload_values(hkey: HKEY) -> std::io::Result<Vec<String>> {
    use windows::Win32::System::Registry::{RegEnumValueW, REG_NONE};

    let mut entries: Vec<(u32, String)> = Vec::new();
    let mut index = 0u32;
    loop {
        let mut name_buf = [0u16; 163];
        let mut name_len = name_buf.len() as u32;
        let mut value_buf = [0u16; 64];
        let mut value_len = (value_buf.len() * 2) as u32;
        let mut vtype = REG_NONE;

        // SAFETY: fixed-size caller buffers, lengths initialized; hkey is
        // a valid open key from RegOpenKeyExW above.
        let code = unsafe {
            RegEnumValueW(
                hkey,
                index,
                Some(PWSTR(name_buf.as_mut_ptr())),
                &mut name_len,
                None,
                Some(&mut vtype as *mut _ as *mut u32),
                Some(value_buf.as_mut_ptr() as *mut _),
                Some(&mut value_len),
            )
        };
        if code == ERROR_INSUFFICIENT_BUFFER {
            // Value longer than 64 chars - skip defensively.
            index += 1;
            continue;
        }
        if code != ERROR_SUCCESS {
            break; // ERROR_NO_MORE_ITEMS or unexpected - stop.
        }
        let value_name = from_wide(&name_buf[..name_len as usize]);
        let order: u32 = value_name.parse().unwrap_or(u32::MAX);
        entries.push((
            order,
            from_wide(&value_buf[..(value_len as usize / 2).min(value_buf.len())]),
        ));
        index += 1;
    }
    entries.sort_by_key(|(order, _)| *order);
    Ok(entries.into_iter().map(|(_, v)| v).collect())
}

/// Windows liveness: OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION) +
/// GetExitCodeProcess (STILL_ACTIVE). Pid-reuse risk is mitigated by the
/// heartbeat; this trait only reports what the OS says.
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsLiveness;

impl ProcessLiveness for WindowsLiveness {
    fn is_alive(&self, pid: u32) -> bool {
        if pid == 0 {
            return false;
        }
        // SAFETY: OpenProcess with a query-only access right on a caller-
        // supplied pid; the returned handle is closed on all paths below.
        let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) };
        let Ok(handle) = handle else { return false };
        let mut exit_code = 0u32;
        // SAFETY: handle is a valid PROCESS handle from OpenProcess;
        // exit_code is a caller-allocated out-param.
        let ok = unsafe { GetExitCodeProcess(handle, &mut exit_code) };
        // SAFETY: handle is owned from OpenProcess above; closed exactly once.
        unsafe {
            let _ = windows::Win32::Foundation::CloseHandle(handle);
        }
        ok.is_ok() && exit_code == 259 // STILL_ACTIVE
    }
}
