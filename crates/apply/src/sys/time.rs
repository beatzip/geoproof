//! SYSTEMTIME-shaped plain data + the current system date, safe to use in
//! tests and decoding logic.

/// Plain copy of Win32 SYSTEMTIME so non-sys modules never touch FFI types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SystemDate {
    pub year: u16,
    pub month: u16,
    pub day_of_week: u16,
    pub day: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
    pub millis: u16,
}

#[cfg(windows)]
pub fn current_system_date() -> SystemDate {
    let st = crate::sys::winapi::get_local_systemtime();
    SystemDate {
        year: st.wYear,
        month: st.wMonth,
        day_of_week: st.wDayOfWeek,
        day: st.wDay,
        hour: st.wHour,
        minute: st.wMinute,
        second: st.wSecond,
        millis: st.wMilliseconds,
    }
}

#[cfg(not(windows))]
pub fn current_system_date() -> SystemDate {
    // Non-Windows dev builds: derive the year from the Unix clock.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Civil-from-days algorithm (Howard Hinnant) without any dependency.
    let days = (secs / 86_400) as i64;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let year = (y + 1) as u16;
    SystemDate {
        year,
        ..Default::default()
    }
}
