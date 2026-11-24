//! Offline tests: all four seams mocked, order checked via a shared log.
//! Covers tests 1-7 of the stage-4 spec.

#![allow(clippy::unwrap_used)] // tests: unwrap on controlled fixtures is fine

use std::sync::{Arc, Mutex};

use apply::{
    AdminCheck, AppliedChange, Applier, ApplyError, ServiceController, ServiceStartType,
    TimezoneApplier, TimezoneValue, TzAutoUpdateState, TzRegistry, TzRules, WinTzSetter, ZoneData,
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
    zones: Arc<Mutex<std::collections::HashMap<String, ZoneData>>>,
}

impl MockRegistry {
    fn with_zones(zones: &[(String, ZoneData)]) -> Self {
        let map = zones.iter().cloned().collect();
        MockRegistry {
            log: Log::default(),
            zones: Arc::new(Mutex::new(map)),
        }
    }
}

impl TzRegistry for MockRegistry {
    fn read_zone(&self, key_name: &str) -> Result<Option<ZoneData>, ApplyError> {
        self.log.push("read_zone");
        Ok(self.zones.lock().unwrap().get(key_name).cloned())
    }
}

#[derive(Clone)]
struct MockService {
    log: Log,
    start_type: Arc<Mutex<ServiceStartType>>,
    running: Arc<Mutex<bool>>,
    fail_query: bool,
}

impl MockService {
    fn new(initial: ServiceStartType) -> Self {
        MockService {
            log: Log::default(),
            start_type: Arc::new(Mutex::new(initial)),
            running: Arc::new(Mutex::new(initial != ServiceStartType::Disabled)),
            fail_query: false,
        }
    }
}

impl ServiceController for MockService {
    fn query_start_type(&self, _s: &str) -> Result<Option<ServiceStartType>, ApplyError> {
        if self.fail_query {
            return Err(ApplyError::ServiceOp(
                "tzautoupdate".into(),
                std::io::Error::other("query failed"),
            ));
        }
        self.log.push("query_start_type");
        Ok(Some(*self.start_type.lock().unwrap()))
    }
    fn set_start_type(&self, _s: &str, start: ServiceStartType) -> Result<(), ApplyError> {
        self.log.push("set_start_type");
        *self.start_type.lock().unwrap() = start;
        if start == ServiceStartType::Disabled {
            *self.running.lock().unwrap() = false;
        }
        Ok(())
    }
    fn stop(&self, _s: &str) -> Result<(), ApplyError> {
        self.log.push("stop");
        *self.running.lock().unwrap() = false;
        Ok(())
    }
    fn start(&self, _s: &str) -> Result<(), ApplyError> {
        self.log.push("start");
        *self.running.lock().unwrap() = true;
        Ok(())
    }
}

#[derive(Clone)]
struct MockSetter {
    log: Log,
    current: Arc<Mutex<TimezoneValue>>,
    fail_set: bool,
}

impl MockSetter {
    fn new(current: TimezoneValue) -> Self {
        MockSetter {
            log: Log::default(),
            current: Arc::new(Mutex::new(current)),
            fail_set: false,
        }
    }
}

impl WinTzSetter for MockSetter {
    fn set_dynamic(&self, value: &TimezoneValue, _d: &ZoneData) -> Result<(), ApplyError> {
        self.log.push("set_dynamic");
        if self.fail_set {
            return Err(ApplyError::SetTzFailed(std::io::Error::other("set failed")));
        }
        *self.current.lock().unwrap() = value.clone();
        Ok(())
    }
    fn read_current(&self) -> Result<TimezoneValue, ApplyError> {
        Ok(self.current.lock().unwrap().clone())
    }
}

#[derive(Clone, Default)]
struct MockAdmin {
    admin: bool,
}

impl AdminCheck for MockAdmin {
    fn is_admin(&self) -> bool {
        self.admin
    }
}

fn zone(key: &str) -> (String, ZoneData) {
    (
        key.to_string(),
        ZoneData {
            names: Default::default(),
            rules: TzRules {
                bias_minutes: -180,
                ..Default::default()
            },
        },
    )
}

fn tz(key: &str, dst: bool) -> TimezoneValue {
    TimezoneValue::new(key, dst)
}

fn tokyo_value() -> TimezoneValue {
    tz("Tokyo Standard Time", false)
}

