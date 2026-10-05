//! Crash-consistency matrix.
//!
//! Each scenario runs once per (fault kind, commit index).
//! After the fault the store is re-opened and the boot reconciliation runs twice.
//! Then the same recovery boot is itself crashed at every one of its commits (depth 2) and run twice again.
//! The invariants I1..I8 are checked after every one of those runs.

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use super::support::*;
use crate::ota::persist::fault_kv::{FaultKv, LogEntry, Op, Val};
use crate::ota::persist::SettleOutcome;
use crate::ota::persist::{
    reconcile_boot, BootDisposition, BootOutcome, HardwareFacts, OtaStore, PendingReportView,
};
use crate::ota::wire::RollbackReason;
use crate::ota::{SlotState, Version};

#[derive(Clone, Copy)]
enum Expect {
    /// Only the generic invariants.
    Generic,
    /// An attempt that existed at the fault ends as exactly one report and one refusal; none existing means no report.
    AttemptReported { left: Option<Version> },
    /// An attempt that existed at the fault is completed.
    AttemptCompleted,
    /// An attempt that existed at the fault stays and the boot demands a rollback.
    AttemptRollBackNow,
    /// A request armed at the fault ends as exactly one report.
    RequestReported,
    /// Like `RequestReported`, and the report may already be delivered (`rb == 0`).
    RequestReportedMaybeDelivered,
    /// The slot the request leaves still runs: the request is dropped, nothing is reported, and a leftover refusal is kept untouched.
    RequestDropped,
}

struct Scenario {
    name: &'static str,
    setup: fn(&FaultKv),
    run: fn(&mut OtaStore<FaultKv>),
    /// Hardware as seen by every recovery boot.
    hw: HardwareFacts,
    running_version: Option<Version>,
    expect: Expect,
}

struct Before {
    attempt_id: Option<u32>,
    request_id: Option<u32>,
    refused: Option<String>,
}

fn u8_of_val(v: &Val) -> Option<u8> {
    if let Val::U8(x) = v {
        Some(*x)
    } else {
        None
    }
}

fn before_of(map: &BTreeMap<String, Val>) -> Before {
    let kv = FaultKv::from_map(map.clone());
    let attempt = kv.has("att_ver") && kv.has("att_slot");
    let request = kv.has("rq_from");
    Before {
        attempt_id: if attempt { kv.u32_of("att_id") } else { None },
        request_id: if request { kv.u32_of("rq_id") } else { None },
        refused: kv.str_of("rej_ver"),
    }
}

fn boot(kv: &FaultKv, sc: &Scenario) -> BootOutcome {
    reconcile_boot(&mut store(kv), Ok(sc.hw), sc.running_version)
}

fn durable(
    log: &[LogEntry],
    upto: usize,
    base: &BTreeMap<String, Val>,
    key: &str,
    want: &Val,
) -> bool {
    let mut current = base.get(key).cloned();
    for entry in &log[..upto] {
        if entry.key == key {
            current = match entry.op {
                Op::Set => entry.value.clone(),
                _ => None,
            };
        }
    }
    current.as_ref() == Some(want)
}

