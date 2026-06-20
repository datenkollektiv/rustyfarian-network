//! ESP-IDF (std) network drivers for ESP32.
//!
//! This crate consolidates six ESP-IDF networking domains into one, each
//! gated behind a Cargo feature:
//!
//! | Feature | Domain |
//! |:--------|:-------|
//! | `wifi` | Wi-Fi STA connection manager + SoftAP lifecycle manager |
//! | `mqtt` | MQTT builder + handle, auto-reconnecting background event loop |
//! | `lora` | SX1262 radio driver + LoRaWAN Class A adapter for lorawan-device 0.12 |
//! | `espnow` | ESP-NOW peer-to-peer driver with channel scanning |
//! | `ota` | Streaming firmware download + SHA-256 verify + partition swap |
//! | `provisioning` | SoftAP captive-portal provisioning (requires `wifi`) |
//!
//! Enable only the features you need; `default = []` so no domain code is
//! compiled unless explicitly opted in.
//!
//! ## Cargo features
//!
//! ### Domain features (opt-in)
//!
//! - **`wifi`** — Wi-Fi connection manager and SoftAP lifecycle handler via `esp-idf-svc`.
//!   Provides [`wifi::WiFiManager`] for STA mode and [`wifi::SoftApManager`] for AP mode.
//!
//! - **`mqtt`** — Auto-reconnecting MQTT client with builder API and background event loop.
//!   Provides [`mqtt::MqttBuilder`] and [`mqtt::MqttHandle`]. **This feature compiles
//!   without `wifi`** — the MQTT client is transport-agnostic at the type level.
//!   (Note: bundled examples additionally depend on `wifi` because they must bring up
//!   Wi-Fi before connecting to MQTT; see [`[[example]]` `required-features`](https://docs.rs/cargo/1.0.0/cargo/reference/manifest.html#example)
//!   entries in `Cargo.toml`). You may depend on `features = ["mqtt"]` alone if you
//!   provide your own transport layer.
//!
//! - **`lora`** — SX1262 radio driver and LoRaWAN Class A adapter. Implements
//!   [`lora::sx1262_driver::EspIdfLoraRadio`] for use with `lorawan-device 0.12`.
//!   Provides full initialization, RX/TX, and LoRaWAN protocol bindings.
//!
//! - **`espnow`** — ESP-NOW peer driver with automatic channel scanning. Provides
//!   [`espnow::EspIdfEspNow`] for point-to-point frame exchange.
//!
//! - **`ota`** — OTA firmware update via streaming HTTP download with SHA-256
//!   verification. Provides [`ota::OtaSession`] for fetch-and-apply operations.
//!
//! - **`provisioning`** — SoftAP captive-portal provisioning for Wi-Fi and
//!   LoRaWAN credentials. Requires `wifi` feature (SoftAP setup). Provides
//!   [`provisioning::ProvisioningBuilder`] and [`provisioning::ProvisioningSession`].
//!
//! ## Re-export patterns
//!
//! Each domain module in this crate re-exports the corresponding types from
//! [`juggler`] (the platform-agnostic core) for consumer convenience:
//!
//! - `wifi::*` exports [`wifi::WiFiConfig`], [`wifi::WiFiDriver`], etc. from `juggler::wifi`
//! - `mqtt::*` exports [`mqtt::MqttConnectionState`], validation functions, etc. from `juggler::mqtt`
//! - `lora::*` exports [`lora::LoraRadio`], [`lora::LorawanDevice`], [`lora::Region`], etc. from `juggler::lora`
//! - `espnow::*` exports [`espnow::EspNowDriver`], [`espnow::PeerTracker`], etc. from `juggler::espnow`
//! - `ota::*` exports [`ota::OtaError`], [`ota::StreamingVerifier`], etc. from `juggler::ota`
//! - `provisioning::*` exports [`provisioning::ProvisioningState`], [`provisioning::SchemaProfile`], etc. from `juggler::provisioning`

/// Wi-Fi STA connection manager and SoftAP lifecycle manager.
#[cfg(feature = "wifi")]
pub mod wifi;

/// MQTT builder, handle, and background event loop.
#[cfg(feature = "mqtt")]
pub mod mqtt;

/// SX1262 LoRa radio driver and LoRaWAN Class A adapter.
#[cfg(feature = "lora")]
pub mod lora;

/// ESP-NOW peer-to-peer driver.
#[cfg(feature = "espnow")]
pub mod espnow;

/// OTA firmware update driver.
#[cfg(feature = "ota")]
pub mod ota;

/// SoftAP captive-portal provisioning driver.
#[cfg(feature = "provisioning")]
pub mod provisioning;
