#![no_std]
//! Platform-independent provisioning primitives — form parsing, field
//! validation, SoftAP SSID derivation, and a backend-neutral state machine.
//!
//! This crate is the host-testable core of the SoftAP captive-portal
//! provisioning triad (see `docs/features/softap-provisioning-v1.md`). It owns
//! the `application/x-www-form-urlencoded` parser, the structured per-field
//! error model the HTTP layer renders, and the provisioning state machine that
//! the ESP-IDF crate drives. It holds no platform dependencies so a future
//! `rustyfarian-esp-hal-provisioning` can adopt it without an API break.
//!
//! OTAA credential validation delegates to [`lora_pure::LoraConfig::from_hex_strings`]
//! and Wi-Fi validation to [`wifi_pure`], keeping one authoritative
//! implementation of each rule.
//!
//! All public APIs are experimental.

#[cfg(test)]
extern crate alloc;

pub mod config;
pub mod error;
pub mod form;
pub mod ssid;
pub mod state;

pub use config::{
    ProvisioningConfig, DEVICE_NAME_MAX_LEN, EXTRA_FIELDS_MAX, EXTRA_KEY_MAX_LEN,
    EXTRA_VALUE_MAX_LEN, MAX_FIELD_ERRORS, OTA_URL_MAX_LEN,
};
pub use error::{Field, FieldError, FieldErrors, ValidationError};
pub use form::{parse_form, ExtraField};
pub use ssid::derive_softap_ssid;
pub use state::{InvalidTransition, ProvisioningInput, ProvisioningState};
