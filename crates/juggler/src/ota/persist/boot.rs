//! Boot orchestration: the early-boot slot note and the two-phase boot reconciliation.
//!
//! This module never reboots, never marks an image valid and never publishes; it returns what the runtime must do.

use super::kv::OtaKv;
use super::store::{OtaStore, SettleOutcome, StoreError};
use crate::ota::wire::RollbackReason;
use crate::ota::Admission;
use crate::ota::{reconcile, BootFacts, ReconcileAction, SlotId, SlotState, Version};

/// Hardware facts only; the runtime adds the version and report facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HardwareFacts {
    /// The slot currently running; [`FACTORY_SLOT`](super::keys::FACTORY_SLOT) when it is not `ota_0` / `ota_1`.
    pub running_slot: SlotId,
    /// Bootloader state of the running slot.
    pub running_state: SlotState,
    /// The slot an update would be written to and its state, or why it is unknown.
    ///
    /// Soft for the decision core: [`UpdateSlot::Absent`] and [`UpdateSlot::Unreadable`] both become `None` in `BootFacts`.
    pub update_slot: UpdateSlot,
}

/// What the hardware reader learned about the update slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateSlot {
    /// The slot an update would be written to, and its state.
    Present(SlotId, SlotState),
    /// The partition table has no update partition.
    Absent,
    /// The read failed; carries the backend error code.
    Unreadable(i32),
}

impl UpdateSlot {
    /// The decision core's view: `Some` only for [`UpdateSlot::Present`].
    pub const fn facts(self) -> Option<(SlotId, SlotState)> {
        match self {
            UpdateSlot::Present(slot, state) => Some((slot, state)),
            UpdateSlot::Absent | UpdateSlot::Unreadable(_) => None,
        }
    }
}

/// Why the hardware facts could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardwareReadError {
    /// The OTA handle stayed busy through every retry.
    Busy,
    /// The running slot could not be read; carries the backend error code.
    Read(i32),
}

/// Tallies failed attempts of a bounded retry loop and names the final error.
///
/// The platform reader records every failed try; after the last one [`BusyTally::into_error`] says
/// [`HardwareReadError::Busy`] only if every failure was "busy".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BusyTally {
    any: bool,
    all_busy: bool,
    last_code: i32,
}

impl BusyTally {
    /// An empty tally.
    pub const fn new() -> Self {
        Self {
            any: false,
            all_busy: true,
            last_code: 0,
        }
    }

    /// Records one failed try with its backend `code`; `busy` marks the "handle in use" failure.
    pub fn record(&mut self, code: i32, busy: bool) {
        self.any = true;
        self.all_busy &= busy;
        self.last_code = code;
    }

    /// The error to return once the tries are used up.
    pub const fn into_error(self) -> HardwareReadError {
        if self.any && !self.all_busy {
            HardwareReadError::Read(self.last_code)
        } else {
            HardwareReadError::Busy
        }
    }
}

impl Default for BusyTally {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of [`note_boot_slot`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteBoot {
    /// No attempt exists.
    NoAttempt,
    /// The attempt was already marked activated.
    AlreadyActivated,
    /// The running slot is not the attempt's slot.
    OtherSlot,
    /// The attempt was marked activated.
    Activated,
}

/// Why [`note_boot_slot`] failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteBootError {
    /// A record could not be read or written.
    Store(StoreError),
    /// The running slot could not be read.
    Hardware(HardwareReadError),
}

impl From<StoreError> for NoteBootError {
    fn from(e: StoreError) -> Self {
        NoteBootError::Store(e)
    }
}

/// What made the boot fail closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BootFault {
    /// Hardware facts were unavailable; nothing was written.
    Hardware(HardwareReadError),
    /// A record could not be read or was corrupt.
    Store(StoreError),
}

impl core::fmt::Display for HardwareReadError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            HardwareReadError::Busy => f.write_str("OTA handle stayed busy"),
            HardwareReadError::Read(code) => write!(f, "running slot unreadable (code {code})"),
        }
    }
}

impl core::fmt::Display for NoteBootError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            NoteBootError::Store(e) => write!(f, "{e}"),
            NoteBootError::Hardware(e) => write!(f, "{e}"),
        }
    }
}

impl core::fmt::Display for BootFault {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BootFault::Hardware(e) => write!(f, "boot facts unavailable: {e}"),
            BootFault::Store(e) => write!(f, "boot records unusable: {e}"),
        }
    }
}

impl core::error::Error for HardwareReadError {}

impl core::error::Error for NoteBootError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            NoteBootError::Store(e) => Some(e),
            NoteBootError::Hardware(e) => Some(e),
        }
    }
}

