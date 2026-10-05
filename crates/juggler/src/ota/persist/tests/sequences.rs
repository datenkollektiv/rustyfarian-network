//! Write order of every sequence, pinned as exact `(op, key)` lists.

use super::support::*;
use crate::ota::persist::fault_kv::FaultKv;
use crate::ota::persist::{
    Delivery, KvError, PendingReport, PendingReportView, ReasonNote, RefusalNote, RepairedRecord,
    SettleOutcome, StoreError, FACTORY_SLOT,
};
use crate::ota::wire::RollbackReason;
use crate::ota::{Admission, BlockedBy, SlotId, Version};

#[test]
fn ids_epoch_first_never_zero() {
    let kv = FaultKv::new();
    let mut s = store(&kv);
    assert_eq!(s.next_attempt_id(), Ok(1));
    assert_eq!(kv.ops(), strs(&["set:att_epoch", "set:att_ctr"]));
    kv.reset_counters();
    assert_eq!(s.next_attempt_id(), Ok(2));
    assert_eq!(kv.ops(), strs(&["set:att_ctr"]));
    assert_eq!(kv.u32_of("att_epoch"), Some(EPOCH));
    kv.put_u32("att_ctr", u32::MAX);
    assert_eq!(s.next_attempt_id(), Ok(1));
}

#[test]
fn begin_attempt_order_and_commit_marker() {
    let kv = FaultKv::new();
    put_counter(&kv, 4);
    put_attempt(&kv, 4, "1.5.0", "ota_1", true, true);
    let mut s = store(&kv);
    assert_eq!(
        s.begin_attempt(V_NEW, SLOT_A, false),
        Err(StoreError::NotAdmitted(BlockedBy::AttemptUnresolved))
    );
    assert!(kv.ops().is_empty());

    // Orphans without the two markers do not block and are cleared first.
    let kv = FaultKv::new();
    put_counter(&kv, 4);
    kv.put_str("att_why", "x")
        .put_u8("att_act", 1)
        .put_u8("att_boot", 1)
        .put_u32("att_id", 3);
    kv.put_str("rej_ver", "1.5.0");
    let mut s = store(&kv);
    assert_eq!(s.begin_attempt(V_NEW, SLOT_B, true), Ok(5));
    assert_eq!(
        kv.ops(),
        strs(&[
            "set:att_ctr",
            "rm:att_why",
            "rm:att_act",
            "rm:att_boot",
            "rm:att_id",
            "set:att_id",
            "set:att_inv",
            "set:att_ver",
            "set:att_slot"
        ])
    );
    assert_eq!(kv.str_of("att_slot").as_deref(), Some("ota_1"));
    assert_eq!(kv.str_of("att_ver").as_deref(), Some("2.0.0"));
    assert_eq!(kv.u8_of("att_inv"), Some(1));
    assert_eq!(kv.str_of("rej_ver").as_deref(), Some("1.5.0"));
}

#[test]
fn begin_attempt_never_deletes_request_or_report() {
    let kv = FaultKv::new();
    put_request(&kv, 7, "operator", "ota_0");
    let mut s = store(&kv);
    assert_eq!(
        s.begin_attempt(V_NEW, SLOT_B, false),
        Err(StoreError::NotAdmitted(BlockedBy::AttemptUnresolved))
    );
    assert!(kv.ops().is_empty());
    assert!(kv.has("rq_from") && kv.has("rq_id") && kv.has("rq_why"));

    let kv = FaultKv::new();
    kv.put_u8("rb", 1)
        .put_u32("rb_id", 3)
        .put_str("rb_why", "bootloader");
    let mut s = store(&kv);
    assert_eq!(
        s.begin_attempt(V_NEW, SLOT_B, false),
        Err(StoreError::NotAdmitted(BlockedBy::ReportPending))
    );
    assert!(kv.ops().is_empty());
}

#[test]
fn begin_attempt_rejects_slots_without_a_target_label() {
    let kv = FaultKv::new();
    let mut s = store(&kv);
    assert_eq!(
        s.begin_attempt(V_NEW, FACTORY_SLOT, false),
        Err(StoreError::InvalidSlot)
    );
    assert_eq!(
        s.begin_attempt(V_NEW, SlotId(2), false),
        Err(StoreError::InvalidSlot)
    );
    assert!(kv.ops().is_empty());
}

