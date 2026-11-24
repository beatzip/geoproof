# Этап 4 — крейт `apply` + `TimezoneApplier` (Rust, Windows)

**Контекст**: workspace (`shared`, `watcher`, `geo`, `snapshot`), toolchain `stable-x86_64-pc-windows-gnu`, правило unsafe = deny в workspace, allow только в `sys/`-модулях. Этап 4 — **первый пишущий**: реально меняем часовой пояс Windows. Текст ТЗ сохранён дословно.

## Архитектурная развилка — зафиксированное решение

`TimezoneApplier` пишет не только TZ, но и состояние службы `tzautoupdate`. Это системное изменение нужно вернуть при restore → **оно хранится в snapshot**. Stage 3 зафиксировал v1-схему без поля `tzautoupdate`.

**Решение: расширение `TimezoneValue` опциональным полем.**

```rust
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
pub struct TimezoneValue {
    pub key_name: String,
    pub dst_disabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_state: Option<TzAutoUpdateState>,  // новое
}
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub enum TzAutoUpdateState { Enabled, Disabled }
```

- Старый JSON `{"key_name":"...","dst_disabled":false}` десериализуется корректно (`None`).
- Никакого `version: 2`, миграций, отдельных state-файлов.
- На уровне `FieldSnapshot<TimezoneValue>` это автоматически совместимо.
- Acceptance: тест `snapshot_v1_without_service_state_roundtrips` (читаем v1-JSON без поля, получаем `service_state: None`).

## Структура крейта `apply`

```
crates/apply/
  Cargo.toml                  # добавить в workspace members
  src/lib.rs                  # #[forbid(unsafe_code)]
  src/traits.rs               # trait Applier, RegistryReader, RegistryWriter,
                              # TzRegistry, WinTzSetter, ServiceController,
                              # AdminCheck
  src/timezone.rs             # TimezoneApplier
  src/timezone/tz_registry.rs # чтение DST-правил из HKLM\...\Time Zones\<Zone>
  src/sys/
    mod.rs                    # #[allow(unsafe_code)]
    winapi.rs                 # SetDynamicTimeZoneInformation, IsUserAnAdmin
    registry.rs               # RegOpenKeyEx/RegQueryValueEx, StartServiceW/
                              # ChangeServiceConfigW, OpenServiceW
    service.rs                # Start/Stop service через Service Control Manager
  tests/                      # интеграционные на моках
  src/bin/apply_smoke.rs      # ручной прогон под админом
```

## trait `Applier` (общий на все последующие апплаеры)

```rust
#[async_trait]
pub trait Applier {
    type Value: Serialize + DeserializeOwned + PartialEq + Eq + Debug + Clone;

    async fn current(&self) -> Result<Self::Value, ApplyError>;
    async fn apply(&mut self, new: &Self::Value) -> Result<AppliedChange, ApplyError>;
    async fn revert(&mut self, original: &Self::Value) -> Result<(), ApplyError>;
}
pub struct AppliedChange { pub field_name: &'static str, /* ... */ }
```

- `apply` идемпотентен: если `new == current` → `Noop`, без сайд-эффектов (правило из stage 1).
- `revert` вызывается только с сохранённым `original` из snapshot; не обязан быть идемпотентным, но должен быть безопасным (повторный revert с тем же `original` не ломает систему).
- `AppliedChange` — структура для logging/telemetry (имя поля, что изменилось), без излишней детализации.

## Что делает `TimezoneApplier::apply`

**Порядок (важен, откат — обратный порядок):**

