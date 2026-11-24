# Этап 2 — крейт `geo`: GeoClient + таблица IANA → Windows (Rust, Windows)

**Контекст**: workspace из этапа 1 (крейты `shared`, `watcher`, toolchain `stable-x86_64-pc-windows-gnu`). Приложение меняет системные настройки Windows под выходной IP VPN. Этап 2: по IP получаем страну, IANA-пояс, языки; плюс конвертация IANA → имя пояса Windows. **Никаких изменений системы, никакого взаимодействия с `watcher`** — интеграция позже.

### Структура
```
crates/geo/
  Cargo.toml            # + добавить "crates/geo" в workspace members
  build.rs
  cldr/windowsZones.xml # закоммиченный, pinned
  src/lib.rs
  src/provider.rs       # GeoProvider, GeoInfo, GeoError
  src/ipapi.rs          # IpApiCo + трейт HttpFetch + reqwest-impl
  src/cache.rs
  src/tz_windows.rs     # include! сгенерённого + публичный API + алиасы
  examples/geo_demo.rs  # опционально, живой lookup для ручной проверки
```
`watcher` и `shared` не трогать (кроме `members` в корне). Зависимости переиспользовать из workspace: `tokio`, `reqwest` (**rustls-tls**, без native-tls — openssl на gnu-цепочке не нужен), `serde`/`serde_json`, `thiserror`, `async-trait`, `tracing`. Build-dep: `quick-xml`.

### 1. GeoClient
```rust
pub struct GeoInfo {
    pub country_code: String,   // ISO 3166-1 alpha-2, нормализовать в uppercase
    pub timezone: String,       // IANA, как вернул провайдер
    pub languages: Vec<String>, // BCP-47, порядок провайдера сохранять
}
#[async_trait]
pub trait GeoProvider: Send + Sync {
    async fn lookup(&self, ip: IpAddr) -> Result<GeoInfo, GeoError>;
}
```
- Реализация `IpApiCo`: `GET https://ipapi.co/{ip}/json/`, явный `User-Agent: vpose/0.1`, таймаут 5 с.
- **Парсинг** (сверено с докой ipapi.co): `country_code` — строка; `timezone` — строка; `languages` — **строка через запятую** → split(','), trim, пустые отбросить; отсутствует/null → пустой вектор, не ошибка. Ответ может содержать `"error": true, "reason": "..."` **даже с HTTP 200** (Invalid/Reserved IP) → `GeoError::Provider(reason)`, без ретрая. Невалидный `country_code` (не 2 латинские буквы) или пустой `timezone` → `GeoError::Parse`.
- **Ретраи**: 3 попытки поверх первой, бэкофф 0.5/1/2 с. Ретраим: транспортные ошибки (timeout/connect), HTTP 429, HTTP 5xx, `reason: "RateLimited"`. Не ретраим: прочие 4xx, `Provider`, `Parse`.
- **HTTP за абстракцией**: `trait HttpFetch { async fn get(&self, url: &str) -> Result<HttpResponse, HttpError> }`, `HttpResponse { status: u16, body: String }`. Reqwest-impl — в проде, мок с очередью ответов и счётчиком вызовов — в тестах. **Тесты без сокетов вообще.**
- **Кэш**: `tokio::sync::Mutex<HashMap<IpAddr, (GeoInfo, tokio::time::Instant)>>`, TTL 24 ч из конфига. Важно: именно `tokio::time::Instant` — `std::time::Instant` не паузится под `tokio::time::pause`, TTL-тест станет флаки. Кэшируем только успехи.
- Конфиг: `GeoClientConfig { timeout, retries, backoff_base, cache_ttl }` с дефолтами выше. Ошибки — `thiserror`, варианты минимум: `Http`, `Status(u16)`, `RateLimited`, `Parse(String)`, `Provider(String)`.
- Механику `pause/advance` бери из проверенного харнесса этапа 1, не изобретай заново; в тестах ретраев проверяй число вызовов мока и суммарную задержку.

