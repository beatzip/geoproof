# GeoProof

Desktop helper (Windows 10/11, Rust) that watches the machine's external IP
(VPN exit node), detects the country, and applies matching Windows region
settings (timezone, locale). On VPN disconnect or app exit the original
settings are restored automatically.

## Layout

- `crates/shared` — common types ([IpEvent])
- `crates/watcher` — [IpWatcher]: async polling of external-IP providers,
  majority voting, debounce, event emission over a tokio broadcast channel
- `crates/watcher/src/bin/watcher-demo.rs` — demo binary printing events
- `crates/geo` — [GeoClient]: IP → country/IANA timezone/languages lookup
  (offline maxmind-style response parsing + HTTP transport), plus the
  IANA → Windows timezone registry table
- `crates/snapshot` — save-once capture of the original system state and
  crash-safe restore planning (snapshot.json + lock file with heartbeat)
- `crates/apply` — appliers that actually change the system:
  timezone (SetDynamicTimeZoneInformation + tzautoupdate service) and
  locale (HKCU registry + settings broadcast), each with rollback
- `crates/app` — the product binary `vpose.exe`: orchestration loop,
  tray icon, autostart (Task Scheduler), graceful shutdown (stage 6)

## Build

Requires a Rust toolchain (rustup). On Windows the GNU toolchain works
without Visual Studio (dlltool needs an assembler: keep `C:\mingw64\bin`
on PATH if you use the GNU toolchain):

    rustup default stable-x86_64-pc-windows-gnu

Build and test from the repository root:

    cargo build --workspace
    cargo test --workspace
    cargo clippy --workspace --all-targets -- -D warnings
    cargo fmt --all -- --check

## Install / autostart / remove

`vpose.exe` runs elevated (timezone + service changes require an admin
token). Stage 6 ships the monolith: the tray UI and the orchestration
logic live in the same elevated binary; the tray/service split
(named pipe + allowlist) is planned for stage 7.

    vpose.exe --autostart install     # creates the "VPosE" scheduled task
                                      # (ONLOGON, HIGHEST); asks UAC once
    vpose.exe --autostart status      # exit 0 = installed, 1 = not installed
    vpose.exe                         # run now (tray + orchestration)
    vpose.exe --autostart uninstall   # remove the scheduled task

What the task does: at logon Windows starts `vpose.exe --run` with the
highest privileges **without a UAC prompt** (UAC is only asked once, at
`install` time). A second instance of the app is refused via a global
named mutex.

Data lives in `%LOCALAPPDATA%\vpose\`: `snapshot.json` (originals),
`snapshot.lock` (process lock + heartbeat), `logs\vpose.log`
(daily rotation, 7 days kept).

## Usage

The tray icon shows the state: green — native settings, blue — VPN region
applied, red — last operation failed. The menu offers the current status,
an `Auto-apply` toggle, `Restore now`, `Open logs`, and `Exit (restore
first)`. On exit (menu, Ctrl+C, logoff/shutdown) the original settings are
always restored; if the process dies without restoring, the next start
detects the stale snapshot and restores before doing anything else.

## Design notes

- **broadcast channel over watch**: events are a chronological log of rare
  state transitions; watch coalesces values, so a slow subscriber could miss
  an intermediate Changed. broadcast preserves every event (bounded queue).
- **Consensus**: a poll is only trusted when all providers agree; any
  disagreement or failure keeps the previous value (logged at warn).
- **Debounce**: a new IP must be confirmed by config.confirmations (default 2)
  consecutive unanimous polls before Changed is emitted; intermediate values
  are logged at debug. The first detected IP emits Connected immediately.
- **Disconnected**: emitted once when every provider fails for
  failures_for_disconnect (default 3) consecutive polls while an IP was known;
  the next accepted IP emits Connected again.
- Configuration defaults: interval 15 s, confirmations 2, failures 3.
- **Crash safety**: originals are saved once before the first apply; the
  snapshot is cleared only when every planned restore write succeeded —
  a partial restore is retried on the next start.
- **Stage map**: stage 6 = monolith (one elevated binary); stage 7 = tray
  without admin rights + privileged backend over a named pipe with an
  allowlist.
