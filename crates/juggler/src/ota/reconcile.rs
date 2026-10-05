//! Boot reconciliation: decide what to do about an in-flight OTA attempt
//! after a reboot, from persisted evidence and bootloader facts.
//!
//! The functions here are pure: the consumer reads the persisted
//! [`AttemptRecord`] and the bootloader state ([`BootFacts`]), calls
//! [`reconcile`], and carries out the returned [`ReconcileAction`].
//!
//! # Persisting the attempt
//!
//! The consumer writes the [`AttemptRecord`] before the download starts, with
//! `activated = false` and `boot_selected = false`.
//! `boot_selected` is persisted right after `fetch_and_apply` returns `Ok`
//! (it switches the boot slot internally) and before rebooting.
//! Never persist it before the call: an interrupted download would then be
//! mistaken for a rollback and a good version refused.
//! If `fetch_and_apply` returns `Err`, call [`reconcile`] with refreshed facts
//! right away instead of waiting for a reboot: the previous image still runs
//! and the boot slot was not switched, so it returns
//! [`ReconcileAction::ClearAttempt`] and admission reopens.
//!
//! The record carries a consumer-assigned `attempt_id`, strictly increasing
//! per attempt and persisted with the attempt.
//!
//! # Attempt ids (consumer-owned contract)
//!
//! The backend deduplicates on (device identity, epoch, `attempt_id`).
//! The id counter must survive deleting the attempt record, and an id must be
//! reserved durably before the update starts.
//! With separate storage writes, persist the counter first, then the attempt:
//! a skipped id is harmless, a reused id can suppress a real report.
//! A factory reset, flash erase, or counter exhaustion restarts or wraps the
//! ids, so the consumer needs an identity or reset policy for that.
//!
//! Recommended epoch scheme: generate a random 32-bit install epoch when the
//! counter is first created (first boot, or after a flash erase finds no
//! counter), store it with the counter, and include it in every report.
//!
//! Consumer-originated rollback reports (no attempt, for example an operator
//! rollback after `mark_valid`) are outside [`reconcile`].
//! If they use the same report record, take their event id from the same
//! durable counter so ids never collide.
//! A rollback reason is consumer naming: persist it next to the attempt before
//! rolling back and attach it when [`ReconcileAction::ReportRollback`] fires.
//!
//! # Admission
//!
//! While an attempt record exists the consumer must not start a new attempt:
//! overwriting the record destroys unresolved evidence.
//! This covers [`ReconcileAction::Defer`], [`ReconcileAction::AwaitHealthCheck`],
//! [`ReconcileAction::RefuseImage`], and a [`ReconcileAction::ReportRollback`]
//! whose writes failed.
//!
//! Admission is also blocked while an undelivered report exists: v1 keeps a
//! single report record, so a second rollback must not overwrite a report that
//! was not delivered yet.
//! The attempt is cleared once report and refusal are durable, but delivery
//! may still be pending (for example the reporting channel is offline).
//!
//! Derive the state with [`Admission::from_records`](super::Admission::from_records)
//! and pass it to [`decide_offer`](super::decide_offer): an offer that would
//! be applied then returns [`OfferDecision::Blocked`](super::OfferDecision::Blocked).
//! The consumer must keep that offer (for example in RAM) and re-evaluate it
//! once admission reopens, after `ClearAttempt`, `CompleteAttempt`, or report
//! delivery; a retained MQTT command is not redelivered without resubscribing.
//!
//! # Reporting
//!
//! Delivery is at least once.
//! The report carries the attempt's `attempt_id` as a stable event id; the
//! backend deduplicates on it.
//! Keep the report record, marked delivered, until the attempt is cleared (see
//! [`BootFacts::report_persisted`]).
//!
//! # Retrying at runtime
//!
//! While the healthy previous image runs, some outcomes can be re-run at
//! runtime with REFRESHED facts instead of waiting for another reboot:
//! - [`ReconcileAction::ReportRollback`] — every step is idempotent given
//!   `report_persisted` and `attempt_id`, and so is the refusal write.
//! - [`ReconcileAction::ClearAttempt`] — idempotent.
//! - [`ReconcileAction::CompleteAttempt`] — idempotent (clearing a refusal
//!   that differs from the running version, then the attempt).
//! - [`ReconcileAction::Defer`] — re-read the update slot and call
//!   [`reconcile`] again.
//!
//! These are NOT safe to re-run: [`ReconcileAction::AwaitHealthCheck`] (the
//! health policy owns it) and [`ReconcileAction::RefuseImage`] (it ends in a
//! reboot).
//! Recommended: a bounded runtime retry scheduled by the consumer with
//! [`ExponentialBackoff`](crate::backoff::ExponentialBackoff) (always
//! available, no feature); the core stays pure and never sleeps.
//! Admission stays blocked until the retry resolves the attempt.
//!
//! # Failing safe
//!
//! On a read error that cannot be expressed in the facts (flash, NVS,
//! partition table, an unmappable slot label) do not call [`reconcile`] with
//! guessed facts: leave all records in place and refuse to mark the running
//! image valid.
//! A version string that fails to parse is expressed as `None` in
//! [`AttemptRecord::version`] / [`BootFacts::running_version`]; a version
//! only matches when both sides are `Some` and equal.
//!
//! # Known limitation
//!
//! A serial flash that erases otadata after `boot_selected` (or `activated`)
//! was persisted reads as a rollback: the previous image runs and the record
//! claims the target was selected.
//!
//! A power loss after the internal boot-slot switch but before `boot_selected`
//! persists, followed by a new image that crashes before `activated`
//! persists, into a slot that was already `Invalid`, reads as an interrupted
//! download ([`ReconcileAction::ClearAttempt`]).
//! This is an accepted v1 limitation, not a rule change: answering `Defer`
//! there would leave a permanent attempt after every interrupted download into
//! a previously rolled-back slot, and otadata sequence numbers are not exposed
//! by esp-idf-svc or the public `esp_ota` API.
//! The cost is bounded: at most one extra download per coincident power
//! failure in that window, because the retry's own `boot_selected` write then
//! catches it.
//! The bound assumes the subsequent persistence succeeds: repeated power
//! losses or storage failures can cause repeated retries.
//! This is a stated limitation, not a universal recovery guarantee.
//!
//! # Example
//!
//! ```
//! use juggler::ota::{
//!     reconcile, AttemptRecord, BootFacts, ReconcileAction, SlotId, SlotState, Version,
//! };
//!
//! let attempt = AttemptRecord {
//!     attempt_id: 7,
//!     version: Some(Version::new(1, 3, 0)),
//!     slot: SlotId(1),
//!     boot_selected: true,
//!     activated: false,
//!     slot_was_invalid: false,
//! };
//! let facts = BootFacts {
//!     running_slot: SlotId(1),
//!     running_state: SlotState::PendingVerify,
//!     running_version: Some(Version::new(1, 3, 0)),
//!     update_slot: Some((SlotId(0), SlotState::Valid)),
//!     report_persisted: false,
//! };
//!
//! match reconcile(Some(&attempt), &facts) {
//!     ReconcileAction::NoAttempt => { /* boot normally */ }
//!     ReconcileAction::AwaitHealthCheck { mark_activated } => {
//!         assert!(mark_activated);
//!         // persist activation, run the health policy, mark valid, clear
//!     }
//!     ReconcileAction::CompleteAttempt => { /* clear a differing refusal, then the record */ }
//!     ReconcileAction::ClearAttempt => { /* delete the abandoned record */ }
//!     ReconcileAction::RefuseImage { .. } => { /* persist, then roll back */ }
//!     ReconcileAction::ReportRollback { .. } => { /* report, refuse, clear */ }
//!     ReconcileAction::Defer => { /* leave records, retry next boot */ }
//! }
//! ```

