//! The DDI action record as raw keys, in every build with the store: corruption probes and repair, the verdict folds of the boot sequences, and the id floor.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use super::support::*;
use crate::ota::persist::fault_kv::FaultKv;
use crate::ota::persist::{
    reconcile_boot, BootDisposition, BootFault, CorruptRecord, HardwareFacts, KvError,
    RepairedRecord, StoreError,
};
use crate::ota::{ReconcileAction, SlotState, Version};

/// The attempt of action `42`, booted into slot B; its success detail is `downgrade`.
fn bound_attempt(kv: &FaultKv) {
    put_counter(kv, 5);
    put_attempt(kv, 5, "2.0.0", "ota_1", true, true);
    put_ddi(kv, "42", 1, Some(5), None);
    kv.put_str("ddi_det0", "downgrade");
}

fn new_image_valid() -> HardwareFacts {
    hw(SLOT_B, SlotState::Valid, None)
}

fn fell_back() -> HardwareFacts {
    hw(SLOT_A, SlotState::Valid, Some((SLOT_B, SlotState::Invalid)))
}

fn ddi_ops(kv: &FaultKv) -> Vec<String> {
    kv.ops()
        .into_iter()
        .filter(|op| op.contains(":ddi_") || op.contains(":att_ver") || op.contains(":rb"))
        .collect()
}

fn assert_success_pending(kv: &FaultKv, tag: &str) {
    assert!(!(kv.has("att_ver") && kv.has("att_slot")), "{tag}: attempt");
    assert_eq!(kv.u8_of("ddi_stage"), Some(2), "{tag}: stage");
    assert_eq!(kv.u32_of("ddi_code"), Some(0), "{tag}: code");
    assert_eq!(
        kv.str_of("ddi_det0").as_deref(),
        Some("downgrade"),
        "{tag}: detail"
    );
    assert!(!kv.has("ddi_det1"), "{tag}: second detail");
    assert_eq!(kv.str_of("ddi_act").as_deref(), Some("42"), "{tag}: act");
}

fn assert_rollback_pending(kv: &FaultKv, tag: &str) {
    assert!(!(kv.has("att_ver") && kv.has("att_slot")), "{tag}: attempt");
    assert_eq!(kv.u8_of("rb"), Some(1), "{tag}: rb");
    assert_eq!(kv.u32_of("rb_id"), Some(5), "{tag}: rb_id");
    assert_eq!(kv.u8_of("ddi_stage"), Some(2), "{tag}: stage");
    assert_eq!(kv.u32_of("ddi_code"), Some(1004), "{tag}: code");
    assert_eq!(
        kv.str_of("ddi_det0").as_deref(),
        Some("rolled_back"),
        "{tag}: detail"
    );
    assert_eq!(
        kv.str_of("ddi_det1").as_deref(),
        Some("health_deadline"),
        "{tag}: reason"
    );
}

// ---- verdict folds -----------------------------------------------------

#[test]
fn completing_a_bound_attempt_persists_success_before_the_clear() {
    let kv = FaultKv::new();
    bound_attempt(&kv);
    kv.reset_counters();
    let out = reconcile_boot(&mut store(&kv), Ok(new_image_valid()), Some(V_NEW));
    assert_eq!(out.action, Some(ReconcileAction::CompleteAttempt));
    assert!(out.admission_open_at_boot);
    assert_eq!(
        ddi_ops(&kv),
        ["set:ddi_code", "set:ddi_stage", "rm:att_ver"],
        "the verdict lands before the attempt is hidden"
    );
    assert_success_pending(&kv, "once");
}

#[test]
fn rolling_back_a_bound_attempt_persists_1004_with_the_reason() {
    let kv = FaultKv::new();
    bound_attempt(&kv);
    kv.put_str("att_why", "health_deadline");
    kv.reset_counters();
    let out = reconcile_boot(&mut store(&kv), Ok(fell_back()), Some(V_OLD));
    assert_eq!(out.disposition, BootDisposition::ReportedRollback);
    assert_eq!(
        ddi_ops(&kv),
        [
            "set:rb_why",
            "set:rb",
            "set:rb_id",
            "set:ddi_code",
            "set:ddi_det0",
            "set:ddi_det1",
            "set:ddi_stage",
            "rm:att_ver",
        ],
        "report, then verdict, then the clear"
    );
    assert_rollback_pending(&kv, "once");
    assert_eq!(kv.str_of("rej_ver").as_deref(), Some("2.0.0"));
}

