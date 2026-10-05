//! Unreadable keys never wedge boot for good, `slot_released`, and the report wedge guard.

use alloc::format;

use super::support::*;
use crate::ota::persist::fault_kv::FaultKv;
use crate::ota::persist::{
    reconcile_boot, BootDisposition, BootFault, BootOutcome, CorruptRecord, Delivery, KvError,
    OtaStore, PendingReport, PendingReportView, RepairedRecord, StoreError,
};
use crate::ota::{Admission, BlockedBy, SlotState};

fn boot_a(s: &mut OtaStore<FaultKv>) -> BootOutcome {
    reconcile_boot(
        s,
        Ok(hw(
            SLOT_A,
            SlotState::Valid,
            Some((SLOT_B, SlotState::Invalid)),
        )),
        Some(V_OLD),
    )
}

fn failed_closed(out: &BootOutcome) -> bool {
    matches!(out.disposition, BootDisposition::FailedClosed(_))
}

fn repair_all(s: &mut OtaStore<FaultKv>) {
    for _ in 0..8 {
        if !matches!(s.repair_corrupt(), Ok(Some(_))) {
            break;
        }
    }
}

/// Every key once: an attempt on `ota_1`, a request from `ota_0`, a delivered report, and a refusal.
fn world(kv: &FaultKv) {
    put_counter(kv, 5);
    put_attempt(kv, 5, "2.0.0", "ota_1", true, true);
    put_request(kv, 9, "operator", "ota_0");
    kv.put_u8("rb", 0)
        .put_u32("rb_id", 2)
        .put_str("rb_why", "operator");
    kv.put_str("rej_ver", "1.5.0");
}

#[test]
fn an_unreadable_reason_key_reads_as_unknown_and_never_fails_boot() {
    for code in [KvError::CODE_TYPE_MISMATCH, KvError::CODE_INVALID_VALUE] {
        // att_why: the attempt is reported with the reason "unknown".
        let kv = FaultKv::new();
        put_counter(&kv, 5);
        put_attempt(&kv, 5, "2.0.0", "ota_1", true, true);
        kv.put_str("att_why", "x");
        kv.corrupt_with("att_why", code);
        let out = boot_a(&mut store(&kv));
        assert_eq!(out.disposition, BootDisposition::ReportedRollback, "{code}");
        assert_eq!(kv.str_of("rb_why").as_deref(), Some("unknown"));
        assert!(!kv.has("att_ver"));

        // rq_why: the request is reported with the reason "unknown".
        let kv = FaultKv::new();
        put_counter(&kv, 5);
        put_request(&kv, 9, "operator", "ota_1");
        kv.corrupt_with("rq_why", code);
        let out = boot_a(&mut store(&kv));
        assert_eq!(out.disposition, BootDisposition::ReportedRollback, "{code}");
        assert_eq!(kv.str_of("rb_why").as_deref(), Some("unknown"));

        // rb_why: the pending report is published with the reason "unknown".
        let kv = FaultKv::new();
        put_counter(&kv, 5);
        kv.put_u8("rb", 1)
            .put_u32("rb_id", 4)
            .put_str("rb_why", "x");
        kv.corrupt_with("rb_why", code);
        let mut s = store(&kv);
        let out = boot_a(&mut s);
        assert!(!failed_closed(&out), "{code}");
        assert!(out.report_pending);
        match s.pending_report() {
            Ok(PendingReportView::Ready(PendingReport {
                attempt_id, why, ..
            })) => {
                assert_eq!((attempt_id, why.as_str()), (4, "unknown"));
            }
            other => panic!("{other:?}"),
        }
    }
}