// ---- 1. Apply happy path: order of calls ----

#[tokio::test]
async fn apply_happy_path_call_order() {
    let mut applier = TimezoneApplier::new(
        MockRegistry::with_zones(&[zone("Tokyo Standard Time")]),
        MockService::new(ServiceStartType::AutoStart),
        MockSetter::new(tz("Russian Standard Time", false)),
        MockAdmin { admin: true },
    );
    let change = applier.apply(&tokyo_value()).await.unwrap();
    assert!(!change.is_noop());
    let AppliedChange::Changed {
        new_key_name,
        old_key_name,
        restart_hint,
        ..
    } = &change
    else {
        panic!("expected Changed");
    };
    assert_eq!(new_key_name, "Tokyo Standard Time");
    assert_eq!(old_key_name, "Russian Standard Time");
    assert_eq!(restart_hint, &vec!["Chrome", "Firefox"]);
    // The applier itself must not record service capture (variant A):
    // capture happens through capture_service_state(), here we only check
    // the mutating sequence.
    let service_log = applier.service.log.entries();
    assert_eq!(
        service_log,
        vec!["query_start_type", "set_start_type", "stop"],
        "service: capture state -> disable -> stop"
    );
    let setter_log = applier.setter.log.entries();
    assert_eq!(
        setter_log,
        vec!["set_dynamic"],
        "registry read via registry, then SetTz"
    );
    let registry_log = applier.registry.log.entries();
    assert_eq!(registry_log, vec!["read_zone"]);
    assert!(!*applier.service.running.lock().unwrap());
    assert_eq!(
        *applier.service.start_type.lock().unwrap(),
        ServiceStartType::Disabled
    );
}

// ---- 2. Apply: new == current -> Noop, nothing called ----

#[tokio::test]
async fn apply_noop_when_equal() {
    let mut applier = TimezoneApplier::new(
        MockRegistry::with_zones(&[]),
        MockService::new(ServiceStartType::AutoStart),
        MockSetter::new(tokyo_value()),
        MockAdmin { admin: true },
    );
    let change = applier.apply(&tokyo_value()).await.unwrap();
    assert!(change.is_noop());
    assert_eq!(applier.service.log.entries(), Vec::<String>::new());
    assert_eq!(applier.setter.log.entries(), Vec::<String>::new());
    assert_eq!(applier.registry.log.entries(), Vec::<String>::new());
}

// ---- 3. Apply: not admin -> error before any call ----

#[tokio::test]
async fn apply_rejects_non_admin() {
    let mut applier = TimezoneApplier::new(
        MockRegistry::with_zones(&[]),
        MockService::new(ServiceStartType::AutoStart),
        MockSetter::new(tz("Russian Standard Time", false)),
        MockAdmin { admin: false },
    );
    let err = applier.apply(&tokyo_value()).await.unwrap_err();
    assert!(matches!(err, ApplyError::InsufficientPrivileges));
    assert_eq!(applier.service.log.entries(), Vec::<String>::new());
    assert_eq!(applier.setter.log.entries(), Vec::<String>::new());
}

// ---- 4. Apply: zone absent -> UnknownZone, service untouched ----

#[tokio::test]
async fn apply_unknown_zone_service_untouched() {
    let mut applier = TimezoneApplier::new(
        MockRegistry::with_zones(&[]),
        MockService::new(ServiceStartType::AutoStart),
        MockSetter::new(tz("Russian Standard Time", false)),
        MockAdmin { admin: true },
    );
    let err = applier.apply(&tokyo_value()).await.unwrap_err();
    assert!(matches!(err, ApplyError::UnknownZone(k) if k == "Tokyo Standard Time"));
    // The registry was read, but the service stayed in AutoStart.
    assert_eq!(
        *applier.service.start_type.lock().unwrap(),
        ServiceStartType::AutoStart
    );
}

// ---- 5. Apply: SetTz fails -> service rolled back automatically ----

