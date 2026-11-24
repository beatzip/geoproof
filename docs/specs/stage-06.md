# ТЗ: Этап 6 — Tray + автозапуск (Rust, Windows)

**Контекст**: workspace (`shared`, `watcher`, `geo`, `snapshot`, `apply`), toolchain `stable-x86_64-pc-windows-gnu`, правило unsafe = deny. Этап 6 — **первый пользовательский интерфейс и оркестрация**. На этом этапе всё собирается в работающий продукт: watcher видит IP → geo определяет регион → snapshot сохраняет оригиналы → apply применяет настройки; при выходе или отключении VPN — restore. Сохрани этот текст как `docs/specs/stage-06.md`, включи в diff.

## Архитектурная развилка: tray с админ-правами vs разделение

Вариант «tray без прав + планировщик с highest» выглядит чисто, но создаёт проблему: tray, запущенный обычным юзером, не может вызвать `SetDynamicTimeZoneInformation` (требуется `SE_TIME_ZONE_NAME` privilege) — а он есть только у процесса с админ-токеном. Планировщик с `RL=HIGHEST` запускает **весь** процесс с админкой, и тогда tray и логика — один бинарник с админ-правами.

**Решение этапа 6 (временное):** один бинарник `vpose.exe` запускается через Task Scheduler с `RL=HIGHEST` при logon. Tray-иконка рендерится в контексте админа — это компромисс, но штатный (многие утилиты так работают). **Разделение tray/service** — на этапе 7 (named pipe, allowlist). Это зафиксировать в README: "этап 6 = monolith, этап 7 = split".

## Структура

```
crates/app/
  Cargo.toml                 # добавить в workspace members
  src/
    main.rs                  # entrypoint, signal handling, orchestration loop
    tray.rs                  # TrayController: иконка, меню, события
    orchestrator.rs          # Orchestration: watcher → geo → snapshot → apply
    autostart.rs             # Task Scheduler setup/teardown
    shutdown.rs              # graceful shutdown coordinator
    sys/
      mod.rs                 # #[allow(unsafe_code)]
      scheduler.rs           # schtasks /Create /Delete через COM TaskScheduler
      console.rs             # Ctrl+C / Ctrl+Break / CloseWindow handlers
      single_instance.rs     # named mutex для "один процесс"
  assets/
    icon.ico                 # tray-иконка (32x32 + 16x16, минимальный размер)
  tests/
    orchestration.rs         # интеграционные тесты с моками всех крейтов
  src/bin/vpose_autostart.rs # helper для schtasks /Create /Delete вручную
```

## Orchestration loop (`main.rs` + `orchestrator.rs`)

```rust
pub struct Orchestration {
    ip_watcher: IpWatcher,
    geo_client: GeoClient,
    snapshot: Snapshot,
    tz_applier: TimezoneApplier,
    locale_applier: LocaleApplier,
    tz_registry: WindowsTzRegistry,
    current_region: Option<RegionState>,  // None = "native"
}

impl Orchestration {
    pub async fn run(&mut self, mut events: tokio::sync::broadcast::Receiver<IpEvent>) {
        while let Ok(event) = events.recv().await {
            match event {
                IpEvent::Connected(ip) | IpEvent::Changed(_, ip) => {
                    self.apply_for_ip(ip).await;
                }
                IpEvent::Disconnected => {
                    self.restore_to_native().await;
                }
            }
        }
    }
}
```

**`apply_for_ip(ip)`:**
1. `geo.lookup(ip)` → `GeoInfo { country_code, timezone, languages }`.
2. Если lookup упал (ошибка сети/rate-limit) — warning, ничего не применяем, сохраняем `current_region`.
3. `tz_registry.iana_to_windows(&info.timezone)` → `Option<TimezoneValue>`.
4. Если `None` — warning ("unknown TZ"), не применяем TZ (но можем применить locale).
5. **Save-once** через `snapshot.save()` — работает только при первом применении.
6. **Capture service state** через `tz_applier.capture_service_state()` — обновляем `TimezoneValue.service_state`.
7. `snapshot.record_applied("timezone", &tz_value)`.
8. `tz_applier.apply(&tz_value)`.
9. Аналогично для `locale_applier`: формируем `LocaleValue` из `country_code` (маппинг ISO 3166-1 alpha-2 → numeric: таблица в коде, 250+ записей), `languages[0]` → `locale_name`, `kbd_preload` из существующего (не меняем пока — это TODO).
10. Логируем через `tracing`: что применили, какие поля изменились.

**`restore_to_native()`:**
1. `snapshot.load_snapshot()` → если нет, no-op.
2. `restore_plan()` по каждому полю, применяем через applier.
3. `complete_restore()` — удаляет snapshot.
4. `current_region = None`.

## Startup decision (на этапе 3 зафиксировано в snapshot)