1. `AdminCheck::is_admin()` — false → `ApplyError::InsufficientPrivileges` без попыток.
2. Читаем текущее состояние службы `tzautoupdate` (через `ServiceController::query_start_type`) — запоминаем в `TimezoneValue.service_state` (будет записано в snapshot снаружи, здесь — вернётся в `AppliedChange` или записывается отдельным методом `capture_service_state()`; см. детали ниже).
3. Отключаем `tzautoupdate`: `ChangeServiceConfigW(SERVICE_DISABLED)` + `ControlService(SERVICE_CONTROL_STOP)`. Если служба уже Disabled + остановлена — no-op.
4. Читаем DST-правила для новой зоны из реестра `HKLM\SOFTWARE\Microsoft\Windows NT\CurrentVersion\Time Zones\<key_name>`: `TZI` binary, `MUI_Std`, `MUI_Dlt`, `Std`, `Dlt`. **Обязательно**: если зона использует Dynamic DST (подпапка `Dynamic DST` с `FirstEntry`/`LastEntry` и массивом `YYYY`=TZI по годам) — брать TZI **текущего года**, а не статический. Если зона не найдена в реестре — `ApplyError::UnknownZone(key_name)`.
5. Строим `DYNAMIC_TIME_ZONE_INFORMATION` (через `windows` crate — `Win32::System::Time::DYNAMIC_TIME_ZONE_INFORMATION`), заполняем `Bias`, `StandardBias`, `DaylightBias`, `StandardDate`, `DaylightDate`, `TimeZoneKeyName`, `DynamicDaylightTimeDisabled` (из `TimezoneValue.dst_disabled`), `StandardName`/`DaylightName` из `Std`/`Dlt`.
6. `SetDynamicTimeZoneInformation` — failure → `ApplyError::SetTzFailed(win32_err)`. **Важно**: если шаг 6 падает, шаги 2–3 уже изменили систему — в `revert` надо уметь вернуть `tzautoupdate` назад даже при откате после частичного apply. Решение: в структуре аплайера хранить `service_state_before: Option<TzAutoUpdateState>` — обновляется на шаге 2, сбрасывается в `None` при успешном apply (тогда revert знает, что откатывать); при ошибке apply — revert вызывается автоматически.
7. Возвращаем `AppliedChange` с предупреждениями: `restart_hint: vec!["Chrome", "Firefox"]` (эти приложения кэшируют TZ до перезапуска).

## `capture_service_state` vs интеграция со snapshot

Два варианта — выбирай осознанно, я принимаю оба:

- **Вариант A (рекомендую):** отдельный метод `TimezoneApplier::capture_service_state() -> Option<TzAutoUpdateState>`. Оркестратор (stage 6) вызовет его перед `apply` и запишет результат в `TimezoneValue.service_state` через `Snapshot::record_applied`. Плюсы: applier не знает про snapshot, чище. Минусы: оркестратор должен помнить вызвать capture.
- **Вариант B:** `apply` сам возвращает `AppliedChange { field_value, service_state }` — оркестратор записывает оба в snapshot. Проще оркестратору, сложнее структура `AppliedChange`.

Я склоняюсь к A, но не блокирую B. **Зафиксируй выбор в README крейта и в stage-04.md.**

## `revert` (обратный порядок)

1. `AdminCheck` — если нет прав, не трогаем ничего (иначе частичный revert хуже полного no-op).
2. `SetDynamicTimeZoneInformation` с `original` (ключ, `dst_disabled`, DST-правила оригинальной зоны — читаем заново из реестра, не храним).
3. Возвращаем `tzautoupdate` в `original.service_state`: если было Enabled → `SERVICE_AUTO_START` (не `SERVICE_DEMAND_START`, это дефолт Win) + `StartServiceW`.

## Тесты

**Offline unit-тесты (моки всех трейтов):**

1. Apply happy-path: проверка порядка вызовов (capture service → stop/disable service → read registry → SetTz) через счётчики в моках.
2. Apply: `new == current` → `Noop`, ничего не вызывается.
3. Apply: не админ → ошибка до любых вызовов.
4. Apply: зона не в реестре → `UnknownZone`, служба не тронута.
5. Apply: SetTz падает → служба возвращена в исходное состояние (автоматический revert внутри apply).
6. Revert happy-path: обратный порядок (SetTz → restore service).
7. Revert: не админ → no-op без ошибок.
8. DST Dynamic: зона с подпапкой `Dynamic DST` → читается TZI текущего года (мок возвращает таблицу 2024/2025/2026, текущий год = 2026, ожидается именно он).
9. DST static: зона без `Dynamic DST` → читается корневой `TZI`.
10. **Snapshot v1 roundtrip**: `TimezoneValue { key_name, dst_disabled }` без `service_state` → `None` → обратно сериализуется как без поля (для backward-compat).

**Smoke (manual, `#[ignore]`, под админом):**

```rust
// 1. Запоминаем текущий TZ
// 2. Применяем "UTC+12" (или что-то редкое: Line Islands Standard Time)
// 3. Проверяем Get-Date / tzutil /g / реестр HKCU\...\TimeZoneKeyName
// 4. Проверяем tzautoupdate в Disabled
// 5. Revert
// 6. Проверяем возврат и Enabled у tzautoupdate
```

Smoke печатает чек-лист в stdout, оператор проходит его вручную.

## `DYNAMIC_TIME_ZONE_INFORMATION` struct

В `windows` crate это `Win32::System::Time::DYNAMIC_TIME_ZONE_INFORMATION` с полями:

