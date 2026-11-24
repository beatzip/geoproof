//! Build-time generation of the IANA -> Windows timezone table from the
//! pinned CLDR windowsZones.xml. Default path is fully offline: the committed
//! XML is the single source. CLDR_DOWNLOAD=1 refreshes the XML via curl.exe
//! from the pinned upstream ref (the result is committed deliberately).

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Pinned upstream ref (CLDR release). Update deliberately, together with the
/// refreshed windowsZones.xml.
const CLDR_REF: &str = "release-46-1";
const CLDR_URL: &str = "https://raw.githubusercontent.com/unicode-org/cldr";
const XML_REL: &str = "cldr/windowsZones.xml";

type BoxError = Box<dyn std::error::Error>;
type Result<T> = std::result::Result<T, BoxError>;

fn main() -> Result<()> {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR")?);
    let xml_path = manifest_dir.join(XML_REL);
    let out_dir = PathBuf::from(env::var("OUT_DIR")?);

    println!("cargo:rerun-if-changed={}", xml_path.display());
    println!("cargo:rerun-if-env-changed=CLDR_DOWNLOAD");

    if env::var("CLDR_DOWNLOAD").is_ok_and(|v| v == "1") {
        download(&xml_path)?;
    }

    let xml = fs::read_to_string(&xml_path)
        .map_err(|e| format!("cannot read {}: {e}", xml_path.display()))?;
    let (table, stats) = parse_and_map(&xml)?;
    let generated = generate(&table, &stats);
    fs::write(out_dir.join("tz_windows_generated.rs"), generated)
        .map_err(|e| format!("cannot write generated table: {e}"))?;
    Ok(())
}

/// Refresh the committed XML from the pinned CLDR ref using the system curl.
fn download(xml_path: &Path) -> Result<()> {
    let url = format!("{CLDR_URL}/{CLDR_REF}/common/supplemental/windowsZones.xml");
    let status = Command::new("curl.exe")
        .args(["-fsSL", "-o"])
        .arg(xml_path)
        .arg(&url)
        .status()
        .map_err(|e| {
            format!(
                "CLDR_DOWNLOAD=1 requires the system curl.exe, but it could not be \
                 spawned: {e}. Download manually:  curl -fsSL -o {} \"{url}\"",
                xml_path.display()
            )
        })?;
    if !status.success() {
        return Err(format!("curl failed with status {status} while downloading {url}").into());
    }
    println!("cargo:warning=windowsZones.xml refreshed from {url}; review and commit it");
    Ok(())
}

struct MapStats {
    total: usize,
    conflicts: Vec<String>,
}

/// Two-pass mapping per spec:
/// pass 1: every mapZone with territory="001" -> each type token maps to other;
/// pass 2: every other territory (incl. ZZ), file order, only still-unmapped
/// zones. A conflict (zone already mapped to a different Windows name) keeps
/// the existing mapping and is recorded.
fn parse_and_map(xml: &str) -> Result<(BTreeMap<String, String>, MapStats)> {
    use quick_xml::events::Event;

    let mut total_iana: BTreeSet<String> = BTreeSet::new();
    let mut table: BTreeMap<String, String> = BTreeMap::new();
    let mut conflicts: Vec<String> = Vec::new();

    // (territory, other, types) in file order.
    let mut zones: Vec<(String, String, Vec<String>)> = Vec::new();
    let mut in_windows_zones = false;

    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    loop {
        match reader.read_event()? {
            Event::Start(e) | Event::Empty(e) => {
                let name = e.name();
                let name = name.as_ref();
                if name == b"windowsZones" {
                    in_windows_zones = true;
                } else if in_windows_zones && name == b"mapZone" {
                    let (mut other, mut territory, mut types) =
                        (String::new(), String::new(), String::new());
                    for attr in e.attributes() {
                        let attr = attr?;
                        match attr.key.as_ref() {
                            b"other" => other = attr.unescape_value()?.into_owned(),
                            b"territory" => territory = attr.unescape_value()?.into_owned(),
                            b"type" => types = attr.unescape_value()?.into_owned(),
                            _ => {}
                        }
                    }
                    let tokens: Vec<String> = types
                        .split_whitespace()
                        .filter(|tz| *tz != "Etc/Unknown")
                        .map(String::from)
                        .collect();
                    for tz in &tokens {
                        total_iana.insert(tz.clone());
                    }
                    zones.push((territory, other, tokens));
                }
            }
            Event::End(e) if e.name().as_ref() == b"windowsZones" => {
                in_windows_zones = false;
            }
            Event::Eof => break,
            _ => {}
        }
    }

    let insert = |table: &mut BTreeMap<String, String>,
                  tz: &str,
                  other: &str,
                  conflicts: &mut Vec<String>| {
        match table.get(tz) {
            Some(existing) if existing != other => {
                conflicts.push(format!("{tz}: kept {existing}, skipped {other}"))
            }
            Some(_) => {}
            None => {
                table.insert(tz.to_string(), other.to_string());
            }
        }
    };

    // Pass 1: territory 001 (priority).
    for (territory, other, tokens) in &zones {
        if territory == "001" {
            for tz in tokens {
                insert(&mut table, tz, other, &mut conflicts);
            }
        }
    }
    // Pass 2: all other territories (including ZZ), file order.
    for (territory, other, tokens) in &zones {
        if territory != "001" {
            for tz in tokens {
                insert(&mut table, tz, other, &mut conflicts);
            }
        }
    }

    println!(
        "cargo:metadata=total={} mapped={} conflicts={}",
        total_iana.len(),
        table.len(),
        conflicts.len()
    );
    let stats = MapStats {
        total: total_iana.len(),
        conflicts,
    };
    Ok((table, stats))
}

fn generate(table: &BTreeMap<String, String>, stats: &MapStats) -> String {
    let mut src = String::new();
    src.push_str("// @generated by build.rs from cldr/windowsZones.xml; do not edit or commit.\n");
    src.push_str("#[rustfmt::skip]\n");
    src.push_str("pub static TABLE: &[(&str, &str)] = &[\n");
    for (iana, win) in table {
        src.push_str(&format!("    ({iana:?}, {win:?}),\n"));
    }
    src.push_str("];\n");
    src.push_str("#[rustfmt::skip]\n");
    src.push_str("pub static CONFLICTS: &[&str] = &[\n");
    for c in &stats.conflicts {
        src.push_str(&format!("    {c:?},\n"));
    }
    src.push_str("];\n");
    src.push_str(&format!("pub const TOTAL_IANA: usize = {};\n", stats.total));
    src
}
