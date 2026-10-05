//! Persisted OTA records: the attempt, the rollback report, the operator-rollback request and the refused version.
//!
//! All record semantics, write ordering and boot orchestration live here as pure generic code over the key-level [`OtaKv`] trait, so the crash-consistency rules are host-tested with fault injection.
//! A platform crate supplies only the key-value backend (on ESP-IDF, NVS).
//! This module never reboots, never marks an image valid and never publishes.
//!
//! # Layout of namespace [`NVS_NAMESPACE`]
//!
//! | Key | Type | Meaning |
//! |:----|:-----|:--------|
//! | `rb` | u8 | 1 while a report awaits delivery, 0 once delivered; any other value reads as not pending |
//! | `rb_why` | str | reason of the report, kept raw, kept after delivery |
//! | `rb_id` | u32 | attempt or request id of the report, kept after delivery |
//! | `att_ctr` | u32 | id counter, bumped before an id is handed out, never removed |
//! | `att_epoch` | u32 | install epoch, created with the first id, never removed |
//! | `att_id` | u32 | id of the attempt |
//! | `att_ver` | str | promised version; written third from last, removed first (hide marker) |
//! | `att_slot` | str | partition label of the attempt; written last (commit marker) |
//! | `att_boot` | u8 | 1 once the boot slot was switched |
//! | `att_act` | u8 | 1 once the attempted image was booted |
//! | `att_inv` | u8 | 1 when the target slot was already `Invalid` |
//! | `att_why` | str | why a rollback of the attempt started, first reason wins |
//! | `rq_id` | u32 | event id of an operator rollback without an attempt |
//! | `rq_why` | str | reason of that rollback |
//! | `rq_from` | str | slot label the rollback leaves; written last (arm marker), removed first |
//! | `rej_ver` | str | version last left by a rollback, refused until a different one is offered |
//!
//! An attempt exists iff `att_ver` and `att_slot` exist, a request iff `rq_from` exists, a report iff `rb` exists.
//!
//! # Layout rule
//!
//! Layout changes are additive only: existing keys keep their type and meaning forever, and an incompatible change must use a new namespace.
//! So a rollback from a newer image to this build never wedges.
//!
//! # Fail closed
//!
//! A read error or a corrupt record (unknown slot label) means: no write of any kind, admission closed, never mark valid.
//! [`OtaStore::repair_corrupt`] is the explicit, caller-triggered exit from a corrupt record.
//! Unreadable reason keys (`att_why`, `rq_why`, `rb_why`) are the exception: they read as the label `"unknown"` and never fail a boot.

pub mod boot;
pub mod keys;
pub mod kv;
pub mod store;

pub use boot::{
    note_boot_slot, reconcile_boot, BootDisposition, BootFault, BootOutcome, BootWarnings,
    BusyTally, HardwareFacts, HardwareReadError, NoteBoot, NoteBootError, UpdateSlot,
};
pub use keys::{slot_for_partition, slot_from_label, slot_label, FACTORY_SLOT};
pub use kv::{KvError, KvStr, OtaKv};
pub use store::{
    CorruptRecord, Delivery, FailedDownload, OtaStore, PendingReport, PendingReportView,
    ReasonNote, RefusalNote, RepairedRecord, ReportRecord, RollbackNoted, RollbackRequest,
    SettleOutcome, StoreError, StoredAttempt,
};

/// The NVS namespace of the records.
pub const NVS_NAMESPACE: &str = "ota";

#[cfg(test)]
pub(in crate::ota) mod fault_kv;
#[cfg(test)]
mod tests;
