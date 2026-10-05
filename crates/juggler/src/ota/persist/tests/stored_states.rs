//! Stored states the store must read back and settle without losing anything.

use super::support::*;
use crate::ota::persist::fault_kv::FaultKv;
use crate::ota::persist::{
    reconcile_boot, BootDisposition, Delivery, PendingReport, PendingReportView, SettleOutcome,
};
use crate::ota::{Admission, BlockedBy, ReconcileAction, SlotState};

#[test]
fn empty_namespace_is_a_clean_open_boot() {
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
    assert!(out.admission_open_at_boot);
}

#[test]
fn in_flight_attempt_survives_with_and_without_activation() {
    for act in [false, true] {
        let kv = FaultKv::new();
        put_counter(&kv, 3);
        put_attempt(&kv, 3, "2.0.0", "ota_1", true, act);
        let mut s = store(&kv);
        let out = reconcile_boot(
            &mut s,
            Ok(hw(SLOT_B, SlotState::PendingVerify, None)),
            Some(V_NEW),
        );
        assert_eq!(
            out.action,
            Some(ReconcileAction::AwaitHealthCheck {
                mark_activated: !act
            })
        );
        assert_eq!(kv.u32_of("att_id"), Some(3));
        assert_eq!(kv.u8_of("att_act"), Some(1));
        assert!(!out.admission_open_at_boot);
    }
}

#[test]
fn undelivered_report_with_id_is_published_with_the_old_epoch() {
    let kv = FaultKv::new();
    put_counter(&kv, 3);
    kv.put_u8("rb", 1)
        .put_u32("rb_id", 3)
        .put_str("rb_why", "health_deadline");
    kv.put_str("rej_ver", "2.0.0");
    let mut s = store(&kv);
    let out = reconcile_boot(&mut s, Ok(hw(SLOT_A, SlotState::Valid, None)), Some(V_OLD));
    assert!(out.report_pending && !out.admission_open_at_boot);
    assert_eq!(
        s.pending_report(),
        Ok(PendingReportView::Ready(PendingReport {
            attempt_id: 3,
            epoch: EPOCH,
            why: "health_deadline".try_into().unwrap(),
        }))
    );
    assert_eq!(s.mark_report_delivered(3), Ok(Delivery::Marked));
    assert_eq!(s.settle_request(SLOT_A), Ok(SettleOutcome::NoRequest));
    assert_eq!(s.admission(), Admission::Open);
    assert_eq!(s.refused(), Ok(Some(V_NEW)));
}

#[test]
fn undelivered_report_without_id_gets_a_fresh_id_later() {
    let kv = FaultKv::new();
    put_counter(&kv, 3);
    kv.put_u8("rb", 1).put_str("rb_why", "unhealthy");
    let mut s = store(&kv);
    let out = reconcile_boot(&mut s, Ok(hw(SLOT_A, SlotState::Valid, None)), Some(V_OLD));
    assert!(out.report_pending);
    assert!(!kv.has("rb_id"), "no attempt: no id is burned at boot");
    match s.pending_report().unwrap() {
        PendingReportView::Ready(r) => {
            assert_eq!(r.attempt_id, 4);
            assert_eq!(r.why.as_str(), "unhealthy");
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn undelivered_report_without_id_is_adopted_by_its_attempt() {
    let kv = FaultKv::new();
    put_counter(&kv, 3);
    put_attempt(&kv, 3, "2.0.0", "ota_1", true, true);
    kv.put_u8("rb", 1).put_str("rb_why", "bootloader");
    let mut s = store(&kv);
    let facts = hw(SLOT_A, SlotState::Valid, Some((SLOT_B, SlotState::Invalid)));
    let out = reconcile_boot(&mut s, Ok(facts), Some(V_OLD));
    assert_eq!(out.disposition, BootDisposition::ReportedRollback);
    assert_eq!(kv.u32_of("rb_id"), Some(3));
    assert_eq!(kv.u32_of("att_ctr"), Some(3), "no fresh id was burned");
}

#[test]
fn delivered_report_and_refusal_keep_admission_open() {
    let kv = FaultKv::new();
    kv.put_u8("rb", 0)
        .put_u32("rb_id", 2)
        .put_str("rb_why", "operator");
    kv.put_str("rej_ver", "2.0.0");
    let mut s = store(&kv);
    assert_eq!(s.admission(), Admission::Open);
    assert_eq!(s.refused(), Ok(Some(V_NEW)));
    let out = reconcile_boot(&mut s, Ok(hw(SLOT_A, SlotState::Valid, None)), Some(V_OLD));
    assert!(out.admission_open_at_boot && !out.report_pending);
}

#[test]
fn armed_request_is_reported_when_another_slot_runs() {
    let kv = FaultKv::new();
    put_counter(&kv, 6);
    put_request(&kv, 6, "operator", "ota_1");
    let mut s = store(&kv);
    assert_eq!(
        s.admission(),
        Admission::Blocked(BlockedBy::AttemptUnresolved)
    );
    let out = reconcile_boot(&mut s, Ok(hw(SLOT_A, SlotState::Valid, None)), Some(V_OLD));
    assert_eq!(out.disposition, BootDisposition::ReportedRollback);
    assert_eq!(kv.u32_of("rb_id"), Some(6));
    assert_eq!(kv.str_of("rb_why").as_deref(), Some("operator"));
    assert!(!kv.has("rq_from"));
}

#[test]
fn armed_request_is_dropped_when_the_same_slot_still_runs() {
    let kv = FaultKv::new();
    put_request(&kv, 6, "operator", "ota_1");
    let mut s = store(&kv);
    let out = reconcile_boot(&mut s, Ok(hw(SLOT_B, SlotState::Valid, None)), Some(V_NEW));
    assert_eq!(
        out.disposition,
        BootDisposition::Settled(SettleOutcome::DroppedNotRolledBack)
    );
    assert!(out.admission_open_at_boot && !kv.has("rb"));
}
