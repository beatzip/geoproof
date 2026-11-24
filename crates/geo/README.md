# geo

Geo-lookup by IP (ipapi.co) + IANA → Windows timezone mapping. Stage 2 of the
GeoProof workspace: no system changes, no interaction with `watcher`.

## API example

```rust
use std::net::Ipv4Addr;
use geo::{GeoClient, GeoClientConfig, GeoProvider};
use geo::tz_windows::iana_to_windows;

let client = GeoClient::with_reqwest(GeoClientConfig::default());
let info = client.lookup(std::net::IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))).await?;
// info.country_code: "US" (uppercased ISO 3166-1 alpha-2)
// info.timezone:     IANA name as returned by the provider
// info.languages:    BCP-47 list in provider order
let windows_tz = iana_to_windows(&info.timezone); // Option<&'static str>
```

Defaults: timeout 5 s, 3 retries on top of the first attempt, backoff
0.5/1/2 s, cache TTL 24 h (successes only). Retried: transport errors,
HTTP 429, HTTP 5xx, `reason: "RateLimited"`. Not retried: other 4xx,
`Provider` (error:true at HTTP 200), `Parse`.

## CLDR pin

- Source: `cldr/windowsZones.xml`, committed and pinned.
- Ref: `release-46-1` (`https://raw.githubusercontent.com/unicode-org/cldr/release-46-1/common/supplemental/windowsZones.xml`).
- Downloaded: 2024 stage-2 work session; file attributes `otherVersion="7e11800"`, `typeVersion="2021a"`.
- Build statistic snapshot (asserted in tests): 444 unique IANA tokens, 444 mapped, 0 conflicts. Coverage 444/444 = 100% ≥ 0.90.

## Updating CLDR

```sh
cd crates/geo
CLDR_DOWNLOAD=1 cargo build -p geo   # refetches windowsZones.xml via curl.exe
```

Review the refreshed XML, update the snapshot numbers in
`src/tz_windows/tests.rs` (`snapshot_statistics_match_committed_cldr`), and
commit the XML deliberately. Build fails with a clear message if curl.exe is
missing. The default build is fully offline: it reads only the committed XML.

## Notes / deviations

- The pinned CLDR (typeVersion 2021a) has no `Europe/Kyiv`; only legacy
  `Europe/Kiev`, whose 001 mapping is `FLE Standard Time` (`W. Europe` 001 is
  `Europe/Berlin`). The spec's `Europe/Kyiv → W. Europe` example is therefore
  asserted as `Europe/Kyiv → FLE Standard Time` via the modern→legacy alias
  table.
- Bare `UTC` is provided as an alias to `Etc/UTC` (the XML maps only
  `Etc/UTC`/`Etc/GMT`), so `iana_to_windows("UTC") == Some("UTC")`.

- Toolchain note: on this machine, a clean build from scratch requires
  `PATH` to contain `C:\mingw64\bin` (for `gcc.exe`/`dlltool.exe`) and
  `TMP`/`TEMP`/`TMPDIR` pointing to a writable directory with no restrictive
  ACL (the system `%TEMP%` is denied to the MinGW gcc; the workspace-local
  `./.tmp` is used). Example:

      export PATH="/c/mingw64/bin:$PATH" TMP="$PWD/.tmp" TEMP="$PWD/.tmp" TMPDIR="$PWD/.tmp" cargo build --offline
