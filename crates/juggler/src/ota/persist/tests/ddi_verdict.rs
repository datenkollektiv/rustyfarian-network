//! The typed DDI action API: the loop guard, the offer gate, persisted verdicts and their delivery, with a fault at every write step.

use alloc::format;
use alloc::string::String;

use super::support::*;
use crate::ota::ddi::wire::{ActionId, Finished, Href, PollResponse, ProfileCode};
use crate::ota::ddi::{AdmittedOffer, CancelTarget, OfferGate, Refusal};
use crate::ota::persist::fault_kv::{FaultKv, INJECTED};
use crate::ota::persist::{
    reconcile_boot, CorruptRecord, DdiVerdict, FailedDownload, HardwareFacts, KvError, OtaStore,
    StoreError, UpdateSlot, VerdictDelivery, VerdictNote, VerdictResult, FACTORY_SLOT,
};
use crate::ota::wire::FailReason;
use crate::ota::{BlockedBy, OtaError, SlotState};

fn id(text: &str) -> ActionId {
    ActionId::parse(text).expect("canonical id")
}

fn link() -> Href {
    PollResponse::parse(br#"{"_links":{"deploymentBase":{"href":"http://h/fw.bin"}}}"#)
        .expect("poll")
        .deployment_base
        .expect("link")
}

fn offer(action: &str, success_detail: Option<FailReason>) -> AdmittedOffer {
    AdmittedOffer {
        action: id(action),
        version: V_NEW,
        artifact: link(),
        sha256: [0; 32],
        size: 1234,
        success_detail,
    }
}

fn refusal(action: &str) -> DdiVerdict {
    DdiVerdict::refused(&Refusal {
        action: id(action),
        code: ProfileCode::Policy,
        reason: FailReason::UpToDate,
        url_case: None,
    })
}

fn begin(s: &mut OtaStore<FaultKv>, action: &str) -> Result<u32, StoreError> {
    s.begin_ddi_attempt(&offer(action, None), SLOT_B, false)
}

fn new_image_valid() -> HardwareFacts {
    hw(SLOT_B, SlotState::Valid, None)
}

fn interrupted() -> HardwareFacts {
    hw(SLOT_A, SlotState::Valid, Some((SLOT_B, SlotState::Unknown)))
}

/// Action `7` closed: its success verdict was delivered.
fn closed_seven(kv: &FaultKv) {
    put_counter(kv, 4);
    put_ddi(kv, "7", 3, Some(4), Some(0));
}

fn reboot(kv: &FaultKv) -> FaultKv {
    FaultKv::from_map(kv.snapshot())
}

// ---- begin -------------------------------------------------------------------

#[test]
fn begin_binds_the_attempt_and_writes_the_stage_last() {
    let kv = FaultKv::new();
    closed_seven(&kv);
    kv.put_str("ddi_det1", "stale");
    let mut s = store(&kv);
    kv.reset_counters();
    let got = s.begin_ddi_attempt(
        &offer("42", Some(FailReason::PreviouslyRolledBack)),
        SLOT_B,
        false,
    );
    assert_eq!(got, Ok(5));
    let ops = kv.ops();
    assert_eq!(ops.last().map(String::as_str), Some("set:ddi_stage"));
    let pos = |op: &str| ops.iter().position(|o| o == op).expect(op);
    assert!(
        ops[..pos("rm:ddi_stage")]
            .iter()
            .all(|o| !o.contains(":ddi_")),
        "the attempt (and every read) comes before the first DDI key: {ops:?}"
    );
    assert!(pos("set:att_slot") < pos("rm:ddi_stage"));
    assert!(pos("set:ddi_act") < pos("set:ddi_att"));
    assert_eq!(kv.u8_of("ddi_stage"), Some(1));
    assert_eq!(kv.str_of("ddi_act").as_deref(), Some("42"));
    assert_eq!(kv.u32_of("ddi_att"), Some(5));
    assert_eq!(
        kv.str_of("ddi_det0").as_deref(),
        Some("previously_rolled_back")
    );
    assert!(!kv.has("ddi_code") && !kv.has("ddi_det1"));
    assert_eq!(s.is_action_closed(&id("42")), Ok(false));
    assert_eq!(s.offer_gate(), OfferGate::Blocked);
}

/// A world in which `begin_ddi_attempt` for `action` must fail with `error`.
struct Refused {
    name: &'static str,
    setup: fn(&FaultKv),
    action: &'static str,
    error: StoreError,
}

#[test]
fn begin_refuses_without_a_write() {
    let cases = [
        Refused {
            name: "closed action",
            setup: closed_seven,
            action: "7",
            error: StoreError::ActionClosed,
        },
        Refused {
            name: "pending verdict",
            setup: |kv| put_ddi(kv, "6", 2, None, Some(1007)),
            action: "42",
            error: StoreError::VerdictPending,
        },
        Refused {
            name: "attempt",
            setup: |kv| put_attempt(kv, 5, "2.0.0", "ota_1", false, false),
            action: "42",
            error: StoreError::NotAdmitted(BlockedBy::AttemptUnresolved),
        },
        Refused {
            name: "corrupt record",
            setup: |kv| put_ddi(kv, "07", 3, None, Some(0)),
            action: "42",
            error: StoreError::Corrupt(CorruptRecord::DdiActionInvalid),
        },
    ];
    for case in cases {
        let kv = FaultKv::new();
        (case.setup)(&kv);
        kv.reset_counters();
        assert_eq!(
            begin(&mut store(&kv), case.action),
            Err(case.error),
            "{}",
            case.name
        );
        assert_eq!(kv.commits(), 0, "{}", case.name);
    }
    let kv = FaultKv::new();
    assert_eq!(
        store(&kv).begin_ddi_attempt(&offer("42", None), FACTORY_SLOT, false),
        Err(StoreError::InvalidSlot)
    );
    assert_eq!(kv.commits(), 0);
}

#[test]
fn begin_leaves_either_no_record_or_a_bound_one_after_a_fault() {
    each_fault(
        &closed_seven,
        &|s| {
            let _ = s.begin_ddi_attempt(&offer("42", Some(FailReason::Downgrade)), SLOT_B, false);
        },
        |kv, fault, n| {
            let tag = format!("{fault:?} at {n}");
            let s = store(kv);
            assert_eq!(s.corrupt_present(), Ok(false), "{tag}");
            match kv.u8_of("ddi_stage") {
                None => {}
                Some(1) => {
                    assert_eq!(kv.str_of("ddi_act").as_deref(), Some("42"), "{tag}");
                    assert_eq!(kv.u32_of("ddi_att"), kv.u32_of("att_id"), "{tag}");
                    assert!(kv.has("att_slot"), "{tag}: bound to a live attempt");
                    assert_eq!(kv.str_of("ddi_det0").as_deref(), Some("downgrade"), "{tag}");
                }
                Some(3) => {
                    assert_eq!(kv.str_of("ddi_act").as_deref(), Some("7"), "{tag}");
                    assert_ne!(
                        kv.u32_of("ddi_att"),
                        kv.u32_of("att_id"),
                        "{tag}: an attempt written before the fault is not bound to the closed action"
                    );
                }
                other => panic!("{tag}: stage {other:?}"),
            }
            // The interrupted begin never booted: the next boot clears any attempt and the action can begin again.
            reconcile_boot(&mut store(kv), Ok(interrupted()), Some(V_OLD));
            assert_eq!(s.is_action_closed(&id("42")), Ok(false), "{tag}");
            assert!(begin(&mut store(kv), "42").is_ok(), "{tag}");
        },
    );
}

// ---- the loop guard and the gate --------------------------------------------------

#[test]
fn a_closed_action_is_never_applied_again_after_reboot() {
    let kv = FaultKv::new();
    let mut s = store(&kv);
    begin(&mut s, "42").expect("begins");
    s.mark_boot_selected().expect("selected");
    let kv = reboot(&kv);
    reconcile_boot(&mut store(&kv), Ok(new_image_valid()), Some(V_NEW));
    let mut s = store(&kv);
    assert_eq!(s.is_action_closed(&id("42")), Ok(true));
    let pending = s.pending_verdict().expect("readable").expect("pending");
    assert_eq!(
        s.mark_verdict_delivered(&pending.action),
        Ok(VerdictDelivery::Marked)
    );
    let kv = reboot(&kv);
    let mut s = store(&kv);
    assert_eq!(s.is_action_closed(&id("42")), Ok(true));
    assert_eq!(s.offer_gate(), OfferGate::Open);
    assert_eq!(begin(&mut s, "42"), Err(StoreError::ActionClosed));
    assert_eq!(s.is_action_closed(&id("43")), Ok(false));
    assert_eq!(begin(&mut s, "43"), Ok(2));
}

#[test]
fn the_offer_gate_follows_the_records() {
    let kv = FaultKv::new();
    let s = store(&kv);
    assert_eq!(s.offer_gate(), OfferGate::Open);
    put_ddi(&kv, "42", 2, None, Some(1007));
    assert_eq!(s.offer_gate(), OfferGate::Blocked);
    kv.put_u8("ddi_stage", 3);
    assert_eq!(s.offer_gate(), OfferGate::Open);
    kv.put_u8("ddi_stage", 1);
    assert_eq!(s.offer_gate(), OfferGate::Open, "an action without attempt");
    kv.put_u8("rb", 1).put_u32("rb_id", 3);
    assert_eq!(s.offer_gate(), OfferGate::Blocked, "an undelivered report");
    kv.put_u8("rb", 0);
    put_attempt(&kv, 5, "2.0.0", "ota_1", false, false);
    assert_eq!(s.offer_gate(), OfferGate::Blocked, "an attempt");
    kv.put_str("ddi_act", "042");
    assert_eq!(s.offer_gate(), OfferGate::RecordsUnreadable);
    kv.put_str("ddi_act", "42").put_str("att_slot", "ota_7");
    assert_eq!(s.offer_gate(), OfferGate::RecordsUnreadable);
}

#[test]
fn a_transient_read_error_blocks_the_gate() {
    let kv = FaultKv::new();
    put_ddi(&kv, "42", 3, None, Some(0));
    let reads = {
        kv.reset_counters();
        store(&kv).offer_gate();
        kv.reads()
    };
    for n in 1..=reads {
        kv.reset_counters();
        kv.fail_read_at(n);
        assert_eq!(store(&kv).offer_gate(), OfferGate::Blocked, "read {n}");
        kv.revive();
    }
}

// ---- the running action and the cancel look-up -------------------------------------

#[test]
fn a_begun_attempt_is_the_running_action() {
    let kv = FaultKv::new();
    let mut s = store(&kv);
    begin(&mut s, "42").expect("begins");
    assert_eq!(s.running_ddi_action(), Ok(Some(id("42"))));
    assert_eq!(
        s.settle_failed_ddi_download(
            interrupted(),
            Some(V_OLD),
            FailReason::Ota(OtaError::Cancelled)
        ),
        Ok(FailedDownload::Cleared)
    );
    assert_eq!(kv.u8_of("ddi_stage"), Some(1));
    assert_eq!(s.running_ddi_action(), Ok(None), "a dangling ddi_att");
}

#[test]
fn a_record_without_its_attempt_is_not_running() {
    let kv = FaultKv::new();
    let s = store(&kv);
    assert_eq!(s.running_ddi_action(), Ok(None), "no record");
    put_ddi(&kv, "42", 1, Some(5), None);
    assert_eq!(s.running_ddi_action(), Ok(None), "no attempt");
    put_attempt(&kv, 6, "2.0.0", "ota_1", false, false);
    assert_eq!(s.running_ddi_action(), Ok(None), "another attempt");
    put_attempt(&kv, 5, "2.0.0", "ota_1", false, false);
    assert_eq!(s.running_ddi_action(), Ok(Some(id("42"))));
    put_ddi(&kv, "42", 2, Some(5), Some(0));
    assert_eq!(s.running_ddi_action(), Ok(None), "a verdict is pending");
    put_ddi(&kv, "42", 3, Some(5), Some(0));
    assert_eq!(s.running_ddi_action(), Ok(None), "the action is closed");
}

#[test]
fn a_corrupt_record_fails_the_running_lookup_closed() {
    let kv = FaultKv::new();
    put_attempt(&kv, 5, "2.0.0", "ota_1", false, false);
    put_ddi(&kv, "042", 1, Some(5), None);
    assert_eq!(
        store(&kv).running_ddi_action(),
        Err(StoreError::Corrupt(CorruptRecord::DdiActionInvalid))
    );
}

#[test]
fn cancel_target_by_what_the_device_knows() {
    let kv = FaultKv::new();
    let s = store(&kv);
    assert_eq!(
        s.cancel_target(&id("42"), Some(&id("42"))),
        CancelTarget::Downloading,
        "even on an empty store"
    );
    assert_eq!(s.cancel_target(&id("42"), None), CancelTarget::NotRunning);
    assert_eq!(
        s.cancel_target(&id("42"), Some(&id("7"))),
        CancelTarget::NotRunning
    );

    let kv = FaultKv::new();
    closed_seven(&kv);
    let mut s = store(&kv);
    s.persist_verdict(&refusal("42")).expect("persists");
    assert_eq!(
        s.cancel_target(&id("42"), None),
        CancelTarget::VerdictPending
    );
    assert_eq!(s.cancel_target(&id("43"), None), CancelTarget::NotRunning);

    let kv = FaultKv::new();
    let mut s = store(&kv);
    begin(&mut s, "42").expect("begins");
    assert_eq!(s.cancel_target(&id("42"), None), CancelTarget::Activated);
    assert_eq!(
        s.cancel_target(&id("42"), Some(&id("7"))),
        CancelTarget::Activated
    );
    assert_eq!(s.cancel_target(&id("43"), None), CancelTarget::NotRunning);

    let kv = FaultKv::new();
    put_attempt(&kv, 5, "2.0.0", "ota_1", false, false);
    put_ddi(&kv, "42", 2, Some(5), Some(0));
    let s = store(&kv);
    assert_eq!(s.pending_verdict(), Ok(None), "the attempt still exists");
    assert_eq!(
        s.cancel_target(&id("42"), None),
        CancelTarget::Activated,
        "the verdict is written, the attempt is not cleared"
    );
    assert_eq!(s.cancel_target(&id("43"), None), CancelTarget::NotRunning);
    put_ddi(&kv, "42", 2, Some(6), Some(0));
    assert_eq!(
        s.cancel_target(&id("42"), None),
        CancelTarget::NotRunning,
        "a verdict bound to another attempt"
    );

    let kv = FaultKv::new();
    kv.put_str("ddi_act", "042");
    assert_eq!(
        store(&kv).cancel_target(&id("42"), None),
        CancelTarget::Unreadable
    );
}

#[test]
fn a_cancel_never_reads_as_nothing_ran_while_the_attempt_is_unresolved() {
    let setup = |kv: &FaultKv| {
        begin(&mut store(kv), "42").expect("begins");
    };
    let mut verdict_without_clear = 0;
    each_fault(
        &setup,
        &|s| {
            let _ = s.complete_attempt(V_NEW);
        },
        |kv, fault, n| {
            let tag = format!("{fault:?} at {n}");
            let s = store(kv);
            let target = s.cancel_target(&id("42"), None);
            assert_ne!(target, CancelTarget::NotRunning, "{tag}");
            if kv.u8_of("ddi_stage") == Some(2) && kv.has("att_ver") {
                verdict_without_clear += 1;
                assert_eq!(target, CancelTarget::Activated, "{tag}");
            }
        },
    );
    assert!(
        verdict_without_clear > 0,
        "the scenario reaches a written verdict with its attempt left"
    );
}

#[test]
fn a_transient_read_error_makes_the_cancel_target_unreadable() {
    let kv = FaultKv::new();
    begin(&mut store(&kv), "42").expect("begins");
    let reads = {
        kv.reset_counters();
        store(&kv).cancel_target(&id("42"), None);
        kv.reads()
    };
    assert!(reads > 0);
    for n in 1..=reads {
        kv.reset_counters();
        kv.fail_read_at(n);
        assert_eq!(
            store(&kv).cancel_target(&id("42"), None),
            CancelTarget::Unreadable,
            "read {n}"
        );
        kv.revive();
    }
}

// ---- persisted verdicts -------------------------------------------------------------

#[test]
fn a_refusal_is_persisted_and_survives_a_reboot_before_the_send() {
    let kv = FaultKv::new();
    closed_seven(&kv);
    let verdict = refusal("42");
    let mut s = store(&kv);
    kv.reset_counters();
    assert_eq!(s.persist_verdict(&verdict), Ok(VerdictNote::Written));
    let ops = kv.ops();
    assert_eq!(ops.first().map(String::as_str), Some("rm:ddi_stage"));
    assert_eq!(ops.last().map(String::as_str), Some("set:ddi_stage"));
    let kv = reboot(&kv);
    let s = store(&kv);
    let pending = s.pending_verdict().expect("readable").expect("pending");
    assert_eq!(pending, verdict);
    assert_eq!(pending.finished(), Finished::Failure);
    assert_eq!(pending.code(), Some(ProfileCode::Policy));
    assert_eq!(pending.detail_strs().as_slice(), ["up_to_date"]);
    assert!(!kv.has("ddi_att"), "a refusal has no attempt");
    assert_eq!(s.is_action_closed(&id("42")), Ok(true));
    assert_eq!(s.offer_gate(), OfferGate::Blocked);
}

#[test]
fn a_pending_verdict_wins_over_a_second_one() {
    let kv = FaultKv::new();
    let mut s = store(&kv);
    let first = DdiVerdict::records_unreadable(id("42"));
    assert_eq!(s.persist_verdict(&first), Ok(VerdictNote::Written));
    kv.reset_counters();
    assert_eq!(
        s.persist_verdict(&refusal("43")),
        Err(StoreError::VerdictPending)
    );
    assert_eq!(
        s.persist_verdict(&refusal("42")),
        Ok(VerdictNote::KeptPending)
    );
    assert_eq!(kv.commits(), 0);
    assert_eq!(s.pending_verdict(), Ok(Some(first)));
    assert_eq!(
        s.mark_verdict_delivered(&id("43")),
        Ok(VerdictDelivery::OtherAction)
    );
    assert_eq!(
        s.mark_verdict_delivered(&id("42")),
        Ok(VerdictDelivery::Marked)
    );
    assert_eq!(
        s.mark_verdict_delivered(&id("42")),
        Ok(VerdictDelivery::NotPending)
    );
    assert_eq!(
        s.persist_verdict(&refusal("42")),
        Ok(VerdictNote::AlreadyClosed)
    );
    assert_eq!(s.persist_verdict(&refusal("43")), Ok(VerdictNote::Written));
}

#[test]
fn a_verdict_waits_for_an_attempt() {
    let kv = FaultKv::new();
    put_attempt(&kv, 5, "2.0.0", "ota_1", false, false);
    kv.reset_counters();
    assert_eq!(
        store(&kv).persist_verdict(&refusal("42")),
        Err(StoreError::NotAdmitted(BlockedBy::AttemptUnresolved))
    );
    assert_eq!(kv.commits(), 0);
}

#[test]
fn a_persisted_verdict_is_whole_or_absent_after_a_fault() {
    let verdict = DdiVerdict::failed(id("42"), FailReason::Ota(OtaError::ChecksumMismatch))
        .expect("reported");
    let setup = |kv: &FaultKv| {
        closed_seven(kv);
        kv.put_str("ddi_det0", "downgrade")
            .put_str("ddi_det1", "stale");
    };
    each_fault(
        &setup,
        &|s| {
            let _ = s.persist_verdict(
                &DdiVerdict::failed(id("42"), FailReason::Ota(OtaError::ChecksumMismatch))
                    .expect("reported"),
            );
        },
        |kv, fault, n| {
            let tag = format!("{fault:?} at {n}");
            let mut s = store(kv);
            match s.pending_verdict() {
                Ok(Some(pending)) => assert_eq!(pending, verdict, "{tag}"),
                Ok(None) => {
                    assert_ne!(kv.u8_of("ddi_stage"), Some(2), "{tag}");
                    assert_eq!(
                        s.persist_verdict(&verdict),
                        Ok(VerdictNote::Written),
                        "{tag}"
                    );
                    assert_eq!(s.pending_verdict(), Ok(Some(verdict.clone())), "{tag}");
                }
                Err(e) => panic!("{tag}: {e}"),
            }
        },
    );
}

#[test]
fn a_failed_download_persists_its_verdict_before_the_clear() {
    let kv = FaultKv::new();
    let mut s = store(&kv);
    begin(&mut s, "42").expect("begins");
    kv.reset_counters();
    let got = s.settle_failed_ddi_download(
        interrupted(),
        Some(V_OLD),
        FailReason::Ota(OtaError::DownloadFailed { status: 0 }),
    );
    assert_eq!(got, Ok(FailedDownload::Cleared));
    let ops = kv.ops();
    let pos = |op: &str| ops.iter().position(|o| o == op).expect(op);
    assert!(pos("set:ddi_stage") < pos("rm:att_ver"));
    let pending = s.pending_verdict().expect("readable").expect("pending");
    assert_eq!(
        pending.result,
        VerdictResult::Failure(ProfileCode::Transfer)
    );
    assert_eq!(pending.detail_strs().as_slice(), ["download_failed"]);
}

#[test]
fn a_failed_download_verdict_survives_a_fault_at_every_step() {
    let setup = |kv: &FaultKv| {
        begin(&mut store(kv), "42").expect("begins");
    };
    let reason = FailReason::Ota(OtaError::ChecksumMismatch);
    each_fault(
        &setup,
        &|s| {
            let _ = s.settle_failed_ddi_download(interrupted(), Some(V_OLD), reason);
        },
        |kv, fault, n| {
            let tag = format!("{fault:?} at {n}");
            let mut s = store(kv);
            let _ = s.settle_failed_ddi_download(interrupted(), Some(V_OLD), reason);
            assert!(!kv.has("att_ver"), "{tag}");
            let pending = s.pending_verdict().expect("readable").expect("pending");
            assert_eq!(pending.code(), Some(ProfileCode::Checksum), "{tag}");
            assert_eq!(
                pending.detail_strs().as_slice(),
                ["checksum_mismatch"],
                "{tag}"
            );
        },
    );
}

#[test]
fn a_cancel_or_a_kept_attempt_writes_no_verdict() {
    let kv = FaultKv::new();
    let mut s = store(&kv);
    begin(&mut s, "42").expect("begins");
    let mut deferred = interrupted();
    deferred.update_slot = UpdateSlot::Unreadable(5);
    assert!(matches!(
        s.settle_failed_ddi_download(deferred, Some(V_OLD), FailReason::ManifestFetch),
        Ok(FailedDownload::Kept(_))
    ));
    assert_eq!(kv.u8_of("ddi_stage"), Some(1));
    assert_eq!(
        s.settle_failed_ddi_download(
            interrupted(),
            Some(V_OLD),
            FailReason::Ota(OtaError::Cancelled)
        ),
        Ok(FailedDownload::Cleared)
    );
    assert_eq!(kv.u8_of("ddi_stage"), Some(1));
    assert_eq!(s.pending_verdict(), Ok(None));
    assert_eq!(s.is_action_closed(&id("42")), Ok(false));
}

// ---- delivery ---------------------------------------------------------------------

/// The rolled-back attempt of action `42`: report and verdict pending.
fn rolled_back(kv: &FaultKv) {
    let mut s = store(kv);
    begin(&mut s, "42").expect("begins");
    s.mark_boot_selected().expect("selected");
    s.mark_activated().expect("activated");
    let facts = hw(SLOT_A, SlotState::Valid, Some((SLOT_B, SlotState::Invalid)));
    reconcile_boot(&mut s, Ok(facts), Some(V_OLD));
}

#[test]
fn the_rollback_verdict_carries_the_reason_and_its_delivery_clears_the_report() {
    let kv = FaultKv::new();
    rolled_back(&kv);
    let mut s = store(&kv);
    let pending = s.pending_verdict().expect("readable").expect("pending");
    assert_eq!(pending.code(), Some(ProfileCode::RolledBack));
    assert_eq!(
        pending.detail_strs().as_slice(),
        ["rolled_back", "bootloader"]
    );
    assert!(s.report_undelivered().expect("readable"));
    kv.reset_counters();
    assert_eq!(
        s.mark_verdict_delivered(&id("42")),
        Ok(VerdictDelivery::Marked)
    );
    assert_eq!(kv.ops(), ["set:rb", "set:ddi_stage"]);
    assert_eq!(kv.u8_of("rb"), Some(0));
    assert_eq!(s.offer_gate(), OfferGate::Open);
}

#[test]
fn delivery_never_closes_the_action_with_its_report_still_pending() {
    each_fault(
        &rolled_back,
        &|s| {
            let _ = s.mark_verdict_delivered(&id("42"));
        },
        |kv, fault, n| {
            let tag = format!("{fault:?} at {n}");
            match kv.u8_of("ddi_stage") {
                Some(2) => {
                    let mut s = store(kv);
                    assert!(s.pending_verdict().expect("readable").is_some(), "{tag}");
                    assert_eq!(
                        s.mark_verdict_delivered(&id("42")),
                        Ok(VerdictDelivery::Marked),
                        "{tag}"
                    );
                }
                Some(3) => {}
                other => panic!("{tag}: stage {other:?}"),
            }
            assert_eq!(kv.u8_of("rb"), Some(0), "{tag}");
            assert_eq!(store(kv).offer_gate(), OfferGate::Open, "{tag}");
        },
    );
}

#[test]
fn delivery_leaves_a_report_of_another_attempt_alone() {
    let kv = FaultKv::new();
    put_ddi(&kv, "42", 2, Some(5), Some(1004));
    kv.put_u8("rb", 1).put_u32("rb_id", 3);
    assert_eq!(
        store(&kv).mark_verdict_delivered(&id("42")),
        Ok(VerdictDelivery::Marked)
    );
    assert_eq!(kv.u8_of("rb"), Some(1));
}

#[test]
fn a_read_error_while_delivering_writes_nothing() {
    let kv = FaultKv::new();
    rolled_back(&kv);
    let reads = {
        kv.reset_counters();
        let probe = reboot(&kv);
        let _ = store(&probe).mark_verdict_delivered(&id("42"));
        probe.reads()
    };
    for n in 1..=reads {
        let kv = reboot(&kv);
        kv.fail_read_at(n);
        assert_eq!(
            store(&kv).mark_verdict_delivered(&id("42")),
            Err(StoreError::Kv(KvError::new(INJECTED))),
            "read {n}"
        );
        assert_eq!(kv.commits(), 0, "read {n}");
    }
}

#[test]
fn unreadable_details_read_as_unknown() {
    let kv = FaultKv::new();
    put_ddi(&kv, "42", 2, None, Some(1002));
    kv.put_u32("ddi_det0", 1);
    let pending = store(&kv)
        .pending_verdict()
        .expect("readable")
        .expect("pending");
    assert_eq!(pending.detail_strs().as_slice(), ["unknown"]);
}

#[test]
fn a_success_verdict_reads_back_without_a_code() {
    let kv = FaultKv::new();
    put_ddi(&kv, "42", 2, Some(5), Some(0));
    let pending = store(&kv)
        .pending_verdict()
        .expect("readable")
        .expect("pending");
    assert_eq!(pending.finished(), Finished::Success);
    assert_eq!(pending.code(), None);
    assert!(pending.details.is_empty());
}

// ---- review fixes -------------------------------------------------------------------

#[test]
fn a_booted_attempt_without_app_rollback_reports_success() {
    let kv = FaultKv::new();
    let mut s = store(&kv);
    begin(&mut s, "42").expect("begins");
    s.mark_boot_selected().expect("selected");
    let kv = reboot(&kv);
    let no_rollback = hw(SLOT_B, SlotState::Unknown, None);
    reconcile_boot(&mut store(&kv), Ok(no_rollback), Some(V_NEW));
    let s = store(&kv);
    assert!(!kv.has("att_ver"), "the attempt is cleared");
    let pending = s.pending_verdict().expect("readable").expect("pending");
    assert_eq!(pending.finished(), Finished::Success);
    assert_eq!(s.is_action_closed(&id("42")), Ok(true));
}

#[test]
fn a_cleared_attempt_that_did_not_boot_its_version_is_offered_again() {
    for (name, facts, running) in [
        ("interrupted download", interrupted(), Some(V_OLD)),
        (
            "another version, no app rollback",
            hw(SLOT_B, SlotState::Unknown, None),
            Some(V_OLD),
        ),
        (
            "slot marked invalid",
            hw(SLOT_B, SlotState::Invalid, None),
            Some(V_NEW),
        ),
    ] {
        let kv = FaultKv::new();
        let mut s = store(&kv);
        begin(&mut s, "42").expect("begins");
        let kv = reboot(&kv);
        reconcile_boot(&mut store(&kv), Ok(facts), running);
        let s = store(&kv);
        assert!(!kv.has("att_ver"), "{name}: the attempt is cleared");
        assert_eq!(s.pending_verdict(), Ok(None), "{name}");
        assert_eq!(s.is_action_closed(&id("42")), Ok(false), "{name}");
    }
}

#[test]
fn a_verdict_waits_for_its_attempt_to_clear() {
    let kv = FaultKv::new();
    put_attempt(&kv, 5, "2.0.0", "ota_1", true, true);
    put_ddi(&kv, "42", 2, Some(5), Some(1004));
    kv.put_u8("rb", 1).put_u32("rb_id", 5);
    let mut s = store(&kv);
    assert_eq!(s.pending_verdict(), Ok(None));
    kv.reset_counters();
    assert_eq!(
        s.mark_verdict_delivered(&id("42")),
        Ok(VerdictDelivery::NotPending)
    );
    assert_eq!(kv.commits(), 0);
    s.clear_attempt().expect("cleared");
    assert!(s.pending_verdict().expect("readable").is_some());
    assert_eq!(
        s.mark_verdict_delivered(&id("42")),
        Ok(VerdictDelivery::Marked)
    );
    assert_eq!(kv.u8_of("rb"), Some(0));
}

#[test]
fn a_records_unreadable_verdict_repairs_first() {
    for (name, setup) in [
        (
            "corrupt attempt",
            (|kv: &FaultKv| {
                put_attempt(kv, 5, "2.0.0", "ota_7", false, false);
            }) as fn(&FaultKv),
        ),
        ("corrupt DDI record", |kv: &FaultKv| {
            put_ddi(kv, "042", 3, None, Some(0))
        }),
        ("corrupt stage", |kv: &FaultKv| {
            put_ddi(kv, "7", 9, None, Some(0))
        }),
    ] {
        let kv = FaultKv::new();
        setup(&kv);
        let mut s = store(&kv);
        assert_eq!(s.offer_gate(), OfferGate::RecordsUnreadable, "{name}");
        assert_eq!(
            s.persist_verdict(&DdiVerdict::records_unreadable(id("42"))),
            Ok(VerdictNote::Written),
            "{name}"
        );
        assert_eq!(s.corrupt_present(), Ok(false), "{name}");
        let pending = s.pending_verdict().expect("readable").expect("pending");
        assert_eq!(pending.code(), Some(ProfileCode::Device), "{name}");
        assert_eq!(
            pending.detail_strs().as_slice(),
            ["records_unreadable"],
            "{name}"
        );
    }
}

#[test]
fn delivering_a_rollback_verdict_clears_a_report_whose_id_a_repair_removed() {
    let kv = FaultKv::new();
    put_ddi(&kv, "42", 2, Some(5), Some(1004));
    kv.put_u8("rb", 1);
    let mut s = store(&kv);
    assert_eq!(
        s.mark_verdict_delivered(&id("42")),
        Ok(VerdictDelivery::Marked)
    );
    assert_eq!(kv.u8_of("rb"), Some(0));

    let kv = FaultKv::new();
    put_ddi(&kv, "42", 2, Some(5), Some(1002));
    kv.put_u8("rb", 1);
    store(&kv)
        .mark_verdict_delivered(&id("42"))
        .expect("delivered");
    assert_eq!(
        kv.u8_of("rb"),
        Some(1),
        "only a rollback verdict owns an orphan report"
    );
}

#[test]
fn a_read_error_while_beginning_leaves_every_ddi_key_alone() {
    let kv = FaultKv::new();
    closed_seven(&kv);
    let reads = {
        let probe = reboot(&kv);
        probe.reset_counters();
        let _ = begin(&mut store(&probe), "42");
        probe.reads()
    };
    for n in 1..=reads {
        let kv = reboot(&kv);
        kv.reset_counters();
        kv.fail_read_at(n);
        assert!(begin(&mut store(&kv), "42").is_err(), "read {n}");
        assert!(
            kv.ops().iter().all(|o| !o.contains(":ddi_")),
            "read {n}: {:?}",
            kv.ops()
        );
        assert_eq!(kv.u8_of("ddi_stage"), Some(3), "read {n}");
        assert_eq!(kv.str_of("ddi_act").as_deref(), Some("7"), "read {n}");
    }
}
