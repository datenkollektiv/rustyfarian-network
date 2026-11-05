//! Platform-independent provisioning primitives — form parsing, field
//! validation, SoftAP SSID derivation, and a backend-neutral state machine.
//!
//! This module is the host-testable core of the SoftAP captive-portal
//! provisioning triad (see `docs/features/archive/softap-provisioning-v1.md`). It owns
//! the `application/x-www-form-urlencoded` parser, the structured per-field
//! error model the HTTP layer renders, and the provisioning state machine that
//! the ESP-IDF crate drives. It holds no platform dependencies so a future
//! `rustyfarian-esp-hal-network` can adopt it without an API break.
//!
//! OTAA credential validation delegates to [`crate::lora::LoraConfig::from_hex_strings`],
//! Wi-Fi validation to [`crate::wifi`], and MQTT client-ID validation to
//! [`crate::mqtt`], keeping one authoritative implementation of each rule.
//!
//! Both tiers share the captive portal's logic from here (ADR 023): the
//! request sequencer [`PortalMachine`] (rung 3), the NorFlash store
//! `flash_store` (rung 1, feature `flash-store`), and the flat helpers
//! (nonce, pre-fill, start-up plan, DNS codec, boot classification). The tiers
//! keep only the I/O.
//!
//! The schema is expressed as one of two [`SchemaProfile`]s — `LorawanFieldDevice`
//! (Core + LoRaWAN + OTA) and `WifiMqttDevice` (Core + MQTT + OTA) — selected by
//! the caller of [`parse_form`].
//!
//! All public APIs are experimental.

pub mod boot;
pub mod config;
pub mod defaults;
pub mod device_name;
pub mod dns;
pub mod error;
#[cfg(feature = "flash-store")]
pub mod flash_store;
pub mod form;
pub mod html_json_escape;
#[cfg(feature = "std")]
pub mod kv_store;
pub mod portal;
pub mod portal_flow;
pub mod profile;
pub mod reentry;
pub(crate) mod secret;
#[cfg(feature = "std")]
pub mod session;
pub mod ssid;
pub mod state;
pub mod templates;

#[cfg(feature = "std")]
pub use boot::portal_outcome;
pub use boot::{classify_wifi_mqtt, ClassifyError, PortalOutcome, WifiMqttClass};
pub use config::{
    ProvisioningConfig, APP_KEY_HEX_LEN, DEVICE_NAME_MAX_LEN, EUI_HEX_LEN, EXTRA_FIELDS_MAX,
    EXTRA_KEY_MAX_LEN, EXTRA_VALUE_MAX_LEN, MAX_FIELD_ERRORS, MQTT_HOST_MAX_LEN, MQTT_PASS_MAX_LEN,
    MQTT_USER_MAX_LEN, OTA_URL_MAX_LEN,
};
pub use defaults::PortalDefaults;
pub use device_name::{derive_device_name, DeviceName};
pub use error::{Field, FieldError, FieldErrors, ValidationError};
pub use form::{parse_form, ExtraField};
#[cfg(feature = "std")]
pub use kv_store::{KvProvisioningStore, ProvisioningKv};
pub use portal::{
    body_nonce_matches, compose_mqtt_uri, format_nonce, nonce_matches, plan_portal, PortalPlan,
    PortalPlanError, PortalPlanInput, Prefill,
};
pub use portal_flow::{PortalAction, PortalInput, PortalMachine};
pub use profile::{LoraFields, MqttFields, SchemaProfile};
pub use reentry::{gate_reentry, ReentryGate, ReentryRequest};
#[cfg(feature = "std")]
pub use session::{SessionOutcome, SessionState};
pub use ssid::{derive_softap_ssid, resolve_softap_ssid};
pub use state::{
    resolve_wait, InvalidTransition, ProvisioningInput, ProvisioningState, WaitResolution,
};
