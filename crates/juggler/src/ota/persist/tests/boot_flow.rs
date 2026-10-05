//! `reconcile_boot` outcomes and the early-boot slot note.

use alloc::format;

use super::support::*;
use crate::ota::persist::fault_kv::FaultKv;
use crate::ota::persist::{
    note_boot_slot, reconcile_boot, BootDisposition, BootFault, BootWarnings, CorruptRecord,
    HardwareReadError, KvError, NoteBoot, NoteBootError, SettleOutcome, StoreError, UpdateSlot,
    FACTORY_SLOT,
};
use crate::ota::{ReconcileAction, SlotState, Version};

fn booted_new_image(kv: &FaultKv, act: bool) {
    put_counter(kv, 5);
    put_attempt(kv, 5, "2.0.0", "ota_1", true, act);
}

#[test]
fn clean_boot_settles_without_writes() {
    let kv = FaultKv::new();
    let mut s = store(&kv);
    let out = reconcile_boot(
        &mut s,
        Ok(hw(SLOT_A, SlotState::Unknown, None)),
        Some(V_OLD),
    );
    assert_eq!(
        out.disposition,
        BootDisposition::Settled(SettleOutcome::NoRequest)
    );
    assert_eq!(out.action, Some(ReconcileAction::NoAttempt));
    assert!(out.admission_open_at_boot && !out.refuse_mark_valid && !out.roll_back_now);
    assert!(kv.ops().is_empty());

    let out = reconcile_boot(&mut s, Ok(hw(SLOT_A, SlotState::Valid, None)), Some(V_OLD));
    assert!(out.admission_open_at_boot);
    assert!(kv.ops().is_empty());
}

#[test]
fn unverified_running_image_keeps_admission_closed() {
    let kv = FaultKv::new();
    let mut s = store(&kv);
    let out = reconcile_boot(
        &mut s,
        Ok(hw(SLOT_A, SlotState::PendingVerify, None)),
        Some(V_OLD),
    );
    assert_eq!(
        out.disposition,
        BootDisposition::Settled(SettleOutcome::NoRequest)
    );
    assert!(!out.admission_open_at_boot);
}

#[test]
fn attempted_image_awaits_health_check_and_records_activation() {
    let kv = FaultKv::new();
    booted_new_image(&kv, false);
    let mut s = store(&kv);
    let out = reconcile_boot(
        &mut s,
        Ok(hw(SLOT_B, SlotState::PendingVerify, None)),
        Some(V_NEW),
    );
    assert_eq!(out.disposition, BootDisposition::AwaitHealthCheck);
    assert_eq!(kv.ops(), strs(&["set:att_act"]));
    assert!(!out.admission_open_at_boot && !out.refuse_mark_valid && !out.roll_back_now);
}

#[test]
fn wrong_image_running_unverified_rolls_back_now_and_notes_the_reason() {
    let kv = FaultKv::new();
    booted_new_image(&kv, false);
    let mut s = store(&kv);
    let out = reconcile_boot(
        &mut s,
        Ok(hw(SLOT_B, SlotState::PendingVerify, None)),
        Some(V_OLD),
    );
    assert_eq!(out.disposition, BootDisposition::RollBackNow);
    assert!(out.roll_back_now && out.refuse_mark_valid && !out.admission_open_at_boot);
    assert_eq!(kv.str_of("att_why").as_deref(), Some("version_mismatch"));
    assert!(kv.has("att_ver"));
    assert_eq!(kv.u8_of("att_act"), Some(1));
}

#[test]
fn fell_back_image_is_reported_refused_and_cleared_once() {
    let kv = FaultKv::new();
    booted_new_image(&kv, true);
    let mut s = store(&kv);
    let facts = hw(SLOT_A, SlotState::Valid, Some((SLOT_B, SlotState::Invalid)));
    let out = reconcile_boot(&mut s, Ok(facts), Some(V_OLD));
    assert_eq!(out.disposition, BootDisposition::ReportedRollback);
    assert!(out.report_pending && !out.admission_open_at_boot);
    assert_eq!(kv.u32_of("rb_id"), Some(5));
    assert_eq!(kv.str_of("rej_ver").as_deref(), Some("2.0.0"));
    assert!(!kv.has("att_ver"));
    kv.reset_counters();
    let again = reconcile_boot(&mut s, Ok(facts), Some(V_OLD));
    assert_eq!(
        again.disposition,
        BootDisposition::Settled(SettleOutcome::NoRequest)
    );
    assert!(again.report_pending);
    assert!(kv.ops().is_empty());
}