При старте вызываем `snapshot.startup_decision()`:
- `RestoreNeeded` → запускаем restore **до** watcher, иначе первое IP-событие перезапишет оригиналы.
- `LockHeld` (живой process) → panic с понятным сообщением (single-instance защита).
- `NoSnapshot` / `LockStale` без snapshot → идём в обычный цикл.

Важно: **restore выполняется синхронно до запуска watcher**, иначе race: watcher увидел IP → geo → save (перезаписал оригиналы).

## Tray-иконка (`tray.rs`)

**Зависимость:** `tray-icon` crate (легковесный, cross-platform, работает с `windows` crate) или `tao` (более тяжёлый, но даёт window + event loop). Рекомендую `tray-icon` + `tokio` event loop через `channel` — без GUI-цикла.

**Меню:**
- `Status: connected (FR) / disconnected / applying...` — read-only, обновляется из orchestration.
- `Restore now` → вызывает `restore_to_native()` вручную.
- `Toggle auto-apply` → вкл/выкл реакции на IP events (флаг в Orchestration).
- `Open logs` → `ShellExecute` открывает папку `%LocalAppData%\vpose\logs\`.
- `Exit (restore first)` → graceful shutdown: restore + выход.
- `Exit (no restore)` — опционально, только в debug-сборке (не в release).

**Иконка-состояние** (3 варианта, 3 файла в `assets/`):
- `icon_native.ico` — disconnected/native.
- `icon_vpn.ico` — применяется регион VPN.
- `icon_error.ico` — последняя операция упала.

Обновление иконки через `tray_icon.set_icon()` из главного event loop.

## Graceful shutdown (`shutdown.rs`)

**Источники сигнала:**
- Ctrl+C / Ctrl+Break — `SetConsoleCtrlHandler` (WinAPI).
- Закрытие окна/tray-menu "Exit" — внутренний shutdown-канал.
- Windows logoff / shutdown — `WM_QUERYENDSESSION` / `WM_ENDSESSION` (если есть hidden window; tray-icon сам по себе не даёт этого — добавить минимальный hidden window через `windows` crate).

**Shutdown coordinator:**
```rust
pub struct ShutdownCoordinator {
    shutdown_tx: tokio::sync::broadcast::Sender<()>,
    restore_tx: tokio::sync::oneshot::Sender<()>,  // signal "restore complete"
}
```

**Логика:**
1. Любой источник сигнала → `shutdown_tx.send(())`.
2. Orchestration loop выходит (recv возвращает Err или select! с shutdown-каналом).
3. **Всегда** restore перед выходом (независимо от текущего региона).
4. После restore — `restore_tx.send(())` → main ждёт подтверждения или таймаут (5 с).
5. Exit 0.

**Почему всегда restore:** если процесс умер без restore, stage 3 (heartbeat) обработает это при следующем старте. Но graceful shutdown обязан restore сам — это требование этапа 1/3.

## Автостарт через Task Scheduler (`autostart.rs`)

**API:** COM-интерфейс `ITaskService` / `ITaskFolder` / `ITaskDefinition` из `windows` crate (features `Win32_System_TaskScheduler`). Либо CLI-обёртка через `schtasks.exe` — **предпочитаю CLI** для простоты и отлаживаемости, COM оставим на потом.

**Команды:**
```
vpose.exe --autostart install    # schtasks /Create /TN "VPosE" /TR "\"<path>\" --run" /SC ONLOGON /RL HIGHEST /F
vpose.exe --autostart uninstall  # schtasks /Delete /TN "VPosE" /F
vpose.exe --autostart status     # schtasks /Query /TN "VPosE" /FO CSV
vpose.exe --run                  # запуск от имени задачи (скрыто от пользователя? нет, tray нужен UI)
```

**Особенности:**
- `schtasks` требует админки для `/RL HIGHEST` — `autostart install` вызывает UAC (ShellExecute `runas` для `vpose.exe` самого себя с аргументами install).
- Путь к исполняемому файлу — `std::env::current_exe()`, экранирование пробелов.
- Задача не должна быть скрытой (tray нужен UI-session) — `/IT` (interactive) для совместимости.
- `--run` проверяет single-instance (named mutex) — если второй инстанс, exit 1 с понятным сообщением.

## Single-instance защита (`sys/single_instance.rs`)

**Механизм:** named mutex `"Global\\VPosE_Instance_Mutex"`. Если `CreateMutexW` возвращает `ERROR_ALREADY_EXISTS` — exit с кодом 1 и сообщением в stderr/log. Это решает race при ручном запуске + автозапуске.

## Логирование в файл

- Путь: `%LocalAppData%\vpose\logs\vpose.log`.
- `tracing-subscriber` + `tracing-appender::rolling::daily` — rotation по дням, keep 7 days.
- Уровень: `info` по умолчанию, `debug` через env `RUST_LOG`.
- Все ошибки apply/restore — отдельный warning с деталями.

## Тесты

**Offline (моки всех крейтов):**
1. Orchestration: `Connected(IP)` → последовательность вызовов geo → snapshot.save → snapshot.record → tz_applier.apply → locale_applier.apply.
2. Orchestration: `Disconnected` → restore_to_native → complete_restore.
3. Orchestration: geo упал → warning, ничего не применено.
4. Orchestration: TZ unknown → warning по TZ, locale всё равно применяется.
5. Startup: `RestoreNeeded` → restore выполняется **до** watcher.start.
6. Startup: `LockHeld` → panic с понятным сообщением.
7. Shutdown: graceful через Ctrl+C → restore вызван, process ждёт restore или таймаут.
8. Shutdown: graceful через tray-menu → то же.
9. Autostart: `--autostart status` возвращает 0 если задача есть, 1 если нет (мок `schtasks`).
10. Single-instance: второй процесс → exit 1.

**Smoke (manual, `#[ignore]`):**
```rust
// 1. vpose.exe --autostart install (под админом)
// 2. Logoff + logon → vpose запустился, tray виден
// 3. Включение VPN → tray меняет иконку, TZ меняется (timedate /t)
// 4. Выключение VPN → TZ возвращается
// 5. "Exit (restore first)" → процесс выходит, TZ в native
// 6. vpose.exe --autostart uninstall
```
Чек-лист печатается, оператор проходит.

