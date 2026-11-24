# ТЗ: Этап 5 — `LocaleApplier` (Rust, Windows)

**Контекст**: workspace (`shared`, `watcher`, `geo`, `snapshot`, `apply`), toolchain `stable-x86_64-pc-windows-gnu`, правило unsafe = deny в workspace. Этап 5 — **второй пишущий**: меняем региональные форматы, гео-нацию, список раскладок. **Не требует админки** (всё в HKCU). 

## Архитектурное решение: один аплайер, три поля

LocaleApplier управляет **тремя связанными полями** snapshot: `locale_name`, `geo_nation`, `kbd_preload`. Все три требуют `WM_SETTINGCHANGE` broadcast после изменений → **один аплайер с одной структурой `LocaleValue`**, один broadcast в конце.

```rust
#[derive(Serialize, Deserialize, Debug, PartialEq, Eq, Clone)]
pub struct LocaleValue {
    pub locale_name: String,        // "ru-RU"
    pub geo_nation: String,         // "203" (ISO 3166-1 numeric)
    pub kbd_preload: Vec<String>,   // ["00000409", "00000419"] (упорядоченный)
}
```

**UI language (интерфейс Windows)** — **НЕ делаем** на этом этапе. Требует установленного языкового пакета и перелогина, мгновенно не применимо. Оставляем как TODO для этапа 8 или отдельной фичи. README крейта фиксирует это ограничение.

## Структура (добавляем в существующий `crates/apply`)

```
crates/apply/
  src/locale.rs                 # LocaleApplier, LocaleValue
  src/sys/
    registry_locale.rs          # чтение/запись HKCU\Control Panel\International\*, Keyboard Layout\Preload
    wm.rs                       # SendMessageTimeout(HWND_BROADCAST, WM_SETTINGCHANGE)
```

**Trait'ы** (добавляем в `traits.rs`):
```rust
pub trait LocaleRegistry: Send + Sync {
    fn read_locale_name(&self) -> Result<String, ApplyError>;
    fn write_locale_name(&self, value: &str) -> Result<(), ApplyError>;
    fn read_geo_nation(&self) -> Result<String, ApplyError>;
    fn write_geo_nation(&self, value: &str) -> Result<(), ApplyError>;
    fn read_kbd_preload(&self) -> Result<Vec<String>, ApplyError>;
    fn write_kbd_preload(&self, values: &[String]) -> Result<(), ApplyError>;
}
pub trait SettingChangeBroadcast: Send + Sync {
    fn broadcast_locale(&self) -> Result<(), ApplyError>;
    fn broadcast_keyboard(&self) -> Result<(), ApplyError>;
}
```

## Что делает `LocaleApplier::apply`

**Порядок:**
1. Читаем текущее состояние через `LocaleRegistry` → `LocaleValue`.
2. Сравниваем с `new` по полям.
3. Если **все три поля** равны → `Noop`, ничего не вызывается (идемпотентность).
4. Иначе:
   - `locale_name` отличается → `write_locale_name` (HKCU\Control Panel\International\LocaleName = REG_SZ).
   - `geo_nation` отличается → `write_geo_nation` (HKCU\Control Panel\International\Geo\Nation = REG_SZ).
   - `kbd_preload` отличается → `write_kbd_preload` (HKCU\Keyboard Layout\Preload = REG_MULTI_SZ, упорядоченный список).
5. **Один broadcast**: `broadcast_locale()` + `broadcast_keyboard()` (оба `WM_SETTINGCHANGE` с `lParam` = "Locale" и "Keyboard Layout" соответственно, или просто `lParam = 0` для универсального refresh). Таймаут `SendMessageTimeout` = 5000ms, флаг `SMTO_ABORTIFHUNG`.
6. Возвращаем `AppliedChange` с предупреждениями: `restart_hint: vec!["Chrome", "Firefox"]` (браузеры кэшируют `Intl.DateTimeFormat` до перезапуска, но частично подхватят broadcast).

## `revert` (обратный порядок, но broadcast в конце)

1. Читаем текущее состояние.
2. Сравниваем с `original`.
3. Если всё равно → `Noop`.
4. Иначе: восстанавливаем изменённые поля через `write_*`.
5. Broadcast (те же два вызова).
6. Возвращаем `AppliedChange`.

**Важно**: если `original` содержит пустые строки или пустой `kbd_preload` — это валидное значение (значит поле не было установлено), пишем как есть. Не пытаемся "угадать" дефолт.

## Реестр: детали

**`HKCU\Control Panel\International\LocaleName`**:
- Тип: `REG_SZ`
- Значение: строка `"ru-RU"` (BCP-47)
- Чтение: `RegQueryValueExW`, если ключ отсутствует → `ApplyError::RegistryReadFailed("LocaleName not found")`.

**`HKCU\Control Panel\International\Geo\Nation`**:
- Тип: `REG_SZ`
- Значение: строка `"203"` (ISO 3166-1 numeric для России, `"840"` для US)
- Чтение: аналогично.