#[test]
fn activation_marks_are_single_commits() {
    let kv = FaultKv::new();
    let mut s = store(&kv);
    s.mark_boot_selected().unwrap();
    s.mark_activated().unwrap();
    assert_eq!(kv.ops(), strs(&["set:att_boot", "set:att_act"]));
}

#[test]
fn report_rollback_persists_report_then_refusal_then_clears() {
    let kv = FaultKv::new();
    put_counter(&kv, 5);
    put_attempt(&kv, 5, "2.0.0", "ota_1", true, true);
    kv.put_str("att_why", "health_deadline");
    kv.put_u8("rb", 0)
        .put_u32("rb_id", 3)
        .put_str("rb_why", "operator");
    let mut s = store(&kv);
    assert_eq!(s.report_rollback(5, Some(V_NEW), false), Ok(()));
    assert_eq!(
        kv.ops(),
        strs(&[
            "rm:rb_id",
            "set:rb_why",
            "set:rb",
            "set:rb_id",
            "set:rej_ver",
            "rm:att_ver",
            "rm:att_slot",
            "rm:att_act",
            "rm:att_boot",
            "rm:att_inv",
            "rm:att_id",
            "rm:att_why"
        ])
    );
    assert_eq!(kv.str_of("rb_why").as_deref(), Some("health_deadline"));
    assert_eq!(kv.u8_of("rb"), Some(1));
    assert_eq!(kv.u32_of("rb_id"), Some(5));
    assert_eq!(s.admission(), Admission::Blocked(BlockedBy::ReportPending));
}

#[test]
fn report_rollback_without_reason_says_bootloader_and_already_skips_the_report() {
    let kv = FaultKv::new();
    put_attempt(&kv, 5, "2.0.0", "ota_1", true, false);
    let mut s = store(&kv);
    s.report_rollback(5, None, false).unwrap();
    assert_eq!(kv.str_of("rb_why").as_deref(), Some("bootloader"));
    assert!(!kv.has("rej_ver"));

    let kv = FaultKv::new();
    put_attempt(&kv, 6, "2.0.0", "ota_1", true, false);
    let mut s = store(&kv);
    s.report_rollback(6, Some(V_NEW), true).unwrap();
    assert_eq!(kv.ops()[0], "set:rej_ver");
    assert!(!kv.has("rb"));
}

#[test]
fn report_rollback_keeps_unknown_reason_labels_raw() {
    let kv = FaultKv::new();
    put_attempt(&kv, 5, "2.0.0", "ota_1", true, true);
    kv.put_str("att_why", "from_the_future");
    store(&kv).report_rollback(5, None, false).unwrap();
    assert_eq!(kv.str_of("rb_why").as_deref(), Some("from_the_future"));
}

#[test]
fn single_report_guard_keeps_the_attempt() {
    let kv = FaultKv::new();
    put_attempt(&kv, 6, "2.0.0", "ota_1", true, true);
    kv.put_u8("rb", 1).put_u32("rb_id", 3);
    let mut s = store(&kv);
    assert_eq!(
        s.report_rollback(6, Some(V_NEW), false),
        Err(StoreError::ReportPending)
    );
    assert!(kv.ops().is_empty());
    assert!(kv.has("att_ver"));
}

#[test]
fn unparseable_refusal_never_blocks_report_or_completion() {
    let kv = FaultKv::new();
    put_attempt(&kv, 5, "2.0.0", "ota_1", true, true);
    kv.put_str("rej_ver", "garbage");
    let mut s = store(&kv);
    assert_eq!(s.refused(), Ok(None));
    s.report_rollback(5, Some(V_NEW), false).unwrap();
    assert_eq!(kv.str_of("rej_ver").as_deref(), Some("2.0.0"));

    let kv = FaultKv::new();
    put_attempt(&kv, 5, "2.0.0", "ota_1", true, true);
    kv.put_str("rej_ver", "garbage");
    store(&kv).complete_attempt(V_NEW).unwrap();
    assert!(!kv.has("rej_ver") && !kv.has("att_ver"));
}

