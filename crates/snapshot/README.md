# snapshot

Stage 3 of the GeoProof restore pipeline: capture the four user-visible
Windows settings the tool changes, remember what was applied, and compute a
restore plan. **This crate never writes to the system** — it only reads state
and stores its own files; the actual applier/restore executor arrives in later
stages and drives this crate's engine.

## Fields captured

| Field        | Source                                                     |
|--------------|------------------------------------------------------------|
| timezone     | GetDynamicTimeZoneInformation (key name + DST-off flag)    |
| locale_name  | HKCU Control Panel\International -> LocaleName             |
| geo_nation   | HKCU Control Panel\International\Geo -> Nation            |
| kbd_preload  | HKCU Keyboard Layout\Preload values, sorted by name       |

## Storage

- Snapshot: %LocalAppData%\vpose\snapshot.json, schema version 1:
  {"version": 1, "created_utc": "...", "fields": {...}}, every field carrying
  original and nullable applied. Writes are atomic (temp file + rename); a
  failed write leaves the previous file intact and removes the temp.
- Lock: %LocalAppData%\vpose\snapshot.lock with pid + heartbeat_unix.
- The base directory is injectable (Store::new(base)), which the tests use.

## Engine semantics

- save is save-once: an unrestored snapshot is never overwritten, so a second
  start cannot clobber the originals.
- record_applied(field, value) marks what the applier actually set.
- restore_plan(current), per field:
  - applied == null -> skipped silently (we never touched it);
  - current == applied -> planned for restore to original;
  - current != applied -> skipped with a UserChanged warning (the user
    changed it themselves; we do not clobber).
- complete_restore() removes snapshot and lock; a repeated plan is empty
  (idempotent).
- startup_decision returns RestoreNeeded only when a lock exists, its pid is
  dead or its heartbeat is stale (default threshold: 60 s), and a snapshot
  exists.

## unsafe

The only unsafe in the crate lives in src/sys/ (thin Win32 FFI wrappers, each
block justified by a // SAFETY: comment). Every other module is unsafe-free.

## Tests

cargo test -p snapshot runs the offline suite (mocks + workspace-local
tempdirs, no network, no side effects). The Windows read smoke test is
ignored by default:

    cargo test -p snapshot --test windows_smoke -- --ignored

The read-only demo binary prints the current state as JSON:

    cargo run -p snapshot --bin snapshot-demo

Cross-check its output against tzutil /g, Get-WinHomeLocation and the
registry keys listed above.
