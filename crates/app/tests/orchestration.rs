//! Offline orchestration tests (stage 6 spec, scenarios 1-10). All
//! dependencies are mocked; the snapshot Engine runs against a temp dir
//! under target/test-tmp (the DSH test host does not allow %TEMP%).

#![allow(clippy::unwrap_used)] // tests: unwrap on controlled fixtures is fine

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use app::orchestrator::{Orchestration, StartupOutcome, Status, TrayCommand};
use app::shutdown::ShutdownReason;
use apply::error::{AppliedChange, ApplyError};
use apply::{
    AdminCheck, LocaleApplier, LocaleRegistry, LocaleValue, ServiceController, ServiceStartType,
    SettingChangeBroadcast, TimezoneApplier, TimezoneValue, TzRegistry, TzRules, WinTzSetter,
    ZoneData,
};
use geo::{GeoError, GeoInfo, GeoProvider};
use snapshot::{Clock, Engine, FieldId, FieldValue, ProcessLiveness, StateReader, Store};

use shared::IpEvent;

// ---------- mocks ----------

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

type GeoResult = Result<GeoInfo, GeoError>;

/// Scripted geo provider: returns the scripted results in order, repeating
/// the last one once the script is exhausted (models retry scenarios).
#[derive(Clone)]
struct MockGeo {
    log: Log,
    results: Arc<Mutex<Vec<GeoResult>>>,
    idx: Arc<AtomicUsize>,
    /// Simulated lookup latency (ms) — lets tests observe the D5 shutdown
    /// abort while a lookup is in flight.
    delay_ms: Arc<AtomicU64>,
}

impl MockGeo {
    fn scripted(results: Vec<GeoResult>) -> Self {
        assert!(!results.is_empty(), "geo script must not be empty");
        MockGeo {
            log: Log::default(),
            results: Arc::new(Mutex::new(results)),
            idx: Arc::new(AtomicUsize::new(0)),
            delay_ms: Arc::new(AtomicU64::new(0)),
        }
    }

    fn set_delay_ms(&self, ms: u64) {
        self.delay_ms.store(ms, Ordering::SeqCst);
    }

    fn lookups(&self) -> usize {
        self.log
            .entries()
            .iter()
            .filter(|e| e.as_str() == "lookup")
            .count()
    }
}

#[async_trait::async_trait]
impl GeoProvider for MockGeo {
    async fn lookup(&self, _ip: IpAddr) -> GeoResult {
        self.log.push("lookup");
        // Async sleep: a synchronous one would block the current-thread
        // runtime and freeze the very timer the D5 test races against.
        let delay = self.delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
        }
        let results = self.results.lock().unwrap();
        let i = self
            .idx
            .fetch_add(1, Ordering::SeqCst)
            .min(results.len() - 1);
        results[i].clone()
    }
}

struct MockRegistry {
    log: Log,
    zones: Arc<Mutex<std::collections::HashMap<String, ZoneData>>>,
}

impl MockRegistry {
    fn with_zones(zones: &[(&str, ZoneData)]) -> Self {
        let map = zones
            .iter()
            .map(|(k, v)| (k.to_string(), v.clone()))
            .collect();
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
}

impl ServiceController for MockService {
    fn query_start_type(&self, _s: &str) -> Result<Option<ServiceStartType>, ApplyError> {
        self.log.push("query_start_type");
        Ok(Some(*self.start_type.lock().unwrap()))
    }
    fn set_start_type(&self, _s: &str, start: ServiceStartType) -> Result<(), ApplyError> {
        self.log.push("set_start_type");
        *self.start_type.lock().unwrap() = start;
        Ok(())
    }
    fn stop(&self, _s: &str) -> Result<(), ApplyError> {
        self.log.push("stop");
        Ok(())
    }
    fn start(&self, _s: &str) -> Result<(), ApplyError> {
        self.log.push("start");
        Ok(())
    }
}

#[derive(Clone)]
struct MockSetter {
    log: Log,
    current: Arc<Mutex<TimezoneValue>>,
    /// Simulated stall (ms) of the Win32 setter call; lets tests observe the
    /// restore watchdog (H6) on a slow revert.
    delay_ms: Arc<AtomicU64>,
    /// Fired right after set_dynamic mutates the system, before the
    /// orchestrator records it — a test breaks the snapshot store here to
    /// reach the H2 record-failure rollback.
    after_set: Hook,
}

impl MockSetter {
    fn set_delay_ms(&self, ms: u64) {
        self.delay_ms.store(ms, Ordering::SeqCst);
    }

    fn set_after_set(&self, f: Option<Box<dyn FnMut() + Send>>) {
        *self.after_set.lock().unwrap() = f;
    }
}

impl WinTzSetter for MockSetter {
    fn set_dynamic(&self, value: &TimezoneValue, _d: &ZoneData) -> Result<(), ApplyError> {
        self.log.push("set_dynamic");
        let delay = self.delay_ms.load(Ordering::SeqCst);
        if delay > 0 {
            std::thread::sleep(std::time::Duration::from_millis(delay));
        }
        *self.current.lock().unwrap() = value.clone();
        if let Some(cb) = self.after_set.lock().unwrap().as_mut() {
            cb();
        }
        Ok(())
    }
    fn read_current(&self) -> Result<TimezoneValue, ApplyError> {
        Ok(self.current.lock().unwrap().clone())
    }
}

#[derive(Clone)]
struct MockAdmin {
    admin: bool,
}

impl AdminCheck for MockAdmin {
    fn is_admin(&self) -> bool {
        self.admin
    }
}

#[derive(Clone)]
struct MockLocaleRegistry {
    log: Log,
    state: Arc<Mutex<LocaleValue>>,
    /// When set, every registry WRITE fails (reads stay truthful) — injects
    /// locale apply failures for the H10 rollback tests.
    fail_writes: Arc<AtomicBool>,
    /// Fired after a successful write, before the orchestrator records the
    /// change — a test breaks the snapshot store here to reach the H2
    /// locale record-failure rollback.
    after_write: Hook,
}

impl MockLocaleRegistry {
    fn fail_writes(&self, on: bool) {
        self.fail_writes.store(on, Ordering::SeqCst);
    }

    fn set_after_write(&self, f: Option<Box<dyn FnMut() + Send>>) {
        *self.after_write.lock().unwrap() = f;
    }