#[test]
fn complete_attempt_order_and_refusal_rules() {
    let kv = FaultKv::new();
    put_attempt(&kv, 5, "2.0.0", "ota_1", true, true);
    kv.put_str("rej_ver", "1.5.0");
    store(&kv).complete_attempt(V_NEW).unwrap();
    assert_eq!(
        kv.ops(),
        strs(&[
            "rm:rej_ver",
            "rm:att_ver",
            "rm:att_slot",
            "rm:att_act",
            "rm:att_boot",
            "rm:att_inv",
            "rm:att_id"
        ])
    );

    let kv = FaultKv::new();
    put_attempt(&kv, 5, "2.0.0", "ota_1", true, true);
    kv.put_str("rej_ver", "2.0.0");
    store(&kv).complete_attempt(V_NEW).unwrap();
    assert!(kv.has("rej_ver"));
}

#[test]
fn complete_attempt_treats_a_corrupt_refusal_as_dropped() {
    for code in [KvError::CODE_TYPE_MISMATCH, KvError::CODE_INVALID_VALUE] {
        let kv = FaultKv::new();
        put_attempt(&kv, 5, "2.0.0", "ota_1", true, true);
        kv.put_str("rej_ver", "1.5.0");
        kv.corrupt_with("rej_ver", code);
        assert_eq!(store(&kv).complete_attempt(V_NEW), Ok(()), "{code}");
        assert!(!kv.has("rej_ver") && !kv.has("att_ver"), "{code}");
    }
}

#[test]
fn complete_attempt_keeps_the_attempt_on_a_transient_refusal_read_error() {
    let kv = FaultKv::new();
    put_attempt(&kv, 5, "2.0.0", "ota_1", true, true);
    kv.put_str("rej_ver", "1.5.0");
    kv.fail_read_at(1);
    kv.reset_counters();
    kv.fail_read_at(1);
    assert!(matches!(
        store(&kv).complete_attempt(V_NEW),
        Err(StoreError::Kv(_))
    ));
    assert!(kv.ops().is_empty());
    assert!(kv.has("rej_ver") && kv.has("att_ver"));
}

#[test]
fn complete_attempt_write_error_keeps_the_attempt() {
    let kv = FaultKv::new();
    put_attempt(&kv, 5, "2.0.0", "ota_1", true, true);
    kv.put_str("rej_ver", "1.5.0");
    kv.fail_at(1);
    assert!(store(&kv).complete_attempt(V_NEW).is_err());
    assert!(kv.has("att_ver"));
}

#[test]
fn note_rollback_attempt_branch_first_reason_wins() {
    let kv = FaultKv::new();
    put_attempt(&kv, 5, "2.0.0", "ota_1", true, true);
    let mut s = store(&kv);
    let Ok(ReasonNote::Written(noted)) = s.note_rollback(RollbackReason::HealthDeadline, SLOT_B)
    else {
        panic!("the first reason is written");
    };
    assert_eq!(
        s.note_rollback(RollbackReason::Operator, SLOT_B),
        Ok(ReasonNote::KeptFirst)
    );
    assert_eq!(kv.str_of("att_why").as_deref(), Some("health_deadline"));
    kv.reset_counters();
    s.withdraw_rollback(noted).unwrap();
    assert_eq!(kv.ops(), strs(&["rm:att_why"]));
    assert!(!kv.has("att_why"));
}

#[test]
fn note_rollback_request_branch_arms_last_and_first_reason_wins() {
    let kv = FaultKv::new();
    let mut s = store(&kv);
    let Ok(ReasonNote::Written(noted)) = s.note_rollback(RollbackReason::Operator, SLOT_B) else {
        panic!("the first reason is written");
    };
    assert_eq!(
        kv.ops(),
        strs(&[
            "set:att_epoch",
            "set:att_ctr",
            "set:rq_id",
            "set:rq_why",
            "set:rq_from"
        ])
    );
    assert_eq!(kv.str_of("rq_from").as_deref(), Some("ota_1"));
    assert_eq!(
        s.note_rollback(RollbackReason::Unhealthy, SLOT_B),
        Ok(ReasonNote::KeptFirst)
    );
    assert_eq!(kv.str_of("rq_why").as_deref(), Some("operator"));
    assert_eq!(
        s.admission(),
        Admission::Blocked(BlockedBy::AttemptUnresolved)
    );
    kv.reset_counters();
    s.withdraw_rollback(noted).unwrap();
    assert_eq!(kv.ops(), strs(&["rm:rq_from", "rm:rq_why", "rm:rq_id"]));
    assert_eq!(s.admission(), Admission::Open);
}