#[test]
fn the_success_verdict_survives_a_fault_at_every_step() {
    each_fault(
        &bound_attempt,
        &|s| {
            let _ = reconcile_boot(s, Ok(new_image_valid()), Some(V_NEW));
        },
        |kv, fault, n| {
            for _ in 0..2 {
                reconcile_boot(&mut store(kv), Ok(new_image_valid()), Some(V_NEW));
            }
            assert_success_pending(kv, &format!("{fault:?} at {n}"));
        },
    );
}

#[test]
fn the_rollback_verdict_survives_a_fault_at_every_step() {
    let setup = |kv: &FaultKv| {
        bound_attempt(kv);
        kv.put_str("att_why", "health_deadline");
    };
    each_fault(
        &setup,
        &|s| {
            let _ = reconcile_boot(s, Ok(fell_back()), Some(V_OLD));
        },
        |kv, fault, n| {
            for _ in 0..2 {
                reconcile_boot(&mut store(kv), Ok(fell_back()), Some(V_OLD));
            }
            assert_rollback_pending(kv, &format!("{fault:?} at {n}"));
        },
    );
}

#[test]
fn an_attempt_of_another_id_writes_no_verdict() {
    let kv = FaultKv::new();
    bound_attempt(&kv);
    kv.put_u32("ddi_att", 4);
    reconcile_boot(&mut store(&kv), Ok(new_image_valid()), Some(V_NEW));
    assert!(!kv.has("att_ver"));
    assert_eq!(kv.u8_of("ddi_stage"), Some(1));
    assert!(!kv.has("ddi_code"));
}

#[test]
fn an_interrupted_download_leaves_the_action_open() {
    let kv = FaultKv::new();
    put_counter(&kv, 5);
    put_attempt(&kv, 5, "2.0.0", "ota_1", false, false);
    put_ddi(&kv, "42", 1, Some(5), None);
    let facts = hw(SLOT_A, SlotState::Valid, Some((SLOT_B, SlotState::Unknown)));
    let out = reconcile_boot(&mut store(&kv), Ok(facts), Some(V_OLD));
    assert_eq!(out.action, Some(ReconcileAction::ClearAttempt));
    assert!(!kv.has("att_ver"));
    assert_eq!(kv.u8_of("ddi_stage"), Some(1));
    assert!(!kv.has("ddi_code"));
}

#[test]
fn a_verdict_already_pending_is_not_rewritten() {
    let kv = FaultKv::new();
    bound_attempt(&kv);
    kv.put_u8("ddi_stage", 2).put_u32("ddi_code", 0);
    kv.reset_counters();
    reconcile_boot(&mut store(&kv), Ok(new_image_valid()), Some(V_NEW));
    assert!(!kv.ops().iter().any(|op| op.contains(":ddi_")));
    assert_success_pending(&kv, "kept");
}

// ---- corruption ------------------------------------------------------------

/// A record that cannot be read, with the error every read of it returns.
struct Bad {
    name: &'static str,
    setup: fn(&FaultKv),
    error: StoreError,
}

fn mismatch() -> StoreError {
    StoreError::Kv(KvError::new(KvError::CODE_TYPE_MISMATCH))
}