#[test]
fn factory_running_slot_is_a_fall_back_not_a_failure() {
    let kv = FaultKv::new();
    booted_new_image(&kv, true);
    let mut s = store(&kv);
    let facts = hw(FACTORY_SLOT, SlotState::Unknown, None);
    let out = reconcile_boot(&mut s, Ok(facts), Some(V_OLD));
    assert_eq!(out.disposition, BootDisposition::ReportedRollback);
    assert_eq!(kv.u32_of("rb_id"), Some(5));
}

#[test]
fn healthy_attempted_image_completes_when_valid() {
    let kv = FaultKv::new();
    booted_new_image(&kv, true);
    kv.put_str("rej_ver", "1.5.0");
    let mut s = store(&kv);
    let out = reconcile_boot(&mut s, Ok(hw(SLOT_B, SlotState::Valid, None)), Some(V_NEW));
    assert_eq!(
        out.disposition,
        BootDisposition::Settled(SettleOutcome::NoRequest)
    );
    assert_eq!(out.action, Some(ReconcileAction::CompleteAttempt));
    assert!(out.admission_open_at_boot);
    assert!(!kv.has("rej_ver") && !kv.has("att_ver"));
}

#[test]
fn interrupted_download_clears_the_attempt_and_keeps_the_refusal() {
    let kv = FaultKv::new();
    put_counter(&kv, 5);
    put_attempt(&kv, 5, "2.0.0", "ota_1", false, false);
    kv.put_str("rej_ver", "1.5.0");
    let mut s = store(&kv);
    let facts = hw(SLOT_A, SlotState::Valid, Some((SLOT_B, SlotState::Unknown)));
    let out = reconcile_boot(&mut s, Ok(facts), Some(V_OLD));
    assert_eq!(out.action, Some(ReconcileAction::ClearAttempt));
    assert!(out.admission_open_at_boot);
    assert_eq!(kv.str_of("rej_ver").as_deref(), Some("1.5.0"));
    assert!(!kv.has("att_ver"));
}

#[test]
fn unreadable_update_slot_defers_without_writing() {
    let kv = FaultKv::new();
    put_counter(&kv, 5);
    put_attempt(&kv, 5, "2.0.0", "ota_1", false, false);
    let mut s = store(&kv);
    let out = reconcile_boot(&mut s, Ok(hw(SLOT_A, SlotState::Valid, None)), Some(V_OLD));
    assert_eq!(out.disposition, BootDisposition::Deferred);
    assert!(!out.admission_open_at_boot && !out.refuse_mark_valid);
    assert!(kv.ops().is_empty());
}

#[test]
fn write_failure_while_reporting_keeps_the_attempt() {
    let kv = FaultKv::new();
    booted_new_image(&kv, true);
    kv.fail_at(1);
    let mut s = store(&kv);
    let facts = hw(SLOT_A, SlotState::Valid, Some((SLOT_B, SlotState::Invalid)));
    let out = reconcile_boot(&mut s, Ok(facts), Some(V_OLD));
    assert!(matches!(
        out.disposition,
        BootDisposition::Incomplete(StoreError::Kv(_))
    ));
    assert!(!out.admission_open_at_boot && !out.refuse_mark_valid);
    assert!(kv.has("att_ver"));
}

#[test]
fn hardware_failure_fails_closed_without_touching_records() {
    for e in [HardwareReadError::Busy, HardwareReadError::Read(-3)] {
        let kv = FaultKv::new();
        booted_new_image(&kv, true);
        let mut s = store(&kv);
        let out = reconcile_boot(&mut s, Err(e), Some(V_OLD));
        assert_eq!(
            out.disposition,
            BootDisposition::FailedClosed(BootFault::Hardware(e))
        );
        assert!(!out.admission_open_at_boot && out.refuse_mark_valid && out.action.is_none());
        assert_eq!(kv.commits(), 0);
    }
}

