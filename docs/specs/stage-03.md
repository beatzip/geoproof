# Этап 3 — крейт `snapshot` (Rust, Windows)

**Контекст**: workspace (`shared`, `watcher`, `geo`), toolchain `stable-x86_64-pc-windows-gnu`.
Приложение меняет настройки Windows под VPN и обязано вернуть всё назад при отключении/выходе/аварии.
Этап 3: захват текущего состояния, персистентность, логика восстановления.
**Этап строго read-only: никаких изменений системы.** Апплаеры появятся на этапах 4–5, поэтому snapshot
не исполняет восстановление, а вычисляет *план* восстановления.

## Архитектурное решение

`snapshot` — **синхронная** библиотека (без tokio): движок + персистентность + примитивы crash-detection.
Весь доступ к ОС — за трейтами, Windows-impl тонкий, тесты на моках. Крейт не зависит от `watcher`/`geo`.

```
crates/snapshot/
  src/lib.rs
  src/model.rs      # SystemState, FieldSnapshot, схема v1
  src/engine.rs     # save-once, record_applied, restore_plan, complete_restore
  src/store.rs      # JSON-файл, атомарная запись, lock-файл
  src/traits.rs     # StateReader, ProcessLiveness, Clock
  src/sys/mod.rs    # ЕДИНСТВЕННОЕ место unsafe: Windows FFI
  src/bin/snapshot_demo.rs
```

## Изменение правила unsafe (действует с этапа 3 и далее)

Плановое «без unsafe» для Windows API невыполнимо — FFI в `windows` crate это `unsafe fn`.
Новое правило: **unsafe разрешён только в `src/sys/`**, тонкие обёртки, каждый блок с комментарием
`// SAFETY:`; остальные модули — `#![forbid(unsafe_code)]` на уровне модуля;
clippy `undocumented_unsafe_blocks` = deny, `unsafe_op_in_unsafe_fn` = deny.
Вся бизнес-логика — безопасный Rust на моках.

## Модель и схема (версионируется)

```json
{ "version": 1, "created_utc": "...",
  "fields": {
    "timezone":    { "original": { "key_name": "Russian Standard Time", "dst_disabled": false }, "applied": null },
    "locale_name": { "original": "ru-RU", "applied": null },
    "geo_nation":  { "original": "203", "applied": null },
    "kbd_preload": { "original": ["00000409", "00000419"], "applied": null }
  } }```

Читаем ровно то, что будем писать на этапах 4–5: TZ через `GetDynamicTimeZoneInformation`
(берём `TimeZoneKeyName` + `DynamicDaylightTimeDisabled`), остальное — реестр:
`HKCU\Control Panel\International\LocaleName`,
`HKCU\Control Panel\International\Geo\Nation`,
`HKCU\Keyboard Layout\Preload` (упорядоченный список).
Features `windows`: `Win32_System_Time`, `Win32_System_Registry`, `Win32_System_Threading`, `Win32_Foundation` — минимум.
Чтение не требует админки.

## Семантика движка

- **save-once**: `save()` при существующем невосстановленном snapshot — no-op (оригиналы не перезатираются между стартами).
- **record_applied(field, value)** — вызывается апплаером (этап 4+) после успешной смены.
- **restore_plan(current) -> Vec<RestoreItem>** по полям: `applied == null` → пропуск (не трогали);
  `current == applied` → в план (вернуть original); `current != applied` → пропуск + warning (пользователь сам поменял — не затираем).
- **complete_restore()** — удалить snapshot и lock; повторный `restore_plan` → пустой (идемпотентность).
- Неизвестная `version` → `SnapshotError::VersionMismatch`, файл не трогать.

## Персистентность и crash-detection

- Путь: `%LocalAppData%\vpose\snapshot.json` (решение зафиксировано: до этапа 7 один процесс;
  переезд в `%ProgramData%` — на этапе 7). Базовый каталог инжектируется (тесты — tempdir).
- Атомарная запись: temp-файл в том же каталоге + `fs::rename`. Ошибка записи не повреждает старый файл.
- Lock-файл: pid + heartbeat-метка. `touch_heartbeat()` — примитив (писать будет демон на этапе 6).
  При старте: lock есть && (pid мёртв || heartbeat протух, порог конфигурируемый, дефолт 60 с) &&
  snapshot не восстановлен → движок возвращает `RestoreNeeded`.
  Liveness за трейтом (Windows: `OpenProcess` + `GetExitCodeProcess`, учти риск переиспользования pid —
  heartbeat решает). Исполнение плана — этапы 4+, здесь только решение.

## Тесты (offline, моки StateReader/Liveness/Clock, tempdir)

1. Круговой: save → mock-смена + record_applied → plan → mock-исполнение → состояние == оригиналу; повторный plan пуст.
2. Грязное состояние: пользователь поменял поле после applied → план его пропускает, warning.
3. Нетронутое поле (`applied: null`) → в план не попадает.
4. save-once: повторный save не затирает оригиналы.
5. `version: 99` → VersionMismatch, файл цел.
6. Протухший heartbeat / мёртвый pid → RestoreNeeded; живой pid + свежий heartbeat → нет.
7. Атомарность: при ошибке сериализации старый файл нетронут, temp убран.
8. Windows-чтение: smoke-тест `#[ignore]` + ручной чек-лист: `snapshot_demo` печатает JSON, сверить с
   `tzutil /g`, `Get-WinHomeLocation`, реестром. Демо read-only.

## Гейты / DoD

`build / test --workspace / clippy --all-targets -D warnings / fmt --check` зелёные;
тесты без сети и без сайд-эффектов; `docs/specs/stage-03.md` в diff; README крейта:
путь хранения, семантика restore, правило unsafe. Не трогать `watcher`/`geo` (кроме members),
не интегрировать watcher, не писать в систему.