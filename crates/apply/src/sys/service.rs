//! Service Control Manager wrappers for `tzautoupdate`: query start type,
//! set start type, stop, start. Thin; every unsafe block commented.

use windows::Win32::System::Services::{
    ChangeServiceConfigW, CloseServiceHandle, ControlService, OpenSCManagerW, OpenServiceW,
    QueryServiceConfigW, QueryServiceStatus, StartServiceW, QUERY_SERVICE_CONFIGW, SC_HANDLE,
    SERVICE_STATUS, SERVICE_STATUS_CURRENT_STATE,
};
use windows::Win32::System::Services::{
    SERVICE_AUTO_START, SERVICE_CHANGE_CONFIG, SERVICE_CONTROL_STOP, SERVICE_DEMAND_START,
    SERVICE_DISABLED, SERVICE_ERROR_NORMAL, SERVICE_NO_CHANGE, SERVICE_QUERY_CONFIG,
    SERVICE_QUERY_STATUS, SERVICE_START, SERVICE_STOP, SERVICE_WIN32_OWN_PROCESS,
};

use crate::error::ApplyError;
use windows::core::PCWSTR;

use crate::sys::registry::{pcw, widestring};
use crate::traits::{ServiceController, ServiceStartType};

const SCM_ACCESS: u32 = SERVICE_QUERY_CONFIG
    | SERVICE_CHANGE_CONFIG
    | SERVICE_QUERY_STATUS
    | SERVICE_START
    | SERVICE_STOP;

fn io_err(context: &str, e: &windows::core::Error) -> std::io::Error {
    std::io::Error::other(format!("{context} failed: {e}"))
}

fn svc_err(service: &str, context: &str, e: &windows::core::Error) -> ApplyError {
    ApplyError::ServiceOp(format!("{service}: {context}"), io_err(context, e))
}

fn start_type_from(
    start: windows::Win32::System::Services::SERVICE_START_TYPE,
) -> ServiceStartType {
    match start.0 {
        2 => ServiceStartType::AutoStart,
        3 => ServiceStartType::DemandStart,
        4 => ServiceStartType::Disabled,
        other => ServiceStartType::Unknown(other),
    }
}

/// SCM-backed controller. Stateless; handles are opened per call and closed
/// on all paths.
#[derive(Debug, Default, Clone, Copy)]
pub struct WindowsServiceController;

impl ServiceController for WindowsServiceController {
    fn query_start_type(&self, service: &str) -> Result<Option<ServiceStartType>, ApplyError> {
        let mut h = ScmHandles::open(service)?;
        let out = h.query_start_type();
        // handles close on Drop in ScmHandles
        out
    }

    fn set_start_type(&self, service: &str, start: ServiceStartType) -> Result<(), ApplyError> {
        let start_type = windows::Win32::System::Services::SERVICE_START_TYPE(match start {
            ServiceStartType::AutoStart => SERVICE_AUTO_START.0,
            ServiceStartType::DemandStart => SERVICE_DEMAND_START.0,
            ServiceStartType::Disabled => SERVICE_DISABLED.0,
            ServiceStartType::Unknown(v) => {
                return Err(ApplyError::ServiceOp(
                    service.to_string(),
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!("cannot set unknown start type {v}"),
                    ),
                ))
            }
        });
        let h = ScmHandles::open(service)?;
        // SAFETY: hsvc is a valid open SERVICE handle with CHANGE_CONFIG
        // access; string out-params are all None so nothing is written
        // beyond the start type.
        unsafe {
            ChangeServiceConfigW(
                h.hsvc,
                SERVICE_WIN32_OWN_PROCESS,
                start_type,
                SERVICE_ERROR_NORMAL,
                None,
                None,
                None,
                None,
                None,
                None,
                None,
            )
        }
        .map_err(|e| svc_err(service, "ChangeServiceConfigW", &e))
    }

    fn stop(&self, service: &str) -> Result<(), ApplyError> {
        let mut h = ScmHandles::open(service)?;
        let status = h.query_status()?;
        if status.dwCurrentState == SERVICE_STATUS_CURRENT_STATE(1u32) {
            // 1 == SERVICE_STOPPED: nothing to do.
            return Ok(());
        }
        let mut status = SERVICE_STATUS::default();
        // SAFETY: hsvc is a valid open SERVICE handle with STOP access;
        // status is a caller-allocated out-struct.
        unsafe { ControlService(h.hsvc, SERVICE_CONTROL_STOP, &mut status) }
            .map_err(|e| svc_err(service, "ControlService(STOP)", &e))
    }

    fn start(&self, service: &str) -> Result<(), ApplyError> {
        let mut h = ScmHandles::open(service)?;
        let status = h.query_status()?;
        if status.dwCurrentState != SERVICE_STATUS_CURRENT_STATE(1u32) {
            // Already running or pending: no-op.
            return Ok(());
        }
        // SAFETY: hsvc is a valid open SERVICE handle with START access;
        // argv is None.
        unsafe { StartServiceW(h.hsvc, None) }.map_err(|e| svc_err(service, "StartServiceW", &e))
    }
}

