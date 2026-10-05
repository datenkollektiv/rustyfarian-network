//! Platform-independent OTA primitives — Version parsing, streaming SHA-256,
//! sidecar metadata, backend-neutral state machine, update decision policy,
//! offer decisions (with a refused-version loop guard), boot reconciliation,
//! and the cooperative download deadline.
//!
//! The wire contract (command, manifest, status JSON, reason codes) lives in [`wire`] behind the `ota-wire` feature.
//! The consumer runtime's pure state machines (command intake, worker gates, health policy, report delivery, manifest limits, configuration) live in [`runtime`] behind the same feature; only its configuration and verdict types are re-exported flat.
//!
//! All public APIs are experimental.

pub mod deadline;
pub mod decision;
pub mod error;
pub mod metadata;
#[cfg(feature = "ota-wire")]
pub mod persist;
pub mod reconcile;
#[cfg(feature = "ota-wire")]
pub mod runtime;
pub mod state;
pub mod verifier;
pub mod version;
#[cfg(feature = "ota-wire")]
pub mod wire;

pub use deadline::{ActivationPermit, Deadline};
pub use decision::{
    decide_offer, decide_update, Admission, BlockedBy, OfferDecision, UpdateDecision,
};
pub use error::{classify_read_error, OtaError};
pub use metadata::ImageMetadata;
#[cfg(feature = "ota-wire")]
pub use persist::{
    note_boot_slot, reconcile_boot, slot_for_partition, slot_from_label, slot_label,
    BootDisposition, BootFault, BootOutcome, BootWarnings, BusyTally, CorruptRecord, Delivery,
    FailedDownload, HardwareFacts, HardwareReadError, KvError, KvStr, NoteBoot, NoteBootError,
    OtaKv, OtaStore, PendingReport, PendingReportView, ReasonNote, RefusalNote, RepairedRecord,
    ReportRecord, RollbackNoted, RollbackRequest, SettleOutcome, StoreError, StoredAttempt,
    UpdateSlot, FACTORY_SLOT, NVS_NAMESPACE,
};
pub use reconcile::{reconcile, AttemptRecord, BootFacts, ReconcileAction, SlotId, SlotState};
#[cfg(feature = "ota-wire")]
pub use runtime::{
    ConfigError, DeadlineAction, HealthVerdict, OtaSettings, OtaStacks, OtaTimings,
    MIN_STACK_BYTES, MIN_WORKER_STACK_BYTES,
};
pub use state::OtaState;
pub use verifier::{bytes_to_hex, hex_to_bytes, StreamingVerifier};
pub use version::Version;
#[cfg(feature = "ota-wire")]
pub use wire::{
    CommandError, FailReason, Manifest, ManifestError, OtaCommand, OtaStatus, RollbackReason,
    StatusError, MAX_COMMAND_BYTES, MAX_MANIFEST_BYTES,
};