fn bad_records() -> [Bad; 15] {
    let act = StoreError::Corrupt(CorruptRecord::DdiActionInvalid);
    let stage = StoreError::Corrupt(CorruptRecord::DdiStageInvalid);
    let verdict = StoreError::Corrupt(CorruptRecord::DdiVerdictInvalid);
    [
        Bad {
            name: "leading zero",
            setup: |kv| put_ddi(kv, "042", 3, None, Some(0)),
            error: act,
        },
        Bad {
            name: "twenty digits",
            setup: |kv| put_ddi(kv, "12345678901234567890", 3, None, Some(0)),
            error: act,
        },
        Bad {
            name: "above i64",
            setup: |kv| put_ddi(kv, "9223372036854775808", 3, None, Some(0)),
            error: act,
        },
        Bad {
            name: "empty id",
            setup: |kv| put_ddi(kv, "", 3, None, Some(0)),
            error: act,
        },
        Bad {
            name: "not digits",
            setup: |kv| put_ddi(kv, "4a", 1, None, None),
            error: act,
        },
        Bad {
            name: "non-canonical id without a stage",
            setup: |kv| {
                kv.put_str("ddi_act", "007");
            },
            error: act,
        },
        Bad {
            name: "stage without id",
            setup: |kv| {
                kv.put_u8("ddi_stage", 1);
            },
            error: act,
        },
        Bad {
            name: "id of another type",
            setup: |kv| {
                kv.put_u32("ddi_act", 42).put_u8("ddi_stage", 3);
            },
            error: mismatch(),
        },
        Bad {
            name: "stage 0",
            setup: |kv| put_ddi(kv, "42", 0, None, None),
            error: stage,
        },
        Bad {
            name: "stage 4",
            setup: |kv| put_ddi(kv, "42", 4, None, None),
            error: stage,
        },
        Bad {
            name: "stage of another type",
            setup: |kv| {
                kv.put_str("ddi_act", "42").put_str("ddi_stage", "2");
            },
            error: mismatch(),
        },
        Bad {
            name: "pending verdict without code",
            setup: |kv| put_ddi(kv, "42", 2, None, None),
            error: verdict,
        },
        Bad {
            name: "code 1008",
            setup: |kv| put_ddi(kv, "42", 2, None, Some(1008)),
            error: verdict,
        },
        Bad {
            name: "code 1 on a running action",
            setup: |kv| put_ddi(kv, "42", 1, Some(5), Some(1)),
            error: verdict,
        },
        Bad {
            name: "attempt id of another type",
            setup: |kv| {
                put_ddi(kv, "42", 1, None, None);
                kv.put_str("ddi_att", "5");
            },
            error: mismatch(),
        },
    ]
}

#[test]
fn an_unreadable_record_fails_the_boot_closed_without_a_write() {
    for bad in bad_records() {
        let kv = FaultKv::new();
        (bad.setup)(&kv);
        kv.reset_counters();
        let out = reconcile_boot(&mut store(&kv), Ok(new_image_valid()), Some(V_NEW));
        assert_eq!(
            out.disposition,
            BootDisposition::FailedClosed(BootFault::Store(bad.error)),
            "{}",
            bad.name
        );
        assert!(!out.admission_open_at_boot, "{}", bad.name);
        assert_eq!(kv.commits(), 0, "{}", bad.name);
        assert_eq!(store(&kv).corrupt_present(), Ok(true), "{}", bad.name);
    }
}

#[test]
fn an_unreadable_record_keeps_a_bound_attempt() {
    for bad in bad_records() {
        let kv = FaultKv::new();
        put_counter(&kv, 5);
        put_attempt(&kv, 5, "2.0.0", "ota_1", true, true);
        (bad.setup)(&kv);
        kv.reset_counters();
        assert_eq!(
            store(&kv).complete_attempt(V_NEW),
            Err(bad.error),
            "{}",
            bad.name
        );
        assert_eq!(kv.commits(), 0, "{}", bad.name);
    }
}

#[test]
fn repair_removes_an_unreadable_record_stage_first() {
    for bad in bad_records() {
        let kv = FaultKv::new();
        (bad.setup)(&kv);
        kv.put_str("ddi_det0", "up_to_date");
        let mut s = store(&kv);
        kv.reset_counters();
        assert_eq!(
            s.repair_corrupt(),
            Ok(Some(RepairedRecord::DdiAction)),
            "{}",
            bad.name
        );
        let ops = kv.ops();
        if kv.log().iter().any(|e| e.key == "ddi_stage") {
            assert_eq!(ops[0], "rm:ddi_stage", "{}: stage first", bad.name);
        }
        assert!(DDI_KEYS.iter().all(|key| !kv.has(key)), "{}", bad.name);
        assert_eq!(s.repair_corrupt(), Ok(None), "{}", bad.name);
        assert_eq!(s.corrupt_present(), Ok(false), "{}", bad.name);
    }
}