impl core::error::Error for BootFault {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            BootFault::Hardware(e) => Some(e),
            BootFault::Store(e) => Some(e),
        }
    }
}

/// How the boot ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BootDisposition {
    /// Nothing is in flight.
    ///
    /// Carries what the operator-rollback request settle step did, so the app can see a dropped or merged request.
    /// [`SettleOutcome::DroppedNotRolledBack`]: the rollback never happened.
    /// [`SettleOutcome::MergedIntoPending`]: the request's reason was lost because another report was undelivered.
    /// [`SettleOutcome::Reported`] never appears here; it ends as [`BootDisposition::ReportedRollback`].
    Settled(SettleOutcome),
    /// The attempted image runs and awaits its health check.
    AwaitHealthCheck,
    /// A rollback must start now; the attempt is kept.
    RollBackNow,
    /// A rollback report is persisted and awaits delivery.
    ReportedRollback,
    /// Evidence is missing; every record is unchanged and the next boot retries.
    Deferred,
    /// A write failed; the attempt is kept and the next boot repeats the sequence.
    Incomplete(StoreError),
    /// Facts or records were unreadable; no write happened.
    FailedClosed(BootFault),
}

/// Writes of [`reconcile_boot`] that failed without changing the safe action.
///
/// The sequence goes on after each of these, because the action (await the health check, roll back now) must not depend on bookkeeping; the runtime logs them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BootWarnings {
    /// The attempt could not be marked activated.
    ///
    /// The next boot finds the attempt un-activated and repeats the decision.
    pub activation_not_recorded: Option<StoreError>,
    /// The reason of a refused image (`version_mismatch`) could not be stored.
    ///
    /// The rollback still starts; the report that follows then names the reason `bootloader` instead.
    pub reason_not_recorded: Option<StoreError>,
}

/// Everything the runtime needs after [`reconcile_boot`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootOutcome {
    /// How the boot ended.
    pub disposition: BootDisposition,
    /// The decision core's action, when it ran.
    pub action: Option<ReconcileAction>,
    /// The hardware read succeeded and the running image is not `PendingVerify`, independent of every record.
    ///
    /// Stays `true` on `FailedClosed(BootFault::Store(..))` when the slot itself was readable and verified.
    pub slot_released: bool,
    /// Boot-time snapshot: `slot_released && store.admission() == Open && !failed_closed`.
    ///
    /// It goes stale as soon as a report is persisted or an attempt starts.
    /// The runtime must gate every offer on a live [`OtaStore::admission`] (plus `slot_released`), so a `Valid` boot after `ReportedRollback` answers `failed{report_pending}` and retains the offer.
    pub admission_open_at_boot: bool,
    /// The running image must not be marked valid.
    pub refuse_mark_valid: bool,
    /// The runtime must roll back now.
    pub roll_back_now: bool,
    /// A report awaits delivery.
    ///
    /// Fails closed: when the record cannot be read this is `true`, like [`OtaStore::admission`] blocking on an unreadable record.
    pub report_pending: bool,
    /// Bookkeeping writes that failed while the safe action was kept.
    pub warnings: BootWarnings,
}

/// Early-boot step: marks the attempt activated when the running slot is the attempt's slot.
///
/// `read_running` is called at most once and only when an unactivated attempt exists.
pub fn note_boot_slot<K: OtaKv>(
    store: &mut OtaStore<K>,
    read_running: impl FnOnce() -> Result<SlotId, HardwareReadError>,
) -> Result<NoteBoot, NoteBootError> {
    let attempt = match store.read_attempt()? {
        None => return Ok(NoteBoot::NoAttempt),
        Some(attempt) => attempt,
    };
    if attempt.record.activated {
        return Ok(NoteBoot::AlreadyActivated);
    }
    let running = read_running().map_err(NoteBootError::Hardware)?;
    if running != attempt.record.slot {
        return Ok(NoteBoot::OtherSlot);
    }
    store.mark_activated()?;
    Ok(NoteBoot::Activated)
}

fn settle<K: OtaKv>(
    store: &mut OtaStore<K>,
    running_slot: SlotId,
) -> Result<BootDisposition, StoreError> {
    Ok(match store.settle_request(running_slot)? {
        SettleOutcome::Reported => BootDisposition::ReportedRollback,
        outcome => BootDisposition::Settled(outcome),
    })
}

fn note_activation<K: OtaKv>(store: &mut OtaStore<K>, warnings: &mut BootWarnings) {
    if let Err(e) = store.mark_activated() {
        warnings.activation_not_recorded = Some(e);
    }
}

