//! Bare-metal flash credential store for provisioning data.
//!
//! This crate implements a two-sector wear-levelled flash store that persists a
//! [`provisioning_pure::ProvisioningConfig`] across device reboots. The on-flash
//! layout is a CRC-protected TLV record with a sequence counter for torn-write
//! recovery.
//!
//! # No-std
//!
//! This crate is `#![no_std]` and host-testable end-to-end using a
//! [`MockFlash`](store::MockNorFlash) test double in the test suite. No chip
//! features are needed for Phase 1.
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

pub use store::{ProvisioningStore, StoreError};