fn finish(sc: &Scenario, kv: &FaultKv, base: &BTreeMap<String, Val>, tag: &str) {
    let before = before_of(&kv.snapshot());
    boot(kv, sc);
    let log_before_last = kv.log().len();
    let last = boot(kv, sc);
    let attempt_now = kv.has("att_ver") && kv.has("att_slot");
    let log = kv.log();
    let ctx = format!("{}: {tag}", sc.name);

    // The second boot is idempotent: it writes nothing.
    if let Some(entry) = log[log_before_last..].first() {
        panic!("{ctx}: second boot wrote {}", entry.key);
    }

    // I5: admission is open only when nothing is in flight and the running image is verified.
    if last.admission_open_at_boot {
        assert!(!attempt_now, "{ctx}: I5 attempt");
        assert!(!kv.has("rq_from"), "{ctx}: I5 request");
        assert_ne!(kv.u8_of("rb"), Some(1), "{ctx}: I5 report");
        assert_ne!(
            sc.hw.running_state,
            SlotState::PendingVerify,
            "{ctx}: I5 pending"
        );
    }

    // I3 / I8: one report identity per run, hence at most one pending report.
    let mut ids: Vec<u32> = log
        .iter()
        .filter(|e| e.key == "rb_id" && e.op == Op::Set)
        .filter_map(|e| {
            if let Some(Val::U32(v)) = e.value {
                Some(v)
            } else {
                None
            }
        })
        .collect();
    ids.dedup();
    assert!(ids.len() <= 1, "{ctx}: I3 ids {ids:?}");

    // I6: ids are handed out strictly increasing.
    let counters: Vec<u32> = log
        .iter()
        .filter(|e| e.key == "att_ctr" && e.op == Op::Set)
        .filter_map(|e| {
            if let Some(Val::U32(v)) = e.value {
                Some(v)
            } else {
                None
            }
        })
        .collect();
    assert!(
        counters.windows(2).all(|w| w[0] < w[1]),
        "{ctx}: I6 {counters:?}"
    );

    // I7: first reason wins.
    let mut whys: Vec<_> = log
        .iter()
        .filter(|e| e.key == "att_why" && e.op == Op::Set)
        .map(|e| e.value.clone())
        .collect();
    whys.dedup();
    assert!(whys.len() <= 1, "{ctx}: I7 {whys:?}");

    match sc.expect {
        Expect::Generic => {}
        Expect::AttemptReported { left } => {
            let Some(id) = before.attempt_id else {
                // No attempt at the fault: it never existed, or it was cleared after its report and refusal were durable.
                let reported = log.iter().any(|e| e.key == "rb_id" && e.op == Op::Set);
                assert!(
                    reported || kv.has("rb") == base.contains_key("rb"),
                    "{ctx}: no report without an attempt"
                );
                if let (true, Some(left)) = (reported, left) {
                    assert_eq!(
                        kv.str_of("rej_ver").as_deref(),
                        Some(format!("{left}").as_str()),
                        "{ctx}: I4"
                    );
                }
                return;
            };
            assert!(!attempt_now, "{ctx}: I2 attempt left");
            assert_eq!(kv.u8_of("rb"), Some(1), "{ctx}: I2 report");
            assert_eq!(kv.u32_of("rb_id"), Some(id), "{ctx}: I2 id");
            if let Some(left) = left {
                assert_eq!(
                    kv.str_of("rej_ver").as_deref(),
                    Some(format!("{left}").as_str()),
                    "{ctx}"
                );
                // I4: report and refusal are durable before the attempt is cleared.
                if let Some(at) = log
                    .iter()
                    .position(|e| e.key == "att_ver" && e.op == Op::Remove)
                {
                    assert!(
                        durable(&log, at, base, "rb_id", &Val::U32(id)),
                        "{ctx}: I4 report"
                    );
                    let want = Val::Str(format!("{left}").as_str().try_into().unwrap());
                    assert!(
                        durable(&log, at, base, "rej_ver", &want),
                        "{ctx}: I4 refusal"
                    );
                }
            }
        }
        Expect::AttemptCompleted => {
            if before.attempt_id.is_some() {
                assert!(!attempt_now, "{ctx}");
                assert!(!kv.has("rej_ver"), "{ctx}");
                assert!(!kv.has("rb") || kv.u8_of("rb") != Some(1), "{ctx}");
            }
        }
        Expect::AttemptRollBackNow => {
            if before.attempt_id.is_some() {
                assert!(
                    attempt_now && last.roll_back_now && last.refuse_mark_valid,
                    "{ctx}"
                );
                assert!(!kv.has("rb"), "{ctx}");
            } else {
                assert!(!last.roll_back_now, "{ctx}");
            }
        }
        Expect::RequestDropped => {
            assert_eq!(
                last.disposition,
                BootDisposition::Settled(SettleOutcome::NoRequest),
                "{ctx}"
            );
            assert!(!kv.has("rq_from"), "{ctx}: request left");
            assert_eq!(
                kv.u8_of("rb"),
                base.get("rb").and_then(u8_of_val),
                "{ctx}: rb touched"
            );
            if let Some(id) = before.request_id {
                assert_ne!(kv.u32_of("rb_id"), Some(id), "{ctx}: request id reported");
            }
            assert!(
                !log.iter().any(|e| e.key == "rb_id" && e.op == Op::Set),
                "{ctx}: a drop never writes a report"
            );
            // The refusal of the dropped request is kept as the fault left it: neither restored nor removed.
            assert_eq!(kv.str_of("rej_ver"), before.refused, "{ctx}: refusal");
        }
        Expect::RequestReported | Expect::RequestReportedMaybeDelivered => {
            if let Some(id) = before.request_id {
                assert!(!kv.has("rq_from"), "{ctx}: request left");
                match (sc.expect, kv.u8_of("rb")) {
                    (_, Some(1)) | (Expect::RequestReportedMaybeDelivered, Some(0)) => {}
                    (_, other) => panic!("{ctx}: report state {other:?}"),
                }
                assert_eq!(kv.u32_of("rb_id"), Some(id), "{ctx}");
            }
        }
    }
}

