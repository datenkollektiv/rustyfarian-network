//! Worker decisions: the gates of update, rollback and repair, the offer plan, and the retained blocked offer.
//!
//! The worker thread (ESP-IDF tier) executes these in order; every function here is pure or touches the store only through the generic [`OtaStore`].

use crate::ota::persist::{
    reconcile_boot, BootDisposition, BootFault, BootOutcome, HardwareFacts, HardwareReadError,
    OtaKv, OtaStore, StoreError, UpdateSlot, FACTORY_SLOT,
};
use crate::ota::{
    decide_offer, Admission, FailReason, ImageMetadata, Manifest, OfferDecision, OtaCommand,
    SlotId, SlotState, Version,
};

/// How a hardware read failure is reported: a handle that stayed busy is `busy`, anything else `partition_not_found`.
fn hardware_reason(e: HardwareReadError) -> FailReason {
    match e {
        HardwareReadError::Busy => FailReason::Busy,
        HardwareReadError::Read(_) => FailReason::PartitionNotFound,
    }
}

/// The first gate of an update, before the manifest is fetched.
///
/// `admission_open` is the library flag the health policy sets when it reaches a verdict for which `HealthVerdict::opens_admission` is true (never after `SlotUnreadable`); until then (or while the running image is still pending verification) no update starts, so `mark_valid` and the download never overlap.
/// `read_hardware` is called only when the flag is open, so a command that arrives early never competes with the health policy for the OTA handle.
///
/// # Errors
///
/// `pending_verify` while the flag is closed or the running slot is pending verification, `busy` / `partition_not_found` when the hardware facts could not be read.
pub fn plan_update_gate(
    admission_open: bool,
    read_hardware: impl FnOnce() -> Result<HardwareFacts, HardwareReadError>,
) -> Result<HardwareFacts, FailReason> {
    if !admission_open {
        return Err(FailReason::PendingVerify);
    }
    let hw = read_hardware().map_err(hardware_reason)?;
    if hw.running_state == SlotState::PendingVerify {
        return Err(FailReason::PendingVerify);
    }
    Ok(hw)
}

/// The early gate of an update, before any hardware is read: an update that arrives while the health policy has not yet opened admission nor settled is blocked, not failed.
///
/// A blocked offer is retained and re-evaluated silently once admission opens.
/// Once the health policy has settled without opening admission (for example `SlotUnreadable`), the update is not blocked here and fails in [`plan_update_gate`] instead.
pub const fn early_update_block(admission_open: bool, health_settled: bool) -> Option<OfferPlan> {
    if !admission_open && !health_settled {
        Some(OfferPlan::Blocked(FailReason::PendingVerify))
    } else {
        None
    }
}

/// The refused version for [`plan_offer`]: an unreadable record reads as absent.
///
/// The record is only a loop guard, so the offer proceeds; a completed update heals the record.
pub fn refused_or_absent(read: Result<Option<Version>, StoreError>) -> Option<Version> {
    read.unwrap_or_else(|e| {
        log::warn!(
            "[ota] the refused version is unreadable, ignored (a completed update heals it): {e}"
        );
        None
    })
}

/// Where an accepted offer will be written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proceed {
    /// The promised image: version and SHA-256.
    pub metadata: ImageMetadata,
    /// The slot the image is written to.
    pub slot: SlotId,
    /// The slot was already `Invalid` before the attempt.
    pub slot_was_invalid: bool,
}

/// The decision about one fetched manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OfferPlan {
    /// Start the attempt.
    Proceed(Proceed),
    /// Admission is blocked: retain the offer and re-evaluate when admission reopens; report the reason only on first sight.
    Blocked(FailReason),
    /// Reject the offer with this reason.
    Fail(FailReason),
}

