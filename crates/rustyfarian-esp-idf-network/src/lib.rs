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
