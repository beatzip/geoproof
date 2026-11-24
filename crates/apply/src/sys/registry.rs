//! Registry reads (HKLM) for timezone definitions. Open/query/close only.

use windows::core::PCWSTR;
use windows::Win32::Foundation::{ERROR_SUCCESS, WIN32_ERROR};
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_LOCAL_MACHINE, KEY_READ, REG_BINARY,
    REG_DWORD, REG_SZ, REG_VALUE_TYPE,
};

use crate::error::ApplyError;

pub(crate) fn widestring(s: &str) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

pub(crate) fn pcw(buf: &[u16]) -> PCWSTR {
    PCWSTR(buf.as_ptr())
}

fn io_err(context: &str, code: WIN32_ERROR) -> std::io::Error {
    std::io::Error::other(format!("{context} failed: win32 error {:?}", code.0))
}

/// Low-level registry reader. Everything the applier needs is a plain
/// value read under HKLM; writes never happen through this type.
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsRegistry;

impl WindowsRegistry {
    /// Does this HKLM key exist? (Open with read access and close again.)
    pub fn key_exists(&self, path: &str) -> Result<bool, ApplyError> {
        let path_w = widestring(path);
        let mut hkey = HKEY::default();
        // SAFETY: path_w is NUL-terminated; hkey is an out-param closed below.
        let code =
            unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, pcw(&path_w), None, KEY_READ, &mut hkey) };
        if code == ERROR_SUCCESS {
            // SAFETY: hkey was opened above; closed exactly once.
            unsafe {
                let _ = RegCloseKey(hkey);
            }
            Ok(true)
        } else if code == windows::Win32::Foundation::ERROR_FILE_NOT_FOUND {
            Ok(false)
        } else {
            Err(ApplyError::RegistryRead(
                path.to_string(),
                io_err("RegOpenKeyExW", code),
            ))
        }
    }

    /// Read a REG_SZ value; None when the value is absent.
    pub fn read_sz(&self, path: &str, value: &str) -> Result<String, ApplyError> {
        let raw = self.read_value(path, value)?;
        match raw {
            None => Ok(String::new()),
            Some((REG_SZ, bytes)) => {
                let words: Vec<u16> = bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .take_while(|c| c[0] != 0 || c[1] != 0)
                    .map(|c| u16::from_le_bytes(*c))
                    .collect();
                Ok(String::from_utf16_lossy(&words))
            }
            Some((_, _)) => Err(reg_type_err(path, value)),
        }
    }

    /// Read a REG_BINARY value; None when the value is absent.
    pub fn read_binary(&self, path: &str, value: &str) -> Result<Option<Vec<u8>>, ApplyError> {
        let raw = self.read_value(path, value)?;
        match raw {
            None => Ok(None),
            Some((REG_BINARY, bytes)) => Ok(Some(bytes)),
            Some((_, _)) => Err(reg_type_err(path, value)),
        }
    }

    /// Read a REG_DWORD value; None when the value is absent. Dynamic DST
    /// stores FirstEntry/LastEntry as REG_DWORD, unlike the REG_BINARY TZI
    /// blobs next to them.
    pub fn read_dword(&self, path: &str, value: &str) -> Result<Option<u32>, ApplyError> {
        let raw = self.read_value(path, value)?;
        match raw {
            None => Ok(None),
            Some((REG_DWORD, bytes)) => {
                let b: [u8; 4] = bytes
                    .get(..4)
                    .and_then(|s| s.try_into().ok())
                    .ok_or_else(|| reg_type_err(path, value))?;
                Ok(Some(u32::from_le_bytes(b)))
            }
            Some((_, _)) => Err(reg_type_err(path, value)),
        }
    }

    fn read_value(
        &self,
        path: &str,
        value: &str,
    ) -> Result<Option<(REG_VALUE_TYPE, Vec<u8>)>, ApplyError> {
        let path_w = widestring(path);
        let value_w = widestring(value);
        let mut hkey = HKEY::default();
        // SAFETY: path_w is NUL-terminated; hkey is an out-param closed below.
        let code =
            unsafe { RegOpenKeyExW(HKEY_LOCAL_MACHINE, pcw(&path_w), None, KEY_READ, &mut hkey) };
        if code != ERROR_SUCCESS {
            return Err(ApplyError::RegistryRead(
                path.to_string(),
                io_err("RegOpenKeyExW", code),
            ));
        }
        let result = query_value(hkey, &value_w);
        // SAFETY: hkey was opened above; closed exactly once on all paths.
        unsafe {
            let _ = RegCloseKey(hkey);
        }
        result
    }
}

/// Read one value of an open key; None when the value does not exist.
fn query_value(
    hkey: HKEY,
    value_w: &[u16],
) -> Result<Option<(REG_VALUE_TYPE, Vec<u8>)>, ApplyError> {
    let mut vtype = REG_VALUE_TYPE::default();
    let mut size = 0u32;
    // SAFETY: value_w is NUL-terminated; the first call only queries the
    // required size (lpdata is None), the second uses a sized caller buffer.
    let code = unsafe {
        RegQueryValueExW(
            hkey,
            pcw(value_w),
            None,
            Some(&mut vtype),
            None,
            Some(&mut size),
        )
    };
    if code == windows::Win32::Foundation::ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if code != ERROR_SUCCESS {
        return Err(ApplyError::RegistryRead(
            "value".to_string(),
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
            pcw(value_w),
            None,
            Some(&mut vtype),
            Some(buf.as_mut_ptr()),
            Some(&mut size2),
        )
    };
    if code != ERROR_SUCCESS {
        return Err(ApplyError::RegistryRead(
            "value".to_string(),
            io_err("RegQueryValueExW", code),
        ));
    }
    buf.truncate(size2 as usize);
    Ok(Some((vtype, buf)))
}

fn reg_type_err(path: &str, value: &str) -> ApplyError {
    let msg = format!("{path}: {value} has an unexpected registry type");
    ApplyError::RegistryRead(
        msg.clone(),
        std::io::Error::new(std::io::ErrorKind::InvalidData, msg),
    )
}