#[test]
fn an_older_request_is_kept_and_a_stale_token_withdraws_nothing() {
    // The first request wins; the second call gets no token, so only the first token exists.
    let kv = FaultKv::new();
    let mut s = store(&kv);
    let Ok(ReasonNote::Written(first)) = s.note_rollback(RollbackReason::Operator, SLOT_B) else {
        panic!("the first reason is written");
    };
    assert_eq!(
        s.note_rollback(RollbackReason::Unhealthy, SLOT_B),
        Ok(ReasonNote::KeptFirst)
    );
    assert!(kv.has("rq_from"));

    // The request is settled and another one armed: the old token must not remove the new request.
    assert_eq!(s.settle_request(SLOT_A), Ok(SettleOutcome::Reported));
    assert_eq!(s.mark_report_delivered(1), Ok(Delivery::Marked));
    let Ok(ReasonNote::Written(_second)) = s.note_rollback(RollbackReason::Unhealthy, SLOT_A)
    else {
        panic!("a new request is armed");
    };
    kv.reset_counters();
    s.withdraw_rollback(first).unwrap();
    assert!(kv.ops().is_empty(), "{:?}", kv.ops());
    assert_eq!(kv.str_of("rq_why").as_deref(), Some("unhealthy"));

    // An attempt token does nothing once the attempt is gone.
    let kv = FaultKv::new();
    put_attempt(&kv, 5, "2.0.0", "ota_1", true, true);
    let mut s = store(&kv);
    let Ok(ReasonNote::Written(noted)) = s.note_rollback(RollbackReason::Operator, SLOT_B) else {
        panic!("the first reason is written");
    };
    s.clear_attempt().unwrap();
    kv.reset_counters();
    s.withdraw_rollback(noted).unwrap();
    assert!(kv.ops().is_empty());
}

#[test]
fn note_rollback_from_factory_slot_arms_a_factory_request() {
    let kv = FaultKv::new();
    let mut s = store(&kv);
    s.note_rollback(RollbackReason::Operator, FACTORY_SLOT)
        .unwrap();
    assert_eq!(kv.str_of("rq_from").as_deref(), Some("factory"));
}

#[test]
fn settle_request_variants() {
    // same slot still running: nothing was rolled back
    let kv = FaultKv::new();
    put_request(&kv, 7, "operator", "ota_1");
    assert_eq!(
        store(&kv).settle_request(SLOT_B),
        Ok(SettleOutcome::DroppedNotRolledBack)
    );
    assert_eq!(kv.ops(), strs(&["rm:rq_from", "rm:rq_why", "rm:rq_id"]));

    // different slot: a report is persisted, request removed last
    let kv = FaultKv::new();
    put_request(&kv, 7, "operator", "ota_1");
    assert_eq!(
        store(&kv).settle_request(SLOT_A),
        Ok(SettleOutcome::Reported)
    );
    assert_eq!(
        kv.ops(),
        strs(&[
            "set:rb_why",
            "set:rb",
            "set:rb_id",
            "rm:rq_from",
            "rm:rq_why",
            "rm:rq_id"
        ])
    );
    assert_eq!(kv.u32_of("rb_id"), Some(7));
    assert_eq!(kv.str_of("rb_why").as_deref(), Some("operator"));

    // crash after the report: same id, no second report, also once delivered
    for rb in [0u8, 1u8] {
        let kv = FaultKv::new();
        put_request(&kv, 7, "operator", "ota_1");
        kv.put_u8("rb", rb)
            .put_u32("rb_id", 7)
            .put_str("rb_why", "operator");
        assert_eq!(
            store(&kv).settle_request(SLOT_A),
            Ok(SettleOutcome::AlreadyReported)
        );
        assert_eq!(kv.ops(), strs(&["rm:rq_from", "rm:rq_why", "rm:rq_id"]));
    }

    // crash between rb=1 and rb_id: adopt the request id, never a fresh one
    let kv = FaultKv::new();
    put_request(&kv, 7, "operator", "ota_1");
    kv.put_u8("rb", 1).put_str("rb_why", "operator");
    assert_eq!(
        store(&kv).settle_request(SLOT_A),
        Ok(SettleOutcome::AlreadyReported)
    );
    assert_eq!(kv.u32_of("rb_id"), Some(7));
    assert!(!kv.has("att_ctr"));

    // another report is undelivered: merged, reason lost by design
    let kv = FaultKv::new();
    put_request(&kv, 7, "operator", "ota_1");
    kv.put_u8("rb", 1)
        .put_u32("rb_id", 3)
        .put_str("rb_why", "bootloader");
    assert_eq!(
        store(&kv).settle_request(SLOT_A),
        Ok(SettleOutcome::MergedIntoPending)
    );
    assert_eq!(kv.u32_of("rb_id"), Some(3));
    assert!(!kv.has("rq_from"));

    assert_eq!(
        store(&FaultKv::new()).settle_request(SLOT_A),
        Ok(SettleOutcome::NoRequest)
    );
}

