# apply

Reversible appliers: change Windows system state and roll it back.
Stage 4 scope: the timezone (`TimezoneApplier`) + the `tzautoupdate`
service state. Stage 5 scope: the locale field group (`LocaleApplier`):
regional format (`locale_name`), geo nation (`geo_nation`) and the
ordered keyboard preload list (`kbd_preload`) - all under HKCU, so no
elevation is required.

## Locale (stage 5): known limitations

- **UI language (Windows display language) is NOT changed** - TODO for
  stage 8 or a separate feature: it requires an installed language pack
  and a re-login, so it is not instantly applicable.
- Browsers cache `Intl.DateTimeFormat` until restarted; the
  `WM_SETTINGCHANGE` broadcast is picked up only partially. `apply`
  returns `restart_hint: ["Chrome", "Firefox"]` for that reason.
- A broadcast timeout on individual windows is a logged warning, never an
  error: the registry writes are already committed at that point.

## Snapshot integration: variant A (fixed)

The applier never touches the `snapshot` crate. The orchestrator (stage 6)
must call `TimezoneApplier::capture_service_state()` **before** `apply` and
store the result in `TimezoneValue.service_state` via
`Snapshot::record_applied`. If the orchestrator forgets, the snapshot keeps
`service_state: None` and revert will not touch the service (safe default,
see below).

Variant B (service state returned inside `AppliedChange`) was rejected:
it couples the change-report type to capture ordering and encourages
forgetting the capture anyway.

## Semantics

- `apply` is idempotent: `new == current` -> `AppliedChange::Noop`, zero
  side effects.
- Apply order: admin check -> read current -> capture service state ->
  disable+stop `tzautoupdate` -> read DST rules of the new zone ->
  `SetDynamicTimeZoneInformation`.
- A failure after the service was touched rolls the service back
  automatically before the error is returned.
- `revert` order: admin check (refuse silently when absent) ->
  `SetDynamicTimeZoneInformation` with `original` (rules re-read from the
  registry, never stored) -> restore `tzautoupdate`.
- **`original.service_state = None`** (a v1 snapshot): the service is NOT
  touched. Guessing `Enabled` could switch on something the user had
  disabled; "do not know -> do not touch" is the safer reading.
- Revert with `service_state = Some(Enabled)` sets `SERVICE_AUTO_START`
  (the Windows default for this service) and starts the service.

## unsafe rule

unsafe is allowed only in `src/sys/` (thin FFI wrappers, every block
commented `// SAFETY:`); every other module and the whole crate surface
carry `#![forbid(unsafe_code)]`. Workspace-level `unsafe_code = deny` is
declared in the root `[workspace.lints]`; this crate opts in via
`[lints] workspace = true`.

## Running the smoke checklist

```
cargo build -p apply --release
# elevated shell:
target/release/apply-smoke.exe
```

The binary applies `Line Islands Standard Time` (UTC+12), then reverts.
Follow the printed checklist and verify with `tzutil /g`, `Get-Date`,
and `sc qc tzautoupdate`.
