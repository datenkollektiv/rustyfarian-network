//! A lost `att_ctr` must never make an id repeat that is still stored (a delivered report keeps `rb_id`).

use alloc::format;

use super::support::*;
use crate::ota::persist::fault_kv::FaultKv;
use crate::ota::persist::{
    reconcile_boot, BootDisposition, Delivery, KvError, OtaKv, OtaStore, PendingReportView,
    StoreError,
};
use crate::ota::wire::RollbackReason;
use crate::ota::SlotState;

/// A delivered report for id 1 (`rb = 0`), the epoch, and no counter: only `att_ctr` is lost.
fn delivered_report_world(kv: &FaultKv) {
    kv.put_u32("att_epoch", EPOCH);
    kv.put_u8("rb", 0)
        .put_u32("rb_id", 1)
        .put_str("rb_why", "health_deadline");
}

/// One full attempt that ends rolled back: begin, switch, boot, note the reason, reconcile on the old slot.
fn attempt_rollback_boot(s: &mut OtaStore<FaultKv>) -> BootDisposition {
    s.begin_attempt(V_NEW, SLOT_B, false).unwrap();
    s.mark_boot_selected().unwrap();
    s.mark_activated().unwrap();
    s.note_rollback(RollbackReason::HealthDeadline, SLOT_B)
        .unwrap();
    boot_old(s)
}

fn boot_old(s: &mut OtaStore<FaultKv>) -> BootDisposition {
    reconcile_boot(
        s,
        Ok(hw(
            SLOT_A,
            SlotState::Valid,
            Some((SLOT_B, SlotState::Invalid)),
        )),
        Some(V_OLD),
    )
    .disposition
}

#[test]
fn the_counter_never_reissues_a_stored_id() {
    for (key, stored) in [("att_id", 7), ("rb_id", 9), ("rq_id", 11), ("att_ctr", 3)] {
        let kv = FaultKv::new();
        kv.put_u32("att_epoch", EPOCH).put_u32(key, stored);
        assert_eq!(store(&kv).next_attempt_id(), Ok(stored + 1), "{key}");
        assert_eq!(kv.u32_of("att_ctr"), Some(stored + 1), "{key}");
    }
    let kv = FaultKv::new();
    kv.put_u32("att_epoch", EPOCH)
        .put_u32("att_ctr", 2)
        .put_u32("att_id", 8)
        .put_u32("rb_id", 5)
        .put_u32("rq_id", 6);
    assert_eq!(store(&kv).next_attempt_id(), Ok(9));
}

#[test]
fn the_floor_wraps_past_the_maximum_like_the_counter() {
    let kv = FaultKv::new();
    kv.put_u32("att_epoch", EPOCH).put_u32("rb_id", u32::MAX);
    assert_eq!(store(&kv).next_attempt_id(), Ok(1));
}

#[test]
fn an_unreadable_floor_key_fails_closed_without_a_write() {
    for key in ["att_ctr", "att_id", "rb_id", "rq_id"] {
        let kv = FaultKv::new();
        kv.put_u32("att_epoch", EPOCH)
            .put_u32("att_ctr", 1)
            .put_u32("att_id", 1)
            .put_u32("rb_id", 1)
            .put_u32("rq_id", 1);
        kv.corrupt(key);
        let result = store(&kv).next_attempt_id();
        assert!(
            matches!(result, Err(StoreError::Kv(_))),
            "{key}: {result:?}"
        );
        assert_eq!(kv.u32_of("att_ctr"), Some(1), "{key}");
    }
    // A read error at any position of the id reservation is propagated, and nothing is written.
    let probe = FaultKv::new();
    delivered_report_world(&probe);
    probe.reset_counters();
    store(&probe).next_attempt_id().unwrap();
    let reads = probe.reads();
    for n in 1..=reads {
        let kv = FaultKv::new();
        delivered_report_world(&kv);
        kv.reset_counters();
        kv.fail_read_at(n);
        let result = store(&kv).next_attempt_id();
        assert!(matches!(result, Err(StoreError::Kv(_))), "read {n}");
        assert!(!kv.has("att_ctr"), "read {n}");
    }
}

