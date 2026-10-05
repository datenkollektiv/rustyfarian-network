//! The record store: all record semantics and write ordering over an [`OtaKv`].
//!
//! Rule for every sequence below: one commit per key operation, the listed order IS the crash order, an error stops the sequence with `?`.

use core::fmt::Write as _;

use super::boot::HardwareFacts;
use super::keys as k;
use super::keys::{slot_from_label, slot_label, FACTORY_SLOT};
use super::kv::{KvError, KvStr, OtaKv};
use crate::ota::wire::RollbackReason;
use crate::ota::{
    reconcile, Admission, AttemptRecord, BlockedBy, BootFacts, ReconcileAction, SlotId, Version,
};

/// A record shape that cannot be interpreted and is not healed automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorruptRecord {
    /// `att_slot` holds a label that is not `ota_0` or `ota_1`.
    UnknownAttemptSlotLabel,
    /// `rq_from` holds a label that is not `ota_0`, `ota_1` or `factory`.
    UnknownRequestSlotLabel,
    /// `att_ver` and `att_slot` exist but `att_id` does not; the id is stored before the attempt is committed, so no write sequence leaves this.
    AttemptWithoutId,
    /// `rq_from` exists but `rq_id` does not; the id is stored before the request is armed, so no write sequence leaves this.
    RequestWithoutId,
}

/// Why a store operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreError {
    /// The backend failed.
    Kv(KvError),
    /// A record is corrupt; see [`OtaStore::repair_corrupt`].
    Corrupt(CorruptRecord),
    /// A new attempt was refused because admission is blocked.
    NotAdmitted(BlockedBy),
    /// Another report is still undelivered.
    ReportPending,
    /// The slot has no stored label or cannot be an attempt target.
    InvalidSlot,
}

impl core::fmt::Display for CorruptRecord {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            CorruptRecord::UnknownAttemptSlotLabel => {
                f.write_str("attempt record names an unknown slot")
            }
            CorruptRecord::UnknownRequestSlotLabel => {
                f.write_str("rollback request names an unknown slot")
            }
            CorruptRecord::AttemptWithoutId => f.write_str("attempt record has no id"),
            CorruptRecord::RequestWithoutId => f.write_str("rollback request has no id"),
        }
    }
}

impl core::fmt::Display for StoreError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            StoreError::Kv(e) => write!(f, "OTA store backend failed: {e}"),
            StoreError::Corrupt(c) => write!(f, "OTA record corrupt: {c}"),
            StoreError::NotAdmitted(BlockedBy::AttemptUnresolved) => {
                f.write_str("OTA not admitted: an attempt is unresolved")
            }
            StoreError::NotAdmitted(BlockedBy::ReportPending) => {
                f.write_str("OTA not admitted: a report is undelivered")
            }
            StoreError::ReportPending => f.write_str("another OTA report is still undelivered"),
            StoreError::InvalidSlot => f.write_str("slot cannot be stored as an OTA target"),
        }
    }
}

impl core::error::Error for CorruptRecord {}

impl core::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            StoreError::Kv(e) => Some(e),
            StoreError::Corrupt(c) => Some(c),
            StoreError::NotAdmitted(_) | StoreError::ReportPending | StoreError::InvalidSlot => {
                None
            }
        }
    }
}

impl From<KvError> for StoreError {
    fn from(e: KvError) -> Self {
        StoreError::Kv(e)
    }
}

/// The persisted attempt with its raw rollback reason.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredAttempt {
    /// The attempt as the decision core consumes it.
    pub record: AttemptRecord,
    /// `att_why` exactly as stored (not parsed), if present.
    pub why: Option<KvStr>,
}

/// The persisted rollback report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportRecord {
    /// The report awaits delivery (`rb == 1`; any other value reads as not pending).
    pub pending: bool,
    /// Reason exactly as stored, `"unknown"` when absent.
    pub why: KvStr,
    /// Attempt or request id the report belongs to.
    pub attempt_id: Option<u32>,
}

/// A report ready to publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingReport {
    /// Event id; deduplicate downstream on `(epoch, attempt_id)`.
    pub attempt_id: u32,
    /// Install epoch.
    pub epoch: u32,
    /// Raw reason label, exactly as stored.
    pub why: KvStr,
}

impl PendingReport {
    /// The reason parsed leniently: a label this build does not know reads as [`RollbackReason::Unknown`].
    ///
    /// `why` keeps the raw text for diagnostics.
    pub fn reason(&self) -> RollbackReason {
        RollbackReason::from_label(&self.why)
    }
}

/// What [`OtaStore::pending_report`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PendingReportView {
    /// Nothing to deliver.
    None,
    /// A report is stored but its attempt still exists: boot reconciliation has not finished.
    AwaitingBootReconcile,
    /// Publish this report.
    Ready(PendingReport),
}

/// An armed operator-rollback request (a rollback without an attempt).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RollbackRequest {
    /// Event id reserved for the report.
    pub id: u32,
    /// Raw reason label.
    pub why: KvStr,
    /// The slot the rollback leaves.
    pub from: SlotId,
}

