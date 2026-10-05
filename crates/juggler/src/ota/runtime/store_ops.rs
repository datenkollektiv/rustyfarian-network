//! Record sequences of the runtime that need more than one store call.
//!
//! Every function takes `&mut OtaStore<K>` so the driver locks the store only for the duration of one call and holds it across nothing else (no HTTP, no `OtaSession`, no hardware read, no publish).
//! None of them reboots, marks an image valid or rolls back.

use crate::ota::persist::{
    Delivery, FailedDownload, HardwareFacts, HardwareReadError, OtaKv, OtaStore, ReasonNote,
    RefusalNote, RollbackNoted, StoreError,
};
use crate::ota::{Admission, BlockedBy, FailReason, RollbackReason, SlotId, Version};

fn blocked_reason(by: BlockedBy) -> FailReason {
    match by {
        BlockedBy::AttemptUnresolved => FailReason::AttemptUnresolved,
        BlockedBy::ReportPending => FailReason::ReportPending,
    }
}

/// What [`rollback_arm`] wrote, so [`rollback_undo`] can take back exactly that.
///
/// Must be passed to [`rollback_undo`] if the rollback itself fails to start; it is dropped once the rollback has started (the device reboots).
#[must_use = "pass it to `rollback_undo` if the rollback fails to start"]
#[derive(Debug)]
pub struct ArmedRollback {
    noted: Option<RollbackNoted>,
    refusal: Option<Option<Version>>,
}

impl ArmedRollback {
    /// This call recorded the rollback reason (an operator request).
    pub fn wrote_reason(&self) -> bool {
        self.noted.is_some()
    }

    /// This call replaced or created the refused version.
    pub fn wrote_refusal(&self) -> bool {
        self.refusal.is_some()
    }
}

/// Arms an operator rollback of the running image: records the reason, then refuses the running version.
///
/// Runs inside ONE store lock, and starts with the live admission check, so a report that is still undelivered, an unresolved attempt or an armed request rejects the rollback (`report_pending` or `attempt_unresolved`) WITHOUT writing anything.
/// That closes the window in which the reporter or the health policy could change the records between a check and the writes.
///
/// A storage failure is `attempt_not_persisted`; if the reason was written and the refusal could not be, the reason is withdrawn again.
/// A running slot without a stored label (the factory image) is `rollback_unavailable`.
///
/// # Errors
///
/// The [`FailReason`] to report; on `Err` the records are as they were (or the best-effort withdraw failed, which leaves an unarmed request without its marker).
pub fn rollback_arm<K: OtaKv>(
    store: &mut OtaStore<K>,
    running_slot: SlotId,
    running: Version,
) -> Result<ArmedRollback, FailReason> {
    if let Admission::Blocked(by) = store.admission() {
        return Err(blocked_reason(by));
    }
    let noted = match store.note_rollback(RollbackReason::Operator, running_slot) {
        Ok(ReasonNote::Written(token)) => Some(token),
        Ok(ReasonNote::KeptFirst) => None,
        Err(StoreError::InvalidSlot) => return Err(FailReason::RollbackUnavailable),
        Err(_) => return Err(FailReason::AttemptNotPersisted),
    };
    let refusal = match store.refuse_version(running) {
        Ok(RefusalNote::Written { previous }) => Some(previous),
        Ok(RefusalNote::AlreadyRefused) => None,
        Err(_) => {
            if let Some(token) = noted {
                let _ = store.withdraw_rollback(token);
            }
            return Err(FailReason::AttemptNotPersisted);
        }
    };
    Ok(ArmedRollback { noted, refusal })
}

/// The result of each independent step of [`rollback_undo`]; `None` means the step had nothing to undo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Undone {
    /// Restoring the refusal this call replaced.
    pub refusal: Option<Result<(), StoreError>>,
    /// Withdrawing the reason or request this call wrote.
    pub reason: Option<Result<(), StoreError>>,
}