fn run_matrix(sc: &Scenario) {
    each_fault(&sc.setup, &sc.run, |kv, fault, n| {
        let tag = format!("{fault:?} at {n}");
        // Depth 2 starts from the flash content right after the fault.
        let faulted = kv.snapshot();
        let probe = FaultKv::from_map(faulted.clone());
        boot(&probe, sc);
        for k in 1..=probe.commits() {
            let again = FaultKv::from_map(faulted.clone());
            again.crash_after(k);
            boot(&again, sc);
            again.revive();
            again.reset_counters();
            finish(
                sc,
                &again,
                &again.snapshot(),
                &format!("{tag}, recovery crash at {k}"),
            );
        }
        let base = {
            let fresh = FaultKv::new();
            (sc.setup)(&fresh);
            fresh.snapshot()
        };
        finish(sc, kv, &base, &tag);
    });
}

const COUNTER: u32 = 4;

fn setup_counter(kv: &FaultKv) {
    put_counter(kv, COUNTER);
}

fn setup_attempt_new(kv: &FaultKv) {
    put_counter(kv, 5);
    put_attempt(kv, 5, "2.0.0", "ota_1", true, true);
}

fn run_begin(s: &mut OtaStore<FaultKv>) {
    if s.begin_attempt(V_NEW, SLOT_B, false).is_ok() {
        let _ = s.mark_boot_selected();
    }
}

#[test]
fn begin_attempt_then_new_image_boots_and_awaits_its_health_check() {
    run_matrix(&Scenario {
        name: "begin+good",
        setup: setup_counter,
        run: run_begin,
        hw: hw(SLOT_B, SlotState::PendingVerify, None),
        running_version: Some(V_NEW),
        expect: Expect::Generic,
    });
}

#[test]
fn begin_attempt_then_new_image_dies_before_activation() {
    run_matrix(&Scenario {
        name: "begin+died",
        setup: setup_counter,
        run: run_begin,
        hw: hw(SLOT_A, SlotState::Valid, Some((SLOT_B, SlotState::Invalid))),
        running_version: Some(V_OLD),
        expect: Expect::AttemptReported { left: Some(V_NEW) },
    });
}

#[test]
fn begin_attempt_then_mislabelled_image_must_be_rolled_back() {
    run_matrix(&Scenario {
        name: "begin+mislabelled",
        setup: setup_counter,
        run: run_begin,
        hw: hw(SLOT_B, SlotState::PendingVerify, None),
        running_version: Some(Version::new(3, 0, 0)),
        expect: Expect::AttemptRollBackNow,
    });
}

#[test]
fn begin_attempt_then_healthy_image_completes() {
    run_matrix(&Scenario {
        name: "begin+complete",
        setup: |kv| {
            setup_counter(kv);
            kv.put_str("rej_ver", "1.5.0");
        },
        run: run_begin,
        hw: hw(SLOT_B, SlotState::Valid, None),
        running_version: Some(V_NEW),
        expect: Expect::AttemptCompleted,
    });
}

