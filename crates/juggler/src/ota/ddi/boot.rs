//! The boot sequence of a DDI runtime as flat plan functions (ADR 023 rung 2): what a tier's `DdiRuntime::start` decides before its worker runs.
//!
//! The sequence is short and runs once, on the caller, in this order, so it stays flat code rather than a machine:
//!
//! 1. the tier reads the hardware facts, then [`plan_boot`] reconciles the records with them ([`reconcile_boot`]) and derives the [`RuntimeFlags`];
//! 2. when the reconciliation failed closed on a store fault ([`BootPlan::needs_repair`]), [`repair_at_boot`] repairs up to [`BOOT_REPAIR_PASSES`] record groups, the tier reads the hardware again outside any store borrow, and [`after_repair`] re-runs the reconciliation and says whether a 1005 `records_unreadable` report is owed ([`repair_report_owed`]);
//! 3. the tier copies the flags into its own, rolls back at once when [`RuntimeFlags::roll_back_now_at_boot`] is set, and warns when [`unbound_report_blocks`] holds.
//!
//! Every function takes the store for one short borrow and reads no clock and no hardware; the hardware arrives as an input.

use crate::ota::health::{
    refresh_after_repair, repair_all, repair_report_owed, RefreshedFlags, RepairError, RuntimeFlags,
};
use crate::ota::persist::{
    reconcile_boot, BootOutcome, HardwareFacts, HardwareReadError, OtaKv, OtaStore,
};
use crate::ota::{Admission, BlockedBy, Version};

/// Experimental: API may change before 1.0.
///
/// Repair passes of the boot repair; the store has only a handful of record groups, one per pass.
pub const BOOT_REPAIR_PASSES: u32 = 32;

/// Experimental: API may change before 1.0.
///
/// What the boot reconciliation said, and the flags the runtime starts with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BootPlan {
    /// The reconciliation outcome, for logging.
    pub outcome: BootOutcome,
    /// The flags derived from it ([`RuntimeFlags::from_boot`]); [`after_repair`] updates them.
    pub flags: RuntimeFlags,
}

impl BootPlan {
    /// The reconciliation failed closed on a store fault: run [`repair_at_boot`], then [`after_repair`].
    pub fn needs_repair(&self) -> bool {
        self.outcome.records_suspect()
    }
}

/// Experimental: API may change before 1.0.
///
/// Step 1: reconciles the boot records with the hardware facts the tier read, for the running image `running`, and derives the flags.
pub fn plan_boot<K: OtaKv>(
    store: &mut OtaStore<K>,
    hw: Result<HardwareFacts, HardwareReadError>,
    running: Version,
) -> BootPlan {
    let outcome = reconcile_boot(store, hw, Some(running));
    let flags = RuntimeFlags::from_boot(&outcome);
    BootPlan { outcome, flags }
}

/// Experimental: API may change before 1.0.
///
/// Step 2a: repairs the unreadable records, at most [`BOOT_REPAIR_PASSES`] groups ([`repair_all`]).
///
/// # Errors
///
/// As [`repair_all`]; the result goes to [`after_repair`] either way.
pub fn repair_at_boot<K: OtaKv>(store: &mut OtaStore<K>) -> Result<u32, RepairError> {
    repair_all(store, BOOT_REPAIR_PASSES)
}

/// Experimental: API may change before 1.0.
///
/// What [`after_repair`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AfterRepair {
    /// The re-run of the reconciliation, `None` when the hardware could not be read (the flags are then unchanged).
    pub refreshed: Option<RefreshedFlags>,
    /// The next offered action is reported 1005 `records_unreadable` instead of being applied ([`repair_report_owed`]).
    pub report_owed: bool,
}

/// Experimental: API may change before 1.0.
///
/// Step 2b: after [`repair_at_boot`] returned `repaired` and the tier read the hardware again, re-runs the reconciliation ([`refresh_after_repair`]), updates `flags.records_suspect` and `flags.refuse_mark_valid`, and decides whether a report is owed.
///
/// A rollback the re-run would demand is only reported in [`AfterRepair::refreshed`], never acted on: the health policy owns that image.
pub fn after_repair<K: OtaKv>(
    store: &mut OtaStore<K>,
    repaired: Result<u32, RepairError>,
    hw: Result<HardwareFacts, HardwareReadError>,
    running: Version,
    flags: &mut RuntimeFlags,
) -> AfterRepair {
    let refreshed = refresh_after_repair(store, hw, Some(running), flags.roll_back_now_at_boot);
    if let Some(r) = &refreshed {
        flags.records_suspect = r.records_suspect;
        flags.refuse_mark_valid = r.refuse_mark_valid;
    }
    AfterRepair {
        refreshed,
        report_owed: repair_report_owed(repaired, flags.records_suspect),
    }
}