    fn fire_after_write(&self) {
        if let Some(cb) = self.after_write.lock().unwrap().as_mut() {
            cb();
        }
    }
}

impl LocaleRegistry for MockLocaleRegistry {
    fn read_locale_name(&self) -> Result<String, ApplyError> {
        self.log.push("read_locale_name");
        Ok(self.state.lock().unwrap().locale_name.clone())
    }
    fn write_locale_name(&self, value: &str) -> Result<(), ApplyError> {
        self.log.push("write_locale_name");
        if self.fail_writes.load(Ordering::SeqCst) {
            return Err(ApplyError::ServiceOp(
                "write_locale_name".into(),
                std::io::Error::other("injected write failure"),
            ));
        }
        self.state.lock().unwrap().locale_name = value.to_string();
        self.fire_after_write();
        Ok(())
    }
    fn read_geo_nation(&self) -> Result<String, ApplyError> {
        self.log.push("read_geo_nation");
        Ok(self.state.lock().unwrap().geo_nation.clone())
    }
    fn write_geo_nation(&self, value: &str) -> Result<(), ApplyError> {
        self.log.push("write_geo_nation");
        if self.fail_writes.load(Ordering::SeqCst) {
            return Err(ApplyError::ServiceOp(
                "write_geo_nation".into(),
                std::io::Error::other("injected write failure"),
            ));
        }
        self.state.lock().unwrap().geo_nation = value.to_string();
        self.fire_after_write();
        Ok(())
    }
    fn read_kbd_preload(&self) -> Result<Vec<String>, ApplyError> {
        self.log.push("read_kbd_preload");
        Ok(self.state.lock().unwrap().kbd_preload.clone())
    }
    fn write_kbd_preload(&self, values: &[String]) -> Result<(), ApplyError> {
        self.log.push("write_kbd_preload");
        if self.fail_writes.load(Ordering::SeqCst) {
            return Err(ApplyError::ServiceOp(
                "write_kbd_preload".into(),
                std::io::Error::other("injected write failure"),
            ));
        }
        self.state.lock().unwrap().kbd_preload = values.to_vec();
        self.fire_after_write();
        Ok(())
    }
}

#[derive(Clone, Default)]
struct MockBroadcast {
    log: Log,
}

impl SettingChangeBroadcast for MockBroadcast {
    fn broadcast_locale(&self) -> Result<(), ApplyError> {
        self.log.push("broadcast_locale");
        Ok(())
    }
    fn broadcast_keyboard(&self) -> Result<(), ApplyError> {
        self.log.push("broadcast_keyboard");
        Ok(())
    }
}

#[derive(Clone)]
struct MockReader(Arc<Mutex<Vec<(FieldId, FieldValue)>>>);

impl MockReader {
    fn set_field(&self, id: FieldId, value: FieldValue) {
        let mut vals = self.0.lock().unwrap();
        for (vid, v) in vals.iter_mut() {
            if *vid == id {
                *v = value;
                return;
            }
        }
        vals.push((id, value));
    }
}

impl StateReader for MockReader {
    fn read_all(&self) -> std::io::Result<Vec<(FieldId, FieldValue)>> {
        Ok(self.0.lock().unwrap().clone())
    }
}

struct MockClock(AtomicU64);

impl Clock for MockClock {
    fn now_unix(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct MockLiveness {
    alive: bool,
}

impl ProcessLiveness for MockLiveness {
    fn is_alive(&self, _pid: u32) -> bool {
        self.alive
    }
}

// ---------- fixtures ----------

const NATIVE_TZ: &str = "Russian Standard Time";
const FR_TZ: &str = "Romance Standard Time";
const DE_TZ: &str = "W. Europe Standard Time";
const KGD_TZ: &str = "Kaliningrad Standard Time";

fn zone(key: &str) -> (&'static str, ZoneData) {
    (
        Box::leak(key.to_string().into_boxed_str()),
        ZoneData {
            names: Default::default(),
            rules: TzRules {
                bias_minutes: -60,
                ..Default::default()
            },
        },
    )
}

fn geo_fr() -> GeoInfo {
    GeoInfo {
        country_code: "FR".into(),
        timezone: "Europe/Paris".into(),
        languages: vec!["fr-FR".into()],
    }
}

fn geo_ru() -> GeoInfo {
    GeoInfo {
        country_code: "RU".into(),
        timezone: "Europe/Moscow".into(),
        languages: vec!["ru-RU".into()],
    }
}

fn geo_de() -> GeoInfo {
    GeoInfo {
        country_code: "DE".into(),
        timezone: "Europe/Berlin".into(),
        languages: vec!["de-DE".into()],
    }
}

fn temp_store(name: &str) -> Store {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-tmp")
        .join(name);
    let _ = std::fs::remove_dir_all(&dir);
    Store::new(dir)
}

/// The home network IP (TEST-NET-1). Most tests model the real flow: the
/// app starts at home, then the user connects the VPN.
/// Shared mutability + late binding for test hooks (set_dynamic / write_*).
type Hook = Arc<Mutex<Option<Box<dyn FnMut() + Send>>>>;
const HOME_IP: IpAddr = IpAddr::V4(Ipv4Addr::new(192, 0, 2, 1));

/// Real-flow preamble (C1): the app observed the home network while nothing
/// was applied — the IP is recorded as native, the status stays Native.
/// The geo script must have the home answer (geo_ru) as its first entry.
async fn fixture_home_first(name: &str, mut rest: Vec<GeoResult>) -> Fixture {
    let mut results = vec![Ok(geo_ru())];
    results.append(&mut rest);
    let mut f = fixture(name, results);
    f.orch.apply_for_ip(HOME_IP).await;
    f
}

/// Native state of the machine as the mock reader reports it.
fn native_reader() -> MockReader {
    reader_with_tz(NATIVE_TZ)
}

/// Reader reporting `tz` as the current timezone (C1 p07 models a system
/// whose native zone differs from what the geo database reports).
fn reader_with_tz(tz: &str) -> MockReader {
    MockReader(Arc::new(Mutex::new(vec![
        (
            FieldId::Timezone,
            FieldValue::Timezone {
                key_name: tz.into(),
                dst_disabled: false,
                service_state: None,
            },
        ),
        (FieldId::LocaleName, FieldValue::LocaleName("ru-RU".into())),
        (FieldId::GeoNation, FieldValue::GeoNation("203".into())),
        (
            FieldId::KbdPreload,
            FieldValue::KbdPreload(vec!["00000419".into()]),
        ),
    ])))
}

struct Fixture {
    orch: Orchestration<
        MockGeo,
        MockReader,
        MockRegistry,
        MockService,
        MockSetter,
        MockAdmin,
        MockLocaleRegistry,
        MockBroadcast,
    >,
    reader: MockReader,
    tz_service: MockService,
    tz_setter: MockSetter,
    locale_registry: MockLocaleRegistry,
    engine: Engine,
    clock: Arc<MockClock>,
    geo: MockGeo,
}

fn fixture(name: &str, results: Vec<GeoResult>) -> Fixture {
    fixture_custom(name, results, None, true)
}

fn fixture_custom(
    name: &str,
    results: Vec<GeoResult>,
    store: Option<Store>,
    admin: bool,
) -> Fixture {
    fixture_full(
        name,
        results,
        store,
        admin,
        native_reader(),
        vec![zone(NATIVE_TZ), zone(FR_TZ), zone(DE_TZ)],
        NATIVE_TZ,
    )
}

/// Generalized fixture: custom reader state, registry zones and setter
/// current zone (C1 p07 models a machine whose native zone differs from
/// the geo answer).
fn fixture_full(
    name: &str,
    results: Vec<GeoResult>,
    store: Option<Store>,
    admin: bool,
    reader: MockReader,
    zones: Vec<(&str, ZoneData)>,
    setter_current: &str,
) -> Fixture {
    let store = store.unwrap_or_else(|| temp_store(name));
    let engine = Engine::new(store);
    let clock = Arc::new(MockClock(AtomicU64::new(1_700_000_000)));
    let geo = MockGeo::scripted(results);
    let tz_registry = MockRegistry::with_zones(&zones);
    let tz_service = MockService {
        log: Log::default(),
        start_type: Arc::new(Mutex::new(ServiceStartType::AutoStart)),
    };
    let tz_setter = MockSetter {
        log: Log::default(),
        current: Arc::new(Mutex::new(TimezoneValue::new(setter_current, false))),
        delay_ms: Arc::new(AtomicU64::new(0)),
        after_set: Arc::new(Mutex::new(None)),
    };
    let admin = MockAdmin { admin };
    let locale_registry = MockLocaleRegistry {
        log: Log::default(),
        state: Arc::new(Mutex::new(LocaleValue {
            locale_name: "ru-RU".into(),
            geo_nation: "203".into(),
            kbd_preload: vec!["00000419".into()],
        })),
        fail_writes: Arc::new(AtomicBool::new(false)),
        after_write: Arc::new(Mutex::new(None)),
    };
    let locale_broadcast = MockBroadcast::default();

    let tz = TimezoneApplier::new(tz_registry, tz_service.clone(), tz_setter.clone(), admin);
    let locale = LocaleApplier::new(locale_registry.clone(), locale_broadcast.clone());
    let orch = Orchestration::new(
        Arc::new(geo.clone()),
        tz,
        locale,
        engine.clone(),
        reader.clone(),
        clock.clone(),
    );

    Fixture {
        orch,
        reader,
        tz_service,
        tz_setter,
        locale_registry,
        engine,
        clock,
        geo,
    }
}

fn snapshot_applied(engine: &Engine) -> Option<snapshot::Snapshot> {
    engine.store().load_snapshot().unwrap()
}

/// Simulate that the system actually holds the VPN values (the real
/// reader reads live state; here we mirror it onto the mock).
fn simulate_applied_system(f: &Fixture) {
    f.reader.set_field(
        FieldId::Timezone,
        FieldValue::Timezone {
            key_name: FR_TZ.into(),
            dst_disabled: false,
            service_state: None,
        },
    );
    f.reader
        .set_field(FieldId::LocaleName, FieldValue::LocaleName("fr-FR".into()));
    f.reader
        .set_field(FieldId::GeoNation, FieldValue::GeoNation("84".into()));
}

/// Mirror the restored native state back onto the mock reader (in reality
/// the live reader follows the restored registry automatically).
fn simulate_native_system(f: &Fixture) {
    f.reader.set_field(
        FieldId::Timezone,
        FieldValue::Timezone {
            key_name: NATIVE_TZ.into(),
            dst_disabled: false,
            service_state: None,
        },
    );
    f.reader
        .set_field(FieldId::LocaleName, FieldValue::LocaleName("ru-RU".into()));
    f.reader
        .set_field(FieldId::GeoNation, FieldValue::GeoNation("203".into()));
}

// ---------- 1. Connected: geo -> save -> record -> tz -> locale ----------

#[tokio::test]
async fn connected_applies_timezone_and_locale() {
    let mut f = fixture_home_first("connected", vec![Ok(geo_fr())]).await;
    let outcome = f
        .orch
        .startup(&MockLiveness { alive: false }, 60, 4242)
        .await;
    assert_eq!(outcome, StartupOutcome::Clean);

    // Snapshot saved with native originals.
    let snap = snapshot_applied(&f.engine).unwrap();
    let FieldValue::Timezone { key_name, .. } = &snap.fields.timezone.original else {
        panic!("tz original");
    };
    assert_eq!(key_name, NATIVE_TZ);

    f.orch
        .apply_for_ip(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)))
        .await;

