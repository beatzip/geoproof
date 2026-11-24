//! Seams between pure applier logic and Windows. Production impls live in
//! [crate::sys]; tests substitute mocks, so the logic stays safe offline.

use crate::error::ApplyError;
use serde::{de::DeserializeOwned, Serialize};

/// Common contract for every applier (stages 4-5).
#[async_trait::async_trait]
pub trait Applier {
    type Value: Serialize + DeserializeOwned + PartialEq + Eq + std::fmt::Debug + Clone + Send;

    /// Read the value the system currently holds.
    async fn current(&self) -> Result<Self::Value, ApplyError>;

    /// Make the system hold `new`. Idempotent: equal values are a Noop with
    /// no side effects. On a partial failure the impl must roll back the
    /// steps it already performed before returning the error.
    async fn apply(&mut self, new: &Self::Value)
        -> Result<crate::error::AppliedChange, ApplyError>;

    /// Return the system to `original` (the snapshot original). Not required
    /// to be idempotent, but a repeated call must stay safe.
    async fn revert(&mut self, original: &Self::Value) -> Result<(), ApplyError>;
}

/// DST rules of one timezone, decoded from the registry `TZI` blob.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TzRules {
    pub bias_minutes: i32,
    pub standard_bias_minutes: i32,
    pub daylight_bias_minutes: i32,
    pub standard_date: crate::sys::time::SystemDate,
    pub daylight_date: crate::sys::time::SystemDate,
}

/// DST transitions, as read from the registry. The `[u16; 32]` name fields
/// of the Win32 struct are filled from [TzNames].
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TzNames {
    pub std: String,
    pub dlt: String,
}

/// The timezone data one zone key holds: names plus DST rules.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ZoneData {
    pub names: TzNames,
    pub rules: TzRules,
}

/// Reads timezone definitions from HKLM\...\Time Zones\<key>.
pub trait TzRegistry: Send + Sync {
    /// `None` when the zone key does not exist; the caller turns that into
    /// [ApplyError::UnknownZone].
    fn read_zone(&self, key_name: &str) -> Result<Option<ZoneData>, ApplyError>;
}

impl<R: TzRegistry + ?Sized> TzRegistry for &R {
    fn read_zone(&self, key_name: &str) -> Result<Option<ZoneData>, ApplyError> {
        (**self).read_zone(key_name)
    }
}

/// Writes the active timezone via SetDynamicTimeZoneInformation.
pub trait WinTzSetter: Send + Sync {
    /// Also returns the current value when the caller needs a read: the
    /// reader half is provided by [Self::read_current].
    fn set_dynamic(&self, value: &TimezoneValue, data: &ZoneData) -> Result<(), ApplyError>;

    /// Read the active timezone (TimeZoneKeyName + dst_disabled).
    fn read_current(&self) -> Result<TimezoneValue, ApplyError>;
}

/// Start type of a Windows service, as observed through the SCM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceStartType {
    AutoStart,
    DemandStart,
    Disabled,
    Unknown(u32),
}

/// Desired runtime state of `tzautoupdate`, as stored in the snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum TzAutoUpdateState {
    Enabled,
    Disabled,
}

/// Controls the `tzautoupdate` service through the SCM.
pub trait ServiceController: Send + Sync {
    /// Read the configured start type; `None` if the service is absent.
    fn query_start_type(&self, service: &str) -> Result<Option<ServiceStartType>, ApplyError>;

    /// Set the start type (ChangeServiceConfigW).
    fn set_start_type(&self, service: &str, start: ServiceStartType) -> Result<(), ApplyError>;

    /// Request a stop; a no-op when the service is already stopped.
    fn stop(&self, service: &str) -> Result<(), ApplyError>;

    /// Start the service; a no-op when it is already running.
    fn start(&self, service: &str) -> Result<(), ApplyError>;
}

/// Privilege probe. The applier is meant to run inside an already elevated
/// process (stage 7); this check just refuses early when the token is not.
pub trait AdminCheck: Send + Sync {
    fn is_admin(&self) -> bool;
}

use crate::timezone::TimezoneValue;

/// Reads/writes the three regional-format fields the locale stage owns,
/// all under HKCU (no elevation required):
/// - `Control Panel\\International\LocaleName` (REG_SZ, BCP-47),
/// - `Control Panel\\International\\Geo\\Nation` (REG_SZ, ISO 3166-1 numeric),
/// - `Keyboard Layout\\Preload`: ordered subvalues "1", "2", ... (REG_SZ
///   layout hex codes); a missing key reads as an empty list.
pub trait LocaleRegistry: Send + Sync {
    fn read_locale_name(&self) -> Result<String, ApplyError>;
    fn write_locale_name(&self, value: &str) -> Result<(), ApplyError>;
    fn read_geo_nation(&self) -> Result<String, ApplyError>;
    fn write_geo_nation(&self, value: &str) -> Result<(), ApplyError>;
    fn read_kbd_preload(&self) -> Result<Vec<String>, ApplyError>;
    fn write_kbd_preload(&self, values: &[String]) -> Result<(), ApplyError>;
}

/// Broadcasts `WM_SETTINGCHANGE` so running processes notice the change.
/// A timeout on individual windows is tolerable: the production impl logs
/// a warning and returns Ok; an Err is treated the same way by the applier.
pub trait SettingChangeBroadcast: Send + Sync {
    fn broadcast_locale(&self) -> Result<(), ApplyError>;
    fn broadcast_keyboard(&self) -> Result<(), ApplyError>;
}

impl<T: LocaleRegistry + ?Sized> LocaleRegistry for &T {
    fn read_locale_name(&self) -> Result<String, ApplyError> {
        (**self).read_locale_name()
    }
    fn write_locale_name(&self, value: &str) -> Result<(), ApplyError> {
        (**self).write_locale_name(value)
    }
    fn read_geo_nation(&self) -> Result<String, ApplyError> {
        (**self).read_geo_nation()
    }
    fn write_geo_nation(&self, value: &str) -> Result<(), ApplyError> {
        (**self).write_geo_nation(value)
    }
    fn read_kbd_preload(&self) -> Result<Vec<String>, ApplyError> {
        (**self).read_kbd_preload()
    }
    fn write_kbd_preload(&self, values: &[String]) -> Result<(), ApplyError> {
        (**self).write_kbd_preload(values)
    }
}

impl<T: SettingChangeBroadcast + ?Sized> SettingChangeBroadcast for &T {
    fn broadcast_locale(&self) -> Result<(), ApplyError> {
        (**self).broadcast_locale()
    }
    fn broadcast_keyboard(&self) -> Result<(), ApplyError> {
        (**self).broadcast_keyboard()
    }
}