#[test]
fn note_boot_slot_cases() {
    let kv = FaultKv::new();
    let mut s = store(&kv);
    let mut called = false;
    assert_eq!(
        note_boot_slot(&mut s, || {
            called = true;
            Ok(SLOT_B)
        }),
        Ok(NoteBoot::NoAttempt)
    );
    assert!(!called);

    booted_new_image(&kv, false);
    assert_eq!(
        note_boot_slot(&mut s, || Ok(SLOT_A)),
        Ok(NoteBoot::OtherSlot)
    );
    assert!(kv.ops().is_empty());
    assert_eq!(
        note_boot_slot(&mut s, || Err(HardwareReadError::Busy)),
        Err(NoteBootError::Hardware(HardwareReadError::Busy))
    );
    assert!(kv.ops().is_empty());
    assert_eq!(
        note_boot_slot(&mut s, || Ok(SLOT_B)),
        Ok(NoteBoot::Activated)
    );
    assert_eq!(kv.ops(), strs(&["set:att_act"]));
    let mut called = false;
    assert_eq!(
        note_boot_slot(&mut s, || {
            called = true;
            Ok(SLOT_B)
        }),
        Ok(NoteBoot::AlreadyActivated)
    );
    assert!(!called);
}

#[test]
fn note_boot_slot_reports_an_attempt_without_id_as_corrupt_without_a_write() {
    let kv = FaultKv::new();
    kv.put_str("att_ver", "2.0.0").put_str("att_slot", "ota_1");
    let mut s = store(&kv);
    assert_eq!(
        note_boot_slot(&mut s, || Ok(SLOT_B)),
        Err(NoteBootError::Store(StoreError::Corrupt(
            CorruptRecord::AttemptWithoutId
        )))
    );
    assert!(kv.ops().is_empty());
}

#[test]
fn a_clean_boot_has_no_warnings() {
    let kv = FaultKv::new();
    booted_new_image(&kv, false);
    let out = reconcile_boot(
        &mut store(&kv),
        Ok(hw(SLOT_B, SlotState::PendingVerify, None)),
        Some(V_NEW),
    );
    assert_eq!(out.warnings, BootWarnings::default());
}

#[test]
fn a_lost_activation_note_is_surfaced_and_the_action_is_kept() {
    let kv = FaultKv::new();
    booted_new_image(&kv, false);
    kv.fail_at(1);
    let out = reconcile_boot(
        &mut store(&kv),
        Ok(hw(SLOT_B, SlotState::PendingVerify, None)),
        Some(V_NEW),
    );
    assert_eq!(out.disposition, BootDisposition::AwaitHealthCheck);
    assert!(matches!(
        out.warnings.activation_not_recorded,
        Some(StoreError::Kv(_))
    ));
    assert_eq!(out.warnings.reason_not_recorded, None);
    assert!(!kv.has("att_act"));
}

#[test]
fn a_refused_image_still_rolls_back_when_its_notes_are_lost() {
    // Commit 1 is `att_act`, commits 2 and 3 are the erase and the set of `att_why`.
    for (failing_commit, activation_lost, reason_lost) in
        [(1, true, false), (2, false, true), (3, false, true)]
    {
        let kv = FaultKv::new();
        booted_new_image(&kv, false);
        kv.fail_at(failing_commit);
        let out = reconcile_boot(
            &mut store(&kv),
            Ok(hw(SLOT_B, SlotState::PendingVerify, None)),
            Some(Version::new(3, 0, 0)),
        );
        let ctx = format!("commit {failing_commit}");
        assert_eq!(out.disposition, BootDisposition::RollBackNow, "{ctx}");
        assert!(out.roll_back_now && out.refuse_mark_valid, "{ctx}");
        assert_eq!(
            out.warnings.activation_not_recorded.is_some(),
            activation_lost,
            "{ctx}"
        );
        assert_eq!(
            out.warnings.reason_not_recorded.is_some(),
            reason_lost,
            "{ctx}"
        );
    }
    // Nothing failing: no warnings, the reason is stored.
    let kv = FaultKv::new();
    booted_new_image(&kv, false);
    let out = reconcile_boot(
        &mut store(&kv),
        Ok(hw(SLOT_B, SlotState::PendingVerify, None)),
        Some(Version::new(3, 0, 0)),
    );
    assert_eq!(out.warnings, BootWarnings::default());
    assert_eq!(kv.str_of("att_why").as_deref(), Some("version_mismatch"));
}