#[test]
fn a_lost_counter_does_not_swallow_the_next_rollback_report() {
    let kv = FaultKv::new();
    delivered_report_world(&kv);
    let mut s = store(&kv);
    assert_eq!(
        attempt_rollback_boot(&mut s),
        BootDisposition::ReportedRollback
    );
    assert_eq!(kv.u8_of("rb"), Some(1));
    assert_eq!(kv.u32_of("rb_id"), Some(2));
    assert_eq!(kv.str_of("rb_why").as_deref(), Some("health_deadline"));
    match s.pending_report() {
        Ok(PendingReportView::Ready(report)) => assert_eq!(report.attempt_id, 2),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_delivered_report_with_the_attempts_id_is_not_adopted() {
    let kv = FaultKv::new();
    put_counter(&kv, 5);
    put_attempt(&kv, 3, "2.0.0", "ota_1", true, true);
    kv.put_u8("rb", 0)
        .put_u32("rb_id", 3)
        .put_str("rb_why", "operator");
    let mut s = store(&kv);
    assert_eq!(boot_old(&mut s), BootDisposition::ReportedRollback);
    assert_eq!(kv.u8_of("rb"), Some(1));
    assert_eq!(kv.u32_of("rb_id"), Some(3));
    assert!(!kv.has("att_ver"));
}

#[test]
fn a_pending_report_with_the_attempts_id_is_still_adopted() {
    let kv = FaultKv::new();
    put_counter(&kv, 5);
    put_attempt(&kv, 3, "2.0.0", "ota_1", true, true);
    kv.put_u8("rb", 1)
        .put_u32("rb_id", 3)
        .put_str("rb_why", "operator");
    let mut s = store(&kv);
    kv.reset_counters();
    assert_eq!(boot_old(&mut s), BootDisposition::ReportedRollback);
    assert_eq!(kv.str_of("rb_why").as_deref(), Some("operator"));
    assert!(!kv.ops().contains(&"set:rb_why".into()), "{:?}", kv.ops());
}

#[test]
fn repairing_the_counter_never_goes_below_a_stored_id() {
    let kv = FaultKv::new();
    put_attempt(&kv, 4, "2.0.0", "ota_1", true, true);
    kv.put_u32("att_epoch", EPOCH).put_u32("att_ctr", 1);
    kv.put_u8("rb", 0).put_u32("rb_id", 9).put_u32("rq_id", 6);
    kv.corrupt_with("att_ctr", KvError::CODE_TYPE_MISMATCH);
    let mut s = store(&kv);
    assert!(s.repair_corrupt().is_ok());
    assert_eq!(kv.u32_of("att_ctr"), Some(9));
    assert!(!kv.has("att_epoch"));
}

/// Loses `att_ctr` before every step of a full attempt: the new report must still be pending with its own id.
#[test]
fn losing_the_counter_at_any_point_of_an_attempt_keeps_the_new_report() {
    for lose_before in 0..5 {
        let kv = FaultKv::new();
        delivered_report_world(&kv);
        let mut s = store(&kv);
        let lose = |kv: &FaultKv, step: usize| {
            if step == lose_before {
                kv.clone().remove("att_ctr").unwrap();
            }
        };
        lose(&kv, 0);
        s.begin_attempt(V_NEW, SLOT_B, false).unwrap();
        lose(&kv, 1);
        s.mark_boot_selected().unwrap();
        s.mark_activated().unwrap();
        lose(&kv, 2);
        s.note_rollback(RollbackReason::HealthDeadline, SLOT_B)
            .unwrap();
        lose(&kv, 3);
        let disposition = boot_old(&mut s);
        lose(&kv, 4);
        let ctx = format!("lost before step {lose_before}");
        assert_eq!(disposition, BootDisposition::ReportedRollback, "{ctx}");
        assert_eq!(kv.u8_of("rb"), Some(1), "{ctx}");
        assert_eq!(kv.u32_of("rb_id"), Some(2), "{ctx}");
        // The next id after any of these losses is still fresh.
        assert_eq!(s.mark_report_delivered(2), Ok(Delivery::Marked));
        assert_eq!(s.next_attempt_id(), Ok(3), "{ctx}");
    }
}

/// The window of `repair_counter`: a crash or failure at each of its commits, then a full attempt.
#[test]
fn a_repair_of_the_counter_cut_short_at_any_commit_keeps_the_next_report() {
    each_fault(
        &|kv| {
            delivered_report_world(kv);
            kv.put_u32("att_ctr", 1);
            kv.corrupt_with("att_ctr", KvError::CODE_TYPE_MISMATCH);
        },
        &|s| {
            for _ in 0..4 {
                if !matches!(s.repair_corrupt(), Ok(Some(_))) {
                    break;
                }
            }
        },
        |kv, fault, n| {
            let mut s = store(kv);
            let ctx = format!("{fault:?} at {n}");
            assert_eq!(
                attempt_rollback_boot(&mut s),
                BootDisposition::ReportedRollback,
                "{ctx}"
            );
            assert_eq!(kv.u8_of("rb"), Some(1), "{ctx}");
            assert_eq!(kv.u32_of("rb_id"), Some(2), "{ctx}");
        },
    );
}
