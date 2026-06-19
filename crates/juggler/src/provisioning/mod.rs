//! Platform-independent provisioning primitives — form parsing, field
//! validation, SoftAP SSID derivation, and a backend-neutral state machine.
//!
//! This module is the host-testable core of the SoftAP captive-portal
//! provisioning triad (see `docs/features/softap-provisioning-v1.md`). It owns
//! the `application/x-www-form-urlencoded` parser, the structured per-field
//! error model the HTTP layer renders, and the provisioning state machine that
//! the ESP-IDF crate drives. It holds no platform dependencies so a future
//! `rustyfarian-esp-hal-provisioning` can adopt it without an API break.
//!
//! OTAA credential validation delegates to [`crate::lora::LoraConfig::from_hex_strings`],
//! Wi-Fi validation to [`crate::wifi`], and MQTT client-ID validation to
//! [`crate::mqtt`], keeping one authoritative implementation of each rule.
//!
//! The schema is expressed as one of two [`SchemaProfile`]s — `LorawanFieldDevice`
//! (Core + LoRaWAN + OTA) and `WifiMqttDevice` (Core + MQTT + OTA) — selected by
//! the caller of [`parse_form`].
//!
//! All public APIs are experimental.

pub mod config;
pub mod error;
pub mod form;
pub mod html_json_escape;
pub mod profile;
pub mod ssid;
pub mod state;
pub mod templates;

pub use config::{
    ProvisioningConfig, DEVICE_NAME_MAX_LEN, EXTRA_FIELDS_MAX, EXTRA_KEY_MAX_LEN,
    EXTRA_VALUE_MAX_LEN, MAX_FIELD_ERRORS, MQTT_HOST_MAX_LEN, MQTT_PASS_MAX_LEN, MQTT_USER_MAX_LEN,
    OTA_URL_MAX_LEN,
};
pub use error::{Field, FieldError, FieldErrors, ValidationError};
pub use form::{parse_form, ExtraField};
pub use profile::{LoraFields, MqttFields, SchemaProfile};
pub use ssid::derive_softap_ssid;
pub use state::{InvalidTransition, ProvisioningInput, ProvisioningState};
