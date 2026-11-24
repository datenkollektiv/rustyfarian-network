//! The DDI action record: the loop guard of a DDI action and its persisted terminal verdict.
//!
//! One record, keys `ddi_act`, `ddi_stage`, `ddi_att`, `ddi_code`, `ddi_det0` and `ddi_det1` of the `ota` namespace.
//! The record exists iff `ddi_stage` exists; `ddi_stage` is the commit marker, written last and removed first.
//!
//! | Stage | Meaning |
//! |:------|:--------|
//! | 1 running | the action was admitted; when `ddi_att` equals `att_id` the attempt belongs to it, and the store writes its verdict before it clears that attempt |
//! | 2 verdict pending | the terminal verdict (`ddi_code`, details) awaits delivery; no new offer is taken |
//! | 3 closed | the verdict was delivered, or the server no longer knows the action; kept as the loop guard |
//!
//! An action whose id is `ddi_act` at stage 2 or 3 is closed and never applied again.
//! The verdict of a DDI-bound attempt is written inside the store's own sequences, always before the attempt is cleared: [`OtaStore::complete_attempt`] writes success (so does the boot sequence when the attempted image runs with app rollback disabled), [`OtaStore::report_rollback`] writes 1004 `rolled_back`, and [`OtaStore::settle_failed_ddi_download`] writes the download failure.
//! A verdict is not delivered while its attempt still exists: the next boot finishes that sequence first.
//! A verdict for an action without an attempt (a refusal, `records_unreadable`) is written by [`OtaStore::persist_verdict`].
//!
//! The raw record rules here are part of every build with the store, so a build without `ota-ddi` still probes, repairs and folds the keys a DDI image left behind; the typed API needs the `ota-ddi` feature.

use super::keys as k;
use super::store::{CorruptRecord, KeyType, OtaStore, StoreError};
use super::{KvStr, OtaKv};

/// `ddi_code` of a success verdict.
const CODE_SUCCESS: u32 = 0;
/// `ddi_code` of a rollback (profile code 1004).
const CODE_ROLLED_BACK: u32 = 1004;
/// `details[0]` of a rollback verdict.
const DETAIL_ROLLED_BACK: &str = "rolled_back";

const DDI_KEYS: [(&str, KeyType); 4] = [
    (k::DDI_ACT, KeyType::Str),
    (k::DDI_STAGE, KeyType::U8),
    (k::DDI_ATT, KeyType::U32),
    (k::DDI_CODE, KeyType::U32),
];

fn known_code(code: u32) -> bool {
    code == CODE_SUCCESS || (1001..=1007).contains(&code)
}

/// The DDI action record as stored, already checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct DdiRecord {
    pub stage: u8,
    pub act: KvStr,
    pub att: Option<u32>,
    pub code: Option<u32>,
}

impl<K: OtaKv> OtaStore<K> {
    /// Reads the DDI action record; `None` when `ddi_stage` is absent.
    ///
    /// Every key but the details is read and checked even without a stage, so a mistyped or invalid leftover is corrupt and fails closed.
    pub(super) fn ddi_record(&self) -> Result<Option<DdiRecord>, StoreError> {
        let stage = self.kv.get_u8(k::DDI_STAGE)?;
        let act = self.kv.get_str(k::DDI_ACT)?;
        let att = self.kv.get_u32(k::DDI_ATT)?;
        let code = self.kv.get_u32(k::DDI_CODE)?;
        if let Some(stage) = stage {
            if !(k::STAGE_RUNNING..=k::STAGE_CLOSED).contains(&stage) {
                return Err(StoreError::Corrupt(CorruptRecord::DdiStageInvalid));
            }
        }
        if act
            .as_ref()
            .is_some_and(|act| !k::is_canonical_action_id(act))
        {
            return Err(StoreError::Corrupt(CorruptRecord::DdiActionInvalid));
        }
        if code.is_some_and(|code| !known_code(code)) {
            return Err(StoreError::Corrupt(CorruptRecord::DdiVerdictInvalid));
        }
        let Some(stage) = stage else {
            return Ok(None);
        };
        let Some(act) = act else {
            return Err(StoreError::Corrupt(CorruptRecord::DdiActionInvalid));
        };
        if stage == k::STAGE_VERDICT_PENDING && code.is_none() {
            return Err(StoreError::Corrupt(CorruptRecord::DdiVerdictInvalid));
        }
        Ok(Some(DdiRecord {
            stage,
            act,
            att,
            code,
        }))
    }