**`HKCU\Keyboard Layout\Preload`**:
- Тип: `REG_MULTI_SZ`
- Значение: массив строк `["00000409", "00000419"]` (hex-коды раскладок, упорядочены по приоритету)
- Чтение: `RegQueryValueExW` возвращает `Vec<u8>`, парсим как null-terminated строки, последняя — пустая (двойной null-terminator для REG_MULTI_SZ).
- Запись: сериализуем `Vec<String>` в `Vec<u8>` с двойным null-terminator.

**WM_SETTINGCHANGE**:
- `SendMessageTimeoutW(HWND_BROADCAST, WM_SETTINGCHANGE, 0, "Locale", SMTO_ABORTIFHUNG, 5000, &mut result)`
- `SendMessageTimeoutW(HWND_BROADCAST, WM_SETTINGCHANGE, 0, "Keyboard Layout", SMTO_ABORTIFHUNG, 5000, &mut result)`
- `lParam` — указатель на UTF-16 строку (или `0` для универсального refresh; рекомендую явные строки "Locale" и "Keyboard Layout").
- Если broadcast возвращает таймаут (некоторые окна не отвечают) — это не ошибка, логируем warning и продолжаем.

## Тесты

**Offline unit-тесты (моки `LocaleRegistry` + `SettingChangeBroadcast`):**
1. Apply happy-path: все три поля отличаются → три write + два broadcast, проверка порядка через счётчики в моках.
2. Apply: `new == current` → `Noop`, ничего не вызывается.
3. Apply: только `locale_name` отличается → один write + broadcast, `geo_nation` и `kbd_preload` не тронуты.
4. Apply: `kbd_preload` пустой → пишем пустой REG_MULTI_SZ (двойной null-terminator).
5. Revert happy-path: восстановление всех трёх полей + broadcast.
6. Revert: `original` с пустыми строками → пишем пустые (не пытаемся угадать дефолт).
7. Broadcast timeout → warning, не ошибка (мок возвращает timeout, аплайер продолжает работу).
8. Registry read failure → `ApplyError::RegistryReadFailed`, broadcast не вызывается.

**Smoke (manual, `#[ignore]`, под обычным юзером — не требует админа):**
```rust
// 1. Запоминаем текущие locale/geo/preload
// 2. Применяем "en-US", "840", ["00000409"]
// 3. Проверяем реестр (reg query или PowerShell)
// 4. Проверяем, что Intl.DateTimeFormat в свежем Chrome меняется (ручная проверка)
// 5. Revert
// 6. Проверяем возврат
```
Smoke печатает чек-лист, оператор проходит вручную.

## Зависимости

- `windows` crate: features `Win32_System_Registry`, `Win32_UI_WindowsAndMessaging`, `Win32_Foundation` (уже подключены на этапе 4).
- Переиспользуем существующий `RegistryReader/Writer` из этапа 4, добавляем специфичные методы для HKCU.

## Что НЕ делать

- Не менять UI language (интерфейс Windows) — требует перелогина, оставляем TODO.
- Не интегрировать с `watcher` или `geo` (этап 6).
- Не писать оркестратор.
- Не использовать `SetThreadLocale` или `SetThreadUILanguage` — эти функции влияют только на текущий поток, не на систему.
- Не использовать `tzutil` или другие CLI-утилиты.

## Гейты / DoD

- `cargo build --workspace` ✅
- `cargo test --workspace` ✅ (все unit-тесты зелёные, smoke ignored)
- `cargo clippy --workspace --all-targets -D warnings` ✅
- `cargo fmt --all -- --check` ✅
- `locale-smoke` вручную под обычным юзером: locale/geo/preload меняются, `Intl.DateTimeFormat` в Chrome подхватывает (частично), после revert — всё возвращается.
- `docs/specs/stage-05.md` в diff, README крейта с ограничением "UI language — TODO".

## На ревью буду смотреть

1. **REG_MULTI_SZ парсинг**: двойной null-terminator корректно обрабатывается? Пустой `kbd_preload` → `[""]` или `[]`? (должно быть `[]`, т.к. REG_MULTI_SZ с двойным null = пустой список).
2. **Broadcast timeout**: если `SendMessageTimeout` возвращает `0` (timeout) — это warning, не ошибка? (некоторые окна могут не отвечать, это нормально).
3. **Частичный apply failure**: если `write_geo_nation` падает после `write_locale_name` — нужно ли откатывать `locale_name`? (ТЗ не требует, но если делаете — тестируйте).
4. **Revert с неизвестным `original`**: если `original.locale_name` не найден в реестре при revert — что делаем? (Рекомендую: `ApplyError::RegistryReadFailed`, не пытаемся угадать).
5. unsafe только в `sys/`, workspace `deny` enforcement (проверено на этапах 3-4).