/// Checks the manifest in the contract order: target chip, metadata, the offer decision, then the update slot.
///
/// `admission` and `refused` come from ONE short store lock (the live admission, not the boot snapshot).
pub fn plan_offer(
    running: Version,
    manifest: &Manifest,
    chip: &str,
    refused: Option<Version>,
    admission: Admission,
    hw: &HardwareFacts,
) -> OfferPlan {
    if let Err(e) = manifest.check_target(chip) {
        return OfferPlan::Fail(e.reason());
    }
    let metadata = match manifest.image_metadata() {
        Ok(metadata) => metadata,
        Err(e) => return OfferPlan::Fail(e.reason()),
    };
    let decision = decide_offer(running, metadata.version, refused, admission);
    if let Some(reason) = FailReason::from_offer(decision, admission) {
        return if decision == OfferDecision::Blocked {
            OfferPlan::Blocked(reason)
        } else {
            OfferPlan::Fail(reason)
        };
    }
    match hw.update_slot {
        UpdateSlot::Present(slot, state) => OfferPlan::Proceed(Proceed {
            metadata,
            slot,
            slot_was_invalid: state == SlotState::Invalid,
        }),
        UpdateSlot::Absent | UpdateSlot::Unreadable(_) => {
            OfferPlan::Fail(FailReason::PartitionNotFound)
        }
    }
}

/// What a failing `begin_attempt` means: a store that refuses admission blocks (the offer is retained), any other failure is `attempt_not_persisted`.
pub fn begin_failure(e: &StoreError) -> OfferPlan {
    match e {
        StoreError::NotAdmitted(by) => OfferPlan::Blocked(
            FailReason::from_offer(OfferDecision::Blocked, Admission::Blocked(*by))
                .unwrap_or(FailReason::AttemptUnresolved),
        ),
        _ => OfferPlan::Fail(FailReason::AttemptNotPersisted),
    }
}

/// Whether `failed` is published for a blocked offer: only on first sight, never on a silent re-evaluation.
pub const fn blocked_report(silent: bool, reason: FailReason) -> Option<FailReason> {
    if silent {
        None
    } else {
        Some(reason)
    }
}

/// Whether the retained offer should run now: one is retained, the library flag is open, and the live store admission is open.
///
/// The worker still has to win the busy flag before it takes the offer.
pub fn retained_due(has_offer: bool, admission_open: bool, store_admission: Admission) -> bool {
    has_offer && admission_open && store_admission == Admission::Open
}

/// The retained blocked offer: at most one, and only an update.
///
/// A newer update replaces it; a rollback or repair command leaves it untouched (it survives a repair and a failed rollback, and a rollback that succeeds reboots).
#[derive(Debug, Default)]
pub struct Retention {
    offer: Option<OtaCommand>,
}

impl Retention {
    /// An empty retention.
    pub const fn new() -> Self {
        Self { offer: None }
    }

    /// A command arrived: an update drops the old retained offer, anything else leaves it.
    ///
    /// The new update is retained only if it ends up blocked, via [`Retention::retain`].
    pub fn on_command(&mut self, command: &OtaCommand) {
        if matches!(command, OtaCommand::Update { .. }) {
            self.offer = None;
        }
    }

    /// Retains a blocked update; any other command is ignored.
    pub fn retain(&mut self, command: OtaCommand) {
        if matches!(command, OtaCommand::Update { .. }) {
            self.offer = Some(command);
        }
    }

    /// Takes the retained offer out to run it.
    pub fn take(&mut self) -> Option<OtaCommand> {
        self.offer.take()
    }

    /// An offer is retained.
    pub fn has_offer(&self) -> bool {
        self.offer.is_some()
    }
}