#[test]
fn repair_of_the_record_finishes_after_a_fault_at_every_step() {
    let setup = |kv: &FaultKv| {
        put_ddi(kv, "042", 2, Some(5), Some(1002));
        kv.put_str("ddi_det0", "checksum_mismatch")
            .put_str("ddi_det1", "x");
    };
    each_fault(
        &setup,
        &|s| {
            let _ = s.repair_corrupt();
        },
        |kv, fault, n| {
            let mut s = store(kv);
            for _ in 0..3 {
                if s.repair_corrupt() == Ok(None) {
                    break;
                }
            }
            let tag = format!("{fault:?} at {n}");
            assert_eq!(s.corrupt_present(), Ok(false), "{tag}");
            assert!(!kv.has("ddi_stage"), "{tag}: no record survives");
        },
    );
}

#[test]
fn unreadable_details_are_never_a_fault() {
    let kv = FaultKv::new();
    put_ddi(&kv, "42", 2, None, Some(1002));
    kv.put_u32("ddi_det0", 7);
    kv.corrupt_with("ddi_det1", KvError::CODE_INVALID_VALUE);
    assert_eq!(store(&kv).corrupt_present(), Ok(false));
    let out = reconcile_boot(&mut store(&kv), Ok(new_image_valid()), Some(V_NEW));
    assert!(out.admission_open_at_boot);
}

#[test]
fn an_injected_invalid_value_is_repaired() {
    let kv = FaultKv::new();
    put_ddi(&kv, "42", 3, None, Some(0));
    kv.corrupt_with("ddi_act", KvError::CODE_INVALID_VALUE);
    let mut s = store(&kv);
    assert_eq!(s.corrupt_present(), Ok(true));
    assert_eq!(s.repair_corrupt(), Ok(Some(RepairedRecord::DdiAction)));
    assert_eq!(s.repair_corrupt(), Ok(None));
}

// ---- id floor ----------------------------------------------------------------

#[test]
fn a_bound_attempt_id_is_never_reissued() {
    let kv = FaultKv::new();
    put_counter(&kv, 3);
    put_ddi(&kv, "42", 3, Some(9), Some(0));
    assert_eq!(store(&kv).next_attempt_id(), Ok(10));
}

#[test]
fn a_repaired_counter_starts_above_the_bound_attempt_id() {
    let kv = FaultKv::new();
    kv.put_u32("att_epoch", EPOCH).put_str("att_ctr", "x");
    put_ddi(&kv, "42", 3, Some(9), Some(0));
    let mut s = store(&kv);
    assert_eq!(s.repair_corrupt(), Ok(Some(RepairedRecord::Counter)));
    assert_eq!(kv.u32_of("att_ctr"), Some(9));
    assert_eq!(s.next_attempt_id(), Ok(10));
}

#[test]
fn an_unreadable_record_is_repaired_before_the_counter() {
    let kv = FaultKv::new();
    kv.put_u32("att_epoch", EPOCH).put_str("att_ctr", "x");
    put_ddi(&kv, "42", 3, None, Some(0));
    kv.put_str("ddi_att", "9");
    let mut s = store(&kv);
    assert_eq!(s.repair_corrupt(), Ok(Some(RepairedRecord::DdiAction)));
    assert_eq!(s.repair_corrupt(), Ok(Some(RepairedRecord::Counter)));
    assert_eq!(s.repair_corrupt(), Ok(None));
    assert_eq!(s.next_attempt_id(), Ok(1));
}

#[test]
fn a_closed_record_of_another_attempt_is_left_alone() {
    let kv = FaultKv::new();
    put_counter(&kv, 5);
    put_attempt(&kv, 5, "2.0.0", "ota_1", true, true);
    put_ddi(&kv, "7", 3, Some(2), Some(1007));
    store(&kv)
        .complete_attempt(Version::new(2, 0, 0))
        .expect("completes");
    assert!(!kv.has("att_ver"));
    assert_eq!(kv.u8_of("ddi_stage"), Some(3));
}
