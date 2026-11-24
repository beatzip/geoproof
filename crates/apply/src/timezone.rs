//! `TimezoneApplier`: change the active Windows timezone plus the
//! `tzautoupdate` service state, and roll both back.

pub mod tz_registry;

use crate::error::{AppliedChange, ApplyError};
use crate::traits::{
    AdminCheck, Applier, ServiceController, ServiceStartType, TzAutoUpdateState, TzRegistry,
    WinTzSetter,
};

/// Value of the timezone field, snapshot-compatible.
///
/// Backward compatible with the stage-3 v1 schema: JSON without
/// `service_state` deserializes to `None`, and `None` is not serialized
/// back. On revert, `None` means "unknown -> do not touch the service".
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct TimezoneValue {
    pub key_name: String,
    pub dst_disabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub service_state: Option<TzAutoUpdateState>,
}

impl TimezoneValue {
    pub fn new(key_name: impl Into<String>, dst_disabled: bool) -> Self {
        TimezoneValue {
            key_name: key_name.into(),
            dst_disabled,
            service_state: None,
        }
    }
}

/// Applies (and reverts) the timezone plus `tzautoupdate` service state.
///
/// `service_state_before` tracks partial application: it is captured at the
/// first mutating step and cleared once the timezone write succeeds, so an
/// error path knows whether the service must be rolled back.
pub struct TimezoneApplier<R, S, V, A>
where
    R: TzRegistry,
    S: ServiceController,
    V: WinTzSetter,
    A: AdminCheck,
{
    pub registry: R,
    pub service: S,
    pub setter: V,
    pub admin: A,
    service_state_before: Option<TzAutoUpdateState>,
    last_current_key: Option<String>,
}

pub const SERVICE_NAME: &str = "tzautoupdate";
const FIELD_NAME: &str = "timezone";

impl<R, S, V, A> TimezoneApplier<R, S, V, A>
where
    R: TzRegistry,
    S: ServiceController,
    V: WinTzSetter,
    A: AdminCheck,
{
    pub fn new(registry: R, service: S, setter: V, admin: A) -> Self {
        TimezoneApplier {
            registry,
            service,
            setter,
            admin,
            service_state_before: None,
            last_current_key: None,
        }
    }

    /// Variant A of the snapshot integration: the orchestrator calls this
    /// before `apply` and stores the result in `TimezoneValue.service_state`
    /// via `Snapshot::record_applied`. The applier itself never touches the
    /// snapshot crate (decoupling, stage 6).
    pub fn capture_service_state(&self) -> Result<Option<TzAutoUpdateState>, ApplyError> {
        Ok(self
            .service
            .query_start_type(SERVICE_NAME)?
            .map(start_type_to_state))
    }
}

fn start_type_to_state(start: ServiceStartType) -> TzAutoUpdateState {
    match start {
        ServiceStartType::Disabled => TzAutoUpdateState::Disabled,
        _ => TzAutoUpdateState::Enabled,
    }
}