/// What a [`ReasonNote::Written`] wrote, and the only way to undo it with [`OtaStore::withdraw_rollback`].
///
/// The token cannot be cloned or built outside this module, so a caller can only withdraw a reason that its own `note_rollback` call wrote.
/// A call that kept an earlier reason ([`ReasonNote::KeptFirst`]) hands out no token, so the first reason or an older request is never withdrawn by mistake.
///
/// ```compile_fail
/// use juggler::ota::persist::RollbackNoted;
///
/// let _ = RollbackNoted { target: 0 };
/// ```
///
/// ```compile_fail
/// use juggler::ota::persist::RollbackNoted;
///
/// fn needs_clone<T: Clone>() {}
/// needs_clone::<RollbackNoted>();
/// ```
#[must_use = "pass the token to `withdraw_rollback` if the rollback fails to start"]
#[derive(Debug, PartialEq, Eq)]
pub struct RollbackNoted {
    target: NotedTarget,
}

#[derive(Debug, PartialEq, Eq)]
enum NotedTarget {
    /// `att_why` of the attempt on this slot.
    AttemptReason { slot: SlotId },
    /// The request armed with this id, leaving this slot.
    Request { slot: SlotId, id: u32 },
}

/// Result of [`OtaStore::note_rollback`].
#[derive(Debug, PartialEq, Eq)]
pub enum ReasonNote {
    /// The reason was recorded; the token undoes exactly this write.
    Written(RollbackNoted),
    /// A reason was already recorded; the first one wins and there is nothing to withdraw.
    KeptFirst,
}

/// Result of [`OtaStore::mark_report_delivered`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delivery {
    /// The pending report for that id is now marked delivered.
    Marked,
    /// No report is pending for any id (none stored, already delivered, or a pending report that has no id); nothing was written.
    NotPending,
    /// A different report is pending; nothing was written.
    OtherReport {
        /// The id of the report that is still pending.
        pending_id: u32,
    },
}

/// Result of [`OtaStore::settle_failed_download`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailedDownload {
    /// No attempt exists; nothing was written.
    NoAttempt,
    /// The decision core said [`ReconcileAction::ClearAttempt`] and the attempt was cleared.
    Cleared,
    /// The attempt was kept because the decision core asked for another action; nothing was written.
    ///
    /// The next boot's [`reconcile_boot`](super::reconcile_boot) acts on it.
    Kept(ReconcileAction),
}

/// Result of [`OtaStore::refuse_version`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalNote {
    /// The version is now refused; `previous` is what was refused before.
    Written {
        /// The refusal that was replaced.
        previous: Option<Version>,
    },
    /// The version was already refused.
    AlreadyRefused,
}

/// Result of [`OtaStore::settle_request`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettleOutcome {
    /// No request was armed.
    NoRequest,
    /// The request's slot is running, so no rollback happened; the request was dropped.
    ///
    /// A refusal written by [`OtaStore::refuse_version`] for that rollback is intentionally KEPT: the request stores no version to compare, and a different version being successfully validated (cleared by `complete_attempt`) lifts it.
    DroppedNotRolledBack,
    /// A report for this request already exists; the request was removed.
    AlreadyReported,
    /// Another report is undelivered; the request was removed and its reason is lost (v1 keeps one report).
    MergedIntoPending,
    /// A report was persisted for the request and the request was removed.
    Reported,
}

/// What [`OtaStore::repair_corrupt`] repaired.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairedRecord {
    /// The attempt record was removed (counter and epoch are kept).
    Attempt,
    /// The request record was removed.
    Request,
    /// `rb` and / or `rb_id` were unreadable: `rb_id` was removed and `rb` was re-armed as pending (`rb = 1`), so the report is delivered again rather than dropped.
    Report,
    /// `att_ctr` and / or `att_epoch` were unreadable: the epoch was removed (a new one is drawn with the next id) and the counter was reset to the highest id still stored.
    Counter,
    /// `rej_ver` was unreadable and was removed.
    Refusal,
}

/// The stored type of a key, for corruption probes.
#[derive(Debug, Clone, Copy)]
enum KeyType {
    U8,
    U32,
    Str,
}

const ATTEMPT_KEYS: [(&str, KeyType); 6] = [
    (k::ATT_VER, KeyType::Str),
    (k::ATT_SLOT, KeyType::Str),
    (k::ATT_ID, KeyType::U32),
    (k::ATT_BOOT, KeyType::U8),
    (k::ATT_ACT, KeyType::U8),
    (k::ATT_INV, KeyType::U8),
];
const REQUEST_KEYS: [(&str, KeyType); 2] = [(k::RQ_FROM, KeyType::Str), (k::RQ_ID, KeyType::U32)];
const REPORT_KEYS: [(&str, KeyType); 2] = [(k::RB, KeyType::U8), (k::RB_ID, KeyType::U32)];
const COUNTER_KEYS: [(&str, KeyType); 2] =
    [(k::ATT_CTR, KeyType::U32), (k::ATT_EPOCH, KeyType::U32)];

