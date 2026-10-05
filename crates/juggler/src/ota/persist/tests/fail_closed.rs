//! A read error or a corrupt record means: no write of any kind, admission closed, never mark valid.

use super::support::*;
use crate::ota::persist::fault_kv::FaultKv;
use crate::ota::persist::SettleOutcome;
use crate::ota::persist::{
    reconcile_boot, BootDisposition, BootFault, CorruptRecord, HardwareReadError, KvError,
    StoreError,
};
use crate::ota::{Admission, BlockedBy, SlotState};

/// Reads of phase R for `busy_world`: attempt 8, request 3, report 3, refusal 1.
const PHASE_R_READS: usize = 17;

fn busy_world(kv: &FaultKv) {
    put_counter(kv, 5);
    put_attempt(kv, 5, "2.0.0", "ota_1", true, true);
    kv.put_str("rej_ver", "1.5.0")
        .put_u8("rb", 0)
        .put_u32("rb_id", 2);
    put_request(kv, 9, "operator", "ota_0");
}

fn reads_in_one_boot() -> usize {
    let kv = FaultKv::new();
    busy_world(&kv);
    let mut s = store(&kv);
    kv.reset_counters();
    let _ = reconcile_boot(&mut s, Ok(hw(SLOT_A, SlotState::Valid, None)), Some(V_OLD));
    kv.reads()
}

#[test]
fn a_read_error_at_any_point_of_phase_r_writes_nothing() {
    let total = reads_in_one_boot();
    let mut fail_closed = 0;
    for n in 1..=total {
        let kv = FaultKv::new();
        busy_world(&kv);
        let mut s = store(&kv);
        kv.reset_counters();
        kv.fail_read_at(n);
        let out = reconcile_boot(&mut s, Ok(hw(SLOT_A, SlotState::Valid, None)), Some(V_OLD));
        if matches!(
            out.disposition,
            BootDisposition::FailedClosed(BootFault::Store(_))
        ) {
            fail_closed += 1;
            assert_eq!(kv.commits(), 0, "read {n}: no write");
            assert!(!out.admission_open_at_boot && out.refuse_mark_valid);
        }
    }
    // every read of phase R fails closed except the single soft read of the refusal
    assert!(fail_closed >= PHASE_R_READS - 1);
    assert!(total > PHASE_R_READS);
}

#[test]
fn a_failed_read_blocks_admission() {
    let kv = FaultKv::new();
    for n in 1..=2 {
        kv.fail_read_at(n);
        assert_eq!(
            store(&kv).admission(),
            Admission::Blocked(BlockedBy::AttemptUnresolved)
        );
        kv.revive();
        kv.reset_counters();
    }
    assert_eq!(store(&kv).admission(), Admission::Open);
}

#[test]
fn unknown_slot_labels_are_corrupt_and_fail_closed() {
    let kv = FaultKv::new();
    put_attempt(&kv, 5, "2.0.0", "ota_7", true, true);
    let mut s = store(&kv);
    let out = reconcile_boot(&mut s, Ok(hw(SLOT_A, SlotState::Valid, None)), Some(V_OLD));
    assert_eq!(
        out.disposition,
        BootDisposition::FailedClosed(BootFault::Store(StoreError::Corrupt(
            CorruptRecord::UnknownAttemptSlotLabel
        )))
    );
    assert_eq!(kv.commits(), 0);
    assert!(!out.admission_open_at_boot && out.refuse_mark_valid);

    let kv = FaultKv::new();
    put_request(&kv, 5, "operator", "bogus");
    let mut s = store(&kv);
    let out = reconcile_boot(&mut s, Ok(hw(SLOT_A, SlotState::Valid, None)), Some(V_OLD));
    assert_eq!(
        out.disposition,
        BootDisposition::FailedClosed(BootFault::Store(StoreError::Corrupt(
            CorruptRecord::UnknownRequestSlotLabel
        )))
    );
    assert_eq!(kv.commits(), 0);
}

