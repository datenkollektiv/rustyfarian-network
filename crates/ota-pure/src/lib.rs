#![no_std]
//! Platform-independent OTA primitives — Version parsing, streaming SHA-256,
//! sidecar metadata, backend-neutral state machine.
//!
//! All public APIs are experimental.

pub mod error;
pub mod metadata;
pub mod state;
pub mod verifier;
pub mod version;

pub use error::OtaError;
pub use metadata::ImageMetadata;
pub use state::OtaState;
pub use verifier::{bytes_to_hex, hex_to_bytes, StreamingVerifier};
pub use version::Version;
