use std::io;

/// Errors of the writing stage. Every variant names the step that failed so
/// the orchestrator (stage 6) can log and decide without string parsing.
#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("admin token required for this operation")]
    InsufficientPrivileges,

    #[error("timezone \"{0}\" not found under HKLM\\...\\Time Zones")]
    UnknownZone(String),

    #[error("SetDynamicTimeZoneInformation failed: {0}")]
    SetTzFailed(io::Error),

    #[error("registry read failed for \"{0}\"")]
    RegistryRead(String, #[source] io::Error),

    #[error("service operation failed for \"{0}\": {1}")]
    ServiceOp(String, #[source] io::Error),
}

/// Result of a successful apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppliedChange {
    /// Nothing matched: new value equals the current one, no side effects.
    Noop,
    /// The timezone was actually changed.
    Changed {
        field_name: &'static str,
        old_key_name: String,
        new_key_name: String,
        /// Which browser restart hints apply; TZ is cached per process.
        restart_hint: Vec<&'static str>,
    },
    /// One or more locale fields (locale_name / geo_nation / kbd_preload)
    /// were changed in one step; they share a single broadcast.
    LocaleChanged {
        /// Names of the fields actually written, in write order.
        changed_fields: Vec<&'static str>,
        restart_hint: Vec<&'static str>,
    },
}

impl AppliedChange {
    pub fn changed(field_name: &'static str, old: &str, new: &str) -> Self {
        AppliedChange::Changed {
            field_name,
            old_key_name: old.to_string(),
            new_key_name: new.to_string(),
            restart_hint: vec!["Chrome", "Firefox"],
        }
    }

    pub fn is_noop(&self) -> bool {
        matches!(self, AppliedChange::Noop)
    }
}
