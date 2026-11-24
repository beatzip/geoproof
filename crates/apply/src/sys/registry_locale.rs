//! HKCU registry access for the locale fields: REG_SZ reads/writes for
//! `LocaleName` / `Geo\\Nation`, and the ordered subvalue scheme of
//! `Keyboard Layout\\Preload` (values named "1", "2", ... holding REG_SZ
//! layout codes) - the only place with the raw FFI for this stage.

use windows::core::PWSTR;
use windows::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_INSUFFICIENT_BUFFER, ERROR_SUCCESS, WIN32_ERROR,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegEnumValueW, RegOpenKeyExW, RegQueryValueExW,
    RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_READ, KEY_SET_VALUE,
    REG_OPTION_NON_VOLATILE, REG_SAM_FLAGS, REG_SZ, REG_VALUE_TYPE,
};

use crate::error::ApplyError;

const INTERNATIONAL: &str = "Control Panel\\International";
const GEO: &str = "Control Panel\\International\\Geo";
const PRELOAD: &str = "Keyboard Layout\\Preload";

fn io_err(context: &str, code: WIN32_ERROR) -> std::io::Error {
    std::io::Error::other(format!("{context} failed: win32 error {:?}", code.0))
}

fn reg_err(path: &str, context: &str, code: WIN32_ERROR) -> ApplyError {
    ApplyError::RegistryRead(path.to_string(), io_err(context, code))
}

/// Encode one REG_SZ string: NUL-terminated UTF-16.
fn sz_to_bytes(value: &str) -> Vec<u8> {
    let mut words: Vec<u16> = value.encode_utf16().collect();
    words.push(0);
    words.iter().flat_map(|w| w.to_le_bytes()).collect()
}

/// Decode REG_SZ bytes: UTF-16LE up to the first NUL.
fn sz_from_bytes(bytes: &[u8]) -> String {
    let words: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .take_while(|c| c[0] != 0 || c[1] != 0)
        .map(|c| u16::from_le_bytes(*c))
        .collect();
    String::from_utf16_lossy(&words)
}

/// The Preload value names this applier owns: "1", "2", ... Any other
/// value (e.g. "Scancode Map") is left untouched on read and on write.
fn preload_order(name: &str) -> Option<u32> {
    name.parse().ok()
}

/// Reduce enumerated (name, value) pairs to the ordered layout list:
/// keep numeric names, sort by number (1, 2, ..., 10 - not lexicographic).
fn parse_preload(entries: &[(String, String)]) -> Vec<String> {
    let mut ordered: Vec<(u32, String)> = entries
        .iter()
        .filter_map(|(name, value)| preload_order(name).map(|order| (order, value.clone())))
        .collect();
    ordered.sort_by_key(|(order, _)| *order);
    ordered.into_iter().map(|(_, v)| v).collect()
}

/// Open an HKCU key; `None` when the key does not exist.
fn open_key(rights: REG_SAM_FLAGS, path: &str) -> Result<Option<HKEY>, ApplyError> {
    let path_w = crate::sys::registry::widestring(path);
    let mut hkey = HKEY::default();
    // SAFETY: path_w is NUL-terminated; hkey is an out-param closed by the
    // caller on success.
    let code = unsafe {
        RegOpenKeyExW(
            HKEY_CURRENT_USER,
            crate::sys::registry::pcw(&path_w),
            None,
            rights,
            &mut hkey,
        )
    };
    if code == ERROR_SUCCESS {
        Ok(Some(hkey))
    } else if code == ERROR_FILE_NOT_FOUND {
        Ok(None)
    } else {
        Err(reg_err(path, "RegOpenKeyExW", code))
    }
}

/// Enumerate all (value name, REG_SZ data) pairs of an open key.
fn enum_values(hkey: HKEY) -> Result<Vec<(String, String)>, ApplyError> {
    let mut entries = Vec::new();
    let mut index = 0u32;
    loop {
        let mut name_buf = [0u16; 163];
        let mut name_len = name_buf.len() as u32;
        let mut data_buf = [0u8; 2048];
        let mut data_len = data_buf.len() as u32;
        let mut vtype = 0u32;
        // SAFETY: fixed-size caller buffers with initialized lengths; hkey
        // is a valid open key.
        let code = unsafe {
            RegEnumValueW(
                hkey,
                index,
                Some(PWSTR(name_buf.as_mut_ptr())),
                &mut name_len,
                None,
                Some(&mut vtype),
                Some(data_buf.as_mut_ptr()),
                Some(&mut data_len),
            )
        };
        if code == ERROR_INSUFFICIENT_BUFFER {
            // Longer than any real layout code - skip defensively.
            index += 1;
            continue;
        }
        if code != ERROR_SUCCESS {
            break; // ERROR_NO_MORE_ITEMS or unexpected - stop.
        }
        let name = String::from_utf16_lossy(&name_buf[..name_len as usize]);
        if vtype == REG_SZ.0 {
            entries.push((name, sz_from_bytes(&data_buf[..data_len as usize])));
        } else {
            // Binary values (e.g. "Scancode Map") are not ours.
            entries.push((name, String::new()));
        }
        index += 1;
    }
    Ok(entries)
}