use super::Version;

/// Experimental: API may change before 1.0.
///
/// OTA slot index. The consumer maps partition labels to indices
/// (`ota_0` -> 0, `ota_1` -> 1).
///
/// The mapping must be one-to-one. An unmappable label (e.g. `factory`) must
/// be treated as a read error, never mapped lossily.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct SlotId(pub u8);

/// Experimental: API may change before 1.0.
///
/// Bootloader-reported state of an OTA slot.
///
/// Mapping from `esp_idf_svc::ota::SlotState`: `Valid` -> `Valid`;
/// `Unverified` -> `PendingVerify` (esp-idf-svc folds NEW and PENDING_VERIFY
/// into `Unverified`); `Invalid` -> `Invalid` (INVALID and ABORTED);
/// `Factory` and `Unknown` -> `Unknown`.
/// Only `PendingVerify` is ever treated as "awaiting verification".
///
/// # Why `Unknown` is conclusive
///
/// `Unknown` is treated as a definite answer ("not awaiting verification"),
/// not as uncertainty.
/// This holds for esp-idf-svc 0.53's mapping and the known IDF states.
/// In esp-idf-svc 0.53 (`ota.rs`, `get_state`) it comes from
/// `ESP_ERR_NOT_FOUND` (no otadata record: the slot was never selected for
/// boot, or otadata was erased by a serial flash) or `ESP_OTA_IMG_UNDEFINED`
/// (bootloader app-rollback disabled: no verification ever happens, so a
/// booted image is final).
/// Answering [`ReconcileAction::Defer`] for `Unknown` would wedge
/// rollback-disabled devices forever.
///
/// Residual risk: the wrapper's `_ =>` catch-all also maps unrecognised state
/// values (otadata corruption or future IDF states) to `Unknown`.
/// A consumer reading raw IDF states should map unrecognised values to a read
/// error instead.
///
/// A FAILED read is not `Unknown`; it is a read error.
/// Fail safe: do not call [`reconcile`] with running-slot facts, and pass
/// `update_slot: None` when only the update slot could not be read.
///
/// If the bootloader's app-rollback is disabled, the running slot never
/// reports pending, so a booted attempt yields
/// [`ReconcileAction::ClearAttempt`] rather than
/// [`ReconcileAction::AwaitHealthCheck`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotState {
    /// The image was marked valid.
    Valid,
    /// The image booted but has not been marked valid yet.
    PendingVerify,
    /// The image was marked invalid or aborted.
    Invalid,
    /// Factory image or state not known (see the type docs: conclusive).
    Unknown,
}