    // TZ applied via the applier chain.
    assert_eq!(f.tz_setter.log.entries(), vec!["set_dynamic"]);
    let setter_current = f.tz_setter.current.lock().unwrap();
    assert_eq!(setter_current.key_name, FR_TZ);
    drop(setter_current);

    // Service captured pre-apply state, then disabled: query (capture) ->
    // query (apply idempotence check) -> set -> stop.
    assert_eq!(
        f.tz_service.log.entries(),
        vec![
            "query_start_type",
            "query_start_type",
            "set_start_type",
            "stop"
        ]
    );

    // Locale fields written (kbd_preload untouched).
    let locale_state = f.locale_registry.state.lock().unwrap();
    assert_eq!(locale_state.locale_name, "fr-FR");
    assert_eq!(locale_state.geo_nation, "84"); // FR Windows GeoId
    assert_eq!(locale_state.kbd_preload, vec!["00000419".to_string()]);
    drop(locale_state);

    // Applied records written for both groups.
    let snap = snapshot_applied(&f.engine).unwrap();
    assert!(snap.fields.timezone.applied.is_some());
    assert!(snap.fields.locale_name.applied.is_some());
    assert!(snap.fields.geo_nation.applied.is_some());

    // Tray status shows the connected region.
    assert_eq!(
        *f.orch.status_handle().lock().unwrap(),
        Status::Connected {
            country: "FR".into()
        }
    );
}

// ---------- 1b. C1: Connected applies; native region restores; reconnect re-applies ----------

/// C1 check 1 (D3): Connected(vpn) as the FIRST event of a run is recorded
/// as the native network and nothing is applied — the HEAD heuristic
/// restored by the review. Known tradeoff: a machine booted with the VPN
/// already up stays native until the network changes.
#[tokio::test]
async fn connected_vpn_as_first_event_is_recorded_native() {
    let mut f = fixture("c1-first-connected", vec![Ok(geo_fr())]);
    f.orch
        .startup(&MockLiveness { alive: false }, 60, 4242)
        .await;

    let vpn = IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4));
    f.orch.handle_ip_event(IpEvent::Connected(vpn)).await;

    assert_eq!(*f.orch.status_handle().lock().unwrap(), Status::Native);
    assert!(f.tz_setter.log.entries().is_empty());
    assert!(f.locale_registry.log.entries().is_empty());
    let snap = snapshot_applied(&f.engine).unwrap();
    assert!(snap.fields.timezone.applied.is_none());
    assert_eq!(snap.native_ips, vec![vpn.to_string()]);
}

/// C1 check 2: after Disconnected (kill-switch), the watcher replays
/// Connected(vpn) for the same IP — the region must be applied again.
#[tokio::test]
async fn reconnect_after_disconnect_applies_region() {
    let mut f = fixture_home_first(
        "c1-reconnect",
        vec![Ok(geo_fr()), Ok(geo_ru()), Ok(geo_fr())],
    )
    .await;
    f.orch
        .startup(&MockLiveness { alive: false }, 60, 4242)
        .await;

    let vpn = IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4));
    f.orch.handle_ip_event(IpEvent::Connected(vpn)).await;
    simulate_applied_system(&f);
    assert_eq!(f.tz_setter.current.lock().unwrap().key_name, FR_TZ);

    // Kill-switch: every connection drops.
    f.orch.handle_ip_event(IpEvent::Disconnected).await;
    assert_eq!(f.tz_setter.current.lock().unwrap().key_name, NATIVE_TZ);
    assert_eq!(*f.orch.status_handle().lock().unwrap(), Status::Native);
    simulate_native_system(&f);

    // The watcher's `current` was cleared, so the same VPN IP comes back as
    // Connected — it must be applied again. Log: apply (FR), revert to
    // native during Disconnected, apply (FR) again.
    //
    // First the home network is observed again (the restore cleared the
    // snapshot, so the next observation re-records the native IP)...
    f.orch.handle_ip_event(IpEvent::Connected(HOME_IP)).await;
    assert_eq!(*f.orch.status_handle().lock().unwrap(), Status::Native);
    // ...then the same VPN IP returns and must be applied.
    f.orch.handle_ip_event(IpEvent::Connected(vpn)).await;
    assert_eq!(
        f.tz_setter.log.entries(),
        vec!["set_dynamic", "set_dynamic", "set_dynamic"]
    );
    assert_eq!(f.tz_setter.current.lock().unwrap().key_name, FR_TZ);
    assert_eq!(
        *f.orch.status_handle().lock().unwrap(),
        Status::Connected {
            country: "FR".into()
        }
    );
}

