$ErrorActionPreference = 'Stop'
Set-Location C:\Users\Admin\Desktop\GeoProof

New-Item -ItemType Directory -Force `
  crates\app\src, crates\app\src\sys, crates\app\src\bin, `
  crates\app\assets, crates\app\tests | Out-Null

$json = Invoke-RestMethod "https://raw.githubusercontent.com/lukes/ISO-3166-Countries-with-Regional-Codes/master/all/all.json"
$sorted = $json | Sort-Object { $_.'alpha-2' }
$lines = foreach ($c in $sorted) { '    ("{0}", "{1}"),' -f $c.'alpha-2', $c.'country-code' }

$body = @"
//! GENERATED from ISO 3166-1 (dataset: lukes/ISO-3166-Countries-with-Regional-Codes, master).
//! ISO 3166-1 alpha-2 -> numeric code, as stored in
//! HKCU\Control Panel\International\Geo "Nation". Do not edit by hand.

/// Sorted by alpha-2 code; numeric codes keep ISO zero-padding.
pub static COUNTRY_NUMERIC: &[(&str, &str)] = &[
"@ + "`n" + (($lines -join "`n") + "`n") + @"
];

/// ISO 3166-1 numeric code for an alpha-2 country code.
pub fn numeric_for(alpha2: &str) -> Option<&'static str> {
    COUNTRY_NUMERIC
        .binary_search_by(|(code, _)| (*code).cmp(alpha2))
        .ok()
        .map(|i| COUNTRY_NUMERIC[i].1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_is_sorted_and_complete() {
        let mut sorted = COUNTRY_NUMERIC.to_vec();
        sorted.sort_unstable();
        assert_eq!(sorted, COUNTRY_NUMERIC.to_vec());
        assert!(
            COUNTRY_NUMERIC.len() >= 249,
            "expected 249+ entries, got {}",
            COUNTRY_NUMERIC.len()
        );
    }

    #[test]
    fn known_codes() {
        assert_eq!(numeric_for("FR"), Some("250"));
        assert_eq!(numeric_for("RU"), Some("643"));
        assert_eq!(numeric_for("US"), Some("840"));
        assert_eq!(numeric_for("DE"), Some("276"));
        assert_eq!(numeric_for("XX"), None);
        assert_eq!(numeric_for("fr"), None, "alpha-2 must be uppercase");
    }
}
"@

$utf8 = New-Object System.Text.UTF8Encoding($false)
[System.IO.File]::WriteAllText(
    (Join-Path (Get-Location) 'crates\app\src\country.rs'),
    $body,
    $utf8
)

(Get-Content crates\app\src\country.rs | Measure-Object -Line).Lines