#[test]
fn settle_failure_keeps_the_request_armed_and_admission_blocked() {
    let kv = FaultKv::new();
    put_request(&kv, 7, "operator", "ota_1");
    kv.put_u8("rb", 0).put_u32("rb_id", 3);
    // commit 1 = rm rb_id, commit 2 = erase rb_why: the set after it fails
    kv.fail_after_erase(2);
    let mut s = store(&kv);
    assert!(s.settle_request(SLOT_A).is_err());
    assert!(kv.has("rq_from"));
    assert_eq!(
        s.admission(),
        Admission::Blocked(BlockedBy::AttemptUnresolved)
    );
    assert_eq!(
        s.begin_attempt(V_NEW, SLOT_B, false),
        Err(StoreError::NotAdmitted(BlockedBy::AttemptUnresolved))
    );
}

#[test]
fn delivery_marks_only_the_matching_report_and_keeps_its_identity() {
    let kv = FaultKv::new();
    kv.put_u8("rb", 1)
        .put_u32("rb_id", 5)
        .put_str("rb_why", "operator");
    let mut s = store(&kv);
    assert_eq!(
        s.mark_report_delivered(6),
        Ok(Delivery::OtherReport { pending_id: 5 })
    );
    assert!(kv.ops().is_empty());
    assert_eq!(s.mark_report_delivered(5), Ok(Delivery::Marked));
    assert_eq!(kv.ops(), strs(&["set:rb"]));
    assert_eq!(kv.u8_of("rb"), Some(0));
    assert_eq!(kv.u32_of("rb_id"), Some(5));
    assert_eq!(kv.str_of("rb_why").as_deref(), Some("operator"));
}

#[test]
fn delivery_reports_not_pending_without_a_write() {
    // Absent, already delivered, and a pending report without an id.
    for setup in [
        (|_: &FaultKv| {}) as fn(&FaultKv),
        |kv| {
            kv.put_u8("rb", 0).put_u32("rb_id", 5);
        },
        |kv| {
            kv.put_u8("rb", 2).put_u32("rb_id", 5);
        },
        |kv| {
            kv.put_u8("rb", 1);
        },
    ] {
        let kv = FaultKv::new();
        setup(&kv);
        let mut s = store(&kv);
        assert_eq!(s.mark_report_delivered(5), Ok(Delivery::NotPending));
        assert!(kv.ops().is_empty());
    }
    // A second delivery of the same id is NotPending, not Marked.
    let kv = FaultKv::new();
    kv.put_u8("rb", 1).put_u32("rb_id", 5);
    let mut s = store(&kv);
    assert_eq!(s.mark_report_delivered(5), Ok(Delivery::Marked));
    assert_eq!(s.mark_report_delivered(5), Ok(Delivery::NotPending));
    // A read error is an error, not an outcome.
    kv.corrupt("rb");
    assert!(s.mark_report_delivered(5).is_err());
}

#[test]
fn rb_other_than_zero_or_one_is_not_pending() {
    let kv = FaultKv::new();
    kv.put_u8("rb", 7);
    let s = store(&kv);
    assert_eq!(s.report_undelivered(), Ok(false));
    assert!(!s.report().unwrap().unwrap().pending);
    assert_eq!(s.admission(), Admission::Open);
}