    /// Whether the attempt `attempt_id` belongs to a running DDI action, so its verdict must be written before the attempt is cleared.
    pub(super) fn ddi_bound_to(&self, attempt_id: u32) -> Result<bool, StoreError> {
        Ok(self
            .ddi_record()?
            .is_some_and(|r| r.stage == k::STAGE_RUNNING && r.att == Some(attempt_id)))
    }

    /// Success: the code, then the stage; `ddi_det0` keeps the success detail written when the attempt began.
    pub(super) fn write_ddi_success(&mut self) -> Result<(), StoreError> {
        self.kv.set_u32(k::DDI_CODE, CODE_SUCCESS)?;
        self.kv.set_u8(k::DDI_STAGE, k::STAGE_VERDICT_PENDING)?;
        Ok(())
    }

    /// 1004 with the details `rolled_back` and `why`.
    pub(super) fn write_ddi_rolled_back(&mut self, why: &str) -> Result<(), StoreError> {
        self.write_ddi_verdict(CODE_ROLLED_BACK, Some(DETAIL_ROLLED_BACK), Some(why))
    }

    /// A failure `code` with `detail` as `details[0]`.
    pub(super) fn write_ddi_failure(&mut self, code: u32, detail: &str) -> Result<(), StoreError> {
        self.write_ddi_verdict(code, Some(detail), None)
    }

    /// The code, the details, then the stage (commit marker); the action and attempt binding stay.
    fn write_ddi_verdict(
        &mut self,
        code: u32,
        det0: Option<&str>,
        det1: Option<&str>,
    ) -> Result<(), StoreError> {
        self.kv.set_u32(k::DDI_CODE, code)?;
        self.put_detail(k::DDI_DET0, det0)?;
        self.put_detail(k::DDI_DET1, det1)?;
        self.kv.set_u8(k::DDI_STAGE, k::STAGE_VERDICT_PENDING)?;
        Ok(())
    }

    fn put_detail(&mut self, key: &str, detail: Option<&str>) -> Result<(), StoreError> {
        match detail {
            Some(detail) => {
                self.scrub(key, KeyType::Str)?;
                self.kv.set_str(key, detail)?;
            }
            None => self.kv.remove(key)?,
        }
        Ok(())
    }

    /// Whether the DDI action record is unreadable; a transient read error is returned.
    pub(super) fn ddi_corrupt(&self) -> Result<bool, StoreError> {
        if self.any_corrupt(&DDI_KEYS)? {
            return Ok(true);
        }
        match self.ddi_record() {
            Err(StoreError::Corrupt(_)) => Ok(true),
            Err(other) => Err(other),
            Ok(_) => Ok(false),
        }
    }

    /// Removes the whole record, stage first, so a power loss mid-removal leaves no record or a still-detectable corrupt one.
    pub(super) fn remove_ddi_record(&mut self) -> Result<(), StoreError> {
        for key in [
            k::DDI_STAGE,
            k::DDI_ACT,
            k::DDI_ATT,
            k::DDI_CODE,
            k::DDI_DET0,
            k::DDI_DET1,
        ] {
            self.kv.remove(key)?;
        }
        Ok(())
    }
}

#[cfg(feature = "ota-ddi")]
pub use typed::{DdiVerdict, VerdictDelivery, VerdictNote, VerdictResult};

#[cfg(feature = "ota-ddi")]
mod typed {
    use super::*;
    use crate::ota::ddi::wire::{ActionId, Finished, ProfileCode};
    use crate::ota::ddi::{AdmittedOffer, CancelTarget, OfferGate, Refusal};
    use crate::ota::persist::boot::HardwareFacts;
    use crate::ota::persist::keys::{slot_label, FACTORY_SLOT};
    use crate::ota::persist::store::label_str;
    use crate::ota::persist::FailedDownload;
    use crate::ota::wire::FailReason;
    use crate::ota::{Admission, BlockedBy, SlotId, Version};