fn sequence<K: OtaKv>(
    store: &mut OtaStore<K>,
    hw: &HardwareFacts,
    action: ReconcileAction,
    running_version: Option<Version>,
    warnings: &mut BootWarnings,
) -> Result<BootDisposition, StoreError> {
    match action {
        ReconcileAction::NoAttempt => settle(store, hw.running_slot),
        ReconcileAction::Defer => Ok(BootDisposition::Deferred),
        ReconcileAction::ClearAttempt => {
            store.clear_attempt()?;
            settle(store, hw.running_slot)
        }
        ReconcileAction::CompleteAttempt => {
            match running_version {
                Some(version) => store.complete_attempt(version)?,
                // Unreachable: the core only completes an attempt whose version equals a `Some` running version.
                None => store.clear_attempt()?,
            }
            settle(store, hw.running_slot)
        }
        ReconcileAction::AwaitHealthCheck { mark_activated } => {
            if mark_activated {
                note_activation(store, warnings);
            }
            Ok(BootDisposition::AwaitHealthCheck)
        }
        ReconcileAction::RefuseImage { mark_activated, .. } => {
            if mark_activated {
                note_activation(store, warnings);
            }
            // The token is dropped on purpose: this boot never withdraws the reason, the rollback is not optional.
            if let Err(e) = store.note_rollback(RollbackReason::VersionMismatch, hw.running_slot) {
                warnings.reason_not_recorded = Some(e);
            }
            Ok(BootDisposition::RollBackNow)
        }
        ReconcileAction::ReportRollback {
            attempt_id,
            left,
            report_already_persisted,
        } => {
            store.report_rollback(attempt_id, left, report_already_persisted)?;
            Ok(BootDisposition::ReportedRollback)
        }
    }
}

fn outcome<K: OtaKv>(
    store: &OtaStore<K>,
    hw: Option<&HardwareFacts>,
    disposition: BootDisposition,
    action: Option<ReconcileAction>,
    warnings: BootWarnings,
) -> BootOutcome {
    let failed_closed = matches!(disposition, BootDisposition::FailedClosed(_));
    let roll_back_now = disposition == BootDisposition::RollBackNow;
    let unverified = match hw {
        Some(hw) => hw.running_state == SlotState::PendingVerify,
        None => true,
    };
    let slot_released = !unverified;
    BootOutcome {
        slot_released,
        admission_open_at_boot: slot_released
            && !failed_closed
            && store.admission() == Admission::Open,
        refuse_mark_valid: failed_closed || roll_back_now,
        roll_back_now,
        report_pending: store.report_undelivered().unwrap_or(true),
        warnings,
        disposition,
        action,
    }
}

/// Boot reconciliation in two phases.
///
/// 1. Read-only: hardware facts and every record; any failure fails closed with no write.
/// 2. Sequence: the decision core's action, with the persistence order of `ReconcileAction`.
///
/// `running_version` is the version embedded in the running image (`None` if it does not parse).
pub fn reconcile_boot<K: OtaKv>(
    store: &mut OtaStore<K>,
    hw: Result<HardwareFacts, HardwareReadError>,
    running_version: Option<Version>,
) -> BootOutcome {
    let hw = match hw {
        Ok(hw) => hw,
        Err(e) => {
            let fault = BootDisposition::FailedClosed(BootFault::Hardware(e));
            return outcome(store, None, fault, None, BootWarnings::default());
        }
    };
    let records = match store.read_phase() {
        Ok(records) => records,
        Err(e) => {
            let fault = BootDisposition::FailedClosed(BootFault::Store(e));
            return outcome(store, Some(&hw), fault, None, BootWarnings::default());
        }
    };
    let facts = BootFacts {
        running_slot: hw.running_slot,
        running_state: hw.running_state,
        running_version,
        update_slot: hw.update_slot.facts(),
        report_persisted: records.report_persisted,
    };
    let action = reconcile(records.attempt.as_ref().map(|a| &a.record), &facts);
    let mut warnings = BootWarnings::default();
    let disposition = sequence(store, &hw, action, running_version, &mut warnings)
        .unwrap_or_else(BootDisposition::Incomplete);
    outcome(store, Some(&hw), disposition, Some(action), warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tally_says_busy_only_when_every_failure_was_busy() {
        let mut tally = BusyTally::new();
        assert_eq!(tally.into_error(), HardwareReadError::Busy);
        tally.record(259, true);
        tally.record(259, true);
        assert_eq!(tally.into_error(), HardwareReadError::Busy);
        tally.record(5, false);
        assert_eq!(tally.into_error(), HardwareReadError::Read(5));
        tally.record(259, true);
        assert_eq!(tally.into_error(), HardwareReadError::Read(259));
    }
}