/// C1 native detection by geo: an IP whose geo matches the snapshot
/// originals restores them instead of applying native country defaults.
#[tokio::test]
async fn return_to_native_region_restores_originals() {
    let mut f = fixture_home_first("c1-native-restore", vec![Ok(geo_fr()), Ok(geo_ru())]).await;
    f.orch
        .startup(&MockLiveness { alive: false }, 60, 4242)
        .await;

    let vpn = IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4));
    let native = IpAddr::V4(Ipv4Addr::new(9, 9, 9, 9));

    f.orch.handle_ip_event(IpEvent::Connected(vpn)).await;
    simulate_applied_system(&f);
    assert_eq!(f.tz_setter.current.lock().unwrap().key_name, FR_TZ);

    // VPN off: the real IP is in the native region (Europe/Moscow ->
    // NATIVE_TZ == snapshot original). Restore, don't re-apply RU defaults.
    f.orch.handle_ip_event(IpEvent::Changed(vpn, native)).await;
    assert_eq!(f.tz_setter.current.lock().unwrap().key_name, NATIVE_TZ);
    let locale_state = f.locale_registry.state.lock().unwrap();
    assert_eq!(locale_state.locale_name, "ru-RU");
    assert_eq!(locale_state.geo_nation, "203");
    drop(locale_state);
    assert_eq!(*f.orch.status_handle().lock().unwrap(), Status::Native);
    simulate_native_system(&f);

    // Re-armed snapshot holds no applied records; originals are native.
    let snap = snapshot_applied(&f.engine).expect("re-armed snapshot present");
    assert!(snap.fields.timezone.applied.is_none());
    assert!(snap.fields.locale_name.applied.is_none());
}

// ---------- 2. Disconnected: restore -> complete_restore ----------

#[tokio::test]
async fn disconnected_restores_and_clears_snapshot() {
    let mut f = fixture_home_first("disconnected", vec![Ok(geo_fr())]).await;
    f.orch
        .startup(&MockLiveness { alive: false }, 60, 4242)
        .await;
    f.orch
        .apply_for_ip(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)))
        .await;
    simulate_applied_system(&f);
    assert!(snapshot_applied(&f.engine).is_some());

    f.orch.restore_to_native().await;

    // Snapshot removed (complete_restore).
    assert!(snapshot_applied(&f.engine).is_none());
    // Native values are back.
    assert_eq!(f.tz_setter.current.lock().unwrap().key_name, NATIVE_TZ);
    let locale_state = f.locale_registry.state.lock().unwrap();
    assert_eq!(locale_state.locale_name, "ru-RU");
    assert_eq!(locale_state.geo_nation, "203");
    drop(locale_state);
    // Service restored from the applied record's pre-apply state.
    assert_eq!(
        *f.tz_service.start_type.lock().unwrap(),
        ServiceStartType::AutoStart
    );
    assert_eq!(*f.orch.status_handle().lock().unwrap(), Status::Native);
}

// ---------- 3. geo failure: error status, retry armed, nothing applied ----------

#[tokio::test]
async fn geo_failure_applies_nothing() {
    let mut f = fixture(
        "geo-fail",
        vec![Err(GeoError::Provider("rate limited".into()))],
    );
    f.orch
        .startup(&MockLiveness { alive: false }, 60, 4242)
        .await;
    f.orch
        .apply_for_ip(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)))
        .await;

    assert!(f.tz_setter.log.entries().is_empty());
    assert!(f.locale_registry.log.entries().is_empty());
    // No applied records.
    let snap = snapshot_applied(&f.engine).unwrap();
    assert!(snap.fields.timezone.applied.is_none());
    assert!(snap.fields.locale_name.applied.is_none());
    // H9: the failure must be VISIBLE (red icon), not the stale native
    // status, and a retry must be armed.
    assert!(matches!(
        *f.orch.status_handle().lock().unwrap(),
        Status::Error(_)
    ));
    assert_eq!(f.geo.lookups(), 1);
    // Backoff not elapsed yet -> no extra lookup, status stays Error.
    f.orch.poll_geo_retry().await;
    assert_eq!(f.geo.lookups(), 1);
    assert!(matches!(
        *f.orch.status_handle().lock().unwrap(),
        Status::Error(_)
    ));
}

// ---------- 4. unknown TZ: TZ skipped, locale applied ----------

#[tokio::test]
async fn unknown_timezone_locale_still_applied() {
    let mut info = geo_fr();
    info.timezone = "Mars/Olympus_Mons".into();
    let mut f = fixture("tz-unknown", vec![Ok(geo_ru()), Ok(info)]);
    f.orch
        .startup(&MockLiveness { alive: false }, 60, 4242)
        .await;
    // Home observation first (C1), then the VPN event with the unknown TZ.
    f.orch.apply_for_ip(HOME_IP).await;
    f.orch
        .apply_for_ip(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)))
        .await;

    // TZ untouched (no read_zone, no set_dynamic).
    assert!(f.tz_setter.log.entries().is_empty());
    // Locale applied.
    assert_eq!(f.locale_registry.state.lock().unwrap().locale_name, "fr-FR");
    let snap = snapshot_applied(&f.engine).unwrap();
    assert!(snap.fields.timezone.applied.is_none());
    assert!(snap.fields.locale_name.applied.is_some());
}

// ---------- 5. startup: RestoreNeeded restores BEFORE the watcher ----------

#[tokio::test]
async fn startup_restores_before_watcher() {
    let mut f = fixture("startup-restore", vec![Ok(geo_fr())]);

    // Simulate a crashed run: snapshot + applied TZ record + stale lock.
    let now = f.clock.now_unix();
    f.engine
        .save(&native_reader(), "crashed-run".into())
        .unwrap();
    f.engine
        .record_applied(
            FieldId::Timezone,
            FieldValue::Timezone {
                key_name: FR_TZ.into(),
                dst_disabled: false,
                service_state: Some(snapshot::TzServiceState::Enabled),
            },
        )
        .unwrap();
    f.engine.store().write_lock(9999, now).unwrap();

    // The machine currently holds the VPN values (== applied), so the plan
    // must include the TZ item.
    f.tz_setter.current.lock().unwrap().key_name = FR_TZ.into();
    f.reader.set_field(
        FieldId::Timezone,
        FieldValue::Timezone {
            key_name: FR_TZ.into(),
            dst_disabled: false,
            service_state: None,
        },
    );

    let outcome = f
        .orch
        .startup(&MockLiveness { alive: false }, 60, 4242)
        .await;
    assert_eq!(outcome, StartupOutcome::Restored);

    // Restored BEFORE any watcher event: native TZ back; the failed-run
    // applied record is gone (fresh save), and the run is re-armed:
    // snapshot + lock exist so later applies are tracked again.
    assert_eq!(f.tz_setter.current.lock().unwrap().key_name, NATIVE_TZ);
    let snap = snapshot_applied(&f.engine).expect("re-armed fresh snapshot after restore");
    assert!(snap.fields.timezone.applied.is_none());
    assert!(snap.fields.locale_name.applied.is_none());
    assert!(f.engine.store().read_lock().unwrap().is_some());
    // Service re-enabled from the applied record.
    assert_eq!(
        *f.tz_service.start_type.lock().unwrap(),
        ServiceStartType::AutoStart
    );
}

// ---------- 6. startup: LockHeld ----------

#[tokio::test]
async fn startup_lock_held() {
    let mut f = fixture("startup-lock", vec![Ok(geo_fr())]);
    let now = f.clock.now_unix();
    f.engine.save(&native_reader(), "run".into()).unwrap();
    f.engine.store().write_lock(4321, now).unwrap();

    let outcome = f
        .orch
        .startup(&MockLiveness { alive: true }, 60, 4242)
        .await;
    assert_eq!(outcome, StartupOutcome::LockHeld);
    // Snapshot untouched: the other instance owns it.
    assert!(snapshot_applied(&f.engine).is_some());
}