/// Delete every numeric-named value of the open Preload key. Enumeration
/// happens first, deletion after - values we do not own are not touched.
fn delete_owned_values(hkey: HKEY, path: &str) -> Result<(), ApplyError> {
    let names: Vec<String> = enum_values(hkey)?
        .into_iter()
        .map(|(name, _)| name)
        .filter(|name| preload_order(name).is_some())
        .collect();
    for name in names {
        let name_w = crate::sys::registry::widestring(&name);
        // SAFETY: name_w is NUL-terminated; hkey is an open key opened with
        // KEY_SET_VALUE.
        let code = unsafe { RegDeleteValueW(hkey, crate::sys::registry::pcw(&name_w)) };
        if code != ERROR_SUCCESS {
            return Err(reg_err(&format!("{path}\\{name}"), "RegDeleteValueW", code));
        }
    }
    Ok(())
}

/// Write the numbered values: values[i] becomes "1", "2", ... (REG_SZ).
fn write_numbered_values(hkey: HKEY, path: &str, values: &[String]) -> Result<(), ApplyError> {
    for (i, value) in values.iter().enumerate() {
        let name = (i + 1).to_string();
        let name_w = crate::sys::registry::widestring(&name);
        let data = sz_to_bytes(value);
        // SAFETY: name_w is NUL-terminated and data lives across the call.
        let code = unsafe {
            RegSetValueExW(
                hkey,
                crate::sys::registry::pcw(&name_w),
                None,
                REG_SZ,
                Some(&data),
            )
        };
        if code != ERROR_SUCCESS {
            return Err(reg_err(&format!("{path}\\{name}"), "RegSetValueExW", code));
        }
    }
    Ok(())
}

/// Production impl of [crate::traits::LocaleRegistry].
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsLocaleRegistry;

impl WindowsLocaleRegistry {
    pub fn new() -> Self {
        Self
    }

