//! Bare-metal flash credential store and SoftAP captive-portal provisioning.
//!
//! This crate provides:
//!
//! 1. A two-sector wear-levelled flash store ([`ProvisioningStore`]) that
//!    persists a [`ProvisioningConfig`] across device reboots.  The on-flash
//!    layout is a CRC-protected TLV record with a sequence counter for
//!    torn-write recovery.
//!
//! 2. A SoftAP captive-portal provisioning substrate ([`ProvisioningBuilder`])
//!    that spawns the DHCP, DNS, and HTTP tasks and returns a
//!    [`ProvisioningSession`] the caller waits on.
//!
//! # No-std
//!
//! This crate is `#![no_std]` and host-testable end-to-end using a
//! `MockFlash` test double in the test suite. The embassy tasks require
//! `features = ["esp32c3", "embassy", "rt"]` (or `esp32c6`).
//!
//! # Real-hardware use
//!
//! Real-hardware use requires `esp-storage = { features = ["critical-section"] }`
//! in the consuming binary. This is a Phase 2 concern; the
//! cache-disable-during-flash-write hazard is tracked in esp-idf#10079.
#![no_std]

#[cfg(test)]
extern crate alloc;

mod record;
mod store;

/// Minimal DHCP server substrate for the SoftAP captive-portal.
pub(crate) mod dhcp;
/// DNS catch-all server substrate for the SoftAP captive-portal.
pub(crate) mod dns_catchall;
/// Captive-portal HTTP router (replaces the generic `http_server` spike).
pub(crate) mod portal;
/// Public provisioning builder / session API.
pub mod session;

pub use store::{ProvisioningStore, StoreError};

// ── Public session API ─────────────────────────────────────────────────────────

pub use session::{
    PortalConfig, ProvisioningBuilder, ProvisioningError, ProvisioningEvent, ProvisioningOutcome,
    ProvisioningSession,
};

// ── Re-exports from provisioning-pure ─────────────────────────────────────────

pub use provisioning_pure::{
    derive_softap_ssid, Field, FieldError, LoraFields, MqttFields, ProvisioningConfig,
    ProvisioningState, SchemaProfile, ValidationError,
};