/// The gate of an operator rollback, before any record is written.
///
/// Order: `from` must equal the running version (`version_mismatch`), the library admission flag must be open (`pending_verify`; the hardware is not read before that), the hardware must be readable, the running slot must not be pending verification (`pending_verify`), and the running image must not be the factory image (`rollback_unavailable`).
/// The record-side checks (undelivered report, unresolved attempt) are made inside the store lock by `rollback_arm`.
///
/// # Errors
///
/// The [`FailReason`] to report.
pub fn plan_rollback(
    from: Version,
    running: Version,
    admission_open: bool,
    read_hardware: impl FnOnce() -> Result<HardwareFacts, HardwareReadError>,
) -> Result<SlotId, FailReason> {
    if from != running {
        return Err(FailReason::VersionMismatch);
    }
    if !admission_open {
        return Err(FailReason::PendingVerify);
    }
    let hw = read_hardware().map_err(hardware_reason)?;
    if hw.running_state == SlotState::PendingVerify {
        return Err(FailReason::PendingVerify);
    }
    if hw.running_slot == FACTORY_SLOT {
        return Err(FailReason::RollbackUnavailable);
    }
    Ok(hw.running_slot)
}

/// What a read-only probe of the records found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Probe {
    /// At least one record is unreadable.
    Corrupt,
    /// Every record is readable.
    Clean,
    /// The probe itself failed (a transient read error).
    Unreadable,
}

impl Probe {
    /// Maps the result of `OtaStore::corrupt_present`.
    pub fn from_store(result: Result<bool, StoreError>) -> Self {
        match result {
            Ok(true) => Probe::Corrupt,
            Ok(false) => Probe::Clean,
            Err(_) => Probe::Unreadable,
        }
    }
}

/// Everything the repair gate looks at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepairGate {
    /// The health policy reached a terminal verdict.
    pub health_settled: bool,
    /// The running slot is pending verification; `None` when it could not be read.
    pub slot_pending: Option<bool>,
    /// The boot ended fail-closed on a store fault.
    pub records_suspect: bool,
    /// What the read-only probe found now.
    pub probe: Probe,
}

impl RepairGate {
    /// Builds the gate without reading any hardware.
    ///
    /// Until the health policy settles it owns the OTA handle, so the slot state is unknown (`None`) and the gate answers `pending_verify`; once settled the slot is known not to be pending.
    pub const fn new(health_settled: bool, records_suspect: bool, probe: Probe) -> Self {
        Self {
            health_settled,
            slot_pending: if health_settled { Some(false) } else { None },
            records_suspect,
            probe,
        }
    }
}

/// The gate of a repair command.
///
/// - While the health policy has not settled and the slot is (or may be) pending verification, the health policy owns the slot and the attempt record: `pending_verify`.
/// - A repair needs a reason: a store fault at boot or an unreadable record now; otherwise `repair_not_needed`, so a retained or redelivered repair command can never become an automatic wipe.
///   A failed probe does not block: the repair itself then fails closed on the same read error.
///
/// # Errors
///
/// The [`FailReason`] to report.
pub fn plan_repair(gate: &RepairGate) -> Result<(), FailReason> {
    if !gate.health_settled && gate.slot_pending != Some(false) {
        return Err(FailReason::PendingVerify);
    }
    if !gate.records_suspect && gate.probe == Probe::Clean {
        return Err(FailReason::RepairNotNeeded);
    }
    Ok(())
}

/// The library flags after a repair, and what the re-run of boot reconciliation said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshedFlags {
    /// The re-run still ends fail-closed on a store fault.
    pub records_suspect: bool,
    /// The running image must still not be marked valid.
    pub refuse_mark_valid: bool,
    /// The full outcome, for logging; a `roll_back_now` here is only logged, never acted on.
    pub outcome: BootOutcome,
}