    /// `details[0]` of a verdict for an offer that arrived while the records were unreadable.
    const DETAIL_RECORDS_UNREADABLE: &str = "records_unreadable";

    /// Calls of [`OtaStore::repair_corrupt`] before a `records_unreadable` verdict: one per record group, with room to spare.
    const REPAIR_PASSES: usize = 8;

    /// Experimental: API may change before 1.0.
    ///
    /// The result of a DDI verdict: `closed` + `success`, or `closed` + `failure` with a profile code.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum VerdictResult {
        /// `finished: success`, no code.
        Success,
        /// `finished: failure` with this code.
        Failure(ProfileCode),
    }

    /// Experimental: API may change before 1.0.
    ///
    /// The terminal verdict of a DDI action: what its `closed` feedback reports.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub struct DdiVerdict {
        /// The action the feedback goes to.
        pub action: ActionId,
        /// Success, or failure with its code.
        pub result: VerdictResult,
        /// The feedback details in order, at most two of at most 32 bytes: the reason first.
        pub details: heapless::Vec<KvStr, 2>,
    }

    impl DdiVerdict {
        fn new(action: ActionId, result: VerdictResult, details: &[&str]) -> Self {
            Self {
                action,
                result,
                details: details.iter().map(|d| label_str(d)).collect(),
            }
        }

        /// The verdict of a refused deployment: its code and reason.
        pub fn refused(refusal: &Refusal) -> Self {
            Self::new(
                refusal.action.clone(),
                VerdictResult::Failure(refusal.code),
                &[refusal.reason.as_str()],
            )
        }

        /// The verdict of an action that failed for `reason` without an attempt, or `None` when the reason is never reported ([`ProfileCode::for_reason`]).
        pub fn failed(action: ActionId, reason: FailReason) -> Option<Self> {
            let code = ProfileCode::for_reason(reason)?;
            Some(Self::new(
                action,
                VerdictResult::Failure(code),
                &[reason.as_str()],
            ))
        }

        /// The 1005 `records_unreadable` verdict for an action offered while the OTA records could not be read.
        pub fn records_unreadable(action: ActionId) -> Self {
            Self::new(
                action,
                VerdictResult::Failure(ProfileCode::Device),
                &[DETAIL_RECORDS_UNREADABLE],
            )
        }

        /// `status.result.finished` of the feedback.
        pub fn finished(&self) -> Finished {
            match self.result {
                VerdictResult::Success => Finished::Success,
                VerdictResult::Failure(_) => Finished::Failure,
            }
        }

        /// `status.code` of the feedback; `None` for success.
        pub fn code(&self) -> Option<ProfileCode> {
            match self.result {
                VerdictResult::Success => None,
                VerdictResult::Failure(code) => Some(code),
            }
        }

        /// The details as `&str`, for [`Feedback::details`](crate::ota::ddi::wire::Feedback::details).
        pub fn detail_strs(&self) -> heapless::Vec<&str, 2> {
            self.details.iter().map(|d| d.as_str()).collect()
        }

        fn is_records_unreadable(&self) -> bool {
            self.result == VerdictResult::Failure(ProfileCode::Device)
                && self.details.first().map(|d| d.as_str()) == Some(DETAIL_RECORDS_UNREADABLE)
        }

        fn stored_code(&self) -> u32 {
            match self.result {
                VerdictResult::Success => CODE_SUCCESS,
                VerdictResult::Failure(code) => u32::from(code.as_u16()),
            }
        }
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Result of [`OtaStore::persist_verdict`].
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum VerdictNote {
        /// The verdict is persisted and awaits delivery.
        Written,
        /// A verdict for the same action already awaits delivery; the first one is kept and nothing was written.
        KeptPending,
        /// The action is closed; nothing was written.
        AlreadyClosed,
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Result of [`OtaStore::mark_verdict_delivered`].
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum VerdictDelivery {
        /// The verdict of that action is delivered and the action is closed.
        Marked,
        /// No verdict awaits delivery; nothing was written.
        NotPending,
        /// The pending verdict belongs to another action; nothing was written.
        OtherAction,
    }

    impl<K: OtaKv> OtaStore<K> {
        /// The details as stored, in order; an unreadable detail reads as `"unknown"`.
        fn ddi_details(&self) -> Result<heapless::Vec<KvStr, 2>, StoreError> {
            let mut details = heapless::Vec::new();
            for key in [k::DDI_DET0, k::DDI_DET1] {
                if let Some(detail) = self.reason(key)? {
                    let _ = details.push(detail);
                }
            }
            Ok(details)
        }

        /// Experimental: API may change before 1.0.
        ///
        /// Whether the records let the device retrieve a new deployment, read at each poll answer.
        ///
        /// [`OfferGate::RecordsUnreadable`] when any record is corrupt ([`OtaStore::corrupt_present`]); [`OfferGate::Blocked`] while admission is blocked ([`OtaStore::admission`]: an attempt, a request or an undelivered report), while a DDI verdict awaits delivery, and on a transient read error.
        /// The store knows nothing of the running slot: the caller also blocks while the running image is pending verification.
        pub fn offer_gate(&self) -> OfferGate {
            match self.corrupt_present() {
                Ok(true) => return OfferGate::RecordsUnreadable,
                Ok(false) => {}
                Err(_) => return OfferGate::Blocked,
            }
            if self.admission() != Admission::Open {
                return OfferGate::Blocked;
            }
            match self.ddi_record() {
                Ok(Some(r)) if r.stage == k::STAGE_VERDICT_PENDING => OfferGate::Blocked,
                Ok(_) => OfferGate::Open,
                Err(_) => OfferGate::Blocked,
            }
        }

        /// Experimental: API may change before 1.0.
        ///
        /// The action whose attempt is running: the DDI record is at stage running and its `ddi_att` is the id of the stored attempt.
        ///
        /// `None` without a record, at any other stage, or when the bound attempt is gone (a cancelled download leaves the record at stage running with a dangling `ddi_att`); the runtime sends the post-boot `proceeding` for it and the cancel look-up names it.
        ///
        /// # Errors
        ///
        /// [`StoreError::Corrupt`] or [`StoreError::Kv`] when the attempt or the record cannot be read; the caller fails closed.
        pub fn running_ddi_action(&self) -> Result<Option<ActionId>, StoreError> {
            self.attempt_bound_action(&[k::STAGE_RUNNING])
        }

        /// The action of the DDI record when its stage is one of `stages` and its `ddi_att` is the id of the stored attempt; `None` otherwise.
        fn attempt_bound_action(&self, stages: &[u8]) -> Result<Option<ActionId>, StoreError> {
            let Some(attempt) = self.read_attempt()? else {
                return Ok(None);
            };
            let Some(r) = self.ddi_record()? else {
                return Ok(None);
            };
            if !stages.contains(&r.stage) || r.att != Some(attempt.record.attempt_id) {
                return Ok(None);
            }
            ActionId::parse(&r.act)
                .map(Some)
                .map_err(|_| StoreError::Corrupt(CorruptRecord::DdiActionInvalid))
        }

        /// Experimental: API may change before 1.0.
        ///
        /// What the device knows of the action a cancel stops (`CancelStep::LookUp`, fleet DDI profile section 5); `downloading` is the action whose download thread the tier runs now.
        ///
        /// In order: the running download of this action is [`CancelTarget::Downloading`]; the pending verdict of this action is [`CancelTarget::VerdictPending`]; an attempt bound to this action with no download thread (boot slot selected, awaiting its reboot or health check, or kept for the next boot) is [`CancelTarget::Activated`]; any read error is [`CancelTarget::Unreadable`]; everything else is [`CancelTarget::NotRunning`].
        ///
        /// A verdict that is written while its attempt could not be cleared yet ([`OtaStore::pending_verdict`] is `None` until it is) belongs to an attempt that still exists, so it is [`CancelTarget::Activated`] too: the image is installed, and a cancel must never read as "nothing ran".
        pub fn cancel_target(
            &self,
            stop_id: &ActionId,
            downloading: Option<&ActionId>,
        ) -> CancelTarget {
            if downloading == Some(stop_id) {
                return CancelTarget::Downloading;
            }
            match self.pending_verdict() {
                Err(_) => return CancelTarget::Unreadable,
                Ok(Some(verdict)) if verdict.action == *stop_id => {
                    return CancelTarget::VerdictPending;
                }
                Ok(_) => {}
            }
            match self.attempt_bound_action(&[k::STAGE_RUNNING, k::STAGE_VERDICT_PENDING]) {
                Err(_) => CancelTarget::Unreadable,
                Ok(Some(action)) if action == *stop_id => CancelTarget::Activated,
                Ok(_) => CancelTarget::NotRunning,
            }
        }

        /// Experimental: API may change before 1.0.
        ///
        /// The loop guard: whether `action` is closed (its verdict is pending or delivered), so it is never applied again.
        ///
        /// Only the last action is remembered; hawkBit never offers an older one again.
        ///
        /// # Errors
        ///
        /// [`StoreError::Corrupt`] or [`StoreError::Kv`] when the record cannot be read; the caller fails closed.
        pub fn is_action_closed(&self, action: &ActionId) -> Result<bool, StoreError> {
            Ok(self.ddi_record()?.is_some_and(|r| {
                r.act.as_str() == action.as_str()
                    && matches!(r.stage, k::STAGE_VERDICT_PENDING | k::STAGE_CLOSED)
            }))
        }

        /// Experimental: API may change before 1.0.
        ///
        /// Stores the attempt of an admitted offer and binds it to the offer's action; returns the attempt id.
        ///
        /// Write order: [`OtaStore::begin_attempt`] (every read of this call happens before or inside it), then `ddi_stage` removed (hides the previous record), the stale code and second detail removed, the success detail (`details[0]` of the later `closed` + `success`), `ddi_act` and `ddi_att` written, then `ddi_stage` = running.
        /// A power loss before the last write leaves the attempt (if written) unbound and never booted, so the next boot clears it and the action is offered again; the previous record's binding names another attempt, so it never takes this attempt's verdict.
        /// While the previous record is hidden its loop guard is gone; hawkBit never offers that closed action again, so nothing relies on it.
        ///
        /// # Errors
        ///
        /// Nothing is written on [`StoreError::NotAdmitted`] (admission blocked), [`StoreError::VerdictPending`] (a verdict awaits delivery), [`StoreError::ActionClosed`] (the loop guard) or [`StoreError::InvalidSlot`]; a read error writes at most the install epoch ([`OtaStore::begin_attempt`]) and leaves every DDI key untouched.
        pub fn begin_ddi_attempt(
            &mut self,
            offer: &AdmittedOffer,
            slot: SlotId,
            slot_was_invalid: bool,
        ) -> Result<u32, StoreError> {
            if let Admission::Blocked(by) = self.admission() {
                return Err(StoreError::NotAdmitted(by));
            }
            if let Some(r) = self.ddi_record()? {
                if r.stage == k::STAGE_VERDICT_PENDING {
                    return Err(StoreError::VerdictPending);
                }
                if r.stage == k::STAGE_CLOSED && r.act.as_str() == offer.action.as_str() {
                    return Err(StoreError::ActionClosed);
                }
            }
            if slot == FACTORY_SLOT || slot_label(slot).is_none() {
                return Err(StoreError::InvalidSlot);
            }
            let id = self.begin_attempt(offer.version, slot, slot_was_invalid)?;
            self.kv.remove(k::DDI_STAGE)?;
            for key in [k::DDI_CODE, k::DDI_DET1] {
                self.kv.remove(key)?;
            }
            self.put_detail(k::DDI_DET0, offer.success_detail.map(|r| r.as_str()))?;
            self.kv.set_str(k::DDI_ACT, offer.action.as_str())?;
            self.kv.set_u32(k::DDI_ATT, id)?;
            self.kv.set_u8(k::DDI_STAGE, k::STAGE_RUNNING)?;
            Ok(id)
        }

        /// Experimental: API may change before 1.0.
        ///
        /// Persists the verdict of an action that has no attempt: a refusal, a failure before the attempt began, or `records_unreadable`.
        ///
        /// Write order: `ddi_stage` removed, `ddi_att` removed, `ddi_act`, `ddi_code`, the details, then `ddi_stage` = verdict pending.
        /// A power loss before the last write leaves no DDI record and nothing is reported; the server offers the action again.
        ///
        /// A verdict that already awaits delivery wins: one for the same action is kept ([`VerdictNote::KeptPending`]), one for another action refuses this one with [`StoreError::VerdictPending`], and the server offers the refused action again once the first verdict is delivered.
        ///
        /// A [`DdiVerdict::records_unreadable`] verdict first repairs every corrupt record ([`OtaStore::repair_corrupt`] until none is left, a corrupt attempt or DDI record included), so it can be stored at all; the 1005 report stands for what the repair dropped.
        /// Once it is stored it also acknowledges the backend's lost-record finding ([`OtaKv::acknowledge_record_loss`]), which until then survives every reboot.
        ///
        /// # Errors
        ///
        /// Nothing is written on [`StoreError::NotAdmitted`] (an attempt exists: its verdict comes from its own sequence), [`StoreError::VerdictPending`], or a read error, except the repairs of a `records_unreadable` verdict.
        pub fn persist_verdict(&mut self, verdict: &DdiVerdict) -> Result<VerdictNote, StoreError> {
            let note = self.persist_verdict_records(verdict)?;
            if verdict.is_records_unreadable() {
                // The report is stored (or its action already answered): the backend's lost-record finding is discharged with it.
                self.kv.acknowledge_record_loss()?;
            }
            Ok(note)
        }

        fn persist_verdict_records(
            &mut self,
            verdict: &DdiVerdict,
        ) -> Result<VerdictNote, StoreError> {
            if verdict.is_records_unreadable() {
                // One group per call; the store has six groups, so this ends with none left or an error.
                for _ in 0..REPAIR_PASSES {
                    if self.repair_corrupt()?.is_none() {
                        break;
                    }
                }
            }
            if self.attempt_exists()? {
                return Err(StoreError::NotAdmitted(BlockedBy::AttemptUnresolved));
            }
            if let Some(r) = self.ddi_record()? {
                let same = r.act.as_str() == verdict.action.as_str();
                match r.stage {
                    k::STAGE_VERDICT_PENDING if same => return Ok(VerdictNote::KeptPending),
                    k::STAGE_VERDICT_PENDING => return Err(StoreError::VerdictPending),
                    k::STAGE_CLOSED if same => return Ok(VerdictNote::AlreadyClosed),
                    _ => {}
                }
            }
            self.kv.remove(k::DDI_STAGE)?;
            self.kv.remove(k::DDI_ATT)?;
            self.kv.set_str(k::DDI_ACT, verdict.action.as_str())?;
            let details = verdict.detail_strs();
            self.write_ddi_verdict(
                verdict.stored_code(),
                details.first().copied(),
                details.get(1).copied(),
            )?;
            Ok(VerdictNote::Written)
        }

        /// Experimental: API may change before 1.0.
        ///
        /// [`OtaStore::settle_failed_download`] for a DDI action: when the attempt is cleared and belongs to a running DDI action, its failure verdict (the code of `reason`, `details[0]` the reason) is written first.
        ///
        /// A reason without a profile code ([`ProfileCode::for_reason`] is `None`, a cancel) writes no verdict: the action stays open and the cancel feedback answers it.
        /// When the attempt is kept, no verdict is written either: the next boot decides the outcome.
        ///
        /// # Errors
        ///
        /// As [`OtaStore::settle_failed_download`].
        pub fn settle_failed_ddi_download(
            &mut self,
            hw: HardwareFacts,
            running_version: Option<Version>,
            reason: FailReason,
        ) -> Result<FailedDownload, StoreError> {
            let verdict = ProfileCode::for_reason(reason)
                .map(|code| (u32::from(code.as_u16()), reason.as_str()));
            self.settle_failed(hw, running_version, verdict)
        }

        /// Experimental: API may change before 1.0.
        ///
        /// The verdict that awaits delivery, if any.
        ///
        /// It is sent until the server answers 200, 404 or 410, then [`OtaStore::mark_verdict_delivered`] closes the action.
        /// `None` while an attempt still exists: its sequence (the next boot repeats it) finishes before the verdict goes out, so a delivered verdict never leaves a report behind that nothing clears.
        ///
        /// # Errors
        ///
        /// [`StoreError::Corrupt`] or [`StoreError::Kv`] when the record cannot be read.
        pub fn pending_verdict(&self) -> Result<Option<DdiVerdict>, StoreError> {
            let Some(r) = self.ddi_record()? else {
                return Ok(None);
            };
            if r.stage != k::STAGE_VERDICT_PENDING || self.attempt_exists()? {
                return Ok(None);
            }
            // `ddi_record` already checked the id (canonical) and the code (known, present at this stage), so these conversions cannot fail.
            let invalid = |record| StoreError::Corrupt(record);
            let action =
                ActionId::parse(&r.act).map_err(|_| invalid(CorruptRecord::DdiActionInvalid))?;
            let result = match r.code {
                Some(CODE_SUCCESS) => VerdictResult::Success,
                code => code
                    .and_then(|code| u16::try_from(code).ok())
                    .and_then(ProfileCode::from_u16)
                    .map(VerdictResult::Failure)
                    .ok_or(invalid(CorruptRecord::DdiVerdictInvalid))?,
            };
            Ok(Some(DdiVerdict {
                action,
                result,
                details: self.ddi_details()?,
            }))
        }

        /// Experimental: API may change before 1.0.
        ///
        /// The server answered the verdict of `action` with 200, 404 or 410: the action is closed.
        ///
        /// Write order: the rollback report of the bound attempt (`rb_id` equal to `ddi_att`, or a 1004 verdict's report whose `rb_id` a repair removed) is marked delivered first, then `ddi_stage` = closed.
        /// A power loss between the two sends the verdict once more, which the server deduplicates; the other order could leave the report pending and admission blocked for good.
        /// Code and details are kept after delivery.
        /// A delivered `records_unreadable` verdict also acknowledges the backend's lost-record finding, in case a power loss kept [`OtaStore::persist_verdict`] from doing so.
        /// [`VerdictDelivery::NotPending`] while an attempt still exists (see [`OtaStore::pending_verdict`]).
        pub fn mark_verdict_delivered(
            &mut self,
            action: &ActionId,
        ) -> Result<VerdictDelivery, StoreError> {
            let Some(r) = self.ddi_record()? else {
                return Ok(VerdictDelivery::NotPending);
            };
            if r.stage != k::STAGE_VERDICT_PENDING || self.attempt_exists()? {
                return Ok(VerdictDelivery::NotPending);
            }
            if r.act.as_str() != action.as_str() {
                return Ok(VerdictDelivery::OtherAction);
            }
            if let Some(att) = r.att {
                let report_of_this_attempt = match self.kv.get_u32(k::RB_ID)? {
                    Some(rb_id) => rb_id == att,
                    None => r.code == Some(CODE_ROLLED_BACK),
                };
                if report_of_this_attempt && self.flag(k::RB)? {
                    self.kv.set_u8(k::RB, 0)?;
                }
            }
            // A power loss between the 1005 verdict's write and its acknowledgement left the backend's lost-record finding set: this delivery answers it.
            if r.code == Some(u32::from(ProfileCode::Device.as_u16()))
                && self.ddi_details()?.first().map(|d| d.as_str())
                    == Some(DETAIL_RECORDS_UNREADABLE)
            {
                self.kv.acknowledge_record_loss()?;
            }
            self.kv.set_u8(k::DDI_STAGE, k::STAGE_CLOSED)?;
            Ok(VerdictDelivery::Marked)
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn stored_codes_match_the_profile_codes() {
            assert_eq!(
                CODE_ROLLED_BACK,
                u32::from(ProfileCode::RolledBack.as_u16())
            );
            for n in 1001..=1007u16 {
                assert!(known_code(u32::from(n)));
                assert!(ProfileCode::from_u16(n).is_some());
            }
            for n in [1u32, 1000, 1008, u32::MAX] {
                assert!(!known_code(n));
            }
        }
    }
}