## Зависимости (workspace)

- `tray-icon` или `tao` — tray/UI
- `tokio` с фичами `rt-multi-thread`, `macros`, `sync`, `signal`, `time`, `io-util`
- `tracing`, `tracing-subscriber`, `tracing-appender`
- `windows` crate: `Win32_System_TaskScheduler`, `Win32_System_Threading` (CreateMutex), `Win32_UI_WindowsAndMessaging` (WM_* handlers), `Win32_System_Console` (SetConsoleCtrlHandler)
- `thiserror`, `serde`/`serde_json` (для конфига)

## Что НЕ делать

- Не разделять tray/service (этап 7).
- Не делать GUI-окна настроек (минимум — tray menu).
- Не менять язык UI Windows (TODO этап 8).
- Не использовать `runas` для повышения прав в рантайме (задача в планировщике уже с highest).

## Гейты / DoD

- `cargo build --workspace` ✅
- `cargo test --workspace` ✅ (все unit-тесты, smoke ignored)
- `cargo clippy --workspace --all-targets -D warnings` ✅
- `cargo fmt --all -- --check` ✅
- Smoke: logoff/logon → автозапуск работает, включение/выключение VPN → TZ меняется и возвращается, graceful exit → TZ в native, uninstall → задача удалена.
- `docs/specs/stage-06.md` в diff.
- README проекта с полной инструкцией: установка → автозапуск → использование → удаление.

## На ревью буду смотреть

1. **Restore до watcher** — критично. Если startup-decision делается после watcher.start, race-condition потеряет оригиналы.
2. **Always-restore на graceful shutdown** — нет флага "restore only if applied", всегда restore при выходе (snapshot сам защитит от no-op если ничего не было).
3. **Single-instance** — named mutex в `Global\\`, не Local, иначе второй юзер в мульти-сессии сможет запустить второй инстанс.
4. **Shutdown timeout** — если restore повис (5 с), process всё равно exit 1 с сообщением. Не должно быть бесконечного hang.
5. **Hidden window для WM_ENDSESSION** — без него logoff/shutdown Windows не отправит сигнал процессу, и graceful shutdown не сработает. Либо hidden window, либо альтернативный механизм.
6. **Tray иконка в контексте админа** — проверить, что UAC-окно при autostart install не показывается каждый logon (задача с highest privileges не требует UAC при запуске, только при создании).
7. unsafe только в `sys/`, workspace `deny` enforcement.

---

## Реализация — зафиксированные решения (этап 6, исполнено)

