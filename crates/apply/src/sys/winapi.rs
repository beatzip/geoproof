//! WinAPI wrappers: SetDynamicTimeZoneInformation, GetLocalSystemTime and
//! the admin probe. Thin conversions only - all policy lives above.

use windows::Win32::Foundation::SYSTEMTIME;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::System::Time::{
    GetDynamicTimeZoneInformation, SetDynamicTimeZoneInformation, DYNAMIC_TIME_ZONE_INFORMATION,
};
use windows::Win32::UI::Shell::IsUserAnAdmin;

use crate::error::ApplyError;
use crate::sys::time::SystemDate;
use crate::timezone::TimezoneValue;
use crate::traits::{AdminCheck, WinTzSetter, ZoneData};

fn wide_buf(s: &str, len: usize) -> Vec<u16> {
    // Truncate to len-1 so a NUL terminator always fits; never panic on a
    // long display name (review point 4 of the spec).
    let mut out: Vec<u16> = s.encode_utf16().take(len - 1).collect();
    out.resize(len, 0);
    out
}

/// Real-timezone writer + reader backed by WinAPI.
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsTzSetter;

impl WinTzSetter for WindowsTzSetter {
    fn set_dynamic(&self, value: &TimezoneValue, data: &ZoneData) -> Result<(), ApplyError> {
        let dtzi = DYNAMIC_TIME_ZONE_INFORMATION {
            Bias: data.rules.bias_minutes,
            StandardBias: data.rules.standard_bias_minutes,
            DaylightBias: data.rules.daylight_bias_minutes,
            StandardDate: systemtime_from(&data.rules.standard_date),
            DaylightDate: systemtime_from(&data.rules.daylight_date),
            StandardName: wide_buf(&data.names.std, 32).try_into().expect("32 u16s"),
            DaylightName: wide_buf(&data.names.dlt, 32).try_into().expect("32 u16s"),
            TimeZoneKeyName: wide_buf(&value.key_name, 128).try_into().expect("128 u16s"),
            DynamicDaylightTimeDisabled: value.dst_disabled,
        };

        // SAFETY: dtzi is a fully initialized caller-allocated struct of the
        // exact expected type; the API only reads it.
        let result = unsafe { SetDynamicTimeZoneInformation(&dtzi) };
        result.map_err(|e| {
            ApplyError::SetTzFailed(std::io::Error::other(format!(
                "SetDynamicTimeZoneInformation failed: {e}"
            )))
        })
    }

    fn read_current(&self) -> Result<TimezoneValue, ApplyError> {
        let mut dtzi = DYNAMIC_TIME_ZONE_INFORMATION::default();
        // SAFETY: dtzi is a caller-allocated out-struct of the exact type.
        let result = unsafe { GetDynamicTimeZoneInformation(&mut dtzi) };
        if result == 0xffffffffu32 {
            return Err(ApplyError::SetTzFailed(std::io::Error::other(
                "GetDynamicTimeZoneInformation failed",
            )));
        }
        let from_wide = |buf: &[u16]| -> String {
            let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
            String::from_utf16_lossy(&buf[..len])
        };
        Ok(TimezoneValue {
            key_name: from_wide(&dtzi.TimeZoneKeyName),
            dst_disabled: dtzi.DynamicDaylightTimeDisabled,
            service_state: None,
        })
    }
}

fn systemtime_from(d: &SystemDate) -> SYSTEMTIME {
    SYSTEMTIME {
        wYear: d.year,
        wMonth: d.month,
        wDayOfWeek: d.day_of_week,
        wDay: d.day,
        wHour: d.hour,
        wMinute: d.minute,
        wSecond: d.second,
        wMilliseconds: d.millis,
    }
}

pub fn get_local_systemtime() -> SYSTEMTIME {
    // SAFETY: no parameters; the API returns the time by value.
    unsafe { GetLocalTime() }
}

/// Admin probe via shell32. Cached on first call (the token does not change
/// mid-process for our lifecycle).
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsAdminCheck;

impl AdminCheck for WindowsAdminCheck {
    fn is_admin(&self) -> bool {
        // SAFETY: no parameters; shell32 reads the process token.
        let admin = unsafe { IsUserAnAdmin() };
        admin.as_bool()
    }
}