/// Experimental: API may change before 1.0.
///
/// Persisted evidence of an OTA attempt in flight.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AttemptRecord {
    /// Consumer-assigned id, strictly increasing per attempt and persisted
    /// with the attempt.
    ///
    /// It identifies the attempt (not an image location) and is the stable
    /// event id of the rollback report.
    pub attempt_id: u32,
    /// The version the manifest promised, or `None` if the stored string
    /// failed to parse (always treated as a mismatch).
    pub version: Option<Version>,
    /// The target slot.
    pub slot: SlotId,
    /// The target slot was selected as the boot slot.
    ///
    /// Persist it right after `fetch_and_apply` returns `Ok` (which switches
    /// the boot slot internally) and before rebooting.
    /// Never persist it before that call: an interrupted download would then
    /// be mistaken for a rollback and a good version refused.
    pub boot_selected: bool,
    /// The target slot was booted at least once.
    pub activated: bool,
    /// The target slot was already `Invalid` when the attempt started.
    pub slot_was_invalid: bool,
}

/// Experimental: API may change before 1.0.
///
/// Facts read from the bootloader and the consumer's persistent state at boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BootFacts {
    /// The slot currently running.
    pub running_slot: SlotId,
    /// State of the running slot.
    pub running_state: SlotState,
    /// Version embedded in the running image, or `None` if it failed to
    /// parse (always treated as a mismatch).
    pub running_version: Option<Version>,
    /// The slot an update would be written to (the non-running slot) and its
    /// state, or `None` if that read failed.
    ///
    /// Only consulted when the previous image runs, so a consumer should NOT
    /// treat a read failure as fatal: pass `None`.
    pub update_slot: Option<(SlotId, SlotState)>,
    /// A rollback report is persisted AND belongs to THIS attempt (not merely
    /// "a request exists", e.g. from a version-mismatch refusal).
    ///
    /// The consumer stores the attempt's `attempt_id` with the report and
    /// compares it; a stale report from an older attempt must not suppress a
    /// new one.
    /// Version and slot identify an image location, not an attempt, and are
    /// not a correlation key.
    /// Keep the report record, marked delivered, until the attempt is
    /// cleared: if delivery erased it while the attempt was retained (a
    /// failed write), the next boot would create a second report for the same
    /// attempt (found by the `ota_lifecycle` simulation).
    /// If the consumer keeps its own rollback-request layer (for example to
    /// record a reason), resolve it before reading this. A
    /// read failure must not default to either value: do not call
    /// [`reconcile`], fail safe.
    pub report_persisted: bool,
}

