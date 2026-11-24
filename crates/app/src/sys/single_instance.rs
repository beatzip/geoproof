//! Named-mutex single-instance guard. The mutex lives in the `Global\`
//! namespace so a second user session cannot start a second vpose (the
//! process itself runs elevated via Task Scheduler, which holds the
//! SeCreateGlobalPrivilege).

use windows::core::PCWSTR;
use windows::Win32::Foundation::{GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::System::Threading::CreateMutexW;

pub const SINGLE_INSTANCE_MUTEX: &str = r"Global\VPosE_Instance_Mutex";

/// An acquired single-instance slot. Dropping it closes the mutex handle.
pub struct SingleInstance {
    _handle: HANDLE,
}

impl SingleInstance {
    /// Try to become the single running instance. `None` when another
    /// instance (any user session) already holds the named mutex.
    pub fn acquire(name: &str) -> Option<SingleInstance> {
        let wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: `wide` is a NUL-terminated UTF-16 buffer valid for the
        // duration of the call; no attributes, not owned.
        let handle = unsafe { CreateMutexW(None, false, PCWSTR(wide.as_ptr())) }.ok()?;
        // SAFETY: GetLastError is valid right after the CreateMutexW call
        // above; ERROR_ALREADY_EXISTS means another instance holds it.
        let exists = unsafe { GetLastError() } == ERROR_ALREADY_EXISTS;
        if exists {
            None
        } else {
            Some(SingleInstance { _handle: handle })
        }
    }
}