- **service_state в snapshot**: FieldValue::Timezone расширен опциональным полем service_state: Option<TzServiceState> (serde default, skip если None; TzServiceState — зеркало pply::TzAutoUpdateState без зависимости snapshot→apply). Applied-запись TZ несёт pre-apply состояние службы из capture_service_state(); при restore оно подставляется в revert-значение. Старые v1-JSON без поля парсятся (backward-compat, тест legacy_applied_timezone_without_service_state).
- **Сравнение applied с current в restore_plan**: matches_current — для Timezone сравниваются только (key_name, dst_disabled), т.к. reader не отдаёт service_state (полное PartialEq никогда не был бы равен → TZ всегда выглядела бы UserChanged).
- **record_applied — после успешного apply**, не до. Для locale — только реально изменённые поля (из AppliedChange::LocaleChanged.changed_fields).
- **Restore частично**: complete_restore() (удаление snapshot) только если ВСЕ revert успешны; иначе snapshot сохраняется и restore повторится на следующем старте.
- **kbd_preload не меняется** (этап 8 TODO): LocaleValue.kbd_preload = текущее системное значение.
- **Unknown IANA timezone**: TZ пропускается (warning), locale всё равно применяется.
- **dst_disabled = true** для VPN-значения (сервис tzautoupdate отключён на время применения).
- **Трей**: 	ray-icon 0.26.1 + выделенный поток-качалка сообщений (PeekMessageW); команды в оркестратор — std mpsc (поллинг 200 мс), события меню — muda::MenuEvent::receiver(); hidden window (top-level, не message-only) ловит WM_QUERYENDSESSION/WM_ENDSESSION для logoff/shutdown.
- **Автостарт**: CLI schtasks (не COM): /Create /TN VPosE /TR "\"<exe>\" --run" /SC ONLOGON /RL HIGHEST /F; install без админа → ShellExecuteW runas самого себя; single-instance — named mutex Global\VPosE_Instance_Mutex.
- **Логи**: %LOCALAPPDATA%\vpose\logs\vpose.log, tracing-appender rolling daily, max 7 файлов, уровень из RUST_LOG (по умолчанию info).
- **unsafe**: workspace unsafe_code = deny переопределён в crates/app на llow с явным переобъявлением clippy-линов; unsafe живёт только в pp::sys (console/session_window/shell/single_instance/scheduler).
- **Store.create_dir_all**: save_snapshot/write_lock создают базовую директорию (первый запуск без %LOCALAPPDATA%\vpose).
- **Тесты**: 20 offline-сценариев в crates/app/tests/orchestration.rs (моки всех швов; tempdir в target/test-tmp); сценарий 9 — реальный schtasks /Query по заведомо отсутствующему имени задачи; сценарий 10 — двойной SingleInstance::acquire в процессе. Два регрессионных теста закрывают дефекты ревью H6 и H10 (watchdog сторожевого таймаута при медленном restore; перезапись applied-записи при откате на повторном применении).
- **Restore в рабочем потоке (ревью H6)**: restore_to_native() выполняется в std::thread через apply::block_on с oneshot-каналом результата; на пути graceful shutdown (консоль/сессия/tray-exit) цикл ждёт через tokio::time::timeout(RESTORE_WATCHDOG, 5 с) и при таймауте ставит restore_timed_out (код выхода 1). Аплаеры шарятся с рабочим потоком через tokio::sync::Mutex (guard — Send); RESTORE_HANDLER_WAIT обработчиков (15 с) заведомо больше сторожа. WindowsTzRegistry владеет WindowsRegistry (значит 'static и доступен рабочему потоку).
- **Перезапись applied-записи при откате (ревью H10)**: при отказе locale на повторном применении rollback_timezone возвращает систему к предыдущему применённому региону, и applied-запись TZ перезаписывается снимком ДО применения (или стирается через Engine::clear_applied, если записи не было) — restore_plan больше не видит ложный UserChanged и complete_restore не удаляет снапшот с чужим поясом. Для locale-полей — аналогичный перезапись/очистка по трём полям после успешного revert.
- **Родная сеть по запомненным IP (ревью C1/D3)**: при старте первый наблюдённый IP записывается в Snapshot::native_ips (serde default, старые снапшоты читаются). Признак родной сети — равенство IP запомненному; гео-совпадение с оригиналами применяется, только если IP не совпал. При совпадении гео-данных с оригиналами статус Native, а не Connected. Известный трейдофф: машина, загруженная с уже поднятым VPN (autostart), остаётся Native до смены сети (поведение HEAD, принято ревью). Память IP живёт в снапшоте и пересоздаётся пустой после restore до следующего домашнего наблюдения.
- **service_state записывается как есть (ревью H4/D4)**: при повторном применении, если в существующей applied-записи service_state отсутствует, в новую запись переносится None (не текущее состояние службы) — restore не оставляет службу отключённой.
- **Прерывание гео-запроса при завершении (ревью H9/D5)**: гео-lookup оборачивается в tokio::select! с wait_shutdown_requested (глобальный флаг SHUTDOWN_REQUESTED из shutdown::request + тестовый probe); при завершении запрос бросается, apply не выполняется — обработчик не ждёт до ~23,5 с и процесс не выходит с кодом 1 без причины. console- и WM_ENDSESSION-обработчики ставят флаг через shutdown::request у источника.
- **Линты крейтов**: shared/watcher/geo/snapshot объявляют собственную таблицу [lints] (unsafe_code = deny; clippy undocumented_unsafe_blocks = deny, unwrap_used = allow — фикстуры и тесты), предупреждение cargo о неунаследованных workspace-линах устранено. Файлы .rs в worktree — LF (*.rs text eol=lf в .gitattributes).