/// Experimental: API may change before 1.0.
///
/// What the consumer must do after [`reconcile`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileAction {
    /// No attempt record exists; boot normally.
    NoAttempt,
    /// The attempted image is running and awaits verification.
    ///
    /// Persistence order: if `mark_activated`, persist the activation flag,
    /// then run the health policy.
    /// After the health check marks the slot valid (`mark_valid` succeeded),
    /// perform the same completion cleanup as [`ReconcileAction::CompleteAttempt`]:
    /// clear the refused version if it differs from the running one, then
    /// clear the attempt.
    /// A reset or failed write in between is recovered by the next boot's
    /// [`ReconcileAction::CompleteAttempt`].
    /// The refused version is deliberately kept until this point, see
    /// [`decide_offer`](super::decide_offer).
    ///
    /// While the attempt exists, admission is blocked (see [`Admission`](super::Admission)).
    AwaitHealthCheck {
        /// Persist `activated = true` on the record.
        mark_activated: bool,
    },
    /// The attempted image is running, matches the manifest, and is `Valid`:
    /// the update succeeded but its cleanup may be unfinished (a power loss
    /// or failed write after `mark_valid`).
    ///
    /// Consumer obligation, in order: clear the refused version if it differs
    /// from the running version, THEN clear the attempt.
    /// If clearing the refusal fails, keep the attempt: the next boot or a
    /// runtime retry repeats it.
    /// Clearing the attempt first would lose the evidence and leave a
    /// previously refused higher version refused forever, contradicting the
    /// refusal lifetime (see [`decide_offer`](super::decide_offer)).
    /// Idempotent: safe to re-run at runtime with refreshed facts.
    ///
    /// Contrast with [`ReconcileAction::ClearAttempt`], which never clears a
    /// refusal.
    CompleteAttempt,
    /// The attempt was abandoned and carries no success evidence; delete the
    /// record.
    ///
    /// This NEVER clears a refused version.
    /// It covers a download interrupted before the boot slot was switched, a
    /// serial flash over the slot (version mismatch, not pending), and a
    /// same-slot version match whose running state is `Invalid` or `Unknown`.
    /// `Unknown` (no otadata record, or app-rollback disabled) means no
    /// health verification passed, so it is not evidence of a healthy,
    /// different version; the refusal is kept rather than risk re-admitting a
    /// failed one.
    ClearAttempt,
    /// The running image is not the one the manifest promised and is still
    /// unverified; it must be rolled back.
    ///
    /// Persistence order: persist activation (if `mark_activated`), never mark
    /// the image valid, then roll back; keep the attempt record.
    /// The core never reads a persisted rollback request: the next boot relies
    /// on the attempt record's `activated` / `boot_selected`.
    /// If the rollback never happened, the same image boots `PendingVerify`
    /// and [`reconcile`] returns `RefuseImage` again.
    /// A consumer-side rollback request or reason is optional bookkeeping the
    /// core does not rely on.
    /// If persisting the activation fails, log it and STILL refuse (never mark
    /// valid) and roll back; do not abort the refusal.
    ///
    /// `mark_activated` MUST be persisted before the rollback so the next
    /// boot has the evidence even when `slot_was_invalid` was true.
    ///
    /// Rollback tier difference: `rustyfarian-esp-idf-network`
    /// `OtaSession::rollback()` reboots on success (it returns only on
    /// failure), while `rustyfarian-esp-hal-network` `OtaManager::rollback()`
    /// returns `Ok` and the caller must reset (e.g.
    /// `esp_hal::system::software_reset()`).
    /// If the rollback fails, never mark the image valid; the health deadline
    /// or the next reset retries.
    ///
    /// While the attempt exists, admission is blocked (see [`Admission`](super::Admission)).
    RefuseImage {
        /// The version the manifest promised (`None` if it failed to parse).
        promised: Option<Version>,
        /// Persist `activated = true` on the record.
        mark_activated: bool,
    },
    /// The device fell back to the previous image after an attempt.
    ///
    /// Reporting is durable and retried until delivered, deduplicated per
    /// attempt.
    /// Delivery is at least once; the report carries `attempt_id` as the
    /// event id for backend deduplication.
    /// Persistence order: if `!report_already_persisted`, persist the report
    /// (with the attempt's version and slot); then persist `left` as the
    /// refused version (see [`decide_offer`](super::decide_offer)); only after
    /// both are durable, clear the attempt.
    /// Neither write is best-effort: if either fails, keep the attempt; the
    /// next boot repeats, and `report_already_persisted` prevents a duplicate
    /// report (even after the report was delivered, see
    /// [`BootFacts::report_persisted`]).
    /// While the attempt exists, admission is blocked (see [`Admission`](super::Admission)).
    /// After the attempt is cleared, admission stays blocked until the report
    /// is delivered (v1 keeps a single report record).
    /// A transient write failure may be retried at runtime with refreshed
    /// facts (see the module docs).
    ///
    /// When `left` is `None` there is nothing to refuse (it cannot be fed to
    /// [`decide_offer`](super::decide_offer)); clear the attempt after the
    /// report.
    ReportRollback {
        /// The attempt's id, echoed as the report's event id.
        attempt_id: u32,
        /// The version the device rolled back from (`None` if it failed to
        /// parse).
        left: Option<Version>,
        /// The report for this attempt is already persisted; do not create
        /// another.
        report_already_persisted: bool,
    },
    /// The previous image runs, the boot slot switch is not evidenced, and the
    /// update slot could not be read, so a rollback cannot be told from an
    /// interrupted download.
    ///
    /// Keep every record unchanged, take no action, and retry next boot.
    /// It is safe to run normally, since a valid previous image is running,
    /// but the consumer must not accept a new offer while the attempt record
    /// exists (and while a report is undelivered).
    /// Re-read the update slot and call [`reconcile`] again at runtime if
    /// desired.
    Defer,
}