#[test]
fn report_rollback_with_reason_and_refusal() {
    run_matrix(&Scenario {
        name: "report",
        setup: |kv| {
            setup_attempt_new(kv);
            kv.put_str("att_why", "health_deadline");
        },
        run: |s| {
            let _ = reconcile_boot(
                s,
                Ok(hw(
                    SLOT_A,
                    SlotState::Valid,
                    Some((SLOT_B, SlotState::Invalid)),
                )),
                Some(V_OLD),
            );
        },
        hw: hw(SLOT_A, SlotState::Valid, Some((SLOT_B, SlotState::Invalid))),
        running_version: Some(V_OLD),
        expect: Expect::AttemptReported { left: Some(V_NEW) },
    });
}

#[test]
fn complete_attempt_drops_a_different_refusal() {
    run_matrix(&Scenario {
        name: "complete",
        setup: |kv| {
            setup_attempt_new(kv);
            kv.put_str("rej_ver", "1.5.0");
        },
        run: |s| {
            let _ = reconcile_boot(s, Ok(hw(SLOT_B, SlotState::Valid, None)), Some(V_NEW));
        },
        hw: hw(SLOT_B, SlotState::Valid, None),
        running_version: Some(V_NEW),
        expect: Expect::AttemptCompleted,
    });
}

#[test]
fn settle_request_reports_exactly_once() {
    run_matrix(&Scenario {
        name: "settle",
        setup: |kv| {
            setup_counter(kv);
            put_request(kv, 7, "operator", "ota_1");
        },
        run: |s| {
            let _ = reconcile_boot(s, Ok(hw(SLOT_A, SlotState::Valid, None)), Some(V_OLD));
        },
        hw: hw(SLOT_A, SlotState::Valid, None),
        running_version: Some(V_OLD),
        expect: Expect::RequestReported,
    });
}

#[test]
fn note_rollback_attempt_branch_keeps_the_first_reason() {
    run_matrix(&Scenario {
        name: "note-attempt",
        setup: setup_attempt_new,
        run: |s| {
            let _ = s.note_rollback(RollbackReason::HealthDeadline, SLOT_B);
            let _ = s.note_rollback(RollbackReason::Operator, SLOT_B);
        },
        hw: hw(SLOT_A, SlotState::Valid, Some((SLOT_B, SlotState::Invalid))),
        running_version: Some(V_OLD),
        expect: Expect::AttemptReported { left: Some(V_NEW) },
    });
}

#[test]
fn note_rollback_request_branch_arms_last() {
    run_matrix(&Scenario {
        name: "note-request",
        setup: |_| {},
        run: |s| {
            let _ = s.note_rollback(RollbackReason::Operator, SLOT_B);
            let _ = s.note_rollback(RollbackReason::Unhealthy, SLOT_B);
        },
        hw: hw(SLOT_A, SlotState::Valid, None),
        running_version: Some(V_OLD),
        expect: Expect::RequestReported,
    });
}

#[test]
fn operator_rollback_with_refusal_then_reboot() {
    run_matrix(&Scenario {
        name: "operator",
        setup: |_| {},
        run: |s| {
            if s.note_rollback(RollbackReason::Operator, SLOT_B).is_ok() {
                let _ = s.refuse_version(V_NEW);
            }
        },
        hw: hw(SLOT_A, SlotState::Valid, None),
        running_version: Some(V_OLD),
        expect: Expect::RequestReported,
    });
}

#[test]
fn refuse_image_keeps_the_attempt_and_demands_the_rollback() {
    run_matrix(&Scenario {
        name: "refuse",
        setup: |kv| {
            put_counter(kv, 5);
            put_attempt(kv, 5, "2.0.0", "ota_1", true, false);
        },
        run: |s| {
            let _ = reconcile_boot(
                s,
                Ok(hw(SLOT_B, SlotState::PendingVerify, None)),
                Some(Version::new(3, 0, 0)),
            );
        },
        hw: hw(SLOT_B, SlotState::PendingVerify, None),
        running_version: Some(Version::new(3, 0, 0)),
        expect: Expect::AttemptRollBackNow,
    });
}