impl Undone {
    /// Every step that had something to undo succeeded.
    pub fn is_clean(&self) -> bool {
        !matches!(self.refusal, Some(Err(_))) && !matches!(self.reason, Some(Err(_)))
    }
}

/// Takes back what [`rollback_arm`] wrote, after `OtaSession::rollback` failed.
///
/// The two steps are attempted INDEPENDENTLY: a failure of one never skips the other, and each only runs if this call wrote it.
/// Both results are returned so the driver can log them.
pub fn rollback_undo<K: OtaKv>(store: &mut OtaStore<K>, armed: ArmedRollback) -> Undone {
    let refusal = armed
        .refusal
        .map(|previous| store.restore_refusal(previous));
    let reason = armed.noted.map(|token| store.withdraw_rollback(token));
    Undone { refusal, reason }
}

/// Marks the report for `attempt_id` delivered, and nothing else.
///
/// Deliberately NO request settling and NO hardware read: boot reconciliation already settled every request armed before this boot, so a settle here could only delete a request armed in THIS boot (an operator rollback, a health-deadline note) and lose its report.
/// Admission reopens by itself once no attempt, no request and no undelivered report remain.
///
/// # Errors
///
/// [`StoreError`] when the flag cannot be read or written; the report stays pending and is retried.
pub fn ack_delivered<K: OtaKv>(
    store: &mut OtaStore<K>,
    attempt_id: u32,
) -> Result<Delivery, StoreError> {
    store.mark_report_delivered(attempt_id)
}

/// The result of one background attempt to record the completed attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionTry {
    /// The attempt record was cleared; admission reopens by itself.
    Completed,
    /// No attempt for the running version remains (cleared by a repair, or it belongs to another version); stop retrying.
    Gone,
    /// The attempt is still present and could not be cleared (or read); retry later.
    StillFailing,
}

/// One background retry of `complete_attempt(running)` after `ApplyNotRecorded`, inside ONE store lock.
///
/// The caller guarantees the running slot is known valid (`mark_valid` succeeded) and holds no lock across the backoff sleeps.
/// It only completes an attempt for `running`: an attempt of another version, or one without a readable id, is left to boot reconciliation and ends the retries.
pub fn retry_completion<K: OtaKv>(store: &mut OtaStore<K>, running: Version) -> CompletionTry {
    match store.read_attempt() {
        Ok(None) => CompletionTry::Gone,
        Ok(Some(stored)) if stored.record.version == Some(running) => {
            match store.complete_attempt(running) {
                Ok(()) => CompletionTry::Completed,
                Err(_) => CompletionTry::StillFailing,
            }
        }
        Ok(Some(_)) => CompletionTry::Gone,
        Err(_) => CompletionTry::StillFailing,
    }
}

/// Why [`repair_all`] did not finish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairError {
    /// The cap was reached and unreadable records remain.
    Capped,
    /// A transient storage error.
    Store(StoreError),
}

impl RepairError {
    /// Both causes are reported as `repair_failed`.
    pub const fn reason(&self) -> FailReason {
        FailReason::RepairFailed
    }
}

/// Repairs record groups until none is left, at most `max_iterations` of them.
///
/// Returns how many groups were repaired (zero means nothing was unreadable).
/// Reaching the cap with records still unreadable is [`RepairError::Capped`], never a success with the cap as the count.
///
/// # Errors
///
/// [`RepairError::Store`] on a transient failure (records repaired so far stay repaired), [`RepairError::Capped`] at the cap.
pub fn repair_all<K: OtaKv>(
    store: &mut OtaStore<K>,
    max_iterations: u32,
) -> Result<u32, RepairError> {
    let mut repaired = 0;
    while repaired < max_iterations {
        match store.repair_corrupt() {
            Ok(Some(_)) => repaired += 1,
            Ok(None) => return Ok(repaired),
            Err(e) => return Err(RepairError::Store(e)),
        }
    }
    match store.corrupt_present() {
        Ok(false) => Ok(repaired),
        Ok(true) => Err(RepairError::Capped),
        Err(e) => Err(RepairError::Store(e)),
    }
}