/// Experimental: API may change before 1.0.
///
/// Decide what to do about an OTA attempt at boot.
///
/// Rules:
/// - no attempt: [`ReconcileAction::NoAttempt`].
/// - the attempted slot is running (versions match only if both are `Some`
///   and equal):
///   - match, `PendingVerify`: [`ReconcileAction::AwaitHealthCheck`]; `Valid`:
///     [`ReconcileAction::CompleteAttempt`]; `Invalid`/`Unknown`:
///     [`ReconcileAction::ClearAttempt`].
///   - mismatch, `PendingVerify`: [`ReconcileAction::RefuseImage`]; other
///     states: [`ReconcileAction::ClearAttempt`] (serial flash over the slot).
/// - otherwise the previous image runs:
///   - `activated` or `boot_selected`: [`ReconcileAction::ReportRollback`]
///     regardless of `update_slot` (the boot slot was switched to the target,
///     yet the previous image runs: the bootloader or the app rolled back).
///     This includes a new image that crashed before `activated` was
///     persisted into a slot that was already `Invalid`.
///   - otherwise `update_slot` is `None`: [`ReconcileAction::Defer`].
///   - otherwise the target slot became `Invalid` during the attempt
///     (`!slot_was_invalid`): [`ReconcileAction::ReportRollback`] (covers
///     the window between the internal boot-slot switch and persisting
///     `boot_selected`).
///   - otherwise [`ReconcileAction::ClearAttempt`] (download interrupted
///     before the boot slot was switched).
///
/// See the [module docs](self) for an example.
pub fn reconcile(attempt: Option<&AttemptRecord>, facts: &BootFacts) -> ReconcileAction {
    let Some(attempt) = attempt else {
        return ReconcileAction::NoAttempt;
    };

    if attempt.slot == facts.running_slot {
        let pending = facts.running_state == SlotState::PendingVerify;
        let matches = attempt.version.is_some() && attempt.version == facts.running_version;
        return if matches {
            match facts.running_state {
                SlotState::PendingVerify => ReconcileAction::AwaitHealthCheck {
                    mark_activated: !attempt.activated,
                },
                SlotState::Valid => ReconcileAction::CompleteAttempt,
                SlotState::Invalid | SlotState::Unknown => ReconcileAction::ClearAttempt,
            }
        } else if pending {
            ReconcileAction::RefuseImage {
                promised: attempt.version,
                mark_activated: !attempt.activated,
            }
        } else {
            ReconcileAction::ClearAttempt
        };
    }

    let rollback = ReconcileAction::ReportRollback {
        attempt_id: attempt.attempt_id,
        left: attempt.version,
        report_already_persisted: facts.report_persisted,
    };
    if attempt.activated || attempt.boot_selected {
        return rollback;
    }
    match facts.update_slot {
        None => ReconcileAction::Defer,
        Some((slot, SlotState::Invalid)) if slot == attempt.slot && !attempt.slot_was_invalid => {
            rollback
        }
        Some(_) => ReconcileAction::ClearAttempt,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const V1: Version = Version::new(1, 0, 0);
    const V2: Version = Version::new(2, 0, 0);
    const ALL_STATES: [SlotState; 4] = [
        SlotState::Valid,
        SlotState::PendingVerify,
        SlotState::Invalid,
        SlotState::Unknown,
    ];
    const BOOLS: [bool; 2] = [false, true];

    fn attempt(activated: bool, slot_was_invalid: bool) -> AttemptRecord {
        AttemptRecord {
            attempt_id: 1,
            version: Some(V2),
            slot: SlotId(1),
            boot_selected: false,
            activated,
            slot_was_invalid,
        }
    }

    fn selected_attempt(slot_was_invalid: bool) -> AttemptRecord {
        AttemptRecord {
            boot_selected: true,
            ..attempt(false, slot_was_invalid)
        }
    }

    /// Running slot 0 (previous image) unless `running_slot` says otherwise;
    /// the update slot is the other one.
    fn facts(
        running_slot: u8,
        running_state: SlotState,
        running_version: Option<Version>,
        update_state: Option<SlotState>,
        report_persisted: bool,
    ) -> BootFacts {
        BootFacts {
            running_slot: SlotId(running_slot),
            running_state,
            running_version,
            update_slot: update_state.map(|s| (SlotId(1 - running_slot), s)),
            report_persisted,
        }
    }

    fn update_slot_variants() -> [Option<SlotState>; 5] {
        [
            None,
            Some(SlotState::Valid),
            Some(SlotState::PendingVerify),
            Some(SlotState::Invalid),
            Some(SlotState::Unknown),
        ]
    }

    #[test]
    fn no_attempt() {
        let f = facts(
            0,
            SlotState::Valid,
            Some(V1),
            Some(SlotState::Invalid),
            true,
        );
        assert_eq!(reconcile(None, &f), ReconcileAction::NoAttempt);
    }

    #[test]
    fn same_slot_version_match_and_mismatch_tables() {
        // (attempt.version, running_version, matches)
        let cases = [
            (Some(V2), Some(V2), true),
            (Some(V2), Some(V1), false),
            (None, Some(V2), false),
            (Some(V2), None, false),
            (None, None, false),
        ];
        for (promised, running, matches) in cases {
            for activated in BOOLS {
                for state in ALL_STATES {
                    let mut a = attempt(activated, false);
                    a.version = promised;
                    let f = facts(1, state, running, Some(SlotState::Valid), false);
                    let pending = state == SlotState::PendingVerify;
                    let expected = match (matches, pending) {
                        (true, true) => ReconcileAction::AwaitHealthCheck {
                            mark_activated: !activated,
                        },
                        (false, true) => ReconcileAction::RefuseImage {
                            promised,
                            mark_activated: !activated,
                        },
                        (true, false) if state == SlotState::Valid => {
                            ReconcileAction::CompleteAttempt
                        }
                        (_, false) => ReconcileAction::ClearAttempt,
                    };
                    assert_eq!(
                        reconcile(Some(&a), &f),
                        expected,
                        "promised={promised:?} running={running:?} \
                         activated={activated} state={state:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn same_slot_branch_ignores_other_facts() {
        for running in [Some(V2), Some(V1), None] {
            for state in ALL_STATES {
                let baseline = {
                    let f = facts(1, state, running, Some(SlotState::Valid), false);
                    reconcile(Some(&attempt(false, false)), &f)
                };
                for was_invalid in BOOLS {
                    for pending in BOOLS {
                        for update in update_slot_variants() {
                            let f = facts(1, state, running, update, pending);
                            assert_eq!(
                                reconcile(Some(&attempt(false, was_invalid)), &f),
                                baseline,
                                "running={running:?} state={state:?} \
                                 was_invalid={was_invalid} pending={pending} update={update:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn previous_image_running_table() {
        // (activated, slot_was_invalid, update_state, expected_kind)
        // kind: 'R' report, 'C' clear, 'D' defer
        let cases = [
            (true, false, Some(SlotState::Valid), 'R'),
            (true, true, Some(SlotState::Invalid), 'R'),
            (true, false, None, 'R'),
            (true, true, None, 'R'),
            (false, false, Some(SlotState::Invalid), 'R'),
            // Slot was already invalid: Invalid state is not evidence.
            (false, true, Some(SlotState::Invalid), 'C'),
            (false, false, Some(SlotState::Valid), 'C'),
            (false, false, Some(SlotState::PendingVerify), 'C'),
            (false, false, Some(SlotState::Unknown), 'C'),
            (false, false, None, 'D'),
            (false, true, None, 'D'),
        ];
        for (activated, was_invalid, update_state, kind) in cases {
            for report_persisted in BOOLS {
                let expected = match kind {
                    'R' => ReconcileAction::ReportRollback {
                        attempt_id: 1,
                        left: Some(V2),
                        report_already_persisted: report_persisted,
                    },
                    'C' => ReconcileAction::ClearAttempt,
                    _ => ReconcileAction::Defer,
                };
                // Invariant over the running image's own state and version.
                for state in ALL_STATES {
                    for running in [Some(V1), Some(V2), None] {
                        let f = facts(0, state, running, update_state, report_persisted);
                        assert_eq!(
                            reconcile(Some(&attempt(activated, was_invalid)), &f),
                            expected,
                            "activated={activated} was_invalid={was_invalid} \
                             update={update_state:?} pending={report_persisted} \
                             state={state:?} running={running:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn boot_selected_always_reports_rollback_when_previous_image_runs() {
        for was_invalid in BOOLS {
            for update in update_slot_variants() {
                for persisted in BOOLS {
                    let f = facts(0, SlotState::Valid, Some(V1), update, persisted);
                    assert_eq!(
                        reconcile(Some(&selected_attempt(was_invalid)), &f),
                        ReconcileAction::ReportRollback {
                            attempt_id: 1,
                            left: Some(V2),
                            report_already_persisted: persisted
                        },
                        "was_invalid={was_invalid} update={update:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn without_boot_selected_old_behaviour_is_kept() {
        let f = facts(0, SlotState::Valid, Some(V1), Some(SlotState::Valid), false);
        assert_eq!(
            reconcile(Some(&attempt(false, false)), &f),
            ReconcileAction::ClearAttempt
        );
    }

    /// Reviewer scenario: a new image crashed before persisting `activated`
    /// into a slot that was already `Invalid` (the usual state after any
    /// rollback). Without `boot_selected` this was cleared and the failed
    /// version re-offered in a loop.
    #[test]
    fn reviewer_scenario_crash_before_activation_into_invalid_slot_reports() {
        let f = facts(
            0,
            SlotState::Valid,
            Some(V1),
            Some(SlotState::Invalid),
            false,
        );
        let a = AttemptRecord {
            boot_selected: true,
            activated: false,
            slot_was_invalid: true,
            ..attempt(false, true)
        };
        assert_eq!(
            reconcile(Some(&a), &f),
            ReconcileAction::ReportRollback {
                attempt_id: 1,
                left: Some(V2),
                report_already_persisted: false
            }
        );
    }

    #[test]
    fn report_rollback_carries_unparseable_version_as_none() {
        let mut a = attempt(true, false);
        a.version = None;
        let f = facts(0, SlotState::Valid, Some(V1), None, false);
        assert_eq!(
            reconcile(Some(&a), &f),
            ReconcileAction::ReportRollback {
                attempt_id: 1,
                left: None,
                report_already_persisted: false
            }
        );
    }

    #[test]
    fn defer_only_when_not_activated_and_update_slot_unknown() {
        let f = facts(0, SlotState::Valid, Some(V1), None, false);
        assert_eq!(
            reconcile(Some(&attempt(false, false)), &f),
            ReconcileAction::Defer
        );
        assert_eq!(
            reconcile(Some(&attempt(true, false)), &f),
            ReconcileAction::ReportRollback {
                attempt_id: 1,
                left: Some(V2),
                report_already_persisted: false
            }
        );
    }

    #[test]
    fn invalid_update_slot_other_than_attempt_slot_is_not_evidence() {
        let f = BootFacts {
            running_slot: SlotId(0),
            running_state: SlotState::Valid,
            running_version: Some(V1),
            update_slot: Some((SlotId(1), SlotState::Invalid)),
            report_persisted: false,
        };
        let mut a = attempt(false, false);
        assert!(matches!(
            reconcile(Some(&a), &f),
            ReconcileAction::ReportRollback { .. }
        ));
        a.slot = SlotId(2);
        assert_eq!(reconcile(Some(&a), &f), ReconcileAction::ClearAttempt);
    }

    /// Boot 1 refuses the wrong image, the action is applied to the record,
    /// boot 2 runs the previous image with the target slot Invalid.
    fn two_boot(slot_was_invalid: bool, report_persisted_boot1: bool) -> ReconcileAction {
        let mut record = attempt(false, slot_was_invalid);

        let boot1 = facts(
            1,
            SlotState::PendingVerify,
            Some(V1),
            Some(SlotState::Valid),
            report_persisted_boot1,
        );
        let action = reconcile(Some(&record), &boot1);
        assert_eq!(
            action,
            ReconcileAction::RefuseImage {
                promised: Some(V2),
                mark_activated: true
            }
        );
        if let ReconcileAction::RefuseImage { mark_activated, .. } = action {
            record.activated |= mark_activated;
        }

        let boot2 = facts(
            0,
            SlotState::Valid,
            Some(V1),
            Some(SlotState::Invalid),
            report_persisted_boot1,
        );
        reconcile(Some(&record), &boot2)
    }

    #[test]
    fn refuse_image_activation_must_persist_for_two_boot_report() {
        assert_eq!(
            two_boot(true, false),
            ReconcileAction::ReportRollback {
                attempt_id: 1,
                left: Some(V2),
                report_already_persisted: false
            }
        );

        // Without the persisted activation the rollback would be swallowed.
        let boot2 = facts(
            0,
            SlotState::Valid,
            Some(V1),
            Some(SlotState::Invalid),
            false,
        );
        assert_eq!(
            reconcile(Some(&attempt(false, true)), &boot2),
            ReconcileAction::ClearAttempt
        );
    }

    #[test]
    fn two_boot_sequence_with_slot_not_previously_invalid() {
        assert_eq!(
            two_boot(false, false),
            ReconcileAction::ReportRollback {
                attempt_id: 1,
                left: Some(V2),
                report_already_persisted: false
            }
        );
    }

    #[test]
    fn report_persisted_does_not_change_refusal_and_propagates_to_boot_two() {
        assert_eq!(
            two_boot(true, true),
            ReconcileAction::ReportRollback {
                attempt_id: 1,
                left: Some(V2),
                report_already_persisted: true
            }
        );
    }
}