/// Whether a read failure means the stored value itself is bad (not a transient flash error).
fn is_corrupt_code(e: &KvError) -> bool {
    matches!(
        e.code,
        KvError::CODE_TYPE_MISMATCH | KvError::CODE_INVALID_VALUE
    )
}

/// Records as read at boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BootRecords {
    pub attempt: Option<StoredAttempt>,
    pub report_persisted: bool,
}

/// The `ota` namespace record store.
///
/// All state is in the backend; the store holds only the entropy source for the install epoch.
pub struct OtaStore<K: OtaKv> {
    kv: K,
    entropy: fn() -> u32,
}

impl<K: OtaKv + core::fmt::Debug> core::fmt::Debug for OtaStore<K> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("OtaStore")
            .field("kv", &self.kv)
            .finish_non_exhaustive()
    }
}

fn label_str(text: &str) -> KvStr {
    KvStr::try_from(text).unwrap_or_default()
}

fn version_str(version: Version) -> KvStr {
    let mut out = KvStr::new();
    let _ = write!(out, "{version}");
    out
}

impl<K: OtaKv> OtaStore<K> {
    /// Wraps a backend; performs no reads and no writes.
    pub fn open(kv: K, entropy: fn() -> u32) -> Self {
        Self { kv, entropy }
    }

    /// Returns the backend.
    pub fn into_kv(self) -> K {
        self.kv
    }

    // ---- reads ---------------------------------------------------------

    /// Reads a reason key; a mistyped, over-long or non-ASCII value reads as the label `"unknown"` (never a fault).
    ///
    /// Other errors (a transient flash failure) still fail.
    fn reason(&self, key: &str) -> Result<Option<KvStr>, KvError> {
        match self.kv.get_str(key) {
            Err(e) if is_corrupt_code(&e) => Ok(Some(label_str("unknown"))),
            other => other,
        }
    }

    /// Whether reading `key` as `ty` fails because the stored value is bad; a transient error is returned.
    fn corrupt_read(&self, key: &str, ty: KeyType) -> Result<bool, StoreError> {
        let read = match ty {
            KeyType::U8 => self.kv.get_u8(key).map(drop),
            KeyType::U32 => self.kv.get_u32(key).map(drop),
            KeyType::Str => self.kv.get_str(key).map(drop),
        };
        match read {
            Ok(()) => Ok(false),
            Err(e) if is_corrupt_code(&e) => Ok(true),
            Err(e) => Err(e.into()),
        }
    }