    /// Read one value of an HKCU key; Err when the key is missing, Err
    /// with the plain name when the value is missing (the locale fields
    /// are expected to exist, absence must not be guessed into a default).
    fn read_value(
        path: &str,
        value: &str,
    ) -> Result<Option<(REG_VALUE_TYPE, Vec<u8>)>, ApplyError> {
        let path_w = crate::sys::registry::widestring(path);
        let value_w = crate::sys::registry::widestring(value);
        let mut hkey = HKEY::default();
        // SAFETY: path_w is NUL-terminated; hkey is an out-param closed below.
        let code = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                crate::sys::registry::pcw(&path_w),
                None,
                KEY_READ,
                &mut hkey,
            )
        };
        if code != ERROR_SUCCESS {
            return Err(ApplyError::RegistryRead(
                format!("{path}\\{value}"),
                io_err("RegOpenKeyExW", code),
            ));
        }
        let mut vtype = REG_VALUE_TYPE::default();
        let mut size = 0u32;
        // SAFETY: value_w is NUL-terminated; the first call only queries the
        // required size (lpdata is None), the second uses a sized buffer.
        let code = unsafe {
            RegQueryValueExW(
                hkey,
                crate::sys::registry::pcw(&value_w),
                None,
                Some(&mut vtype),
                None,
                Some(&mut size),
            )
        };
        if code != ERROR_SUCCESS {
            // SAFETY: hkey was opened above; closed exactly once.
            unsafe {
                let _ = RegCloseKey(hkey);
            }
            return Err(ApplyError::RegistryRead(
                format!("{path}\\{value}"),
                io_err("RegQueryValueExW(size)", code),
            ));
        }
        let mut buf = vec![0u8; size as usize];
        let mut size2 = size;
        // SAFETY: buf is allocated to the exact size reported above; size2
        // bounds the write.
        let code = unsafe {
            RegQueryValueExW(
                hkey,
                crate::sys::registry::pcw(&value_w),
                None,
                Some(&mut vtype),
                Some(buf.as_mut_ptr()),
                Some(&mut size2),
            )
        };
        // SAFETY: hkey was opened above; closed exactly once.
        unsafe {
            let _ = RegCloseKey(hkey);
        }
        if code != ERROR_SUCCESS {
            return Err(ApplyError::RegistryRead(
                format!("{path}\\{value}"),
                io_err("RegQueryValueExW", code),
            ));
        }
        buf.truncate(size2 as usize);
        Ok(Some((vtype, buf)))
    }

    /// Create-if-missing the HKCU key and write one raw value.
    fn write_value(
        path: &str,
        value: &str,
        vtype: REG_VALUE_TYPE,
        data: &[u8],
    ) -> Result<(), ApplyError> {
        let path_w = crate::sys::registry::widestring(path);
        let value_w = crate::sys::registry::widestring(value);
        let mut hkey = HKEY::default();
        // SAFETY: path_w is NUL-terminated; hkey is an out-param closed below.
        let code = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                crate::sys::registry::pcw(&path_w),
                None,
                None,
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                None,
                &mut hkey,
                None,
            )
        };
        if code != ERROR_SUCCESS {
            return Err(ApplyError::RegistryRead(
                path.to_string(),
                io_err("RegCreateKeyExW", code),
            ));
        }
        // SAFETY: value_w is NUL-terminated and data lives across the call.
        let code = unsafe {
            RegSetValueExW(
                hkey,
                crate::sys::registry::pcw(&value_w),
                None,
                vtype,
                Some(data),
            )
        };
        // SAFETY: hkey was opened above; closed exactly once.
        unsafe {
            let _ = RegCloseKey(hkey);
        }
        if code != ERROR_SUCCESS {
            return Err(ApplyError::RegistryRead(
                format!("{path}\\{value}"),
                io_err("RegSetValueExW", code),
            ));
        }
        Ok(())
    }

    fn read_sz_exact(path: &str, value: &str) -> Result<String, ApplyError> {
        match Self::read_value(path, value)? {
            None => Err(ApplyError::RegistryRead(
                format!("{value} not found"),
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("{path}\\{value} missing"),
                ),
            )),
            Some((REG_SZ, bytes)) => Ok(sz_from_bytes(&bytes)),
            Some((_, _)) => Err(ApplyError::RegistryRead(
                format!("{path}\\{value}"),
                std::io::Error::new(std::io::ErrorKind::InvalidData, "unexpected registry type"),
            )),
        }
    }

    /// Read the ordered layout list from the subvalue scheme of `path`.
    /// A missing or empty key reads as `[]` - a fresh profile has no
    /// preloads and that is a state, not an error.
    fn read_preload_values(path: &str) -> Result<Vec<String>, ApplyError> {
        let Some(hkey) = open_key(KEY_READ, path)? else {
            return Ok(Vec::new());
        };
        let result = enum_values(hkey);
        // SAFETY: hkey was opened above; closed exactly once.
        unsafe {
            let _ = RegCloseKey(hkey);
        }
        result.map(|entries| parse_preload(&entries))
    }

    /// Rewrite the numbered subvalue scheme of `path`: delete the values
    /// we own ("1", "2", ...), then write `values` as "1", "2", ... The
    /// key itself is created if missing; non-owned values are preserved.
    fn write_preload_values(path: &str, values: &[String]) -> Result<(), ApplyError> {
        let path_w = crate::sys::registry::widestring(path);
        let mut hkey = HKEY::default();
        // SAFETY: path_w is NUL-terminated; hkey is an out-param closed below.
        let code = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                crate::sys::registry::pcw(&path_w),
                None,
                None,
                REG_OPTION_NON_VOLATILE,
                KEY_QUERY_VALUE | KEY_SET_VALUE,
                None,
                &mut hkey,
                None,
            )
        };
        if code != ERROR_SUCCESS {
            return Err(reg_err(path, "RegCreateKeyExW", code));
        }
        let result = delete_owned_values(hkey, path)
            .and_then(|()| write_numbered_values(hkey, path, values));
        // SAFETY: hkey was opened above; closed exactly once.
        unsafe {
            let _ = RegCloseKey(hkey);
        }
        result
    }
}

impl crate::traits::LocaleRegistry for WindowsLocaleRegistry {
    fn read_locale_name(&self) -> Result<String, ApplyError> {
        Self::read_sz_exact(INTERNATIONAL, "LocaleName")
    }