/// RAII pair: the SCM database handle plus one open service handle, closed
/// exactly once on Drop.
struct ScmHandles {
    hscm: SC_HANDLE,
    hsvc: SC_HANDLE,
}

impl ScmHandles {
    fn open(service: &str) -> Result<Self, ApplyError> {
        // SAFETY: NULL machine and database names mean local machine,
        // default database (an empty string is ERROR_INVALID_NAME, not
        // "local"); the returned handle is closed on Drop.
        let hscm = unsafe { OpenSCManagerW(PCWSTR::null(), PCWSTR::null(), SCM_ACCESS) }
            .map_err(|e| svc_err(service, "OpenSCManagerW", &e))?;
        let svc_w = widestring(service);
        // SAFETY: svc_w is NUL-terminated; hscm is a valid SCM handle from
        // the call above; the service handle is closed on Drop.
        let hsvc = unsafe { OpenServiceW(hscm, pcw(&svc_w), SCM_ACCESS) };
        match hsvc {
            Ok(hsvc) => Ok(ScmHandles { hscm, hsvc }),
            Err(e) => {
                let err = svc_err(service, "OpenServiceW", &e);
                // SAFETY: hscm from OpenSCManagerW above; closed once.
                unsafe {
                    let _ = CloseServiceHandle(hscm);
                }
                Err(err)
            }
        }
    }

    fn query_start_type(&mut self) -> Result<Option<ServiceStartType>, ApplyError> {
        let mut needed = 0u32;
        // SAFETY: hsvc is a valid open SERVICE handle; this first call only
        // reports the required buffer size.
        let size_probe = unsafe { QueryServiceConfigW(self.hsvc, None, 0, &mut needed) };
        let _ = size_probe; // expected to fail with ERROR_INSUFFICIENT_BUFFER
        let mut buf = vec![0u8; needed as usize];
        let cfg = buf.as_mut_ptr() as *mut QUERY_SERVICE_CONFIGW;
        let mut needed2 = needed;
        // SAFETY: buf holds at least the size reported above; cfg points
        // into it and stays valid while buf is alive.
        unsafe { QueryServiceConfigW(self.hsvc, Some(cfg), needed, &mut needed2) }.map_err(
            |e| {
                ApplyError::ServiceOp(
                    SERVICE_FALLBACK.to_string(),
                    io_err("QueryServiceConfigW", &e),
                )
            },
        )?;
        // SAFETY: cfg was written by the successful call above.
        let start = unsafe { (*cfg).dwStartType };
        Ok(Some(start_type_from(start)))
    }

    fn query_status(&mut self) -> Result<SERVICE_STATUS, ApplyError> {
        let mut status = SERVICE_STATUS::default();
        // SAFETY: hsvc is a valid open SERVICE handle with QUERY_STATUS
        // access; status is a caller-allocated out-struct.
        unsafe { QueryServiceStatus(self.hsvc, &mut status) }.map_err(|e| {
            ApplyError::ServiceOp(
                SERVICE_FALLBACK.to_string(),
                io_err("QueryServiceStatus", &e),
            )
        })?;
        Ok(status)
    }
}

const SERVICE_FALLBACK: &str = "service";

impl Drop for ScmHandles {
    fn drop(&mut self) {
        // SAFETY: both handles were opened above; each closed exactly once.
        unsafe {
            let _ = CloseServiceHandle(self.hsvc);
            let _ = CloseServiceHandle(self.hscm);
        }
    }
}

const _: u32 = SERVICE_NO_CHANGE; // documented for future partial changes