#[test]
fn report_pending_fails_closed_when_the_flag_is_unreadable() {
    for code in [KvError::CODE_TYPE_MISMATCH, -2] {
        let kv = FaultKv::new();
        kv.put_u8("rb", 0);
        kv.corrupt_with("rb", code);
        let out = reconcile_boot(
            &mut store(&kv),
            Ok(hw(SLOT_A, SlotState::Valid, None)),
            Some(V_OLD),
        );
        assert!(matches!(out.disposition, BootDisposition::FailedClosed(_)));
        assert!(out.report_pending, "{code}");
        assert!(!out.admission_open_at_boot);

        // The hardware-failure path reads the flag too.
        let out = reconcile_boot(&mut store(&kv), Err(HardwareReadError::Busy), None);
        assert!(out.report_pending, "{code}");
    }
    // A readable flag still says what it says.
    let kv = FaultKv::new();
    kv.put_u8("rb", 0);
    let out = reconcile_boot(
        &mut store(&kv),
        Ok(hw(SLOT_A, SlotState::Valid, None)),
        Some(V_OLD),
    );
    assert!(!out.report_pending);
}

#[test]
fn a_dropped_request_is_visible_in_the_disposition() {
    let kv = FaultKv::new();
    put_request(&kv, 6, "operator", "ota_1");
    let mut s = store(&kv);
    let out = reconcile_boot(&mut s, Ok(hw(SLOT_B, SlotState::Valid, None)), Some(V_NEW));
    assert_eq!(
        out.disposition,
        BootDisposition::Settled(SettleOutcome::DroppedNotRolledBack)
    );
    assert!(!kv.has("rq_from") && !kv.has("rb"));
}

#[test]
fn a_request_merged_into_a_pending_report_is_visible_in_the_disposition() {
    let kv = FaultKv::new();
    put_counter(&kv, 9);
    put_request(&kv, 9, "operator", "ota_0");
    kv.put_u8("rb", 1)
        .put_u32("rb_id", 4)
        .put_str("rb_why", "bootloader");
    let mut s = store(&kv);
    let out = reconcile_boot(&mut s, Ok(hw(SLOT_B, SlotState::Valid, None)), Some(V_NEW));
    assert_eq!(
        out.disposition,
        BootDisposition::Settled(SettleOutcome::MergedIntoPending)
    );
    assert_eq!(kv.str_of("rb_why").as_deref(), Some("bootloader"));
    assert!(out.report_pending);
}

#[test]
fn absent_and_unreadable_update_slots_decide_alike_but_stay_distinguishable() {
    let run = |update: UpdateSlot| {
        let kv = FaultKv::new();
        put_counter(&kv, 5);
        put_attempt(&kv, 5, "2.0.0", "ota_1", false, false);
        let mut facts = hw(SLOT_A, SlotState::Valid, None);
        facts.update_slot = update;
        let out = reconcile_boot(&mut store(&kv), Ok(facts), Some(V_OLD));
        (out, facts.update_slot)
    };
    let (absent, a) = run(UpdateSlot::Absent);
    let (unreadable, u) = run(UpdateSlot::Unreadable(5));
    assert_eq!(absent, unreadable);
    assert_eq!(absent.action, Some(ReconcileAction::Defer));
    assert_ne!(a, u);
    assert_eq!(a.facts(), None);
    assert_eq!(u.facts(), None);
    assert_eq!(
        UpdateSlot::Present(SLOT_B, SlotState::Invalid).facts(),
        Some((SLOT_B, SlotState::Invalid))
    );
}