- `Bias: i32`
- `StandardName: [u16; 32]`
- `StandardDate: SYSTEMTIME`
- `StandardBias: i32`
- `DaylightName: [u16; 32]`
- `DaylightDate: SYSTEMTIME`
- `DaylightBias: i32`
- `TimeZoneKeyName: [u16; 128]`
- `DynamicDaylightTimeDisabled: BOOLEAN`

**Формирование `*Name` полей**: из `Std`/`Dlt` строк реестра. Если строки пустые или `MUI_Std`/`MUI_Dlt` существуют — использовать MUI (но `SetDynamicTimeZoneInformation` принимает только `*Name`, MUI это альтернативный дисплейный путь; достаточно `Std`/`Dlt`). Пустые → забить нулями.

## Зависимости

- `windows` crate: features `Win32_System_Time`, `Win32_System_Registry`, `Win32_System_Services`, `Win32_Security`, `Win32_Foundation`.
- `async-trait`, `thiserror`, `tracing`.
- `snapshot` — **не подключаем**. Applier-трейт должен жить без knowledge of snapshot (decoupling для stage 5/6).

## Что НЕ делать

- Не интегрировать с `watcher` или `geo` (это этап 6).
- Не писать оркестратор.
- Не менять локаль/гео (этап 5).
- Не использовать `tzutil.exe` — только прямой WinAPI через `windows` crate (причина: строки `tzutil` чувствительны к локали, мы управляем программно).
- Не использовать `runas`/ShellExecute для повышения прав — аплайер вызывается уже из привилегированного процесса (stage 7), на этапе 4 предполагаем админ-токен у вызывающего.

## Гейты / DoD

- `cargo build --workspace` ✅
- `cargo test --workspace` ✅ (все unit-тесты зелёные, smoke ignored)
- `cargo clippy --workspace --all-targets -D warnings` ✅
- `cargo fmt --all -- --check` ✅
- `apply_smoke` вручную под админом: пояс меняется, `tzautoupdate` в Disabled, после revert — всё возвращается.
- `docs/specs/stage-04.md` в diff, README крейта с выбранным вариантом A/B интеграции со snapshot.

## На ревью буду смотреть

1. **Частичный apply failure** — при падении `SetDynamicTimeZoneInformation` на шаге 6 служба `tzautoupdate` уже тронута; аплайер обязан вернуть её автоматически. Тест 5 должен это покрывать.
2. **Dynamic DST** — текущий год выбирается через `SystemTime`/`OffsetDateTime`, а не хардкод; учтены зоны, где `FirstEntry` > текущий год (fallback на первый) и `LastEntry` < текущий год (fallback на последний).
3. **Revert с `original.service_state = None`** (из v1-снимка) — аплайер НЕ должен ломать службу: либо интерпретировать как `Enabled` (дефолт Win), либо вообще не трогать службу. Фиксируй семантику в README; рекомендую: `None` при revert → "не знаю, не трогаю" (skip service restore), это безопаснее чем предположить Enabled и включить то, что было Disabled.
4. **`DYNAMIC_TIME_ZONE_INFORMATION`** — `TimeZoneKeyName` копируется в `[u16; 128]` с учётом длины; переполнение буфера → усечение с нуль-терминатором, не паника.
5. unsafe только в `sys/`, с SAFETY-комментариями, workspace `deny` enforcement (проверено на этапе 3).
6. **Нет зависимости apply → snapshot** (decoupling).

---

## Реализация — зафиксированные решения (этап 4, исполнено)

- **Интеграция со snapshot: вариант A** — `TimezoneApplier::capture_service_state()`, оркестратор (этап 6) вызывает его до `apply` и кладёт в `TimezoneValue.service_state` через `Snapshot::record_applied`. Applier не знает про snapshot.
- **Revert с `service_state = None`**: служба не трогается («не знаю — не трогаю»), семантика в README крейта.
- **Откат при частичном apply**: любой сбой после первой мутации службы (шаги 4–6: disable/stop, чтение зоны, SetTz) вызывает автоматический `rollback_service`; сбой самого rollback логируется, исходная ошибка возвращается.
- **Enforcement unsafe**: workspace-level `[workspace.lints] unsafe_code = deny` (+ clippy `undocumented_unsafe_blocks = deny`); `sys/` выводит из-под него явным `#![allow(unsafe_code)]` на уровне модуля. `#![forbid]` на крейт не используется: forbid протекает в дочерние модули и его нельзя перебить allow.
- **Сборка**: dlltool для `windows` crate требует ассемблер — PATH должен содержать `C:\mingw64\bin` (без него dlltool не находит as.exe). Команда: `PATH="/c/mingw64/bin:$PATH" cargo build --workspace`.