#[test]
fn request_report_delivery_and_settle_never_lose_or_duplicate_the_request() {
    run_matrix(&Scenario {
        name: "settle+delivery",
        setup: |kv| {
            setup_counter(kv);
            put_request(kv, 7, "operator", "ota_1");
        },
        run: |s| {
            let _ = s.settle_request(SLOT_A);
            if let Ok(PendingReportView::Ready(report)) = s.pending_report() {
                let _ = s.mark_report_delivered(report.attempt_id);
                let _ = s.settle_request(SLOT_A);
            }
        },
        hw: hw(SLOT_A, SlotState::Valid, None),
        running_version: Some(V_OLD),
        expect: Expect::RequestReportedMaybeDelivered,
    });
}

#[test]
fn operator_recipe_recovering_with_the_slot_not_rolled_back_drops_the_request() {
    run_matrix(&Scenario {
        name: "operator+not-rolled-back",
        setup: |_| {},
        run: |s| {
            if s.note_rollback(RollbackReason::Operator, SLOT_B).is_ok() {
                let _ = s.refuse_version(V_NEW);
            }
        },
        hw: hw(SLOT_B, SlotState::Valid, None),
        running_version: Some(V_NEW),
        expect: Expect::RequestDropped,
    });
}

#[test]
fn operator_recipe_with_an_older_refusal_recovering_not_rolled_back_keeps_what_the_fault_left() {
    run_matrix(&Scenario {
        name: "operator+older-refusal+not-rolled-back",
        setup: |kv| {
            setup_counter(kv);
            kv.put_str("rej_ver", "1.5.0");
        },
        run: |s| {
            if s.note_rollback(RollbackReason::Operator, SLOT_B).is_ok() {
                let _ = s.refuse_version(V_NEW);
            }
        },
        hw: hw(SLOT_B, SlotState::Valid, None),
        running_version: Some(V_NEW),
        expect: Expect::RequestDropped,
    });
}

#[test]
fn a_dropped_request_leaves_its_refusal_in_place() {
    let kv = FaultKv::new();
    let mut s = store(&kv);
    assert!(s.note_rollback(RollbackReason::Operator, SLOT_B).is_ok());
    assert!(s.refuse_version(V_NEW).is_ok());
    let out = reconcile_boot(&mut s, Ok(hw(SLOT_B, SlotState::Valid, None)), Some(V_NEW));
    assert_eq!(
        out.disposition,
        BootDisposition::Settled(SettleOutcome::DroppedNotRolledBack)
    );
    assert_eq!(kv.str_of("rej_ver").as_deref(), Some("2.0.0"));
    assert!(!kv.has("rq_from") && !kv.has("rb"));
}

#[test]
fn a_full_attempt_after_a_lost_counter_ends_as_one_pending_report() {
    run_matrix(&Scenario {
        name: "counter-lost+attempt",
        setup: |kv| {
            kv.put_u32("att_epoch", EPOCH);
            kv.put_u8("rb", 0)
                .put_u32("rb_id", 1)
                .put_str("rb_why", "health_deadline");
        },
        run: |s| {
            if s.begin_attempt(V_NEW, SLOT_B, false).is_ok() {
                let _ = s.mark_boot_selected();
                let _ = s.mark_activated();
                let _ = s.note_rollback(RollbackReason::HealthDeadline, SLOT_B);
                let _ = reconcile_boot(
                    s,
                    Ok(hw(
                        SLOT_A,
                        SlotState::Valid,
                        Some((SLOT_B, SlotState::Invalid)),
                    )),
                    Some(V_OLD),
                );
            }
        },
        hw: hw(SLOT_A, SlotState::Valid, Some((SLOT_B, SlotState::Invalid))),
        running_version: Some(V_OLD),
        expect: Expect::AttemptReported { left: Some(V_NEW) },
    });
}