#[tokio::test]
async fn apply_settz_failure_rolls_back_service() {
    let mut applier = TimezoneApplier::new(
        MockRegistry::with_zones(&[zone("Tokyo Standard Time")]),
        MockService::new(ServiceStartType::AutoStart),
        MockSetter::new(tz("Russian Standard Time", false)),
        MockAdmin { admin: true },
    );
    applier.setter.fail_set = true;
    let err = applier.apply(&tokyo_value()).await.unwrap_err();
    assert!(matches!(err, ApplyError::SetTzFailed(_)));
    // Service restored to AutoStart + running.
    assert_eq!(
        *applier.service.start_type.lock().unwrap(),
        ServiceStartType::AutoStart
    );
    assert!(*applier.service.running.lock().unwrap());
    let service_log = applier.service.log.entries();
    // disable -> stop -> (rollback) set_start_type -> start
    assert_eq!(
        service_log,
        vec![
            "query_start_type",
            "set_start_type",
            "stop",
            "set_start_type",
            "start"
        ]
    );
}

// ---- 6. Revert happy path: SetTz -> restore service ----

#[tokio::test]
async fn revert_happy_path_order() {
    let original = tz("Russian Standard Time", false);
    let mut original = original;
    original.service_state = Some(TzAutoUpdateState::Enabled);
    let mut applier = TimezoneApplier::new(
        MockRegistry::with_zones(&[zone("Russian Standard Time")]),
        MockService::new(ServiceStartType::Disabled),
        MockSetter::new(tokyo_value()),
        MockAdmin { admin: true },
    );
    applier.revert(&original).await.unwrap();
    let setter_log = applier.setter.log.entries();
    assert_eq!(setter_log, vec!["set_dynamic"], "timezone restored first");
    let service_log = applier.service.log.entries();
    assert_eq!(
        service_log,
        vec!["set_start_type", "start"],
        "then AutoStart + start"
    );
    assert_eq!(
        *applier.service.start_type.lock().unwrap(),
        ServiceStartType::AutoStart
    );
    assert!(*applier.service.running.lock().unwrap());
}

// ---- 7. Revert: not admin -> no-op without error ----

#[tokio::test]
async fn revert_non_admin_refuses_and_reports() {
    let original = tz("Russian Standard Time", false);
    let mut applier = TimezoneApplier::new(
        MockRegistry::with_zones(&[]),
        MockService::new(ServiceStartType::Disabled),
        MockSetter::new(tokyo_value()),
        MockAdmin { admin: false },
    );
    // Refuse with an error (so the orchestrator keeps the snapshot for an
    // elevated retry) and touch nothing.
    let err = applier.revert(&original).await.unwrap_err();
    assert!(matches!(err, ApplyError::InsufficientPrivileges));
    assert_eq!(applier.service.log.entries(), Vec::<String>::new());
    assert_eq!(applier.setter.log.entries(), Vec::<String>::new());
    assert_eq!(applier.registry.log.entries(), Vec::<String>::new());
}

// ---- Variant A: capture_service_state ----

#[tokio::test]
async fn capture_service_state_maps_start_types() {
    let applier = TimezoneApplier::new(
        MockRegistry::with_zones(&[]),
        MockService::new(ServiceStartType::AutoStart),
        MockSetter::new(tz("Russian Standard Time", false)),
        MockAdmin { admin: true },
    );
    assert_eq!(
        applier.capture_service_state().unwrap(),
        Some(TzAutoUpdateState::Enabled)
    );
    let applier = TimezoneApplier::new(
        MockRegistry::with_zones(&[]),
        MockService::new(ServiceStartType::Disabled),
        MockSetter::new(tz("Russian Standard Time", false)),
        MockAdmin { admin: true },
    );
    assert_eq!(
        applier.capture_service_state().unwrap(),
        Some(TzAutoUpdateState::Disabled)
    );
}

// ---- Revert with service_state = None: do not touch the service ----

#[tokio::test]
async fn revert_with_none_service_state_skips_service() {
    let original = tz("Russian Standard Time", false);
    assert!(original.service_state.is_none());
    let mut applier = TimezoneApplier::new(
        MockRegistry::with_zones(&[zone("Russian Standard Time")]),
        MockService::new(ServiceStartType::Disabled),
        MockSetter::new(tokyo_value()),
        MockAdmin { admin: true },
    );
    applier.revert(&original).await.unwrap();
    // Only the timezone was restored; the service stays Disabled.
    assert_eq!(applier.service.log.entries(), Vec::<String>::new());
    assert_eq!(
        *applier.service.start_type.lock().unwrap(),
        ServiceStartType::Disabled
    );
}
