//! Platform-independent OTA primitives — Version parsing, streaming SHA-256,
//! sidecar metadata, backend-neutral state machine, update decision policy,
//! offer decisions (with a refused-version loop guard), and boot reconciliation.
//!
//! All public APIs are experimental.

pub mod decision;
pub mod error;
pub mod metadata;
pub mod reconcile;
pub mod state;
pub mod verifier;
pub mod version;

pub use decision::{decide_offer, decide_update, OfferDecision, UpdateDecision};
pub use error::OtaError;
pub use metadata::ImageMetadata;
pub use reconcile::{reconcile, AttemptRecord, BootFacts, ReconcileAction, SlotId, SlotState};
pub use state::OtaState;
pub use verifier::{bytes_to_hex, hex_to_bytes, StreamingVerifier};
pub use version::Version;