// ---------- 7. graceful shutdown (Ctrl+C): always restore ----------

#[tokio::test]
async fn shutdown_signal_triggers_restore() {
    let mut f = fixture_home_first("shutdown-signal", vec![Ok(geo_fr())]).await;
    f.orch
        .startup(&MockLiveness { alive: false }, 60, 4242)
        .await;
    f.orch
        .apply_for_ip(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)))
        .await;
    simulate_applied_system(&f);

    let (event_tx, event_rx) = tokio::sync::broadcast::channel(16);
    let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<TrayCommand>();
    let commands = Arc::new(Mutex::new(cmd_rx));

    let mut orch = f.orch;
    let runner = tokio::spawn(async move {
        orch.run(event_rx, shutdown_rx, commands).await;
    });
    // Let the loop start, then signal shutdown.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    shutdown_tx.send(ShutdownReason::Console).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), runner)
        .await
        .unwrap()
        .unwrap();

    // Always-restore: snapshot cleared even without a Disconnected event.
    assert!(snapshot_applied(&f.engine).is_none());
    assert_eq!(f.tz_setter.current.lock().unwrap().key_name, NATIVE_TZ);
    drop(event_tx);
    drop(cmd_tx);
}

// ---------- 8. tray-menu exit: restore first ----------

#[tokio::test]
async fn tray_exit_command_restores_and_exits() {
    let mut f = fixture_home_first("tray-exit", vec![Ok(geo_fr())]).await;
    f.orch
        .startup(&MockLiveness { alive: false }, 60, 4242)
        .await;
    f.orch
        .apply_for_ip(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)))
        .await;
    simulate_applied_system(&f);

    let (event_tx, event_rx) = tokio::sync::broadcast::channel(16);
    let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<TrayCommand>();
    cmd_tx.send(TrayCommand::Exit { restore: true }).unwrap();
    let commands = Arc::new(Mutex::new(cmd_rx));

    let mut orch = f.orch;
    let runner = tokio::spawn(async move {
        orch.run(event_rx, shutdown_rx, commands).await;
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), runner)
        .await
        .unwrap()
        .unwrap();

    assert!(snapshot_applied(&f.engine).is_none());
    assert_eq!(f.tz_setter.current.lock().unwrap().key_name, NATIVE_TZ);
    drop(event_tx);
    drop(shutdown_tx);
    drop(cmd_tx);
}

// ---------- 9. autostart status (real schtasks, absent task name) ----------

#[test]
#[cfg(windows)]
fn autostart_status_absent_task_is_false() {
    // A task name that cannot exist: deterministic Ok(false) without
    // touching the real "VPosE" task.
    let res = app::autostart::status_for_task("VPosE_NoSuchTask_ZZZ");
    assert_eq!(res, Ok(false));
}

// ---------- 10. single-instance: second acquire is refused ----------

#[test]
#[cfg(windows)]
fn single_instance_second_acquire_fails() {
    let name = format!("Global\\VPosE_Test_{}", std::process::id());
    let first = app::sys::single_instance::SingleInstance::acquire(&name);
    assert!(first.is_some(), "first acquire must win");
    let second = app::sys::single_instance::SingleInstance::acquire(&name);
    assert!(second.is_none(), "second acquire must be refused");
}

// ---------- H2. Broken snapshot store: fail-closed ----------

#[tokio::test]
async fn broken_snapshot_store_applies_nothing() {
    // The store base is an existing FILE, so every snapshot read/write
    // fails: without a working recovery path nothing may be applied.
    let parent = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .join("test-tmp")
        .join("broken-store");
    let _ = std::fs::remove_dir_all(&parent);
    std::fs::create_dir_all(&parent).unwrap();
    let base = parent.join("store-is-a-file");
    std::fs::write(&base, b"x").unwrap();

    let mut f = fixture_custom(
        "broken-store",
        vec![Ok(geo_fr())],
        Some(Store::new(base)),
        true,
    );
    f.orch
        .apply_for_ip(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)))
        .await;

    assert!(
        f.tz_setter.log.entries().is_empty(),
        "set_dynamic must not run without a readable snapshot"
    );
    assert!(f.locale_registry.log.entries().is_empty());
    assert!(f.tz_service.log.entries().is_empty());
    assert!(matches!(
        *f.orch.status_handle().lock().unwrap(),
        Status::Error(_)
    ));
}

// ---------- H4. Repeated apply keeps the original service state ----------

#[tokio::test]
async fn repeated_apply_preserves_original_service_state() {
    let mut f = fixture_home_first("repeated-apply", vec![Ok(geo_fr()), Ok(geo_de())]).await;
    f.orch
        .startup(&MockLiveness { alive: false }, 60, 4242)
        .await;
    let vpn1 = IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4));
    let vpn2 = IpAddr::V4(Ipv4Addr::new(5, 6, 7, 8));

    // First apply: tzautoupdate Enabled -> Disabled, applied records Enabled.
    f.orch.apply_for_ip(vpn1).await;
    assert_eq!(
        *f.tz_service.start_type.lock().unwrap(),
        ServiceStartType::Disabled
    );
    let snap = snapshot_applied(&f.engine).unwrap();
    let FieldValue::Timezone { service_state, .. } = snap.fields.timezone.applied.unwrap() else {
        panic!("tz applied");
    };
    assert_eq!(service_state, Some(snapshot::TzServiceState::Enabled));

    // Server change: second apply to another VPN country must NOT overwrite
    // the recorded original service state with the current (Disabled) one.
    f.orch.apply_for_ip(vpn2).await;
    let snap = snapshot_applied(&f.engine).unwrap();
    let FieldValue::Timezone { service_state, .. } = snap.fields.timezone.applied.unwrap() else {
        panic!("tz applied");
    };
    assert_eq!(service_state, Some(snapshot::TzServiceState::Enabled));

    // Restore must bring the service back to its original start type.
    // Mirror the live system (now holding DE values) onto the reader so the
    // restore plan sees current == applied for both groups.
    f.reader.set_field(
        FieldId::Timezone,
        FieldValue::Timezone {
            key_name: DE_TZ.into(),
            dst_disabled: false,
            service_state: None,
        },
    );
    f.reader
        .set_field(FieldId::LocaleName, FieldValue::LocaleName("de-DE".into()));
    f.reader
        .set_field(FieldId::GeoNation, FieldValue::GeoNation("94".into()));
    f.orch.restore_to_native().await;
    assert_eq!(
        *f.tz_service.start_type.lock().unwrap(),
        ServiceStartType::AutoStart
    );
    assert_eq!(f.tz_setter.current.lock().unwrap().key_name, NATIVE_TZ);
}

// ---------- H9. Geo failure: backoff retry until success ----------

