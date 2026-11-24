//! `LocaleApplier`: change the regional formats, geo nation and keyboard
//! preload list (all under HKCU, no elevation), then broadcast once.
//!
//! The three fields are one coupled snapshot unit: they share a single
//! `WM_SETTINGCHANGE` pair at the end of the step, so they are applied and
//! rolled back together by one applier.

use crate::error::{AppliedChange, ApplyError};
use crate::traits::{Applier, LocaleRegistry, SettingChangeBroadcast};

/// Value of the locale field group, snapshot-compatible.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LocaleValue {
    /// Regional format, e.g. "ru-RU" (BCP-47).
    pub locale_name: String,
    /// GeoNation, e.g. "203" (ISO 3166-1 numeric).
    pub geo_nation: String,
    /// Ordered keyboard preload list, e.g. ["00000409", "00000419"].
    pub kbd_preload: Vec<String>,
}

pub const FIELD_LOCALE_NAME: &str = "locale_name";
pub const FIELD_GEO_NATION: &str = "geo_nation";
pub const FIELD_KBD_PRELOAD: &str = "kbd_preload";

/// Which browser restart hints apply: browsers cache
/// `Intl.DateTimeFormat` until restarted, though a broadcast is picked up
/// partially.
pub const RESTART_HINTS: [&str; 2] = ["Chrome", "Firefox"];

/// Applies (and reverts) the locale/geo/keyboard triplet under HKCU.
pub struct LocaleApplier<R, B>
where
    R: LocaleRegistry,
    B: SettingChangeBroadcast,
{
    pub registry: R,
    pub broadcast: B,
}

impl<R, B> LocaleApplier<R, B>
where
    R: LocaleRegistry,
    B: SettingChangeBroadcast,
{
    pub fn new(registry: R, broadcast: B) -> Self {
        LocaleApplier {
            registry,
            broadcast,
        }
    }

    /// Broadcast pair; every failure here is a warning, never an error:
    /// the registry writes are already committed and per-window hangs must
    /// not fail an applied change.
    fn broadcast_all(&self) {
        if let Err(e) = self.broadcast.broadcast_locale() {
            tracing::warn!("locale broadcast failed: {e}");
        }
        if let Err(e) = self.broadcast.broadcast_keyboard() {
            tracing::warn!("keyboard broadcast failed: {e}");
        }
    }

    /// Roll the already-written fields back to `before`; rollback errors
    /// are logged and never mask the original failure.
    fn rollback(&self, written: &[&'static str], before: &LocaleValue) {
        for field in written {
            let res = match *field {
                FIELD_LOCALE_NAME => self.registry.write_locale_name(&before.locale_name),
                FIELD_GEO_NATION => self.registry.write_geo_nation(&before.geo_nation),
                FIELD_KBD_PRELOAD => self.registry.write_kbd_preload(&before.kbd_preload),
                _ => Ok(()),
            };
            if let Err(e) = res {
                tracing::warn!("rollback of {field} failed: {e}");
            }
        }
    }
}

#[async_trait::async_trait]
impl<R, B> Applier for LocaleApplier<R, B>
where
    R: LocaleRegistry + Send + Sync,
    B: SettingChangeBroadcast + Send + Sync,
{
    type Value = LocaleValue;

    async fn current(&self) -> Result<Self::Value, ApplyError> {
        Ok(LocaleValue {
            locale_name: self.registry.read_locale_name()?,
            geo_nation: self.registry.read_geo_nation()?,
            kbd_preload: self.registry.read_kbd_preload()?,
        })
    }

    async fn apply(&mut self, new: &Self::Value) -> Result<AppliedChange, ApplyError> {
        let current = self.current().await?;
        // Idempotence: equal triplet -> Noop, zero side effects.
        if current == *new {
            return Ok(AppliedChange::Noop);
        }

        // Field diffs, in write order. An empty string or an empty preload
        // list is a valid value and is written as-is (no default guessing).
        let mut changed: Vec<&'static str> = Vec::new();
        if current.locale_name != new.locale_name {
            changed.push(FIELD_LOCALE_NAME);
        }
        if current.geo_nation != new.geo_nation {
            changed.push(FIELD_GEO_NATION);
        }
        if current.kbd_preload != new.kbd_preload {
            changed.push(FIELD_KBD_PRELOAD);
        }

        // Any failure after the first write rolls the written fields back
        // to their pre-apply values (the [Applier] partial-failure rule).
        for field in &changed {
            let res = match *field {
                FIELD_LOCALE_NAME => self.registry.write_locale_name(&new.locale_name),
                FIELD_GEO_NATION => self.registry.write_geo_nation(&new.geo_nation),
                FIELD_KBD_PRELOAD => self.registry.write_kbd_preload(&new.kbd_preload),
                _ => Ok(()),
            };
            if let Err(e) = res {
                self.rollback(
                    &changed[..changed.iter().position(|f| f == field).unwrap_or(0)],
                    &current,
                );
                return Err(e);
            }
        }

        self.broadcast_all();
        Ok(AppliedChange::LocaleChanged {
            changed_fields: changed,
            restart_hint: RESTART_HINTS.to_vec(),
        })
    }

    async fn revert(&mut self, original: &Self::Value) -> Result<(), ApplyError> {
        let current = self.current().await?;
        // A repeated revert on a matching system is safe and a Noop.
        if current == *original {
            return Ok(());
        }

        let mut written: Vec<&'static str> = Vec::new();
        if current.locale_name != original.locale_name {
            self.registry.write_locale_name(&original.locale_name)?;
            written.push(FIELD_LOCALE_NAME);
        }
        if current.geo_nation != original.geo_nation {
            self.registry.write_geo_nation(&original.geo_nation)?;
            written.push(FIELD_GEO_NATION);
        }
        if current.kbd_preload != original.kbd_preload {
            self.registry.write_kbd_preload(&original.kbd_preload)?;
            written.push(FIELD_KBD_PRELOAD);
        }

        self.broadcast_all();
        Ok(())
    }
}