#[test]
fn pending_report_views() {
    let kv = FaultKv::new();
    let mut s = store(&kv);
    assert_eq!(s.pending_report(), Ok(PendingReportView::None));

    kv.put_u8("rb", 1)
        .put_u32("rb_id", 5)
        .put_str("rb_why", "operator");
    assert_eq!(
        s.pending_report(),
        Ok(PendingReportView::Ready(PendingReport {
            attempt_id: 5,
            epoch: EPOCH,
            why: "operator".try_into().unwrap()
        }))
    );
    assert_eq!(kv.u32_of("att_epoch"), Some(EPOCH));

    put_attempt(&kv, 5, "2.0.0", "ota_1", true, true);
    assert_eq!(
        s.pending_report(),
        Ok(PendingReportView::AwaitingBootReconcile)
    );
}

#[test]
fn pending_report_without_id_adopts_the_request_id_else_a_fresh_one() {
    let kv = FaultKv::new();
    put_counter(&kv, 8);
    put_request(&kv, 7, "operator", "ota_1");
    kv.put_u8("rb", 1).put_str("rb_why", "operator");
    let mut s = store(&kv);
    match s.pending_report().unwrap() {
        PendingReportView::Ready(r) => assert_eq!(r.attempt_id, 7),
        other => panic!("{other:?}"),
    }
    assert_eq!(kv.u32_of("att_ctr"), Some(8));

    let kv = FaultKv::new();
    put_counter(&kv, 8);
    kv.put_u8("rb", 1);
    let mut s = store(&kv);
    match s.pending_report().unwrap() {
        PendingReportView::Ready(r) => assert_eq!(r.attempt_id, 9),
        other => panic!("{other:?}"),
    }
}

#[test]
fn refusal_write_and_restore() {
    let kv = FaultKv::new();
    let mut s = store(&kv);
    assert_eq!(
        s.refuse_version(V_NEW),
        Ok(RefusalNote::Written { previous: None })
    );
    assert_eq!(s.refuse_version(V_NEW), Ok(RefusalNote::AlreadyRefused));
    let v3 = Version::new(3, 0, 0);
    assert_eq!(
        s.refuse_version(v3),
        Ok(RefusalNote::Written {
            previous: Some(V_NEW)
        })
    );
    s.restore_refusal(Some(V_NEW)).unwrap();
    assert_eq!(s.refused(), Ok(Some(V_NEW)));
    s.restore_refusal(None).unwrap();
    assert_eq!(s.refused(), Ok(None));
}

#[test]
fn repair_corrupt_clears_only_unreadable_shapes() {
    let kv = FaultKv::new();
    put_attempt(&kv, 5, "2.0.0", "ota_9", true, true);
    let mut s = store(&kv);
    assert!(s.read_attempt().is_err());
    assert_eq!(s.repair_corrupt(), Ok(Some(RepairedRecord::Attempt)));
    assert_eq!(s.read_attempt(), Ok(None));
    assert_eq!(s.repair_corrupt(), Ok(None));

    let kv = FaultKv::new();
    put_request(&kv, 7, "operator", "ota_9");
    let mut s = store(&kv);
    assert_eq!(s.repair_corrupt(), Ok(Some(RepairedRecord::Request)));
    assert!(!kv.has("rq_from"));
}

#[test]
fn a_pending_report_parses_its_reason_leniently_and_keeps_the_raw_label() {
    for (label, reason) in [
        ("health_deadline", RollbackReason::HealthDeadline),
        ("bootloader", RollbackReason::Bootloader),
        ("from_the_future", RollbackReason::Unknown),
        ("unknown", RollbackReason::Unknown),
    ] {
        let kv = FaultKv::new();
        kv.put_u8("rb", 1)
            .put_u32("rb_id", 4)
            .put_str("rb_why", label);
        put_counter(&kv, 4);
        let PendingReportView::Ready(report) = store(&kv).pending_report().unwrap() else {
            panic!("{label}");
        };
        assert_eq!(report.reason(), reason, "{label}");
        assert_eq!(report.why.as_str(), label);
    }
}