#[test]
fn factory_as_attempt_slot_is_corrupt_but_factory_request_is_fine() {
    let kv = FaultKv::new();
    put_attempt(&kv, 5, "2.0.0", "factory", true, true);
    assert!(store(&kv).read_attempt().is_err());

    let kv = FaultKv::new();
    put_request(&kv, 5, "operator", "factory");
    assert!(store(&kv).request().is_ok());
}

#[test]
fn repair_corrupt_ends_the_wedge() {
    let kv = FaultKv::new();
    put_attempt(&kv, 5, "2.0.0", "ota_7", true, true);
    let mut s = store(&kv);
    assert!(s.repair_corrupt().unwrap().is_some());
    let out = reconcile_boot(&mut s, Ok(hw(SLOT_A, SlotState::Valid, None)), Some(V_OLD));
    assert_eq!(
        out.disposition,
        BootDisposition::Settled(SettleOutcome::NoRequest)
    );
    assert!(out.admission_open_at_boot);
}

#[test]
fn a_mistyped_key_is_an_error_not_an_absent_key() {
    let kv = FaultKv::new();
    kv.put_str("att_ver", "2.0.0").put_u8("att_slot", 1);
    let mut s = store(&kv);
    let out = reconcile_boot(&mut s, Ok(hw(SLOT_A, SlotState::Valid, None)), Some(V_OLD));
    assert!(matches!(
        out.disposition,
        BootDisposition::FailedClosed(BootFault::Store(StoreError::Kv(_)))
    ));
    assert_eq!(kv.commits(), 0);
}

#[test]
fn hardware_error_writes_nothing_and_unreadable_refusal_does_not_block() {
    let kv = FaultKv::new();
    busy_world(&kv);
    let mut s = store(&kv);
    let out = reconcile_boot(&mut s, Err(HardwareReadError::Busy), Some(V_OLD));
    assert_eq!(kv.commits(), 0);
    assert!(out.refuse_mark_valid);

    let kv = FaultKv::new();
    kv.corrupt("rej_ver");
    let mut s = store(&kv);
    let out = reconcile_boot(&mut s, Ok(hw(SLOT_A, SlotState::Valid, None)), Some(V_OLD));
    assert_eq!(
        out.disposition,
        BootDisposition::Settled(SettleOutcome::NoRequest)
    );
    assert!(out.admission_open_at_boot);
}

#[test]
fn an_unreadable_refusal_does_not_wedge_completing_a_valid_attempt() {
    let kv = FaultKv::new();
    put_counter(&kv, 5);
    put_attempt(&kv, 5, "2.0.0", "ota_1", true, true);
    kv.put_str("rej_ver", "1.5.0");
    kv.corrupt_with("rej_ver", KvError::CODE_TYPE_MISMATCH);
    let mut s = store(&kv);
    let out = reconcile_boot(&mut s, Ok(hw(SLOT_B, SlotState::Valid, None)), Some(V_NEW));
    assert_eq!(
        out.disposition,
        BootDisposition::Settled(SettleOutcome::NoRequest)
    );
    assert!(out.admission_open_at_boot);
    assert!(!kv.has("rej_ver") && !kv.has("att_ver"));
}

#[test]
fn a_transient_refusal_read_error_keeps_the_attempt_and_writes_nothing_when_completing() {
    let kv = FaultKv::new();
    put_counter(&kv, 5);
    put_attempt(&kv, 5, "2.0.0", "ota_1", true, true);
    kv.put_str("rej_ver", "1.5.0");
    kv.corrupt("rej_ver");
    kv.reset_counters();
    let mut s = store(&kv);
    assert!(matches!(s.complete_attempt(V_NEW), Err(StoreError::Kv(_))));
    assert!(kv.ops().is_empty(), "{:?}", kv.ops());
    assert!(kv.has("att_ver") && kv.has("rej_ver"));

    let out = reconcile_boot(&mut s, Ok(hw(SLOT_B, SlotState::Valid, None)), Some(V_NEW));
    assert!(matches!(out.disposition, BootDisposition::Incomplete(_)));
    assert!(!out.admission_open_at_boot);
    assert!(kv.ops().is_empty(), "{:?}", kv.ops());
    assert!(kv.has("att_ver") && kv.has("rej_ver"));
}