#[test]
fn a_mistyped_reason_key_is_replaced_by_the_next_write() {
    let kv = FaultKv::new();
    put_counter(&kv, 5);
    put_attempt(&kv, 5, "2.0.0", "ota_1", true, true);
    kv.put_u8("att_why", 1)
        .put_u8("rb_why", 1)
        .put_u8("rej_ver", 1);
    let out = boot_a(&mut store(&kv));
    assert_eq!(out.disposition, BootDisposition::ReportedRollback);
    assert_eq!(kv.str_of("rb_why").as_deref(), Some("unknown"));
    assert_eq!(kv.str_of("rej_ver").as_deref(), Some("2.0.0"));
}

/// Puts a value of the wrong type under one key.
type Corrupt = fn(&FaultKv);

/// (key, how to corrupt it)
fn corrupt_cases() -> [(&'static str, Corrupt); 13] {
    fn as_str(kv: &FaultKv, key: &str) {
        kv.put_str(key, "x");
    }
    fn as_u8(kv: &FaultKv, key: &str) {
        kv.put_u8(key, 7);
    }
    [
        ("att_ver", |kv| as_u8(kv, "att_ver")),
        ("att_slot", |kv| as_u8(kv, "att_slot")),
        ("att_id", |kv| as_str(kv, "att_id")),
        ("att_boot", |kv| as_str(kv, "att_boot")),
        ("att_act", |kv| as_str(kv, "att_act")),
        ("att_inv", |kv| as_str(kv, "att_inv")),
        ("rq_from", |kv| as_u8(kv, "rq_from")),
        ("rq_id", |kv| as_str(kv, "rq_id")),
        ("rb", |kv| as_str(kv, "rb")),
        ("rb_id", |kv| as_str(kv, "rb_id")),
        ("att_ctr", |kv| as_str(kv, "att_ctr")),
        ("att_epoch", |kv| as_str(kv, "att_epoch")),
        ("rej_ver", |kv| as_u8(kv, "rej_ver")),
    ]
}

#[test]
fn every_corrupt_key_ends_its_wedge_after_a_crash_injected_repair() {
    for (key, corrupt) in corrupt_cases() {
        let setup = |kv: &FaultKv| {
            world(kv);
            corrupt(kv);
        };
        // Without the repair the unreadable key either fails the boot closed or is tolerated by this boot (the counter and epoch are only read when an id is issued).
        let kv = FaultKv::new();
        setup(&kv);
        let before = boot_a(&mut store(&kv));
        let tolerated = matches!(key, "rej_ver" | "att_ctr" | "att_epoch");
        assert_eq!(failed_closed(&before), !tolerated, "{key}");
        assert!(!before.admission_open_at_boot);

        each_fault(&setup, &repair_all, |kv, fault, n| {
            let tag = format!("{key}: {fault:?} at {n}");
            let mut s = store(kv);
            repair_all(&mut s);
            assert_eq!(s.repair_corrupt(), Ok(None), "{tag}");
            for round in 0..2 {
                let out = boot_a(&mut s);
                assert!(!failed_closed(&out), "{tag}: boot {round} {out:?}");
            }
        });
    }
}

#[test]
fn a_transient_flash_error_still_fails_closed_and_repairs_nothing() {
    let kv = FaultKv::new();
    world(&kv);
    kv.corrupt("att_id");
    let mut s = store(&kv);
    assert!(failed_closed(&boot_a(&mut s)));
    assert_eq!(
        s.repair_corrupt(),
        Err(StoreError::Kv(KvError::new(
            crate::ota::persist::fault_kv::INJECTED
        )))
    );
    assert!(kv.has("att_ver"));
}

#[test]
fn repairing_an_attempt_field_keeps_counter_epoch_report_request_and_refusal() {
    let kv = FaultKv::new();
    world(&kv);
    kv.put_str("att_id", "x");
    let mut s = store(&kv);
    assert_eq!(s.repair_corrupt(), Ok(Some(RepairedRecord::Attempt)));
    assert!(!kv.has("att_ver") && !kv.has("att_slot") && !kv.has("att_id"));
    assert_eq!(kv.u32_of("att_ctr"), Some(5));
    assert_eq!(kv.u32_of("att_epoch"), Some(EPOCH));
    assert_eq!(kv.u32_of("rb_id"), Some(2));
    assert!(kv.has("rq_from"));
    assert_eq!(kv.str_of("rej_ver").as_deref(), Some("1.5.0"));
    assert_eq!(s.repair_corrupt(), Ok(None));
}

#[test]
fn repairing_an_unreadable_report_flag_keeps_the_report_pending() {
    let kv = FaultKv::new();
    world(&kv);
    kv.put_str("rb", "x");
    let mut s = store(&kv);
    assert_eq!(s.repair_corrupt(), Ok(Some(RepairedRecord::Report)));
    assert_eq!(kv.u8_of("rb"), Some(1));
    assert_eq!(kv.u32_of("rb_id"), Some(2));
    assert_eq!(kv.str_of("rb_why").as_deref(), Some("operator"));

    let kv = FaultKv::new();
    world(&kv);
    kv.put_u8("rb", 1).put_str("rb_id", "x");
    let mut s = store(&kv);
    assert_eq!(s.repair_corrupt(), Ok(Some(RepairedRecord::Report)));
    assert_eq!(kv.u8_of("rb"), Some(1));
    assert!(!kv.has("rb_id"));
    assert_eq!(kv.str_of("rb_why").as_deref(), Some("operator"));
}

#[test]
fn repairing_an_unreadable_counter_never_reissues_a_stored_id() {
    let kv = FaultKv::new();
    world(&kv);
    kv.put_str("att_ctr", "x");
    let mut s = store(&kv);
    assert_eq!(s.repair_corrupt(), Ok(Some(RepairedRecord::Counter)));
    assert!(!kv.has("att_epoch"));
    assert_eq!(kv.u32_of("att_ctr"), Some(9));
    assert_eq!(s.next_attempt_id(), Ok(10));
    assert_eq!(kv.u32_of("att_epoch"), Some(EPOCH));
}

#[test]
fn slot_released_is_independent_of_the_records() {
    // A Valid boot that persists a report: the slot is released, admission is not.
    let kv = FaultKv::new();
    put_counter(&kv, 5);
    put_attempt(&kv, 5, "2.0.0", "ota_1", true, true);
    let mut s = store(&kv);
    let out = boot_a(&mut s);
    assert_eq!(out.disposition, BootDisposition::ReportedRollback);
    assert!(out.slot_released && !out.admission_open_at_boot && out.report_pending);
    assert_eq!(s.admission(), Admission::Blocked(BlockedBy::ReportPending));

    // FailedClosed on the store with a readable Valid slot: still released.
    let kv = FaultKv::new();
    put_attempt(&kv, 5, "2.0.0", "ota_7", true, true);
    let out = boot_a(&mut store(&kv));
    assert!(matches!(
        out.disposition,
        BootDisposition::FailedClosed(BootFault::Store(_))
    ));
    assert!(out.slot_released && !out.admission_open_at_boot);

    // An unverified image or an unreadable slot never releases it.
    let kv = FaultKv::new();
    let out = reconcile_boot(
        &mut store(&kv),
        Ok(hw(SLOT_A, SlotState::PendingVerify, None)),
        Some(V_OLD),
    );
    assert!(!out.slot_released && !out.admission_open_at_boot);
    let out = reconcile_boot(
        &mut store(&kv),
        Err(crate::ota::persist::HardwareReadError::Busy),
        Some(V_OLD),
    );
    assert!(!out.slot_released && !out.admission_open_at_boot);

    // A clean Valid boot opens both.
    let out = boot_a(&mut store(&kv));
    assert!(out.slot_released && out.admission_open_at_boot);
}

#[test]
fn a_report_of_an_earlier_event_is_published_so_the_attempt_can_be_reported_after_it() {
    let kv = FaultKv::new();
    put_counter(&kv, 9);
    put_attempt(&kv, 5, "2.0.0", "ota_1", true, true);
    kv.put_u8("rb", 1)
        .put_u32("rb_id", 3)
        .put_str("rb_why", "operator");
    let mut s = store(&kv);

    // The attempt cannot be reported while report 3 is undelivered.
    let out = boot_a(&mut s);
    assert_eq!(
        out.disposition,
        BootDisposition::Incomplete(StoreError::ReportPending)
    );
    assert!(kv.has("att_ver"));

    // Report 3 is published although the attempt still exists.
    let view = s.pending_report().unwrap();
    let PendingReportView::Ready(report) = view else {
        panic!("{view:?}")
    };
    assert_eq!((report.attempt_id, report.why.as_str()), (3, "operator"));
    assert_eq!(s.mark_report_delivered(3), Ok(Delivery::Marked));

    // The next boot persists the attempt's own report and clears it.
    let out = boot_a(&mut s);
    assert_eq!(out.disposition, BootDisposition::ReportedRollback);
    assert!(!kv.has("att_ver"));
    assert_eq!((kv.u8_of("rb"), kv.u32_of("rb_id")), (Some(1), Some(5)));
    let view = s.pending_report().unwrap();
    let PendingReportView::Ready(report) = view else {
        panic!("{view:?}")
    };
    assert_eq!(report.attempt_id, 5);
    assert_eq!(s.mark_report_delivered(5), Ok(Delivery::Marked));
    assert_eq!(kv.u8_of("rb"), Some(0));
    assert_eq!(s.pending_report(), Ok(PendingReportView::None));
}

#[test]
fn a_report_that_belongs_to_the_attempt_or_has_no_id_waits_for_the_boot() {
    let kv = FaultKv::new();
    put_counter(&kv, 9);
    put_attempt(&kv, 5, "2.0.0", "ota_1", true, true);
    kv.put_u8("rb", 1).put_u32("rb_id", 5);
    assert_eq!(
        store(&kv).pending_report(),
        Ok(PendingReportView::AwaitingBootReconcile)
    );
    let kv = FaultKv::new();
    put_counter(&kv, 9);
    put_attempt(&kv, 5, "2.0.0", "ota_1", true, true);
    kv.put_u8("rb", 1);
    assert_eq!(
        store(&kv).pending_report(),
        Ok(PendingReportView::AwaitingBootReconcile)
    );
}

/// The id is stored before the commit marker, so no write sequence leaves an attempt or a request without one: such a record is corrupt, fails boot closed and `repair_corrupt` removes it.
#[test]
fn an_attempt_without_an_id_is_corrupt_and_never_trusted() {
    let setup = |kv: &FaultKv| {
        put_counter(kv, 5);
        kv.put_str("att_ver", "2.0.0")
            .put_str("att_slot", "ota_1")
            .put_u8("att_inv", 0);
        kv.put_u8("att_boot", 1).put_u8("att_act", 1);
    };
    let kv = FaultKv::new();
    setup(&kv);
    let mut s = store(&kv);
    assert_eq!(
        s.read_attempt(),
        Err(StoreError::Corrupt(CorruptRecord::AttemptWithoutId))
    );
    assert_eq!(s.corrupt_present(), Ok(true));
    kv.reset_counters();
    let out = boot_a(&mut s);
    assert!(failed_closed(&out) && !out.admission_open_at_boot);
    assert!(out.refuse_mark_valid && !out.report_pending);
    assert_eq!(kv.commits(), 0, "the failed boot writes nothing");
    assert_eq!(
        s.admission(),
        Admission::Blocked(BlockedBy::AttemptUnresolved)
    );

    each_fault(&setup, &repair_all, |kv, fault, n| {
        let tag = format!("{fault:?} at {n}");
        let mut s = store(kv);
        repair_all(&mut s);
        assert_eq!(s.repair_corrupt(), Ok(None), "{tag}");
        assert!(!kv.has("att_ver"), "{tag}");
        assert_eq!(kv.u32_of("att_ctr"), Some(5), "{tag}");
        let out = boot_a(&mut s);
        assert!(!failed_closed(&out) && out.admission_open_at_boot, "{tag}");
        assert!(
            !kv.has("rb") && !kv.has("rej_ver"),
            "{tag}: nothing was reported"
        );
    });
}

#[test]
fn a_request_without_an_id_is_corrupt_and_never_trusted() {
    let setup = |kv: &FaultKv| {
        put_counter(kv, 5);
        kv.put_str("rq_why", "operator").put_str("rq_from", "ota_1");
    };
    let kv = FaultKv::new();
    setup(&kv);
    let mut s = store(&kv);
    assert_eq!(
        s.request(),
        Err(StoreError::Corrupt(CorruptRecord::RequestWithoutId))
    );
    assert_eq!(s.corrupt_present(), Ok(true));
    kv.reset_counters();
    let out = boot_a(&mut s);
    assert!(failed_closed(&out) && !out.admission_open_at_boot);
    assert_eq!(kv.commits(), 0, "the failed boot writes nothing");
    assert_eq!(
        s.admission(),
        Admission::Blocked(BlockedBy::AttemptUnresolved)
    );

    each_fault(&setup, &repair_all, |kv, fault, n| {
        let tag = format!("{fault:?} at {n}");
        let mut s = store(kv);
        repair_all(&mut s);
        assert_eq!(s.repair_corrupt(), Ok(None), "{tag}");
        assert!(!kv.has("rq_from"), "{tag}");
        let out = boot_a(&mut s);
        assert!(!failed_closed(&out) && out.admission_open_at_boot, "{tag}");
        assert!(!kv.has("rb"), "{tag}: the request was not reported");
    });
}

/// `persist_report` writes `rb` before `rb_id`, so a power loss between them leaves a pending report without an id: the attempt's report is persisted again, not trusted.
#[test]
fn a_pending_report_without_an_id_is_persisted_again_for_the_attempt() {
    let kv = FaultKv::new();
    put_counter(&kv, 3);
    put_attempt(&kv, 3, "2.0.0", "ota_1", true, true);
    kv.put_u8("rb", 1).put_str("rb_why", "stale");
    let out = boot_a(&mut store(&kv));
    assert_eq!(out.disposition, BootDisposition::ReportedRollback);
    assert_eq!(kv.u32_of("rb_id"), Some(3));
    assert_eq!(kv.str_of("rb_why").as_deref(), Some("bootloader"));
    assert_eq!(kv.u32_of("att_ctr"), Some(3), "no fresh id was burned");
}

/// The overwrite of a mistyped `rb` is one commit: a power loss or failure at any point leaves the report armed or repairable, never dropped.
#[test]
fn repairing_an_unreadable_report_flag_never_drops_the_report_at_any_commit() {
    let setup = |kv: &FaultKv| {
        world(kv);
        kv.put_str("rb", "x").put_u32("rb_id", 2);
    };
    let total = total_commits(&setup, &|s| repair_all(s));
    assert_eq!(total, 1, "the repair is a single overwrite of rb");
    each_fault(&setup, &repair_all, |kv, fault, n| {
        let tag = format!("{fault:?} at {n}");
        let armed = kv.u8_of("rb") == Some(1);
        let still_unreadable = kv.str_of("rb").is_some();
        assert!(armed || still_unreadable, "{tag}: rb vanished");
        let mut s = store(kv);
        repair_all(&mut s);
        assert_eq!(kv.u8_of("rb"), Some(1), "{tag}");
        assert_eq!(s.report_undelivered(), Ok(true), "{tag}");
        assert_eq!(kv.u32_of("rb_id"), Some(2), "{tag}");
        assert_eq!(kv.str_of("rb_why").as_deref(), Some("operator"), "{tag}");
    });
}