### 2. Таблица IANA → Windows (build-time)
- **Источник**: закоммиченный `cldr/windowsZones.xml`, скачанный с **релизного тега или конкретного commit SHA** (`https://raw.githubusercontent.com/unicode-org/cldr/<ref>/common/supplemental/windowsZones.xml`), не с `main`. В README зафиксировать: ref, дату, атрибуты `otherVersion`/`typeVersion` из файла.
- **build.rs**: по умолчанию читает только локальный XML (offline-сборка воспроизводима, сети нет). `CLDR_DOWNLOAD=1` → скачивает через системный `curl.exe` (штатный в Win10/11), перезаписывает `cldr/windowsZones.xml` (обновление потом коммитится осознанно); при отсутствии curl — понятная ошибка. `main() -> Result<...>`, без паник. `rerun-if-changed` на XML, `rerun-if-env-changed=CLDR_DOWNLOAD`.
- **Алгоритм маппинга (двухпроходный)**:
  1. Все `mapZone` с `territory="001"`: каждый токен `type` (их может быть несколько через пробел) → `other`.
  2. Остальные территории (включая `ZZ`) в порядке файла: добавляем только ещё не смапленные зоны. `ZZ` обязателен — иначе потеряется `UTC → UTC` и `Etc/*`.
  3. Конфликт (зона уже смаплена на другое имя Windows): оставляем существующее (001 приоритет), `cargo:warning`, считается в статистику.
- **Генерация**: `$OUT_DIR/tz_windows_generated.rs` — отсортированный `static &[(&str, &str)]` + `iana_to_windows(&str) -> Option<&'static str>` через binary search. Плюс константы статистики: всего уникальных IANA в XML, смаплено, список конфликтов. Сгенерённый `.rs` **не коммитим**.
- **Таблица алиасов modern → legacy** (в исходниках, не в XML; XML использует старые написания tzdb — сверено). Минимальный обязательный набор:
  `Asia/Kathmandu→Asia/Katmandu`, `Europe/Kyiv→Europe/Kiev`, `America/Nuuk→America/Godthab`, `Asia/Kolkata→Asia/Calcutta`, `Asia/Yangon→Asia/Rangoon`, `Asia/Ho_Chi_Minh→Asia/Saigon`, `Africa/Asmara→Africa/Asmera`, `Pacific/Chuuk→Pacific/Truk`, `Pacific/Pohnpei→Pacific/Ponape`, `Pacific/Kanton→Pacific/Enderbury`, `America/Indiana/Indianapolis→America/Indianapolis`, `America/Argentina/Buenos_Aires→America/Buenos_Aires`, `America/Kentucky/Louisville→America/Louisville`.
  _LOOKUP_: точное совпадение → при промахе алиас → повторный поиск. Разрешено вместо этого вшивать алиасы в генерённую карту на этапе сборки — на твой выбор, тесты обязательны на оба написания. Список можно расширить, сверяясь с XML, каждое расширение — с тестом.

### 3. Тесты (все offline)
- Маппинги: `Europe/Paris → Romance Standard Time`, `America/New_York → Eastern Standard Time`, `America/Phoenix → US Mountain Standard Time`, `UTC → UTC`, `Asia/Kathmandu → Nepal Standard Time` **и** `Asia/Katmandu → Nepal Standard Time`, `Europe/Kyiv → W. Europe Standard Time` (проверь 001-запись W. Europe в своём XML), каждый алиас из таблицы.
- Неизвестная зона (`Mars/Olympus_Mons`) → `None`, без паники.
- Покрытие: `mapped/total ≥ 0.90` **и** точное равенство закоммиченным снапшот-числам (total, mapped, conflicts) из сгенерённой статистики. Числа обновляются осознанно при бампе CLDR, комментарий в тесте об этом.
- GeoClient на моках: happy path; сетевая ошибка → ретрай → успех (счётчик = 2); 429 → ретрай → успех; исчерпание ретраев → ошибка; malformed JSON → `Parse`; `error:true` с HTTP 200 → `Provider` без ретрая; `languages` отсутствует → `[]`; два lookup одного IP → 1 HTTP-вызов; TTL-истечение под paused clock → 2 вызова.
- Сквозной: мок `GeoProvider` → `GeoInfo { timezone: "Europe/Paris", .. }` → `iana_to_windows` → `"Romance Standard Time"`.

### Качество / гейты
`cargo build`, `cargo test --workspace`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo fmt --all -- --check` — зелёные. Без `unsafe`, без `unwrap` в не-тестовом коде (`expect` — только инварианты старта с сообщением, как в `HttpProvider` этапа 1). README: версия CLDR, процедура обновления (`CLDR_DOWNLOAD=1`), числа покрытия, пример API.

### Что НЕ делать
Не трогать `watcher`/`shared` (кроме members); никаких Windows API, реестра, служб; никакого runtime-парсинга XML; никаких реальных HTTP в тестах.

### DoD
Тесты зелёные без сети; сквозной мок-тест Париж → Romance проходит; `docs/specs/stage-02.md` в diff; сборка из чистого клона без сети работает (offline-путь по умолчанию).