/// Experimental: API may change before 1.0.
///
/// Step 3: a `rolled_back` report of the MQTT OTA runtime that no DDI verdict binds blocks every DDI offer.
///
/// Nothing is written; the tier warns the operator, because only an MQTT OTA image delivers that report; the last release that builds one is 0.8, and the recovery is recorded in `docs/features/ddi-client-v1.md`.
pub fn unbound_report_blocks<K: OtaKv>(store: &OtaStore<K>) -> bool {
    store.admission() == Admission::Blocked(BlockedBy::ReportPending)
        && matches!(store.pending_verdict(), Ok(None))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ota::persist::fault_kv::FaultKv;
    use crate::ota::{BootDisposition, SlotId, SlotState, UpdateSlot};

    fn running() -> Version {
        Version::new(1, 0, 0)
    }

    fn valid_a() -> Result<HardwareFacts, HardwareReadError> {
        Ok(HardwareFacts {
            running_slot: SlotId(0),
            running_state: SlotState::Valid,
            update_slot: UpdateSlot::Present(SlotId(1), SlotState::Unknown),
        })
    }

    #[test]
    fn a_clean_valid_boot_opens_admission_and_needs_no_repair() {
        let kv = FaultKv::new();
        let mut s = OtaStore::open(kv, || 1);
        let plan = plan_boot(&mut s, valid_a(), running());
        assert!(!plan.needs_repair());
        assert!(plan.flags.admission_open);
        assert!(plan.flags.health_settled);
        assert!(!plan.flags.roll_back_now_at_boot);
        assert!(!unbound_report_blocks(&s));
    }

    #[test]
    fn unreadable_hardware_keeps_admission_closed() {
        let kv = FaultKv::new();
        let mut s = OtaStore::open(kv, || 1);
        let plan = plan_boot(&mut s, Err(HardwareReadError::Read(-1)), running());
        assert!(!plan.flags.admission_open);
        assert!(!plan.flags.health_settled);
    }

    #[test]
    fn a_corrupt_record_is_repaired_and_owes_a_report() {
        let kv = FaultKv::new();
        kv.put_str("ddi_act", "042");
        let mut s = OtaStore::open(kv.clone(), || 1);
        let plan = plan_boot(&mut s, valid_a(), running());
        assert!(plan.needs_repair(), "{:?}", plan.outcome.disposition);
        let mut flags = plan.flags;
        let repaired = repair_at_boot(&mut s);
        assert!(matches!(repaired, Ok(n) if n > 0), "{repaired:?}");
        let after = after_repair(&mut s, repaired, valid_a(), running(), &mut flags);
        assert!(after.report_owed);
        let refreshed = after.refreshed.expect("hardware read");
        assert!(!refreshed.records_suspect);
        assert!(!flags.records_suspect, "the flags follow the re-run");
        assert!(!matches!(
            refreshed.outcome.disposition,
            BootDisposition::FailedClosed(_)
        ));
    }

    #[test]
    fn a_repair_with_unreadable_hardware_keeps_the_flags_and_the_debt() {
        let kv = FaultKv::new();
        kv.put_str("ddi_act", "042");
        let mut s = OtaStore::open(kv.clone(), || 1);
        let plan = plan_boot(&mut s, valid_a(), running());
        let mut flags = plan.flags;
        let repaired = repair_at_boot(&mut s);
        let after = after_repair(
            &mut s,
            repaired,
            Err(HardwareReadError::Busy),
            running(),
            &mut flags,
        );
        assert_eq!(after.refreshed, None);
        assert_eq!(flags, plan.flags, "unchanged without a hardware read");
        assert!(after.report_owed);
    }

    #[test]
    fn a_transient_read_fault_owes_no_report() {
        let mut failed_closed = 0;
        for n in 1..30 {
            let kv = FaultKv::new();
            let mut s = OtaStore::open(kv.clone(), || 1);
            kv.reset_counters();
            kv.fail_read_at(n);
            let plan = plan_boot(&mut s, valid_a(), running());
            if !plan.needs_repair() {
                continue;
            }
            failed_closed += 1;
            let mut flags = plan.flags;
            let repaired = repair_at_boot(&mut s);
            let after = after_repair(&mut s, repaired, valid_a(), running(), &mut flags);
            assert_eq!(repaired, Ok(0), "read {n}");
            assert!(
                !after.report_owed,
                "read {n}: a flaky read must not fail a valid action"
            );
        }
        assert!(failed_closed > 0, "some read fails the boot closed");
    }
}