#[async_trait::async_trait]
impl<R, S, V, A> Applier for TimezoneApplier<R, S, V, A>
where
    R: TzRegistry + Send + Sync,
    S: ServiceController + Send + Sync,
    V: WinTzSetter + Send + Sync,
    A: AdminCheck + Send + Sync,
{
    type Value = TimezoneValue;

    async fn current(&self) -> Result<Self::Value, ApplyError> {
        self.setter.read_current()
    }

    async fn apply(&mut self, new: &Self::Value) -> Result<AppliedChange, ApplyError> {
        // 1. Privilege gate: refuse before touching anything.
        if !self.admin.is_admin() {
            return Err(ApplyError::InsufficientPrivileges);
        }

        let current = self.setter.read_current()?;
        // 2. Idempotence: equal value -> Noop, zero side effects.
        if current == *new {
            return Ok(AppliedChange::Noop);
        }
        self.last_current_key = Some(current.key_name.clone());

        // 3. Capture the service state before the first mutating step; this
        //    snapshot survives an automatic rollback below.
        let state_before = self
            .service
            .query_start_type(SERVICE_NAME)?
            .map(start_type_to_state);
        self.service_state_before = state_before;

        // From here on any failure must roll the service back: the service
        // was already mutated (steps 4), so a partial apply must not leave
        // it disabled.
        let outcome = self.apply_mutating(new, state_before != Some(TzAutoUpdateState::Disabled));
        if outcome.is_err() {
            self.rollback_service();
            self.service_state_before = None;
        }
        outcome
    }

    async fn revert(&mut self, original: &Self::Value) -> Result<(), ApplyError> {
        // 1. Without admin refuse: a partial revert is worse than a clean
        //    no-op, and reporting failure keeps the snapshot alive for a
        //    retry from an elevated start (the orchestrator keeps the
        //    snapshot while any planned revert failed).
        if !self.admin.is_admin() {
            return Err(ApplyError::InsufficientPrivileges);
        }

        // 2. Restore the timezone itself. Rules are read fresh from the
        //    registry, never stored.
        if self.setter.read_current()? != *original {
            let zone = self
                .registry
                .read_zone(&original.key_name)?
                .ok_or_else(|| ApplyError::UnknownZone(original.key_name.clone()))?;
            self.setter.set_dynamic(original, &zone)?;
        }

        // 3. Restore the service only when the snapshot knows its state.
        //    None (a v1 snapshot) means "do not touch" - guessing Enabled
        //    could switch on something the user had disabled.
        match original.service_state {
            Some(TzAutoUpdateState::Enabled) => {
                self.service
                    .set_start_type(SERVICE_NAME, ServiceStartType::AutoStart)?;
                self.service.start(SERVICE_NAME)?;
            }
            Some(TzAutoUpdateState::Disabled) => {
                self.service
                    .set_start_type(SERVICE_NAME, ServiceStartType::Disabled)?;
            }
            None => {
                tracing::debug!(
                    service = SERVICE_NAME,
                    "revert: service_state is None; not touched"
                );
            }
        }
        Ok(())
    }
}

impl<R: TzRegistry, S: ServiceController, V: WinTzSetter, A: AdminCheck>
    TimezoneApplier<R, S, V, A>
{
    /// Steps 4-6 of the apply order; every failure path here triggers the
    /// automatic service rollback in [Self::apply].
    fn apply_mutating(
        &mut self,
        new: &TimezoneValue,
        disable_service: bool,
    ) -> Result<AppliedChange, ApplyError> {
        // 4. Disable + stop the auto-update service so it cannot fight us.
        if disable_service {
            self.service
                .set_start_type(SERVICE_NAME, ServiceStartType::Disabled)?;
        }
        self.service.stop(SERVICE_NAME)?;

        // 5. Read the DST rules of the target zone; unknown -> UnknownZone.
        let zone = self
            .registry
            .read_zone(&new.key_name)?
            .ok_or_else(|| ApplyError::UnknownZone(new.key_name.clone()))?;

        // 6. Write the timezone.
        self.setter.set_dynamic(new, &zone)?;

        Ok(AppliedChange::changed(
            FIELD_NAME,
            &self.last_current_key.take().unwrap_or_default(),
            &new.key_name,
        ))
    }
}

impl<R, S, V, A> TimezoneApplier<R, S, V, A>
where
    R: TzRegistry,
    S: ServiceController,
    V: WinTzSetter,
    A: AdminCheck,
{
    /// Automatic rollback after a failed apply: return the service to the
    /// state captured before the first mutating step.
    fn rollback_service(&mut self) {
        match self.service_state_before {
            Some(TzAutoUpdateState::Enabled) => {
                if let Err(e) = self
                    .service
                    .set_start_type(SERVICE_NAME, ServiceStartType::AutoStart)
                    .and_then(|()| self.service.start(SERVICE_NAME))
                {
                    tracing::error!(service = SERVICE_NAME, error = %e, "service rollback failed");
                }
            }
            Some(TzAutoUpdateState::Disabled) => {}
            None => {}
        }
    }
}