#[tokio::test]
async fn geo_failure_retries_with_backoff_until_success() {
    let mut f = fixture(
        "geo-retry",
        vec![
            Ok(geo_ru()),
            Err(GeoError::Provider("rate limited".into())),
            Err(GeoError::Provider("rate limited".into())),
            Ok(geo_fr()),
        ],
    );
    f.orch
        .startup(&MockLiveness { alive: false }, 60, 4242)
        .await;
    // Home observation first (C1); the VPN lookups fail twice, then apply.
    f.orch.apply_for_ip(HOME_IP).await;
    let ip = IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4));
    let t0 = f.clock.now_unix();

    // Attempt 1 fails; retry is armed 30 s out. (Two lookups so far: the
    // home observation plus this one.)
    f.orch.apply_for_ip(ip).await;
    assert!(matches!(
        *f.orch.status_handle().lock().unwrap(),
        Status::Error(_)
    ));

    // Not yet due: no extra lookup.
    f.orch.poll_geo_retry().await;
    assert_eq!(f.geo.lookups(), 2);

    // Attempt 2 (after the 30 s base backoff) also fails.
    f.clock.0.store(t0 + 30, Ordering::SeqCst);
    f.orch.poll_geo_retry().await;
    assert_eq!(f.geo.lookups(), 3);
    assert!(matches!(
        *f.orch.status_handle().lock().unwrap(),
        Status::Error(_)
    ));

    // Attempt 3 (after the doubled backoff) succeeds exactly once.
    f.clock.0.store(t0 + 90, Ordering::SeqCst);
    f.orch.poll_geo_retry().await;
    assert_eq!(f.geo.lookups(), 4);
    assert_eq!(f.tz_setter.log.entries(), vec!["set_dynamic"]);
    assert_eq!(
        *f.orch.status_handle().lock().unwrap(),
        Status::Connected {
            country: "FR".into()
        }
    );

    // Success clears the retry: no further lookups.
    f.orch.poll_geo_retry().await;
    assert_eq!(f.geo.lookups(), 4);
}

// ---------- H10. Apply failure (no admin) -> visible Error ----------

#[tokio::test]
async fn admin_denied_shows_error_and_applies_nothing() {
    let mut f = fixture_custom(
        "admin-denied",
        vec![Ok(geo_ru()), Ok(geo_fr())],
        None,
        false,
    );
    f.orch
        .startup(&MockLiveness { alive: false }, 60, 4242)
        .await;
    // Home observation first (C1); the VPN apply then hits the admin gate.
    f.orch.apply_for_ip(HOME_IP).await;
    f.orch
        .apply_for_ip(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)))
        .await;

    // Timezone apply failed on the admin gate before touching anything.
    assert!(f.tz_setter.log.entries().is_empty());
    assert_eq!(f.tz_service.log.entries(), vec!["query_start_type"]);
    // Locale was never attempted.
    assert!(f.locale_registry.log.entries().is_empty());
    // The icon must be red, not a stale Connected/Native.
    assert!(matches!(
        *f.orch.status_handle().lock().unwrap(),
        Status::Error(_)
    ));
    // Nothing recorded as applied.
    let snap = snapshot_applied(&f.engine).unwrap();
    assert!(snap.fields.timezone.applied.is_none());
    assert!(snap.fields.locale_name.applied.is_none());
}

// ---------- helpers used implicitly ----------

#[test]
fn applied_change_locale_fields_roundtrip() {
    let change = AppliedChange::LocaleChanged {
        changed_fields: vec![apply::locale::FIELD_LOCALE_NAME],
        restart_hint: vec![],
    };
    assert!(!change.is_noop());
}

// ---------- H10 regression: locale failure on a REPEATED apply ----------

/// Review defect H10 / probe p02: VPN change FR -> DE with a locale failure.
/// The tz rollback must return to the PREVIOUS VPN zone (FR), the applied
/// record must describe FR (not DE) with the original service state, and a
/// later restore must return the native zone and restart the service.
/// On the pre-fix code the record said DE, restore_plan skipped the mismatch
/// as UserChanged and complete_restore deleted the snapshot while the live
/// system still held FR with tzautoupdate Disabled.
#[tokio::test]
async fn reapply_locale_failure_restores_native_zone_and_service() {
    let mut f = fixture_home_first("h10-locale-failure", vec![Ok(geo_fr()), Ok(geo_de())]).await;
    f.orch
        .startup(&MockLiveness { alive: false }, 60, 4242)
        .await;
    let vpn1 = IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4));
    let vpn2 = IpAddr::V4(Ipv4Addr::new(5, 6, 7, 8));

    // First apply succeeds: tz FR applied, tzautoupdate Disabled, records in.
    f.orch.apply_for_ip(vpn1).await;
    assert_eq!(
        *f.tz_service.start_type.lock().unwrap(),
        ServiceStartType::Disabled
    );
    // Mirror the live FR state onto the reader (the real reader follows the
    // registry automatically).
    simulate_applied_system(&f);

    // Second apply (FR -> DE): the locale write fails after the tz change.
    f.locale_registry.fail_writes(true);
    f.orch.apply_for_ip(vpn2).await;
    f.locale_registry.fail_writes(false);

    // The orchestrator must show the failure, not a stale Connected.
    assert!(matches!(
        *f.orch.status_handle().lock().unwrap(),
        Status::Error(_)
    ));
    // The tz rollback must have returned to the previous VPN zone.
    assert_eq!(f.tz_setter.current.lock().unwrap().key_name, FR_TZ);
    // The applied record must describe FR with the FIRST pre-apply service
    // state (Enabled captured before the first apply).
    let snap = snapshot_applied(&f.engine).unwrap();
    let FieldValue::Timezone {
        key_name,
        service_state,
        ..
    } = snap.fields.timezone.applied.unwrap()
    else {
        panic!("tz applied record must exist after the rollback");
    };
    assert_eq!(key_name, FR_TZ);
    assert_eq!(service_state, Some(snapshot::TzServiceState::Enabled));

    // Restore must still work end to end: native zone, service restarted,
    // snapshot cleared.
    f.orch.restore_to_native().await;
    assert_eq!(f.tz_setter.current.lock().unwrap().key_name, NATIVE_TZ);
    assert_eq!(
        *f.tz_service.start_type.lock().unwrap(),
        ServiceStartType::AutoStart
    );
    assert!(snapshot_applied(&f.engine).is_none());
}

// ---------- H10 control: the same locale failure on the FIRST apply ----------

/// The first apply fails on locale writes: the tz rollback returns to the
/// native zone, the service back to AutoStart, and — with the H10 fix — the
/// applied record is CLEARED (nothing is applied), so the later restore is a
/// clean no-op. (The pre-fix code left a stale FR record behind; harmless but
/// misleading.)
#[tokio::test]
async fn first_apply_locale_failure_rolls_back_to_clean_native_state() {
    let mut f = fixture_home_first("h10-first-apply-failure", vec![Ok(geo_fr())]).await;
    f.orch
        .startup(&MockLiveness { alive: false }, 60, 4242)
        .await;
    f.locale_registry.fail_writes(true);
    f.orch
        .apply_for_ip(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)))
        .await;
    f.locale_registry.fail_writes(false);

    assert_eq!(f.tz_setter.current.lock().unwrap().key_name, NATIVE_TZ);
    assert_eq!(
        *f.tz_service.start_type.lock().unwrap(),
        ServiceStartType::AutoStart
    );
    assert!(matches!(
        *f.orch.status_handle().lock().unwrap(),
        Status::Error(_)
    ));
    let snap = snapshot_applied(&f.engine).unwrap();
    assert!(snap.fields.timezone.applied.is_none());
    assert!(snap.fields.locale_name.applied.is_none());
    assert!(snap.fields.geo_nation.applied.is_none());

    // The later restore is a no-op and clears the tracking.
    f.orch.restore_to_native().await;
    assert_eq!(f.tz_setter.current.lock().unwrap().key_name, NATIVE_TZ);
    assert_eq!(
        *f.tz_service.start_type.lock().unwrap(),
        ServiceStartType::AutoStart
    );
    assert!(snapshot_applied(&f.engine).is_none());
}

