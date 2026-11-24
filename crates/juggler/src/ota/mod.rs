//! Platform-independent OTA primitives — Version parsing, streaming SHA-256,
//! sidecar metadata, backend-neutral state machine, update decision policy,
//! offer decisions (with a refused-version loop guard), boot reconciliation,
//! the cooperative download deadline, and the firmware download order
//! ([`download::DownloadMachine`], with the shared Content-Length admission rule
//! [`check_content_length`]).
//!
//! The MQTT wire contract (command, manifest, status JSON) lives in [`wire`] behind the `ota-wire` feature; its reason codes ([`FailReason`], [`RollbackReason`]) are part of the base feature.
//! The health policy of the running image and the record upkeep that follows it live in [`health`] (base feature), shared by the MQTT and the DDI runtime.
//! The rest of the consumer runtime's pure state machines (command intake, worker gates, report delivery, manifest limits, configuration) live in [`runtime`] behind `ota-wire`; only its configuration types are re-exported flat.
//! The sans-IO DDI client lives in [`ddi`] behind the `ota-ddi` feature: the poll, admission, feedback and cancel machines, the runtime settings ([`ddi::DdiSettings`]) and the JSON codec (poll, deployment and cancel documents, the feedback encoder, the profile codes); the two wires share [`FailReason`](wire::FailReason).
//! The persisted record store ([`persist`]) is part of the base feature, so the MQTT and the DDI runtime keep their records in the same `ota` namespace.
//! [`UrlForLog`] and [`TextForLog`] make a URL or a server-supplied string safe to log.
//!
//! All public APIs are experimental.

mod action_id;
#[cfg(feature = "ota-ddi")]
pub mod ddi;
pub mod deadline;
pub mod decision;
pub mod download;
pub mod error;
pub mod health;
mod log_text;
pub mod metadata;
pub mod persist;
pub mod reconcile;
#[cfg(feature = "ota-wire")]
#[deprecated(
    since = "0.8.0",
    note = "the MQTT OTA wire is deprecated and will be removed in a later release; use the DDI client (`ota-ddi`, ESP-IDF tier; esp-hal has no DDI client yet, ADR 022 gap)"
)]
pub mod runtime;
pub mod state;
pub mod verifier;
pub mod version;
pub mod wire;
pub mod write_align;

pub use deadline::{ActivationPermit, Deadline};
pub use decision::{
    decide_offer, decide_update, decide_version_gate, Admission, BlockedBy, GateReason,
    OfferDecision, UpdateDecision, VersionFacts, VersionGate,
};
pub use download::{
    bounded_read_request, capped_read_len, check_content_length, classify_follow_up, read_bounded,
    CancelRequest, FollowUp, ImageChip, ReadRequest, FOLLOW_UP_WAIT,
};
pub use error::{classify_read_error, OtaError};
pub use health::{DeadlineAction, HealthVerdict};
pub use log_text::{TextForLog, UrlForLog};
pub use metadata::ImageMetadata;
#[cfg(feature = "ota-flash-kv")]
pub use persist::FlashKv;
pub use persist::{
    note_boot_slot, read_with_busy_retry, reconcile_boot, slot_for_partition, slot_from_label,
    slot_label, BootDisposition, BootFault, BootOutcome, BootWarnings, BusyRetry, BusyTally,
    CorruptRecord, Delivery, FailedDownload, HardwareFacts, HardwareReadError, KvError, KvStr,
    NoteBoot, NoteBootError, OtaKv, OtaStore, PendingReport, PendingReportView, ReasonNote,
    RefusalNote, RepairedRecord, ReportRecord, RollbackNoted, RollbackRequest, SettleOutcome,
    StoreError, StoredAttempt, UpdateSlot, FACTORY_SLOT, NVS_NAMESPACE,
};
pub use reconcile::{reconcile, AttemptRecord, BootFacts, ReconcileAction, SlotId, SlotState};
#[cfg(feature = "ota-wire")]
#[allow(deprecated)]
pub use runtime::{
    ConfigError, OtaSettings, OtaStacks, OtaTimings, MIN_STACK_BYTES, MIN_WORKER_STACK_BYTES,
};
pub use state::OtaState;
pub use verifier::{bytes_to_hex, hex_to_bytes, StreamingVerifier};
pub use version::Version;
#[cfg(feature = "ota-wire")]
#[allow(deprecated)]
pub use wire::{
    CommandError, Manifest, ManifestError, OtaCommand, OtaStatus, StatusError, MAX_COMMAND_BYTES,
    MAX_MANIFEST_BYTES,
};
pub use wire::{FailReason, RollbackReason};
pub use write_align::{round_up, AlignError, ChunkAligner, MAX_WRITE_SIZE};
