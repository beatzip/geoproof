//! Offline tests for the stage-5 locale applier: registry and broadcast
//! seams are mocked, write/broadcast order checked via a shared log.
//! Covers the stage-5 spec tests 1-8 plus the partial-failure rollback.

#![allow(clippy::unwrap_used)] // tests: unwrap on controlled fixtures is fine

use std::sync::{Arc, Mutex};

use apply::{
    AppliedChange, Applier, ApplyError, LocaleApplier, LocaleRegistry, LocaleValue,
    SettingChangeBroadcast,
};

#[derive(Clone, Default)]
struct Log(Arc<Mutex<Vec<String>>>);

impl Log {
    fn push(&self, s: &str) {
        self.0.lock().unwrap().push(s.to_string());
    }
    fn entries(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

#[derive(Clone)]
struct MockRegistry {
    log: Log,
    state: Arc<Mutex<LocaleValue>>,
    /// Fail the geo write once set (used for the rollback test).
    fail_geo_write: bool,
}

impl MockRegistry {
    fn with(state: LocaleValue) -> Self {
        MockRegistry {
            log: Log::default(),
            state: Arc::new(Mutex::new(state)),
            fail_geo_write: false,
        }
    }
}

impl LocaleRegistry for MockRegistry {
    fn read_locale_name(&self) -> Result<String, ApplyError> {
        self.log.push("read_locale_name");
        Ok(self.state.lock().unwrap().locale_name.clone())
    }
    fn write_locale_name(&self, value: &str) -> Result<(), ApplyError> {
        self.log.push("write_locale_name");
        self.state.lock().unwrap().locale_name = value.to_string();
        Ok(())
    }
    fn read_geo_nation(&self) -> Result<String, ApplyError> {
        self.log.push("read_geo_nation");
        Ok(self.state.lock().unwrap().geo_nation.clone())
    }
    fn write_geo_nation(&self, value: &str) -> Result<(), ApplyError> {
        self.log.push("write_geo_nation");
        if self.fail_geo_write {
            return Err(ApplyError::RegistryRead(
                "Geo\\Nation".into(),
                std::io::Error::other("simulated write failure"),
            ));
        }
        self.state.lock().unwrap().geo_nation = value.to_string();
        Ok(())
    }
    fn read_kbd_preload(&self) -> Result<Vec<String>, ApplyError> {
        self.log.push("read_kbd_preload");
        Ok(self.state.lock().unwrap().kbd_preload.clone())
    }
    fn write_kbd_preload(&self, values: &[String]) -> Result<(), ApplyError> {
        self.log.push("write_kbd_preload");
        self.state.lock().unwrap().kbd_preload = values.to_vec();
        Ok(())
    }
}

#[derive(Clone, Default)]
struct MockBroadcast {
    log: Log,
    fail_locale: bool,
}

impl SettingChangeBroadcast for MockBroadcast {
    fn broadcast_locale(&self) -> Result<(), ApplyError> {
        self.log.push("broadcast_locale");
        if self.fail_locale {
            return Err(ApplyError::RegistryRead(
                "broadcast timeout".into(),
                std::io::Error::other("simulated WM_SETTINGCHANGE timeout"),
            ));
        }
        Ok(())
    }
    fn broadcast_keyboard(&self) -> Result<(), ApplyError> {
        self.log.push("broadcast_keyboard");
        Ok(())
    }
}

fn current_ru() -> LocaleValue {
    LocaleValue {
        locale_name: "ru-RU".into(),
        geo_nation: "203".into(),
        kbd_preload: vec!["00000409".into(), "00000419".into()],
    }
}

fn new_us() -> LocaleValue {
    LocaleValue {
        locale_name: "en-US".into(),
        geo_nation: "244".into(),
        kbd_preload: vec!["00000409".into()],
    }
}

fn write_count(log: &Log) -> usize {
    log.entries()
        .iter()
        .filter(|e| e.starts_with("write_"))
        .count()
}

fn broadcast_count(log: &Log) -> usize {
    log.entries()
        .iter()
        .filter(|e| e.starts_with("broadcast_"))
        .count()
}

#[test]
fn apply_happy_path_three_writes_two_broadcasts_in_order() {
    let reg = MockRegistry::with(current_ru());
    let bc = MockBroadcast::default();
    let mut applier = LocaleApplier::new(reg.clone(), bc.clone());

    let res = apply::block_on(applier.apply(&new_us())).unwrap();
    let AppliedChange::LocaleChanged {
        changed_fields,
        restart_hint,
    } = res
    else {
        panic!("expected LocaleChanged, got {res:?}");
    };
    assert_eq!(
        changed_fields,
        vec!["locale_name", "geo_nation", "kbd_preload"]
    );
    assert_eq!(restart_hint, vec!["Chrome", "Firefox"]);
    assert_eq!(
        reg.log.entries(),
        vec![
            "read_locale_name",
            "read_geo_nation",
            "read_kbd_preload",
            "write_locale_name",
            "write_geo_nation",
            "write_kbd_preload",
        ]
    );
    assert_eq!(broadcast_count(&bc.log), 2);
    assert_eq!(bc.log.entries()[0], "broadcast_locale");
    assert_eq!(bc.log.entries()[1], "broadcast_keyboard");
    assert_eq!(*reg.state.lock().unwrap(), new_us());
}

#[test]
fn apply_equal_is_noop_with_zero_side_effects() {
    let reg = MockRegistry::with(current_ru());
    let bc = MockBroadcast::default();
    let mut applier = LocaleApplier::new(reg.clone(), bc.clone());

    let res = apply::block_on(applier.apply(&current_ru())).unwrap();
    assert!(res.is_noop());
    assert_eq!(write_count(&reg.log), 0);
    assert_eq!(broadcast_count(&bc.log), 0);
}

#[test]
fn apply_only_locale_differs_writes_one_field() {
    let reg = MockRegistry::with(current_ru());
    let bc = MockBroadcast::default();
    let mut applier = LocaleApplier::new(reg.clone(), bc.clone());

    let target = LocaleValue {
        locale_name: "de-DE".into(),
        ..current_ru()
    };
    let res = apply::block_on(applier.apply(&target)).unwrap();
    let AppliedChange::LocaleChanged { changed_fields, .. } = res else {
        panic!("expected LocaleChanged, got {res:?}");
    };
    assert_eq!(changed_fields, vec!["locale_name"]);
    assert_eq!(write_count(&reg.log), 1);
    let writes: Vec<String> = reg
        .log
        .entries()
        .into_iter()
        .filter(|e| e.starts_with("write_"))
        .collect();
    assert_eq!(writes, vec!["write_locale_name"]);
    assert_eq!(broadcast_count(&bc.log), 2);
    assert_eq!(reg.state.lock().unwrap().geo_nation, "203");
    assert_eq!(reg.state.lock().unwrap().kbd_preload.len(), 2);
}

#[test]
fn apply_empty_kbd_preload_written_as_is() {
    let reg = MockRegistry::with(current_ru());
    let bc = MockBroadcast::default();
    let mut applier = LocaleApplier::new(reg.clone(), bc.clone());

    let target = LocaleValue {
        kbd_preload: Vec::new(),
        ..current_ru()
    };
    let res = apply::block_on(applier.apply(&target)).unwrap();
    assert!(!res.is_noop());
    assert!(reg.state.lock().unwrap().kbd_preload.is_empty());
}

#[test]
fn revert_happy_path_restores_all_fields_and_broadcasts() {
    let reg = MockRegistry::with(new_us());
    let bc = MockBroadcast::default();
    let mut applier = LocaleApplier::new(reg.clone(), bc.clone());

    apply::block_on(applier.revert(&current_ru())).unwrap();
    assert_eq!(*reg.state.lock().unwrap(), current_ru());
    assert_eq!(broadcast_count(&bc.log), 2);
}

#[test]
fn revert_with_empty_original_writes_empty_values() {
    let reg = MockRegistry::with(new_us());
    let bc = MockBroadcast::default();
    let mut applier = LocaleApplier::new(reg.clone(), bc.clone());

    let original = LocaleValue {
        locale_name: String::new(),
        geo_nation: String::new(),
        kbd_preload: Vec::new(),
    };
    apply::block_on(applier.revert(&original)).unwrap();
    assert_eq!(*reg.state.lock().unwrap(), original);
}

#[test]
fn broadcast_failure_is_warning_not_error() {
    let reg = MockRegistry::with(current_ru());
    let bc = MockBroadcast {
        log: Log::default(),
        fail_locale: true,
    };
    let mut applier = LocaleApplier::new(reg.clone(), bc.clone());

    let res = apply::block_on(applier.apply(&new_us())).unwrap();
    assert!(!res.is_noop());
    // Writes happened and the second broadcast was still attempted.
    assert_eq!(write_count(&reg.log), 3);
    assert_eq!(broadcast_count(&bc.log), 2);
}

#[test]
fn registry_read_failure_stops_before_any_write_or_broadcast() {
    let mut reg = MockRegistry::with(current_ru());
    let bc = MockBroadcast::default();
    // Simulate a missing LocaleName value: reads fail wholesale.
    reg.fail_geo_write = false;
    let mut applier = LocaleApplier::new(ReadFails(reg.clone()), bc.clone());

    let err = apply::block_on(applier.apply(&new_us())).unwrap_err();
    assert!(matches!(err, ApplyError::RegistryRead(_, _)), "got {err:?}");
    assert_eq!(write_count(&reg.log), 0);
    assert_eq!(broadcast_count(&bc.log), 0);
}

/// Wraps the mock so reads fail but the write log stays observable.
struct ReadFails(MockRegistry);

impl LocaleRegistry for ReadFails {
    fn read_locale_name(&self) -> Result<String, ApplyError> {
        Err(ApplyError::RegistryRead(
            "LocaleName not found".into(),
            std::io::Error::new(std::io::ErrorKind::NotFound, "missing"),
        ))
    }
    fn write_locale_name(&self, v: &str) -> Result<(), ApplyError> {
        self.0.write_locale_name(v)
    }
    fn read_geo_nation(&self) -> Result<String, ApplyError> {
        self.0.read_geo_nation()
    }
    fn write_geo_nation(&self, v: &str) -> Result<(), ApplyError> {
        self.0.write_geo_nation(v)
    }
    fn read_kbd_preload(&self) -> Result<Vec<String>, ApplyError> {
        self.0.read_kbd_preload()
    }
    fn write_kbd_preload(&self, v: &[String]) -> Result<(), ApplyError> {
        self.0.write_kbd_preload(v)
    }
}

#[test]
fn partial_apply_failure_rolls_back_written_fields() {
    let mut reg = MockRegistry::with(current_ru());
    reg.fail_geo_write = true;
    let bc = MockBroadcast::default();
    let mut applier = LocaleApplier::new(reg.clone(), bc.clone());

    let err = apply::block_on(applier.apply(&new_us())).unwrap_err();
    assert!(matches!(err, ApplyError::RegistryRead(_, _)), "got {err:?}");
    // locale_name was rolled back to the pre-apply value, nothing broadcast.
    assert_eq!(reg.state.lock().unwrap().locale_name, "ru-RU");
    assert_eq!(broadcast_count(&bc.log), 0);
}