// ---------- H6 regression: shutdown restore watchdog on a slow revert ----------

/// Review defect H6 / probe p01: the restore ran inline on the single-threaded
/// runtime, so the tokio watchdog timer could never fire. With the restore
/// dispatched to a worker thread the 5 s watchdog must fire while the slow
/// Win32 call (simulated 6.5 s) is still running: the loop returns well under
/// 6 s and the timed-out flag is set.
#[tokio::test]
async fn shutdown_restore_watchdog_fires_on_slow_restore() {
    let mut f = fixture_home_first("watchdog-slow-restore", vec![Ok(geo_fr())]).await;
    f.orch
        .startup(&MockLiveness { alive: false }, 60, 4242)
        .await;
    f.orch
        .apply_for_ip(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)))
        .await;
    simulate_applied_system(&f);
    // The revert's set_dynamic call stalls 6.5 s — past the 5 s watchdog.
    f.tz_setter.set_delay_ms(6_500);

    let (event_tx, event_rx) = tokio::sync::broadcast::channel(16);
    let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<TrayCommand>();
    let commands = Arc::new(Mutex::new(cmd_rx));

    let mut orch = f.orch;
    let runner = tokio::spawn(async move {
        orch.run(event_rx, shutdown_rx, commands).await;
    });
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    shutdown_tx.send(ShutdownReason::Console).unwrap();
    let t0 = std::time::Instant::now();
    tokio::time::timeout(std::time::Duration::from_secs(10), runner)
        .await
        .unwrap()
        .unwrap();
    let elapsed = t0.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(6),
        "shutdown restore must return via the 5 s watchdog, took {elapsed:?}"
    );
    assert!(
        app::shutdown::restore_timed_out(),
        "restore_timed_out flag must be set by the watchdog"
    );
    drop(event_tx);
    drop(cmd_tx);
}

// ---------- 14. C1 (D3): combined native detection ----------

#[tokio::test]
async fn c1_start_without_vpn_shows_native_not_connected() {
    let mut f = fixture("c1-home-start", vec![Ok(geo_ru())]);
    let outcome = f
        .orch
        .startup(&MockLiveness { alive: false }, 60, 4242)
        .await;
    assert_eq!(outcome, StartupOutcome::Clean);

    // Home IP, geo matches the originals: nothing is applied and the status
    // is Native, not Connected (p06).
    f.orch
        .apply_for_ip(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10)))
        .await;
    assert_eq!(*f.orch.status_handle().lock().unwrap(), Status::Native);
    assert!(f.tz_setter.log.entries().is_empty());
    assert!(f.locale_registry.log.entries().is_empty());
    let snap = snapshot_applied(&f.engine).unwrap();
    assert!(snap.fields.timezone.applied.is_none());
    assert_eq!(snap.native_ips, vec!["192.168.1.10".to_string()]);
}

#[tokio::test]
async fn c1_kaliningrad_home_ip_restores_original_zone() {
    // The geo database reports Europe/Moscow for the home network, but the
    // machine is in Kaliningrad (p07): the remembered native IP must win.
    // The old geo-only heuristic applied Moscow at home.
    let mut f = fixture_full(
        "c1-kaliningrad",
        vec![Ok(geo_ru()), Ok(geo_fr()), Ok(geo_ru())],
        None,
        true,
        reader_with_tz(KGD_TZ),
        vec![zone(KGD_TZ), zone(FR_TZ), zone(DE_TZ)],
        KGD_TZ,
    );
    f.orch
        .startup(&MockLiveness { alive: false }, 60, 4242)
        .await;
    let home = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 10));
    let vpn = IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7));

    // First observation at home: recorded as native, nothing applied.
    f.orch.apply_for_ip(home).await;
    assert_eq!(*f.orch.status_handle().lock().unwrap(), Status::Native);

    // VPN: FR applied; the home IP stays remembered.
    f.orch.apply_for_ip(vpn).await;
    assert_eq!(f.tz_setter.current.lock().unwrap().key_name, FR_TZ);
    let snap = snapshot_applied(&f.engine).unwrap();
    assert!(snap.native_ips.iter().any(|s| s == "192.168.1.10"));

    // The watcher reports the home IP again: IP equality is sufficient,
    // the zone returns to the original (Kaliningrad), not to Moscow.
    // The reader mirrors the applied zone so the restore has a real item.
    f.reader.set_field(
        FieldId::Timezone,
        FieldValue::Timezone {
            key_name: FR_TZ.into(),
            dst_disabled: false,
            service_state: None,
        },
    );
    f.orch.handle_ip_event(IpEvent::Changed(vpn, home)).await;
    assert_eq!(f.tz_setter.current.lock().unwrap().key_name, KGD_TZ);
    assert_eq!(*f.orch.status_handle().lock().unwrap(), Status::Native);
    // The restore re-arms a fresh snapshot (no applied records); the
    // remembered home IP comes back with the next home observation.
    let snap = snapshot_applied(&f.engine).unwrap();
    assert!(snap.fields.timezone.applied.is_none());
    assert!(snap.native_ips.is_empty());
}

#[tokio::test]
async fn c1_expat_keeps_native_settings_without_vpn() {
    // Russian settings, physically in Germany, first event without VPN
    // (p08): the German geo answer must NOT be applied at first sight.
    let mut f = fixture("c1-expat-start", vec![Ok(geo_de())]);
    f.orch
        .startup(&MockLiveness { alive: false }, 60, 4242)
        .await;
    f.orch
        .apply_for_ip(IpAddr::V4(Ipv4Addr::new(84, 19, 5, 2)))
        .await;
    assert_eq!(*f.orch.status_handle().lock().unwrap(), Status::Native);
    assert_eq!(f.tz_setter.current.lock().unwrap().key_name, NATIVE_TZ);
    assert_eq!(f.locale_registry.state.lock().unwrap().locale_name, "ru-RU");
    // A later VPN connection still applies the German region.
    f.orch
        .apply_for_ip(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7)))
        .await;
    assert_eq!(f.tz_setter.current.lock().unwrap().key_name, DE_TZ);
}

// ---------- 15. H4 (D4): record without service_state stays None ----------

#[tokio::test]
async fn repeated_apply_keeps_missing_service_state_unset() {
    let mut f = fixture_home_first(
        "d4-record-no-service-state",
        vec![Ok(geo_fr()), Ok(geo_de())],
    )
    .await;
    f.orch
        .startup(&MockLiveness { alive: false }, 60, 4242)
        .await;
    f.orch
        .apply_for_ip(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)))
        .await;
    // Models a capture_service_state failure at the first apply or a
    // snapshot left by an old version: the applied record has no
    // service_state.
    f.engine
        .record_applied(
            FieldId::Timezone,
            FieldValue::Timezone {
                key_name: FR_TZ.into(),
                dst_disabled: false,
                service_state: None,
            },
        )
        .unwrap();

    // Second apply: the record must stay None, not pick up the live
    // (already disabled) service state.
    f.orch
        .apply_for_ip(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 5)))
        .await;
    assert_eq!(f.tz_setter.current.lock().unwrap().key_name, DE_TZ);
    let snap = snapshot_applied(&f.engine).unwrap();
    let Some(FieldValue::Timezone { service_state, .. }) = &snap.fields.timezone.applied else {
        panic!("tz applied record");
    };
    assert!(service_state.is_none(), "service_state must stay unset");

    // The system now shows the applied zone (the reader mirrors it), so the
    // restore has a real TZ item.
    f.reader.set_field(
        FieldId::Timezone,
        FieldValue::Timezone {
            key_name: DE_TZ.into(),
            dst_disabled: false,
            service_state: None,
        },
    );

    // Restore: zone back to native; the service is honest-unknown and is
    // left as the first apply left it.
    f.orch.restore_to_native().await;
    assert_eq!(f.tz_setter.current.lock().unwrap().key_name, NATIVE_TZ);
    assert_eq!(
        *f.tz_service.start_type.lock().unwrap(),
        ServiceStartType::Disabled
    );
    assert!(snapshot_applied(&f.engine).is_none());
}

