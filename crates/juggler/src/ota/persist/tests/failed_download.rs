//! `settle_failed_download`: clears the attempt of a failed download only when the decision core says so.

use alloc::format;

use super::support::*;
use crate::ota::persist::fault_kv::{FaultKv, INJECTED};
use crate::ota::persist::{CorruptRecord, FailedDownload, KvError, StoreError, UpdateSlot};
use crate::ota::{Admission, ReconcileAction, SlotState};

/// A download that was interrupted before the boot slot switched: the previous image runs.
fn downloading(kv: &FaultKv) {
    put_counter(kv, 5);
    put_attempt(kv, 5, "2.0.0", "ota_1", false, false);
}

fn old_image(
    update: Option<(crate::ota::SlotId, SlotState)>,
) -> crate::ota::persist::HardwareFacts {
    hw(SLOT_A, SlotState::Valid, update)
}

#[test]
fn an_interrupted_download_is_cleared() {
    let kv = FaultKv::new();
    downloading(&kv);
    let mut s = store(&kv);
    let facts = old_image(Some((SLOT_B, SlotState::Unknown)));
    assert_eq!(
        s.settle_failed_download(facts, Some(V_OLD)),
        Ok(FailedDownload::Cleared)
    );
    assert!(!kv.has("att_ver") && !kv.has("att_slot") && !kv.has("att_id"));
    assert_eq!(kv.u32_of("att_ctr"), Some(5));
    assert_eq!(kv.u32_of("att_epoch"), Some(EPOCH));
    assert_eq!(s.admission(), Admission::Open);
    assert_eq!(
        s.settle_failed_download(facts, Some(V_OLD)),
        Ok(FailedDownload::NoAttempt)
    );
}

#[test]
fn a_boot_time_decision_keeps_the_attempt_and_writes_nothing() {
    let kv = FaultKv::new();
    downloading(&kv);
    let mut s = store(&kv);
    kv.reset_counters();
    // Unreadable update slot: the core defers.
    let mut unreadable = old_image(None);
    unreadable.update_slot = UpdateSlot::Unreadable(5);
    assert_eq!(
        s.settle_failed_download(unreadable, Some(V_OLD)),
        Ok(FailedDownload::Kept(ReconcileAction::Defer))
    );
    // The target slot went Invalid during the attempt: a rollback report is due.
    let facts = old_image(Some((SLOT_B, SlotState::Invalid)));
    assert_eq!(
        s.settle_failed_download(facts, Some(V_OLD)),
        Ok(FailedDownload::Kept(ReconcileAction::ReportRollback {
            attempt_id: 5,
            left: Some(V_NEW),
            report_already_persisted: false,
        }))
    );
    assert_eq!(kv.commits(), 0);
    assert!(kv.has("att_ver"));
}

#[test]
fn the_report_fact_comes_from_the_store() {
    let kv = FaultKv::new();
    downloading(&kv);
    kv.put_u8("rb", 1)
        .put_u32("rb_id", 5)
        .put_str("rb_why", "x");
    let facts = old_image(Some((SLOT_B, SlotState::Invalid)));
    assert_eq!(
        store(&kv).settle_failed_download(facts, Some(V_OLD)),
        Ok(FailedDownload::Kept(ReconcileAction::ReportRollback {
            attempt_id: 5,
            left: Some(V_NEW),
            report_already_persisted: true,
        }))
    );
    // A delivered report of the same id does not count.
    kv.put_u8("rb", 0);
    assert_eq!(
        store(&kv).settle_failed_download(facts, Some(V_OLD)),
        Ok(FailedDownload::Kept(ReconcileAction::ReportRollback {
            attempt_id: 5,
            left: Some(V_NEW),
            report_already_persisted: false,
        }))
    );
}

#[test]
fn an_attempt_without_an_id_fails_closed_without_a_write() {
    let kv = FaultKv::new();
    put_counter(&kv, 5);
    kv.put_str("att_ver", "2.0.0").put_str("att_slot", "ota_1");
    kv.reset_counters();
    let facts = old_image(Some((SLOT_B, SlotState::Unknown)));
    assert_eq!(
        store(&kv).settle_failed_download(facts, Some(V_OLD)),
        Err(StoreError::Corrupt(CorruptRecord::AttemptWithoutId))
    );
    assert_eq!(kv.commits(), 0);
}

#[test]
fn no_attempt_means_nothing_to_do() {
    let kv = FaultKv::new();
    let facts = old_image(None);
    assert_eq!(
        store(&kv).settle_failed_download(facts, None),
        Ok(FailedDownload::NoAttempt)
    );
    assert_eq!(kv.commits(), 0);
}

#[test]
fn a_read_error_keeps_the_attempt_and_writes_nothing() {
    let facts = old_image(Some((SLOT_B, SlotState::Unknown)));
    let setup = |kv: &FaultKv| downloading(kv);
    let reads = {
        let kv = FaultKv::new();
        setup(&kv);
        kv.reset_counters();
        let _ = store(&kv).settle_failed_download(facts, Some(V_OLD));
        kv.reads()
    };
    assert!(reads > 0);
    for n in 1..=reads {
        let kv = FaultKv::new();
        setup(&kv);
        kv.reset_counters();
        kv.fail_read_at(n);
        let got = store(&kv).settle_failed_download(facts, Some(V_OLD));
        let tag = format!("read {n}");
        // The last read is the one `clear_attempt` never makes; every earlier read must fail closed.
        assert_eq!(got, Err(StoreError::Kv(KvError::new(INJECTED))), "{tag}");
        assert_eq!(kv.commits(), 0, "{tag}");
        kv.revive();
        assert!(kv.has("att_ver") && kv.has("att_slot"), "{tag}");
    }
}

#[test]
fn a_corrupt_attempt_record_is_an_error_and_is_kept() {
    let kv = FaultKv::new();
    downloading(&kv);
    kv.put_str("att_slot", "ota_9");
    let facts = old_image(Some((SLOT_B, SlotState::Unknown)));
    assert!(matches!(
        store(&kv).settle_failed_download(facts, Some(V_OLD)),
        Err(StoreError::Corrupt(_))
    ));
    assert!(kv.has("att_ver"));
}

/// A crash or failure at every commit of the clear: the attempt is whole or hidden, and a second call finishes the job.
#[test]
fn a_clear_cut_short_at_any_commit_is_finished_by_the_next_call() {
    let facts = old_image(Some((SLOT_B, SlotState::Unknown)));
    let run = |s: &mut crate::ota::persist::OtaStore<FaultKv>| {
        let _ = s.settle_failed_download(facts, Some(V_OLD));
    };
    each_fault(&downloading, &run, |kv, fault, n| {
        let tag = format!("{fault:?} at {n}");
        let mut s = store(kv);
        let hidden = !kv.has("att_ver");
        let intact = kv.has("att_ver") && kv.has("att_slot") && kv.has("att_id");
        assert!(hidden || intact, "{tag}: half an attempt is visible");
        assert_eq!(kv.u32_of("att_ctr"), Some(5), "{tag}");
        assert_eq!(kv.u32_of("att_epoch"), Some(EPOCH), "{tag}");
        let again = s.settle_failed_download(facts, Some(V_OLD));
        assert!(
            matches!(
                again,
                Ok(FailedDownload::Cleared | FailedDownload::NoAttempt)
            ),
            "{tag}: {again:?}"
        );
        assert!(s.read_attempt().unwrap().is_none(), "{tag}");
        assert_eq!(s.admission(), Admission::Open, "{tag}");
    });
}