    fn any_corrupt(&self, keys: &[(&str, KeyType)]) -> Result<bool, StoreError> {
        for &(key, ty) in keys {
            if self.corrupt_read(key, ty)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Removes `key` when its stored value is bad, so a following `set` cannot hit a type mismatch; no commit otherwise.
    fn scrub(&mut self, key: &str, ty: KeyType) -> Result<(), StoreError> {
        if self.corrupt_read(key, ty)? {
            self.kv.remove(key)?;
        }
        Ok(())
    }

    fn flag(&self, key: &str) -> Result<bool, StoreError> {
        Ok(self.kv.get_u8(key)? == Some(1))
    }

    fn attempt_exists(&self) -> Result<bool, StoreError> {
        Ok(self.kv.get_str(k::ATT_VER)?.is_some() && self.kv.get_str(k::ATT_SLOT)?.is_some())
    }

    /// The attempt's slot, `None` when no attempt exists.
    fn attempt_slot(&self) -> Result<Option<SlotId>, StoreError> {
        if self.kv.get_str(k::ATT_VER)?.is_none() {
            return Ok(None);
        }
        let Some(label) = self.kv.get_str(k::ATT_SLOT)? else {
            return Ok(None);
        };
        match slot_from_label(&label) {
            Some(slot) if slot != FACTORY_SLOT => Ok(Some(slot)),
            _ => Err(StoreError::Corrupt(CorruptRecord::UnknownAttemptSlotLabel)),
        }
    }

    /// Reads the attempt record.
    pub fn read_attempt(&self) -> Result<Option<StoredAttempt>, StoreError> {
        let Some(version) = self.kv.get_str(k::ATT_VER)? else {
            return Ok(None);
        };
        let Some(slot) = self.attempt_slot()? else {
            return Ok(None);
        };
        let Some(attempt_id) = self.kv.get_u32(k::ATT_ID)? else {
            return Err(StoreError::Corrupt(CorruptRecord::AttemptWithoutId));
        };
        Ok(Some(StoredAttempt {
            record: AttemptRecord {
                attempt_id,
                version: Version::parse(&version).ok(),
                slot,
                boot_selected: self.flag(k::ATT_BOOT)?,
                activated: self.flag(k::ATT_ACT)?,
                slot_was_invalid: self.flag(k::ATT_INV)?,
            },
            why: self.reason(k::ATT_WHY)?,
        }))
    }

    /// Reads the rollback report record; `None` when `rb` is absent.
    pub fn report(&self) -> Result<Option<ReportRecord>, StoreError> {
        let Some(rb) = self.kv.get_u8(k::RB)? else {
            return Ok(None);
        };
        Ok(Some(ReportRecord {
            pending: rb == 1,
            why: self
                .reason(k::RB_WHY)?
                .unwrap_or_else(|| label_str("unknown")),
            attempt_id: self.kv.get_u32(k::RB_ID)?,
        }))
    }

    /// Whether a report awaits delivery.
    pub fn report_undelivered(&self) -> Result<bool, StoreError> {
        self.flag(k::RB)
    }

    fn request_armed(&self) -> Result<bool, StoreError> {
        Ok(self.kv.get_str(k::RQ_FROM)?.is_some())
    }

    /// Reads the operator-rollback request.
    pub fn request(&self) -> Result<Option<RollbackRequest>, StoreError> {
        let Some(from) = self.kv.get_str(k::RQ_FROM)? else {
            return Ok(None);
        };
        let from = slot_from_label(&from)
            .ok_or(StoreError::Corrupt(CorruptRecord::UnknownRequestSlotLabel))?;
        let Some(id) = self.kv.get_u32(k::RQ_ID)? else {
            return Err(StoreError::Corrupt(CorruptRecord::RequestWithoutId));
        };
        Ok(Some(RollbackRequest {
            id,
            why: self
                .reason(k::RQ_WHY)?
                .unwrap_or_else(|| label_str("unknown")),
            from,
        }))
    }

    /// The refused version; an unparseable value reads as absent.
    ///
    /// A read error is only a loop guard: callers fail open on it.
    pub fn refused(&self) -> Result<Option<Version>, StoreError> {
        Ok(self
            .kv
            .get_str(k::REJ_VER)?
            .and_then(|s| Version::parse(&s).ok()))
    }

    /// The admission state; any unreadable or unresolved record blocks.
    ///
    /// An unreadable record (including `rb`) maps to `Blocked(AttemptUnresolved)`, wire `attempt_unresolved`.
    /// An armed rollback request also blocks admission (`Blocked(AttemptUnresolved)`): the request must settle at the next boot first.
    pub fn admission(&self) -> Admission {
        let unresolved = match (self.attempt_exists(), self.request_armed()) {
            (Ok(attempt), Ok(request)) => attempt || request,
            _ => true,
        };
        if unresolved {
            return Admission::Blocked(BlockedBy::AttemptUnresolved);
        }
        match self.report_undelivered() {
            Ok(undelivered) => Admission::from_records(false, undelivered),
            Err(_) => Admission::Blocked(BlockedBy::AttemptUnresolved),
        }
    }

    // ---- ids -----------------------------------------------------------

    /// The highest id still stored anywhere among `keys`; a read error is returned (fail closed).
    fn highest_id(&self, keys: &[&str]) -> Result<u32, StoreError> {
        let mut highest = 0;
        for key in keys {
            highest = highest.max(self.kv.get_u32(key)?.unwrap_or(0));
        }
        Ok(highest)
    }

    /// reserves the next attempt id (never 0), creating the install epoch first.
    ///
    /// The id is one above the highest of `att_ctr`, `att_id`, `rb_id` and `rq_id`, so a lost or lowered counter never reissues an id that a record (for example a delivered report) still carries.
    /// A read error of any of them fails the call before anything is written.
    /// After a `u32` wrap ids repeat; that is about four billion attempts away.
    pub fn next_attempt_id(&mut self) -> Result<u32, StoreError> {
        self.epoch()?;
        let floor = self.highest_id(&[k::ATT_CTR, k::ATT_ID, k::RB_ID, k::RQ_ID])?;
        let mut next = floor.wrapping_add(1);
        if next == 0 {
            next = 1;
        }
        self.kv.set_u32(k::ATT_CTR, next)?;
        Ok(next)
    }

    /// The install epoch; created from the entropy source if absent.
    pub fn epoch(&mut self) -> Result<u32, StoreError> {
        if let Some(epoch) = self.kv.get_u32(k::ATT_EPOCH)? {
            return Ok(epoch);
        }
        let epoch = (self.entropy)();
        self.kv.set_u32(k::ATT_EPOCH, epoch)?;
        Ok(epoch)
    }

    // ---- attempt -------------------------------------------------------

    /// stores a new attempt after reserving its id; returns the id.
    ///
    /// Refused with [`StoreError::NotAdmitted`] unless [`OtaStore::admission`] is open, so an armed request is never deleted here.
    pub fn begin_attempt(
        &mut self,
        version: Version,
        slot: SlotId,
        slot_was_invalid: bool,
    ) -> Result<u32, StoreError> {
        if let Admission::Blocked(by) = self.admission() {
            return Err(StoreError::NotAdmitted(by));
        }
        let label = match slot_label(slot) {
            Some(label) if slot != FACTORY_SLOT => label,
            _ => return Err(StoreError::InvalidSlot),
        };
        let id = self.next_attempt_id()?;
        for key in [
            k::ATT_WHY,
            k::ATT_SLOT,
            k::ATT_VER,
            k::ATT_ACT,
            k::ATT_BOOT,
            k::ATT_ID,
        ] {
            self.kv.remove(key)?;
        }
        self.kv.set_u32(k::ATT_ID, id)?;
        self.kv.set_u8(k::ATT_INV, u8::from(slot_was_invalid))?;
        self.kv.set_str(k::ATT_VER, &version_str(version))?;
        self.kv.set_str(k::ATT_SLOT, label)?;
        Ok(id)
    }

    /// the boot slot was switched to the attempt's slot.
    pub fn mark_boot_selected(&mut self) -> Result<(), StoreError> {
        Ok(self.kv.set_u8(k::ATT_BOOT, 1)?)
    }

    /// the attempted image was booted.
    pub fn mark_activated(&mut self) -> Result<(), StoreError> {
        Ok(self.kv.set_u8(k::ATT_ACT, 1)?)
    }

    /// removes the attempt; keeps the counter, epoch, report, request and refusal.
    ///
    /// `att_ver` goes first: it hides the attempt.
    pub fn clear_attempt(&mut self) -> Result<(), StoreError> {
        for key in [
            k::ATT_VER,
            k::ATT_SLOT,
            k::ATT_ACT,
            k::ATT_BOOT,
            k::ATT_INV,
            k::ATT_ID,
            k::ATT_WHY,
        ] {
            self.kv.remove(key)?;
        }
        Ok(())
    }

    /// the attempted image is running and was marked valid; drops a different or corrupt refusal, clears the attempt.
    ///
    /// A transient read error of the refusal returns the error without any write and keeps the attempt (fail closed); the next boot repeats the sequence.
    ///
    /// The caller must mark the image valid BEFORE calling this.
    pub fn complete_attempt(&mut self, running: Version) -> Result<(), StoreError> {
        match self.kv.get_str(k::REJ_VER) {
            Ok(Some(refused)) if Version::parse(&refused).ok() == Some(running) => {}
            Ok(Some(_)) => self.kv.remove(k::REJ_VER)?,
            Err(e) if is_corrupt_code(&e) => self.kv.remove(k::REJ_VER)?,
            Err(e) => return Err(e.into()),
            Ok(None) => {}
        }
        self.clear_attempt()?;
        Ok(())
    }

    // ---- report --------------------------------------------------------

    /// P: persists the single report record for `id`.
    fn persist_report(&mut self, id: u32, why: &str) -> Result<(), StoreError> {
        if self.flag(k::RB)? {
            if let Some(other) = self.kv.get_u32(k::RB_ID)? {
                if other != id {
                    return Err(StoreError::ReportPending);
                }
            }
        }
        self.kv.remove(k::RB_ID)?;
        self.scrub(k::RB_WHY, KeyType::Str)?;
        self.kv.set_str(k::RB_WHY, why)?;
        self.kv.set_u8(k::RB, 1)?;
        self.kv.set_u32(k::RB_ID, id)?;
        Ok(())
    }

    /// persists the report, then the refusal, then clears the attempt.
    ///
    /// `already` means the report for this attempt is persisted.
    /// Any error keeps the attempt; the next boot repeats the sequence.
    pub fn report_rollback(
        &mut self,
        attempt_id: u32,
        left: Option<Version>,
        already: bool,
    ) -> Result<(), StoreError> {
        if !already {
            let why = self
                .reason(k::ATT_WHY)?
                .unwrap_or_else(|| label_str("bootloader"));
            self.persist_report(attempt_id, &why)?;
        }
        if let Some(left) = left {
            self.scrub(k::REJ_VER, KeyType::Str)?;
            self.kv.set_str(k::REJ_VER, &version_str(left))?;
        }
        self.clear_attempt()
    }

    /// marks the report for `attempt_id` delivered; keeps `rb_why` and `rb_id`.
    ///
    /// The outcome says what happened; only [`Delivery::Marked`] wrote anything.
    pub fn mark_report_delivered(&mut self, attempt_id: u32) -> Result<Delivery, StoreError> {
        if !self.flag(k::RB)? {
            return Ok(Delivery::NotPending);
        }
        match self.kv.get_u32(k::RB_ID)? {
            Some(id) if id == attempt_id => {
                self.kv.set_u8(k::RB, 0)?;
                Ok(Delivery::Marked)
            }
            Some(pending_id) => Ok(Delivery::OtherReport { pending_id }),
            None => Ok(Delivery::NotPending),
        }
    }

    /// The report to publish, adopting an owner id for an `rb` without `rb_id` (a power loss between the two writes of the report).
    pub fn pending_report(&mut self) -> Result<PendingReportView, StoreError> {
        if !self.flag(k::RB)? {
            return Ok(PendingReportView::None);
        }
        let owner = self.kv.get_u32(k::RB_ID)?;
        if self.attempt_exists()? {
            // A report that already names another id than the attempt belongs to an earlier event: publish it, or it would block the attempt's own report for good.
            let own = match owner {
                None => true,
                Some(id) => self.kv.get_u32(k::ATT_ID)? == Some(id),
            };
            if own {
                return Ok(PendingReportView::AwaitingBootReconcile);
            }
        }
        let id = match owner {
            Some(id) => id,
            None => {
                let id = match self.kv.get_u32(k::RQ_ID)? {
                    Some(id) if self.request_armed()? => id,
                    _ => self.next_attempt_id()?,
                };
                self.kv.set_u32(k::RB_ID, id)?;
                id
            }
        };
        let why = self
            .reason(k::RB_WHY)?
            .unwrap_or_else(|| label_str("unknown"));
        let epoch = self.epoch()?;
        Ok(PendingReportView::Ready(PendingReport {
            attempt_id: id,
            epoch,
            why,
        }))
    }

    // ---- rollback reasons and requests ----------------------------------

    /// records why a rollback started; the first reason wins.
    ///
    /// With an attempt on `running_slot` the reason goes to `att_why`, otherwise an operator request is armed.
    pub fn note_rollback(
        &mut self,
        why: RollbackReason,
        running_slot: SlotId,
    ) -> Result<ReasonNote, StoreError> {
        if self.attempt_slot()? == Some(running_slot) {
            if self.reason(k::ATT_WHY)?.is_some() {
                return Ok(ReasonNote::KeptFirst);
            }
            self.kv.set_str(k::ATT_WHY, why.as_str())?;
            return Ok(ReasonNote::Written(RollbackNoted {
                target: NotedTarget::AttemptReason { slot: running_slot },
            }));
        }
        if self.request_armed()? {
            return Ok(ReasonNote::KeptFirst);
        }
        let from = match slot_label(running_slot) {
            Some(label) => label,
            None => return Err(StoreError::InvalidSlot),
        };
        self.scrub(k::RQ_WHY, KeyType::Str)?;
        let id = self.next_attempt_id()?;
        self.kv.set_u32(k::RQ_ID, id)?;
        self.kv.set_str(k::RQ_WHY, why.as_str())?;
        self.kv.set_str(k::RQ_FROM, from)?;
        Ok(ReasonNote::Written(RollbackNoted {
            target: NotedTarget::Request {
                slot: running_slot,
                id,
            },
        }))
    }

    /// Undoes the write of a [`ReasonNote::Written`] after the rollback itself failed.
    ///
    /// Removes exactly what that call wrote: the attempt's reason, or the request it armed.
    /// If the record is already gone, or is no longer the one the token describes (another attempt, another request), nothing is removed.
    pub fn withdraw_rollback(&mut self, noted: RollbackNoted) -> Result<(), StoreError> {
        match noted.target {
            NotedTarget::AttemptReason { slot } => {
                if self.attempt_slot()? == Some(slot) {
                    self.kv.remove(k::ATT_WHY)?;
                }
            }
            NotedTarget::Request { slot, id } => {
                if let Some(request) = self.request()? {
                    if request.id == id && request.from == slot {
                        self.remove_request()?;
                    }
                }
            }
        }
        Ok(())
    }

    fn remove_request(&mut self) -> Result<(), StoreError> {
        self.kv.remove(k::RQ_FROM)?;
        self.kv.remove(k::RQ_WHY)?;
        self.kv.remove(k::RQ_ID)?;
        Ok(())
    }

    /// turns an armed request into a report, or drops it when no rollback happened.
    ///
    /// Call it after [`OtaStore::mark_report_delivered`] to reopen admission.
    /// An error leaves the request armed and admission blocked.
    pub fn settle_request(&mut self, running_slot: SlotId) -> Result<SettleOutcome, StoreError> {
        let request = match self.request()? {
            None => return Ok(SettleOutcome::NoRequest),
            Some(request) => request,
        };
        if request.from == running_slot {
            self.remove_request()?;
            return Ok(SettleOutcome::DroppedNotRolledBack);
        }
        let pending = self.flag(k::RB)?;
        let report_id = self.kv.get_u32(k::RB_ID)?;
        if report_id == Some(request.id) {
            self.remove_request()?;
            return Ok(SettleOutcome::AlreadyReported);
        }
        if pending && report_id.is_none() {
            self.kv.set_u32(k::RB_ID, request.id)?;
            self.remove_request()?;
            return Ok(SettleOutcome::AlreadyReported);
        }
        if pending {
            self.remove_request()?;
            return Ok(SettleOutcome::MergedIntoPending);
        }
        self.persist_report(request.id, &request.why)?;
        self.remove_request()?;
        Ok(SettleOutcome::Reported)
    }

    /// Whether a report is pending for `attempt_id`.
    ///
    /// A delivered report (`rb != 1`) never counts, even with the same id: it belongs to an earlier event, and the attempt's own report must still be persisted.
    /// A pending report without an id never counts either (a power loss between the two writes of the report): the attempt's report is persisted again.
    fn report_persisted_for(&self, attempt_id: u32) -> Result<bool, StoreError> {
        let Some(rb) = self.kv.get_u8(k::RB)? else {
            return Ok(false);
        };
        Ok(self.kv.get_u32(k::RB_ID)? == Some(attempt_id) && rb == 1)
    }

    /// After a failed `fetch_and_apply`: clears the attempt only when the decision core says [`ReconcileAction::ClearAttempt`].
    ///
    /// Builds the same `BootFacts` as boot reconciliation (`hw`, `running_version`, and the report fact from the store), runs [`reconcile`], and performs only the clear.
    /// A boot-time action (health check, refusal, rollback report, defer) is left to the next boot and returned as [`FailedDownload::Kept`].
    /// A rollback request is not settled here; that is the boot's job.
    ///
    /// Fails closed: a read error returns before any write and the attempt stays.
    /// The clear is the usual `clear_attempt` order (`att_ver` first), so a power loss mid-clear leaves either the whole attempt or a hidden one, and a repeated call finishes it.
    ///
    /// # Errors
    ///
    /// [`StoreError::Kv`] on a read or write failure; the attempt is then kept or half-cleared as described above.
    pub fn settle_failed_download(
        &mut self,
        hw: HardwareFacts,
        running_version: Option<Version>,
    ) -> Result<FailedDownload, StoreError> {
        let attempt = match self.read_attempt()? {
            None => return Ok(FailedDownload::NoAttempt),
            Some(attempt) => attempt,
        };
        let facts = BootFacts {
            running_slot: hw.running_slot,
            running_state: hw.running_state,
            running_version,
            update_slot: hw.update_slot.facts(),
            report_persisted: self.report_persisted_for(attempt.record.attempt_id)?,
        };
        match reconcile(Some(&attempt.record), &facts) {
            ReconcileAction::ClearAttempt => {
                self.clear_attempt()?;
                Ok(FailedDownload::Cleared)
            }
            other => Ok(FailedDownload::Kept(other)),
        }
    }

    // ---- refusal -------------------------------------------------------

    /// refuses `version` until a different version is successfully validated (cleared by `complete_attempt`); merely offering a different version does not lift the refusal.
    pub fn refuse_version(&mut self, version: Version) -> Result<RefusalNote, StoreError> {
        let previous = match self.kv.get_str(k::REJ_VER)? {
            Some(s) => Version::parse(&s).ok(),
            None => None,
        };
        if previous == Some(version) {
            return Ok(RefusalNote::AlreadyRefused);
        }
        self.kv.set_str(k::REJ_VER, &version_str(version))?;
        Ok(RefusalNote::Written { previous })
    }

    /// Restores the refusal that [`OtaStore::refuse_version`] replaced.
    pub fn restore_refusal(&mut self, previous: Option<Version>) -> Result<(), StoreError> {
        match previous {
            Some(version) => self.kv.set_str(k::REJ_VER, &version_str(version))?,
            None => self.kv.remove(k::REJ_VER)?,
        }
        Ok(())
    }

    // ---- repair --------------------------------------------------------

    /// Explicit exit from a wedge caused by unreadable records; transient flash errors still fail closed.
    ///
    /// Never called by the boot path: the runtime calls it as an explicit diagnostic action after a boot ended in `FailedClosed(BootFault::Store(..))` with a `StoreError::Corrupt` or a `KvError` of code `CODE_TYPE_MISMATCH` / `CODE_INVALID_VALUE`.
    /// A transient flash error (any other code) is returned unchanged and nothing is repaired.
    ///
    /// Each call repairs at most ONE record group and says which; call it until it returns `Ok(None)`.
    /// Groups, in the order they are checked, and the repair of each:
    ///
    /// | Unreadable | Repair |
    /// |:-----------|:-------|
    /// | an attempt key (`att_ver`, `att_slot`, `att_id`, `att_boot`, `att_act`, `att_inv`) or an unknown `att_slot` label | the whole attempt is cleared in the usual order; counter, epoch, report, request and refusal are kept |
    /// | `rq_from` (also an unknown label) or `rq_id` | the request is removed; its reason (`rq_why`) goes with it |
    /// | `rb` | re-armed as pending (`rb = 1`, one overwrite): an unreadable flag is treated as an undelivered report, which is delivered again (deduplicated downstream) rather than dropped; the reason and id are kept |
    /// | `rb_id` | removed; a pending report is re-adopted with a fresh id, so downstream may see it once more |
    /// | `att_ctr` / `att_epoch` | the epoch is removed first (a new one is drawn with the next id) and an unreadable counter is reset to the highest readable `att_id` / `rq_id` / `rb_id` |
    /// | `rej_ver` | removed (the loop guard is lost, which is what an unreadable refusal already meant) |
    ///
    /// Reason keys (`att_why`, `rq_why`, `rb_why`) are never a fault: an unreadable one reads as the label `"unknown"` and is replaced the next time it is written.
    ///
    /// Crash behaviour: every repair is a sequence of single-key removes in an order that stays detectable, so a power loss mid-repair is finished by the next call.
    /// `rb` is re-armed with ONE `set_u8` that replaces the mistyped entry (see [`OtaKv`]'s contract), so a power loss leaves either the unreadable `rb` (the next call repairs it again) or `rb = 1`; the report is never dropped.
    /// On ESP-IDF this relies on `nvs_set_u8` replacing a key of another type: `Storage::writeItem` (`components/nvs_flash/src/nvs_storage.cpp`, ESP-IDF v5.3.3) looks the key up with `ItemType::ANY` (line 397), writes the new entry (line 474), then erases the old one with `ItemType::ANY` (line 514), and returns `ESP_ERR_NVS_TYPE_MISMATCH` only from reads (`nvs_page.cpp` lines 266 and 317), never from a write; only `CONFIG_NVS_LEGACY_DUP_KEYS_COMPATIBILITY=y` keeps the old entry as an unreachable duplicate, which a typed read never sees.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Kv`] on a transient read or write failure.
    pub fn repair_corrupt(&mut self) -> Result<Option<RepairedRecord>, StoreError> {
        if self.any_corrupt(&ATTEMPT_KEYS)? || self.attempt_label_corrupt()? {
            self.clear_attempt()?;
            return Ok(Some(RepairedRecord::Attempt));
        }
        if self.any_corrupt(&REQUEST_KEYS)? || self.request_label_corrupt()? {
            self.remove_request()?;
            return Ok(Some(RepairedRecord::Request));
        }
        if self.any_corrupt(&REPORT_KEYS)? {
            if self.corrupt_read(k::RB_ID, KeyType::U32)? {
                self.kv.remove(k::RB_ID)?;
            }
            if self.corrupt_read(k::RB, KeyType::U8)? {
                self.kv.set_u8(k::RB, 1)?;
            }
            return Ok(Some(RepairedRecord::Report));
        }
        if self.any_corrupt(&COUNTER_KEYS)? {
            self.repair_counter()?;
            return Ok(Some(RepairedRecord::Counter));
        }
        if self.corrupt_read(k::REJ_VER, KeyType::Str)? {
            self.kv.remove(k::REJ_VER)?;
            return Ok(Some(RepairedRecord::Refusal));
        }
        Ok(None)
    }

    /// Whether [`OtaStore::repair_corrupt`] would repair something, without writing anything.
    ///
    /// Checks the same groups in the same order (attempt keys and label, request keys and label, report keys, counter keys, refusal).
    /// A transient read error is returned as an error, never as "nothing corrupt": the caller cannot tell the two apart otherwise.
    ///
    /// The runtime uses it to tell "an operator repair is needed" from "nothing to do" before a repair command may write.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::Kv`] on a transient read failure.
    pub fn corrupt_present(&self) -> Result<bool, StoreError> {
        Ok(self.any_corrupt(&ATTEMPT_KEYS)?
            || self.attempt_label_corrupt()?
            || self.any_corrupt(&REQUEST_KEYS)?
            || self.request_label_corrupt()?
            || self.any_corrupt(&REPORT_KEYS)?
            || self.any_corrupt(&COUNTER_KEYS)?
            || self.corrupt_read(k::REJ_VER, KeyType::Str)?)
    }

    fn attempt_label_corrupt(&self) -> Result<bool, StoreError> {
        match self.attempt_slot() {
            Err(StoreError::Corrupt(_)) => Ok(true),
            Err(other) => Err(other),
            Ok(Some(_)) => Ok(self.kv.get_u32(k::ATT_ID)?.is_none()),
            Ok(None) => Ok(false),
        }
    }

    fn request_label_corrupt(&self) -> Result<bool, StoreError> {
        match self.request() {
            Err(StoreError::Corrupt(_)) => Ok(true),
            Err(other) => Err(other),
            Ok(_) => Ok(false),
        }
    }

    /// Epoch first, so ids issued after a half-done repair never repeat under the old epoch.
    fn repair_counter(&mut self) -> Result<(), StoreError> {
        if self.corrupt_read(k::ATT_EPOCH, KeyType::U32)?
            || self.corrupt_read(k::ATT_CTR, KeyType::U32)?
        {
            self.kv.remove(k::ATT_EPOCH)?;
        }
        if self.corrupt_read(k::ATT_CTR, KeyType::U32)? {
            let highest = self.highest_id(&[k::ATT_ID, k::RB_ID, k::RQ_ID])?;
            self.kv.remove(k::ATT_CTR)?;
            if highest > 0 {
                self.kv.set_u32(k::ATT_CTR, highest)?;
            }
        }
        Ok(())
    }

    // ---- boot phases (used by `reconcile_boot`) --------------------------

    /// Phase R: reads every record; any error or corrupt record aborts before a single write.
    pub(crate) fn read_phase(&self) -> Result<BootRecords, StoreError> {
        let attempt = self.read_attempt()?;
        self.request()?;
        self.report()?;
        // The refusal is only a loop guard: an unreadable value must not block boot.
        let _ = self.refused();
        let report_persisted = match &attempt {
            Some(attempt) => self.report_persisted_for(attempt.record.attempt_id)?,
            None => false,
        };
        Ok(BootRecords {
            attempt,
            report_persisted,
        })
    }
}