    fn write_locale_name(&self, value: &str) -> Result<(), ApplyError> {
        Self::write_value(INTERNATIONAL, "LocaleName", REG_SZ, &sz_to_bytes(value))
    }

    fn read_geo_nation(&self) -> Result<String, ApplyError> {
        Self::read_sz_exact(GEO, "Nation")
    }

    fn write_geo_nation(&self, value: &str) -> Result<(), ApplyError> {
        Self::write_value(GEO, "Nation", REG_SZ, &sz_to_bytes(value))
    }

    fn read_kbd_preload(&self) -> Result<Vec<String>, ApplyError> {
        Self::read_preload_values(PRELOAD)
    }

    fn write_kbd_preload(&self, values: &[String]) -> Result<(), ApplyError> {
        Self::write_preload_values(PRELOAD, values)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sz_roundtrip() {
        let bytes = sz_to_bytes("ru-RU");
        assert_eq!(sz_from_bytes(&bytes), "ru-RU");
        // Empty string encodes as a single NUL, decodes back to "".
        assert_eq!(sz_to_bytes(""), vec![0, 0]);
        assert_eq!(sz_from_bytes(&sz_to_bytes("")), "");
    }

    #[test]
    fn parse_preload_sorts_numerically_not_lexicographically() {
        let entries = vec![
            ("10".to_string(), "00001009".to_string()),
            ("2".to_string(), "00000419".to_string()),
            ("1".to_string(), "00000409".to_string()),
        ];
        assert_eq!(
            parse_preload(&entries),
            vec![
                "00000409".to_string(),
                "00000419".to_string(),
                "00001009".to_string()
            ]
        );
    }

    #[test]
    fn parse_preload_ignores_non_numeric_values() {
        // Real Preload keys may hold e.g. "Scancode Map" (REG_BINARY);
        // only "1", "2", ... are ours.
        let entries = vec![
            ("Scancode Map".to_string(), String::new()),
            ("2".to_string(), "00000419".to_string()),
            ("Layout File".to_string(), String::new()),
            ("1".to_string(), "00000409".to_string()),
        ];
        assert_eq!(
            parse_preload(&entries),
            vec!["00000409".to_string(), "00000419".to_string()]
        );
    }

    #[test]
    fn parse_preload_empty_entries_yield_empty_list() {
        assert!(parse_preload(&[]).is_empty());
    }

    /// Roundtrip against a real HKCU scratch key (ignored by default:
    /// touches the live registry, writes under HKCU\Software). Run with:
    ///   cargo test -p apply --lib -- --ignored
    #[test]
    #[ignore = "writes to the real HKCU registry; run manually"]
    fn kbd_preload_roundtrip_via_registry_subkeys() {
        const SCRATCH: &str = "Software\\GeoProofTest\\PreloadRoundtrip";
        // SAFETY: hkey from open/create below; closed exactly once.
        let cleanup = |path: &str| {
            let path_w = crate::sys::registry::widestring(path);
            // SAFETY: HKEY_CURRENT_USER is a valid pseudo-handle; the key
            // tree under the scratch path is deleted and nothing else.
            unsafe {
                let _ = windows::Win32::System::Registry::RegDeleteTreeW(
                    HKEY_CURRENT_USER,
                    crate::sys::registry::pcw(&path_w),
                );
            }
        };
        cleanup(SCRATCH);

        let res = (|| -> Result<(), ApplyError> {
            WindowsLocaleRegistry::write_preload_values(
                SCRATCH,
                &["00000409".to_string(), "00000419".to_string()],
            )?;
            assert_eq!(
                WindowsLocaleRegistry::read_preload_values(SCRATCH)?,
                vec!["00000409".to_string(), "00000419".to_string()]
            );

            // Rewriting fewer entries must delete the stale "2".
            WindowsLocaleRegistry::write_preload_values(SCRATCH, &["00000419".to_string()])?;
            assert_eq!(
                WindowsLocaleRegistry::read_preload_values(SCRATCH)?,
                vec!["00000419".to_string()]
            );

            // An empty list deletes everything we own.
            WindowsLocaleRegistry::write_preload_values(SCRATCH, &[])?;
            assert!(WindowsLocaleRegistry::read_preload_values(SCRATCH)?.is_empty());
            Ok(())
        })();
        cleanup(SCRATCH);
        res.expect("registry preload roundtrip");
    }
}