/// The result of [`failed_download`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailedSettle {
    /// The store looked at the stalled attempt; see [`FailedDownload`].
    Settled(FailedDownload),
    /// The hardware facts could not be read; the attempt is kept and the next boot decides.
    HardwareUnreadable(HardwareReadError),
    /// A record could not be read or written.
    Store(StoreError),
}

/// After a failed `fetch_and_apply`: lets the decision core reconcile the stalled attempt.
///
/// `hw` is read by the caller OUTSIDE the store lock.
pub fn failed_download<K: OtaKv>(
    store: &mut OtaStore<K>,
    hw: Result<HardwareFacts, HardwareReadError>,
    running_version: Option<Version>,
) -> FailedSettle {
    match hw {
        Err(e) => FailedSettle::HardwareUnreadable(e),
        Ok(hw) => match store.settle_failed_download(hw, running_version) {
            Ok(outcome) => FailedSettle::Settled(outcome),
            Err(e) => FailedSettle::Store(e),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ota::persist::fault_kv::FaultKv;
    use crate::ota::persist::{HardwareFacts, UpdateSlot};
    use crate::ota::SlotState;

    const A: SlotId = SlotId(0);
    const B: SlotId = SlotId(1);
    const V1: Version = Version::new(1, 0, 0);

    fn entropy() -> u32 {
        0xC0FF_EE00
    }

    fn store(kv: &FaultKv) -> OtaStore<FaultKv> {
        OtaStore::open(kv.clone(), entropy)
    }

    fn facts(running: SlotId, state: SlotState) -> HardwareFacts {
        HardwareFacts {
            running_slot: running,
            running_state: state,
            update_slot: UpdateSlot::Present(B, SlotState::Unknown),
        }
    }

    fn arm_commits() -> usize {
        let kv = FaultKv::new();
        let mut s = store(&kv);
        kv.reset_counters();
        assert!(rollback_arm(&mut s, A, V1).is_ok());
        kv.commits()
    }

    #[test]
    fn arm_writes_the_request_and_the_refusal() {
        let kv = FaultKv::new();
        let mut s = store(&kv);
        let armed = rollback_arm(&mut s, A, V1).unwrap();
        assert!(armed.wrote_reason() && armed.wrote_refusal());
        assert_eq!(kv.str_of("rq_from").as_deref(), Some("ota_0"));
        assert_eq!(kv.str_of("rq_why").as_deref(), Some("operator"));
        assert_eq!(kv.str_of("rej_ver").as_deref(), Some("1.0.0"));
    }

    #[test]
    fn an_undelivered_report_rejects_the_rollback_and_writes_nothing() {
        let kv = FaultKv::new();
        kv.put_u8("rb", 1)
            .put_u32("rb_id", 4)
            .put_str("rb_why", "x");
        kv.put_u32("att_ctr", 4).put_u32("att_epoch", 1);
        let mut s = store(&kv);
        kv.reset_counters();
        assert_eq!(
            rollback_arm(&mut s, A, V1).unwrap_err(),
            FailReason::ReportPending
        );
        assert_eq!(kv.commits(), 0);
    }

    #[test]
    fn an_unresolved_attempt_or_an_armed_request_rejects_the_rollback() {
        let kv = FaultKv::new();
        kv.put_u32("att_id", 5)
            .put_str("att_ver", "2.0.0")
            .put_str("att_slot", "ota_1");
        let mut s = store(&kv);
        kv.reset_counters();
        assert_eq!(
            rollback_arm(&mut s, A, V1).unwrap_err(),
            FailReason::AttemptUnresolved
        );
        let kv = FaultKv::new();
        kv.put_u32("rq_id", 9)
            .put_str("rq_why", "operator")
            .put_str("rq_from", "ota_0");
        let mut s = store(&kv);
        kv.reset_counters();
        assert_eq!(
            rollback_arm(&mut s, A, V1).unwrap_err(),
            FailReason::AttemptUnresolved
        );
        assert_eq!(kv.commits(), 0);
    }

    #[test]
    fn a_slot_without_a_stored_label_has_nothing_to_roll_back_to() {
        let kv = FaultKv::new();
        let mut s = store(&kv);
        kv.reset_counters();
        assert_eq!(
            rollback_arm(&mut s, SlotId(2), V1).unwrap_err(),
            FailReason::RollbackUnavailable
        );
        assert!(!kv.has("rq_from") && !kv.has("rej_ver"));
    }

    #[test]
    fn a_failing_refusal_write_withdraws_the_request() {
        let total = arm_commits();
        let kv = FaultKv::new();
        let mut s = store(&kv);
        kv.reset_counters();
        kv.fail_at(total);
        assert_eq!(
            rollback_arm(&mut s, A, V1).unwrap_err(),
            FailReason::AttemptNotPersisted
        );
        assert!(!kv.has("rq_from"), "the request is withdrawn");
        assert!(!kv.has("rej_ver"));
    }

    #[test]
    fn any_failing_write_leaves_no_armed_request() {
        let total = arm_commits();
        for n in 1..=total {
            let kv = FaultKv::new();
            let mut s = store(&kv);
            kv.reset_counters();
            kv.fail_at(n);
            let result = rollback_arm(&mut s, A, V1);
            assert!(result.is_err(), "commit {n}");
            assert!(!kv.has("rq_from"), "commit {n}");
        }
    }

    #[test]
    fn undo_restores_a_replaced_refusal_and_withdraws_the_request() {
        let kv = FaultKv::new();
        kv.put_str("rej_ver", "0.9.0");
        let mut s = store(&kv);
        let armed = rollback_arm(&mut s, A, V1).unwrap();
        assert_eq!(kv.str_of("rej_ver").as_deref(), Some("1.0.0"));
        let undone = rollback_undo(&mut s, armed);
        assert!(undone.is_clean());
        assert_eq!(kv.str_of("rej_ver").as_deref(), Some("0.9.0"));
        assert!(!kv.has("rq_from"));
        assert_eq!(s.admission(), Admission::Open);
    }

    #[test]
    fn undo_attempts_both_steps_even_when_the_first_fails() {
        for failing_commit in [1usize, 2] {
            let kv = FaultKv::new();
            let mut s = store(&kv);
            let armed = rollback_arm(&mut s, A, V1).unwrap();
            kv.reset_counters();
            kv.fail_at(failing_commit);
            let undone = rollback_undo(&mut s, armed);
            assert!(!undone.is_clean(), "commit {failing_commit}");
            if failing_commit == 1 {
                assert!(matches!(undone.refusal, Some(Err(_))));
                assert_eq!(undone.reason, Some(Ok(())));
                assert!(!kv.has("rq_from"), "the request is withdrawn anyway");
                assert!(kv.has("rej_ver"));
            } else {
                assert_eq!(undone.refusal, Some(Ok(())));
                assert!(matches!(undone.reason, Some(Err(_))));
                assert!(!kv.has("rej_ver"), "the refusal is restored anyway");
            }
        }
    }

    #[test]
    fn undo_of_an_already_refused_version_touches_only_the_request() {
        let kv = FaultKv::new();
        kv.put_str("rej_ver", "1.0.0");
        let mut s = store(&kv);
        let armed = rollback_arm(&mut s, A, V1).unwrap();
        assert!(armed.wrote_reason() && !armed.wrote_refusal());
        let undone = rollback_undo(&mut s, armed);
        assert_eq!(undone.refusal, None);
        assert_eq!(undone.reason, Some(Ok(())));
        assert_eq!(kv.str_of("rej_ver").as_deref(), Some("1.0.0"));
    }

    #[test]
    fn ack_marks_only_the_report_and_leaves_a_request_of_this_boot_armed() {
        let kv = FaultKv::new();
        kv.put_u32("att_ctr", 9).put_u32("att_epoch", 1);
        kv.put_u8("rb", 1)
            .put_u32("rb_id", 4)
            .put_str("rb_why", "operator");
        kv.put_u32("rq_id", 9)
            .put_str("rq_why", "operator")
            .put_str("rq_from", "ota_0");
        let mut s = store(&kv);
        assert_eq!(ack_delivered(&mut s, 4), Ok(Delivery::Marked));
        assert_eq!(kv.u8_of("rb"), Some(0));
        assert!(kv.has("rq_from"), "no settle: the armed request survives");
        assert_eq!(
            ack_delivered(&mut s, 5),
            Ok(Delivery::NotPending),
            "already delivered"
        );
    }

    #[test]
    fn an_ack_for_another_id_reports_the_pending_one() {
        let kv = FaultKv::new();
        kv.put_u8("rb", 1)
            .put_u32("rb_id", 4)
            .put_str("rb_why", "operator");
        let mut s = store(&kv);
        assert_eq!(
            ack_delivered(&mut s, 5),
            Ok(Delivery::OtherReport { pending_id: 4 })
        );
        assert_eq!(kv.u8_of("rb"), Some(1));
    }

    #[test]
    fn admission_reopens_on_the_delivery_mark_alone() {
        let kv = FaultKv::new();
        kv.put_u32("att_ctr", 4).put_u32("att_epoch", 1);
        kv.put_u8("rb", 1)
            .put_u32("rb_id", 4)
            .put_str("rb_why", "operator");
        let mut s = store(&kv);
        assert_eq!(s.admission(), Admission::Blocked(BlockedBy::ReportPending));
        assert_eq!(ack_delivered(&mut s, 4), Ok(Delivery::Marked));
        assert_eq!(s.admission(), Admission::Open);
    }

    #[test]
    fn a_failing_delivery_mark_keeps_the_report() {
        let kv = FaultKv::new();
        kv.put_u8("rb", 1)
            .put_u32("rb_id", 4)
            .put_str("rb_why", "operator");
        let mut s = store(&kv);
        kv.reset_counters();
        kv.fail_at(1);
        assert!(ack_delivered(&mut s, 4).is_err());
        assert_eq!(kv.u8_of("rb"), Some(1));
    }

    #[test]
    fn repair_all_loops_until_nothing_is_left() {
        let kv = FaultKv::new();
        kv.put_u32("att_ctr", 5).put_u32("att_epoch", 1);
        kv.put_u32("att_id", 5)
            .put_str("att_ver", "2.0.0")
            .put_str("att_slot", "ota_1");
        kv.put_u32("rq_id", 9).put_str("rq_from", "ota_0");
        kv.put_str("rej_ver", "1.0.0");
        kv.corrupt_with("att_ver", crate::ota::persist::KvError::CODE_INVALID_VALUE);
        kv.corrupt_with("rq_from", crate::ota::persist::KvError::CODE_INVALID_VALUE);
        kv.corrupt_with("rej_ver", crate::ota::persist::KvError::CODE_INVALID_VALUE);
        let mut s = store(&kv);
        assert_eq!(repair_all(&mut s, 32), Ok(3));
        assert_eq!(s.corrupt_present(), Ok(false));
        assert_eq!(repair_all(&mut s, 32), Ok(0));
    }

    #[test]
    fn repair_all_at_the_cap_is_failed_not_repaired() {
        let make = || {
            let kv = FaultKv::new();
            kv.put_u32("att_ctr", 5).put_u32("att_epoch", 1);
            kv.put_str("rej_ver", "1.0.0");
            kv.put_u8("rb", 1);
            kv.corrupt_with("rej_ver", crate::ota::persist::KvError::CODE_INVALID_VALUE);
            kv.corrupt_with("rb", crate::ota::persist::KvError::CODE_INVALID_VALUE);
            kv
        };
        let kv = make();
        let mut s = store(&kv);
        assert_eq!(repair_all(&mut s, 1), Err(RepairError::Capped));
        assert_eq!(RepairError::Capped.reason(), FailReason::RepairFailed);
        let kv = make();
        let mut s = store(&kv);
        assert_eq!(repair_all(&mut s, 2), Ok(2), "exactly the cap is fine");
    }

    #[test]
    fn repair_all_reports_a_transient_error() {
        let kv = FaultKv::new();
        let mut s = store(&kv);
        kv.reset_counters();
        kv.fail_read_at(1);
        assert!(matches!(repair_all(&mut s, 32), Err(RepairError::Store(_))));
        assert_eq!(
            RepairError::Store(StoreError::InvalidSlot).reason(),
            FailReason::RepairFailed
        );
    }

    #[test]
    fn failed_download_clears_an_interrupted_attempt() {
        let kv = FaultKv::new();
        kv.put_u32("att_ctr", 5).put_u32("att_epoch", 1);
        kv.put_u32("att_id", 5)
            .put_u8("att_inv", 0)
            .put_str("att_ver", "2.0.0")
            .put_str("att_slot", "ota_1");
        let mut s = store(&kv);
        let out = failed_download(&mut s, Ok(facts(A, SlotState::Valid)), Some(V1));
        assert_eq!(out, FailedSettle::Settled(FailedDownload::Cleared));
        assert!(!kv.has("att_ver"));
        assert_eq!(s.admission(), Admission::Open);
    }

    #[test]
    fn failed_download_without_hardware_facts_keeps_the_attempt() {
        let kv = FaultKv::new();
        kv.put_u32("att_id", 5)
            .put_str("att_ver", "2.0.0")
            .put_str("att_slot", "ota_1");
        let mut s = store(&kv);
        kv.reset_counters();
        assert_eq!(
            failed_download(&mut s, Err(HardwareReadError::Busy), Some(V1)),
            FailedSettle::HardwareUnreadable(HardwareReadError::Busy)
        );
        assert_eq!(kv.commits(), 0);
        assert!(kv.has("att_ver"));
    }

    const V2: Version = Version::new(2, 0, 0);

    fn with_attempt(version: Version) -> (FaultKv, OtaStore<FaultKv>) {
        let kv = FaultKv::new();
        let mut s = store(&kv);
        s.begin_attempt(version, B, false).unwrap();
        (kv, s)
    }

    #[test]
    fn a_retry_clears_the_attempt_and_reopens_admission() {
        let (_kv, mut s) = with_attempt(V1);
        assert_eq!(
            s.admission(),
            Admission::Blocked(BlockedBy::AttemptUnresolved)
        );
        assert_eq!(retry_completion(&mut s, V1), CompletionTry::Completed);
        assert_eq!(s.admission(), Admission::Open);
    }

    #[test]
    fn a_retry_with_no_attempt_is_gone() {
        let kv = FaultKv::new();
        let mut s = store(&kv);
        assert_eq!(retry_completion(&mut s, V1), CompletionTry::Gone);
    }

    #[test]
    fn an_attempt_of_another_version_is_left_alone() {
        let (kv, mut s) = with_attempt(V2);
        assert_eq!(retry_completion(&mut s, V1), CompletionTry::Gone);
        assert!(kv.has("att_ver"), "not cleared");
    }

    #[test]
    fn a_failing_store_keeps_the_attempt_and_the_block_until_it_recovers() {
        let (kv, mut s) = with_attempt(V1);
        for _ in 0..3 {
            kv.reset_counters();
            kv.fail_at(1);
            assert_eq!(retry_completion(&mut s, V1), CompletionTry::StillFailing);
            assert_eq!(
                s.admission(),
                Admission::Blocked(BlockedBy::AttemptUnresolved)
            );
        }
        kv.revive();
        assert_eq!(retry_completion(&mut s, V1), CompletionTry::Completed);
        assert_eq!(s.admission(), Admission::Open);
    }
}