/// Re-runs boot reconciliation after a repair and derives the flags.
///
/// Reconciliation is built to be repeated after any crash, so a second run with unchanged facts changes no record.
/// `refuse_mark_valid` stays set when the boot demanded a rollback (`roll_back_now_at_boot`: a version mismatch); otherwise it follows the new outcome, which clears a refusal that only came from the unreadable records.
/// Returns `None` when the hardware facts could not be read: nothing is re-run and the flags stay as they are.
pub fn refresh_after_repair<K: OtaKv>(
    store: &mut OtaStore<K>,
    hw: Result<HardwareFacts, HardwareReadError>,
    running_version: Option<Version>,
    roll_back_now_at_boot: bool,
) -> Option<RefreshedFlags> {
    let hw = hw.ok()?;
    let outcome = reconcile_boot(store, Ok(hw), running_version);
    Some(RefreshedFlags {
        records_suspect: matches!(
            outcome.disposition,
            BootDisposition::FailedClosed(BootFault::Store(_))
        ),
        refuse_mark_valid: roll_back_now_at_boot || outcome.refuse_mark_valid,
        outcome,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ota::persist::fault_kv::FaultKv;
    use crate::ota::persist::KvError;
    use crate::ota::BlockedBy;
    use alloc::format;
    use alloc::string::String;

    const A: SlotId = SlotId(0);
    const B: SlotId = SlotId(1);
    const V1: Version = Version::new(1, 0, 0);
    const V2: Version = Version::new(2, 0, 0);
    const HASH: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    fn facts(state: SlotState, update: UpdateSlot) -> HardwareFacts {
        HardwareFacts {
            running_slot: A,
            running_state: state,
            update_slot: update,
        }
    }

    fn valid() -> HardwareFacts {
        facts(SlotState::Valid, UpdateSlot::Present(B, SlotState::Unknown))
    }

    fn manifest(version: &str, sha: &str, target: &str) -> Manifest {
        let body = format!(
            r#"{{"version":"{version}","sha256":"{sha}","url":"http://h/fw.bin","target":"{target}"}}"#
        );
        Manifest::parse(body.as_bytes()).unwrap()
    }

    fn plan(
        m: &Manifest,
        refused: Option<Version>,
        admission: Admission,
        hw: &HardwareFacts,
    ) -> OfferPlan {
        plan_offer(V1, m, "esp32c3", refused, admission, hw)
    }

    #[test]
    fn the_update_gate_orders_flag_hardware_pending() {
        assert_eq!(plan_update_gate(true, || Ok(valid())), Ok(valid()));
        assert_eq!(
            plan_update_gate(false, || Ok(valid())),
            Err(FailReason::PendingVerify)
        );
        let pending = facts(SlotState::PendingVerify, UpdateSlot::Absent);
        assert_eq!(
            plan_update_gate(true, || Ok(pending)),
            Err(FailReason::PendingVerify)
        );
        assert_eq!(
            plan_update_gate(true, || Err(HardwareReadError::Busy)),
            Err(FailReason::Busy)
        );
        assert_eq!(
            plan_update_gate(true, || Err(HardwareReadError::Read(5))),
            Err(FailReason::PartitionNotFound)
        );
    }

    #[test]
    fn an_early_update_is_retained_then_proceeds_once_admission_opens() {
        let good = manifest("2.0.0", HASH, "esp32c3");
        let cmd = update("http://h/a");
        let mut retention = Retention::new();
        retention.on_command(&cmd);
        assert_eq!(
            early_update_block(false, false),
            Some(OfferPlan::Blocked(FailReason::PendingVerify))
        );
        retention.retain(cmd.clone());
        assert!(retention.has_offer());
        assert!(!retained_due(retention.has_offer(), false, Admission::Open));
        assert_eq!(early_update_block(true, false), None);
        assert_eq!(early_update_block(true, true), None);
        assert_eq!(
            early_update_block(false, true),
            None,
            "settled without opening admission fails in the gate instead"
        );
        assert_eq!(
            plan_update_gate(false, || Ok(valid())),
            Err(FailReason::PendingVerify)
        );
        assert!(retained_due(retention.has_offer(), true, Admission::Open));
        assert_eq!(retention.take(), Some(cmd));
        let hw = plan_update_gate(true, || Ok(valid())).unwrap();
        assert!(matches!(
            plan(&good, None, Admission::Open, &hw),
            OfferPlan::Proceed(_)
        ));
    }

    fn boot(
        disposition: crate::ota::persist::BootDisposition,
        released: bool,
    ) -> crate::ota::persist::BootOutcome {
        crate::ota::persist::BootOutcome {
            disposition,
            action: None,
            slot_released: released,
            admission_open_at_boot: false,
            refuse_mark_valid: false,
            roll_back_now: false,
            report_pending: false,
            warnings: Default::default(),
        }
    }

    #[test]
    fn a_valid_boot_accepts_update_rollback_and_repair_at_once() {
        use crate::ota::persist::BootDisposition;
        let initial = super::super::initial_admission(&boot(BootDisposition::Deferred, true));
        assert!(initial);
        let good = manifest("2.0.0", HASH, "esp32c3");
        assert_eq!(early_update_block(initial, initial), None);
        let hw = plan_update_gate(initial, || Ok(valid())).unwrap();
        assert!(matches!(
            plan(&good, None, Admission::Open, &hw),
            OfferPlan::Proceed(_)
        ));
        assert_eq!(
            plan_rollback(V1, V1, initial, || Ok(valid())),
            Ok(valid().running_slot)
        );
        assert_eq!(
            plan_repair(&RepairGate::new(initial, true, Probe::Corrupt)),
            Ok(())
        );
    }

    #[test]
    fn a_pending_verify_boot_keeps_retaining_and_refusing_until_the_policy_opens() {
        use crate::ota::persist::BootDisposition;
        let initial =
            super::super::initial_admission(&boot(BootDisposition::AwaitHealthCheck, false));
        assert!(!initial);
        assert_eq!(
            early_update_block(initial, initial),
            Some(OfferPlan::Blocked(FailReason::PendingVerify))
        );
        assert_eq!(
            plan_rollback(V1, V1, initial, || Ok(valid())),
            Err(FailReason::PendingVerify)
        );
        assert_eq!(
            plan_repair(&RepairGate::new(initial, true, Probe::Corrupt)),
            Err(FailReason::PendingVerify)
        );
    }

    #[test]
    fn an_unreadable_refused_version_reads_as_absent() {
        let good = manifest("2.0.0", HASH, "esp32c3");
        let refused = refused_or_absent(Err(StoreError::Kv(KvError::new(5))));
        assert_eq!(refused, None);
        assert!(matches!(
            plan(&good, refused, Admission::Open, &valid()),
            OfferPlan::Proceed(_)
        ));
        assert_eq!(refused_or_absent(Ok(Some(V2))), Some(V2));
        assert_eq!(refused_or_absent(Ok(None)), None);
        assert_eq!(
            plan(
                &good,
                refused_or_absent(Ok(Some(V2))),
                Admission::Open,
                &valid()
            ),
            OfferPlan::Fail(FailReason::PreviouslyRolledBack)
        );
    }

    #[test]
    fn repair_is_pending_verify_while_health_is_unsettled() {
        for probe in [Probe::Corrupt, Probe::Clean, Probe::Unreadable] {
            for suspect in [true, false] {
                let gate = RepairGate::new(false, suspect, probe);
                assert_eq!(gate.slot_pending, None, "no hardware was read");
                assert_eq!(plan_repair(&gate), Err(FailReason::PendingVerify));
            }
        }
        let settled = RepairGate::new(true, true, Probe::Corrupt);
        assert_eq!(settled.slot_pending, Some(false));
        assert_eq!(plan_repair(&settled), Ok(()));
        assert_eq!(
            plan_repair(&RepairGate::new(true, false, Probe::Clean)),
            Err(FailReason::RepairNotNeeded)
        );
    }

    #[test]
    fn a_closed_flag_never_reads_the_hardware() {
        let reads = core::cell::Cell::new(0);
        let read = || {
            reads.set(reads.get() + 1);
            Ok(valid())
        };
        assert_eq!(
            plan_update_gate(false, read),
            Err(FailReason::PendingVerify)
        );
        assert_eq!(
            plan_rollback(V1, V1, false, read),
            Err(FailReason::PendingVerify)
        );
        assert_eq!(
            plan_rollback(V2, V1, true, read),
            Err(FailReason::VersionMismatch)
        );
        assert_eq!(reads.get(), 0);
    }

    #[test]
    fn plan_offer_follows_the_contract_order() {
        let open = Admission::Open;
        let good = manifest("2.0.0", HASH, "esp32c3");
        match plan(&good, None, open, &valid()) {
            OfferPlan::Proceed(p) => {
                assert_eq!(p.metadata.version, V2);
                assert_eq!((p.slot, p.slot_was_invalid), (B, false));
            }
            other => panic!("{other:?}"),
        }
        let wrong_chip = manifest("2.0.0", HASH, "esp32");
        assert_eq!(
            plan(&wrong_chip, None, open, &valid()),
            OfferPlan::Fail(FailReason::TargetMismatch)
        );
        let same_and_wrong_chip = manifest("1.0.0", HASH, "esp32");
        assert_eq!(
            plan(&same_and_wrong_chip, None, open, &valid()),
            OfferPlan::Fail(FailReason::TargetMismatch),
            "the target is checked before the version"
        );
        let bad_digest = manifest("2.0.0", "zz", "esp32c3");
        assert_eq!(
            plan(&bad_digest, None, open, &valid()),
            OfferPlan::Fail(FailReason::ManifestInvalid)
        );
        assert_eq!(
            plan(&manifest("1.0.0", HASH, "esp32c3"), None, open, &valid()),
            OfferPlan::Fail(FailReason::UpToDate)
        );
        assert_eq!(
            plan(&manifest("0.9.0", HASH, "esp32c3"), None, open, &valid()),
            OfferPlan::Fail(FailReason::Downgrade)
        );
        assert_eq!(
            plan(&good, Some(V2), open, &valid()),
            OfferPlan::Fail(FailReason::PreviouslyRolledBack)
        );
    }

    #[test]
    fn a_blocked_offer_is_blocked_not_failed() {
        let good = manifest("2.0.0", HASH, "esp32c3");
        assert_eq!(
            plan(
                &good,
                None,
                Admission::Blocked(BlockedBy::ReportPending),
                &valid()
            ),
            OfferPlan::Blocked(FailReason::ReportPending)
        );
        assert_eq!(
            plan(
                &good,
                None,
                Admission::Blocked(BlockedBy::AttemptUnresolved),
                &valid()
            ),
            OfferPlan::Blocked(FailReason::AttemptUnresolved)
        );
        let same = manifest("1.0.0", HASH, "esp32c3");
        assert_eq!(
            plan(
                &same,
                None,
                Admission::Blocked(BlockedBy::ReportPending),
                &valid()
            ),
            OfferPlan::Fail(FailReason::UpToDate),
            "a skip is not retained"
        );
    }

    #[test]
    fn the_update_slot_must_be_present() {
        let good = manifest("2.0.0", HASH, "esp32c3");
        for update in [UpdateSlot::Absent, UpdateSlot::Unreadable(5)] {
            let hw = facts(SlotState::Valid, update);
            assert_eq!(
                plan(&good, None, Admission::Open, &hw),
                OfferPlan::Fail(FailReason::PartitionNotFound)
            );
        }
        let hw = facts(SlotState::Valid, UpdateSlot::Present(B, SlotState::Invalid));
        match plan(&good, None, Admission::Open, &hw) {
            OfferPlan::Proceed(p) => assert!(p.slot_was_invalid),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn begin_failures_map_to_blocked_or_not_persisted() {
        assert_eq!(
            begin_failure(&StoreError::NotAdmitted(BlockedBy::ReportPending)),
            OfferPlan::Blocked(FailReason::ReportPending)
        );
        assert_eq!(
            begin_failure(&StoreError::NotAdmitted(BlockedBy::AttemptUnresolved)),
            OfferPlan::Blocked(FailReason::AttemptUnresolved)
        );
        for e in [
            StoreError::Kv(KvError::new(5)),
            StoreError::InvalidSlot,
            StoreError::ReportPending,
        ] {
            assert_eq!(
                begin_failure(&e),
                OfferPlan::Fail(FailReason::AttemptNotPersisted)
            );
        }
    }

    #[test]
    fn blocked_is_reported_only_on_first_sight() {
        assert_eq!(
            blocked_report(false, FailReason::ReportPending),
            Some(FailReason::ReportPending)
        );
        assert_eq!(blocked_report(true, FailReason::ReportPending), None);
    }

    #[test]
    fn the_retained_offer_runs_only_when_both_gates_are_open() {
        let open = Admission::Open;
        let blocked = Admission::Blocked(BlockedBy::ReportPending);
        assert!(retained_due(true, true, open));
        assert!(!retained_due(false, true, open));
        assert!(!retained_due(true, false, open));
        assert!(!retained_due(true, true, blocked));
    }

    fn update(url: &str) -> OtaCommand {
        OtaCommand::Update {
            manifest_url: String::from(url),
            sig_present: false,
        }
    }

    #[test]
    fn only_an_update_replaces_the_retained_offer() {
        let mut r = Retention::new();
        r.retain(update("http://h/a"));
        assert!(r.has_offer());
        r.on_command(&OtaCommand::Repair);
        assert!(r.has_offer(), "repair leaves it");
        r.on_command(&OtaCommand::Rollback { from: V1 });
        assert!(r.has_offer(), "rollback leaves it");
        r.on_command(&update("http://h/b"));
        assert!(!r.has_offer(), "a newer update drops it");
        r.retain(update("http://h/b"));
        assert_eq!(r.take(), Some(update("http://h/b")));
        assert_eq!(r.take(), None);
    }

    #[test]
    fn only_an_update_can_be_retained() {
        let mut r = Retention::new();
        r.retain(OtaCommand::Repair);
        r.retain(OtaCommand::Rollback { from: V1 });
        assert!(!r.has_offer());
    }

    #[test]
    fn the_rollback_gate_orders_version_flag_hardware_pending() {
        assert_eq!(plan_rollback(V1, V1, true, || Ok(valid())), Ok(A));
        assert_eq!(
            plan_rollback(V2, V1, true, || Err(HardwareReadError::Busy)),
            Err(FailReason::VersionMismatch),
            "the version is checked first"
        );
        assert_eq!(
            plan_rollback(V1, V1, true, || Err(HardwareReadError::Read(3))),
            Err(FailReason::PartitionNotFound)
        );
        assert_eq!(
            plan_rollback(V1, V1, true, || Err(HardwareReadError::Busy)),
            Err(FailReason::Busy)
        );
        let pending = facts(SlotState::PendingVerify, UpdateSlot::Absent);
        assert_eq!(
            plan_rollback(V1, V1, true, || Ok(pending)),
            Err(FailReason::PendingVerify)
        );
        let mut factory = valid();
        factory.running_slot = FACTORY_SLOT;
        assert_eq!(
            plan_rollback(V1, V1, true, || Ok(factory)),
            Err(FailReason::RollbackUnavailable),
            "the factory image has nothing to roll back to"
        );
    }

    fn gate(settled: bool, pending: Option<bool>, suspect: bool, probe: Probe) -> RepairGate {
        RepairGate {
            health_settled: settled,
            slot_pending: pending,
            records_suspect: suspect,
            probe,
        }
    }

    #[test]
    fn the_repair_gate() {
        use Probe::*;
        assert_eq!(
            plan_repair(&gate(false, Some(true), true, Corrupt)),
            Err(FailReason::PendingVerify),
            "health owns a pending slot"
        );
        assert_eq!(
            plan_repair(&gate(false, None, true, Corrupt)),
            Err(FailReason::PendingVerify),
            "an unreadable slot may be pending"
        );
        assert_eq!(
            plan_repair(&gate(false, Some(false), false, Corrupt)),
            Ok(())
        );
        assert_eq!(plan_repair(&gate(true, Some(true), true, Clean)), Ok(()));
        assert_eq!(plan_repair(&gate(true, None, false, Corrupt)), Ok(()));
        assert_eq!(
            plan_repair(&gate(true, Some(false), false, Clean)),
            Err(FailReason::RepairNotNeeded)
        );
        assert_eq!(
            plan_repair(&gate(true, Some(false), false, Unreadable)),
            Ok(()),
            "a failed probe does not block; the repair fails closed itself"
        );
    }

    #[test]
    fn the_probe_maps_the_store_result() {
        assert_eq!(Probe::from_store(Ok(true)), Probe::Corrupt);
        assert_eq!(Probe::from_store(Ok(false)), Probe::Clean);
        assert_eq!(
            Probe::from_store(Err(StoreError::Kv(KvError::new(1)))),
            Probe::Unreadable
        );
    }

    fn store(kv: &FaultKv) -> OtaStore<FaultKv> {
        OtaStore::open(kv.clone(), || 0xC0FF_EE00)
    }

    fn rolled_back_world(kv: &FaultKv) {
        kv.put_u32("att_ctr", 5).put_u32("att_epoch", 1);
        kv.put_u32("att_id", 5)
            .put_u8("att_inv", 0)
            .put_u8("att_boot", 1)
            .put_u8("att_act", 1)
            .put_str("att_ver", "2.0.0")
            .put_str("att_slot", "ota_1");
    }

    #[test]
    fn reconcile_boot_is_stable_when_re_run() {
        let kv = FaultKv::new();
        rolled_back_world(&kv);
        let mut s = store(&kv);
        let first = reconcile_boot(&mut s, Ok(valid()), Some(V1));
        assert_eq!(first.disposition, BootDisposition::ReportedRollback);
        let after_first = kv.snapshot();
        let refreshed = refresh_after_repair(&mut s, Ok(valid()), Some(V1), false).unwrap();
        assert!(!refreshed.records_suspect && !refreshed.refuse_mark_valid);
        assert_eq!(kv.snapshot(), after_first);
        kv.reset_counters();
        let again = refresh_after_repair(&mut s, Ok(valid()), Some(V1), false).unwrap();
        assert_eq!(again, refreshed);
        assert_eq!(kv.commits(), 0, "from the second run on nothing is written");
    }

    #[test]
    fn a_repair_clears_the_fail_closed_flags_it_caused() {
        let kv = FaultKv::new();
        rolled_back_world(&kv);
        kv.corrupt_with("att_ver", KvError::CODE_INVALID_VALUE);
        let mut s = store(&kv);
        let boot = reconcile_boot(&mut s, Ok(valid()), Some(V1));
        assert!(matches!(boot.disposition, BootDisposition::FailedClosed(_)));
        assert!(boot.refuse_mark_valid);
        let still = refresh_after_repair(&mut s, Ok(valid()), Some(V1), false).unwrap();
        assert!(still.records_suspect && still.refuse_mark_valid);
        assert_eq!(crate::ota::runtime::repair_all(&mut s, 32), Ok(1));
        let fixed = refresh_after_repair(&mut s, Ok(valid()), Some(V1), false).unwrap();
        assert!(!fixed.records_suspect);
        assert!(
            !fixed.refuse_mark_valid,
            "the refusal came only from the fault"
        );
    }

    #[test]
    fn a_refusal_from_a_demanded_rollback_is_never_cleared() {
        let kv = FaultKv::new();
        let mut s = store(&kv);
        let refreshed = refresh_after_repair(&mut s, Ok(valid()), Some(V1), true).unwrap();
        assert!(refreshed.refuse_mark_valid);
        assert!(!refreshed.records_suspect);
    }

    #[test]
    fn unreadable_hardware_skips_the_re_run() {
        let kv = FaultKv::new();
        let mut s = store(&kv);
        kv.reset_counters();
        assert_eq!(
            refresh_after_repair(&mut s, Err(HardwareReadError::Busy), Some(V1), false),
            None
        );
        assert_eq!(kv.reads() + kv.commits(), 0);
    }
}