// ---------- 16. D5: shutdown during the geo lookup aborts the apply ------

#[tokio::test]
async fn shutdown_during_geo_lookup_aborts_apply_promptly() {
    let mut f = fixture("d5-shutdown-during-geo", vec![Ok(geo_fr())]);
    f.orch
        .startup(&MockLiveness { alive: false }, 60, 4242)
        .await;
    // The lookup stalls 3 s: the old code waited for it before handling the
    // shutdown (~2.9 s); the select! must abort within ~0.2 s.
    f.geo.set_delay_ms(3_000);

    let (event_tx, event_rx) = tokio::sync::broadcast::channel(16);
    let (shutdown_tx, shutdown_rx) = tokio::sync::mpsc::unbounded_channel();
    let (cmd_tx, cmd_rx) = std::sync::mpsc::channel::<TrayCommand>();
    let commands = Arc::new(Mutex::new(cmd_rx));
    event_tx
        .send(IpEvent::Connected(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4))))
        .unwrap();

    // The test probe plays the role of shutdown::request() from the signal
    // source, without touching the process-wide flag.
    let probe = Arc::new(AtomicBool::new(false));
    f.orch.set_shutdown_probe(probe.clone());

    let mut orch = f.orch;
    let runner = tokio::spawn(async move {
        orch.run(event_rx, shutdown_rx, commands).await;
    });
    // The loop starts, the event is consumed and the lookup goes in flight.
    tokio::time::sleep(std::time::Duration::from_millis(150)).await;
    probe.store(true, Ordering::SeqCst);
    shutdown_tx.send(ShutdownReason::Console).unwrap();
    let t0 = std::time::Instant::now();
    tokio::time::timeout(std::time::Duration::from_secs(5), runner)
        .await
        .unwrap()
        .unwrap();
    let elapsed = t0.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "shutdown during the geo lookup must abort the apply, took {elapsed:?}"
    );
    // Nothing was applied and the process exits clean.
    assert_eq!(f.tz_setter.current.lock().unwrap().key_name, NATIVE_TZ);
    drop(event_tx);
    drop(cmd_tx);
}

// ---------- 17. H2: record_applied failure rolls back (probes p04/p05) ----

/// Breaks snapshot WRITES (the tmp file becomes a directory) while reads of
/// the already-written snapshot keep working. Idempotent: the rollback's
/// set_dynamic fires the hook a second time.
fn break_snapshot_writes(engine: &Engine) {
    let tmp = engine
        .store()
        .snapshot_path_full()
        .with_file_name("snapshot.json.tmp");
    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_dir(&tmp);
    std::fs::create_dir(&tmp).unwrap();
}

#[tokio::test]
async fn tz_record_failure_rolls_back_timezone_and_service() {
    let mut f = fixture_home_first("h2-tz-record-rollback", vec![Ok(geo_fr())]).await;
    f.orch
        .startup(&MockLiveness { alive: false }, 60, 4242)
        .await;
    let engine = f.engine.clone();
    f.tz_setter.set_after_set(Some(Box::new(move || {
        // The store is broken right after the system was mutated, so
        // record_applied cannot save.
        break_snapshot_writes(&engine);
    })));

    f.orch
        .apply_for_ip(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)))
        .await;

    // Applied, then rolled back to native including the service.
    assert_eq!(
        f.tz_setter.log.entries(),
        vec!["set_dynamic", "set_dynamic"]
    );
    assert_eq!(f.tz_setter.current.lock().unwrap().key_name, NATIVE_TZ);
    assert_eq!(
        *f.tz_service.start_type.lock().unwrap(),
        ServiceStartType::AutoStart
    );
    assert!(matches!(
        *f.orch.status_handle().lock().unwrap(),
        Status::Error(_)
    ));
    // Locale was never attempted; nothing recorded as applied.
    assert!(f.locale_registry.log.entries().is_empty());

    // Once the store works again, a restore is a clean no-op.
    let tmp = f
        .engine
        .store()
        .snapshot_path_full()
        .with_file_name("snapshot.json.tmp");
    let _ = std::fs::remove_dir(&tmp);
    f.orch.restore_to_native().await;
    assert_eq!(*f.orch.status_handle().lock().unwrap(), Status::Native);
    assert!(snapshot_applied(&f.engine).is_none());
}

#[tokio::test]
async fn locale_record_failure_reverts_locale_and_timezone() {
    let mut f = fixture_home_first("h2-locale-record-rollback", vec![Ok(geo_fr())]).await;
    f.orch
        .startup(&MockLiveness { alive: false }, 60, 4242)
        .await;
    let engine = f.engine.clone();
    f.locale_registry.set_after_write(Some(Box::new(move || {
        break_snapshot_writes(&engine);
    })));

    f.orch
        .apply_for_ip(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)))
        .await;

    // Zone applied then rolled back; locale reverted; service back on.
    assert_eq!(
        f.tz_setter.log.entries(),
        vec!["set_dynamic", "set_dynamic"]
    );
    assert_eq!(f.tz_setter.current.lock().unwrap().key_name, NATIVE_TZ);
    assert_eq!(
        *f.tz_service.start_type.lock().unwrap(),
        ServiceStartType::AutoStart
    );
    let loc = f.locale_registry.state.lock().unwrap();
    assert_eq!(loc.locale_name, "ru-RU");
    assert_eq!(loc.geo_nation, "203");
    drop(loc);
    assert!(matches!(
        *f.orch.status_handle().lock().unwrap(),
        Status::Error(_)
    ));
}

// ---------- 18. H3: stale originals are refreshed before first apply ------

#[tokio::test]
async fn stale_originals_recaptured_before_first_apply() {
    let mut f = fixture("h3-recapture", vec![Ok(geo_ru()), Ok(geo_fr())]);
    // A snapshot captured while the system showed a different state: the
    // reader reports FR, the snapshot stores it as the originals.
    f.reader.set_field(
        FieldId::Timezone,
        FieldValue::Timezone {
            key_name: FR_TZ.into(),
            dst_disabled: false,
            service_state: None,
        },
    );
    f.orch
        .startup(&MockLiveness { alive: false }, 60, 4242)
        .await;
    // The user changes the system back before the first apply (p11).
    simulate_native_system(&f);

    // Home observation (C1): the recapture rebuilds the stale originals.
    f.orch.apply_for_ip(HOME_IP).await;
    f.orch
        .apply_for_ip(IpAddr::V4(Ipv4Addr::new(1, 2, 3, 4)))
        .await;

    // The originals were rebuilt from the live system, not the stale ones.
    let snap = snapshot_applied(&f.engine).unwrap();
    let FieldValue::Timezone { key_name, .. } = &snap.fields.timezone.original else {
        panic!("tz original");
    };
    assert_eq!(key_name, NATIVE_TZ);
    let FieldValue::Timezone {
        key_name: applied, ..
    } = &snap.fields.timezone.applied.expect("tz applied")
    else {
        panic!("tz applied variant");
    };
    assert_eq!(applied, FR_TZ);
}
