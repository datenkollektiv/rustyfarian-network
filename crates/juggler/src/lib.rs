//! Platform-agnostic types, validation, and state machines for Wi-Fi, MQTT,
//! LoRa, ESP-NOW, OTA, and provisioning on ESP32.
//!
//! This crate is `no_std` by default. Enable domain features to compile
//! the corresponding module:
//!
//! | Feature | Module | External deps |
//! |:--------|:-------|:--------------|
//! | (always) | `backoff`, `status_colors` | none |
//! | `wifi` | `wifi` | none |
//! | `mqtt` | `mqtt` (no_std subset) | none |
//! | `std` | `mqtt` (full, incl. thread helpers) | `anyhow` |
//! | `lora` | `lora` | `heapless`, `nb`, `lorawan-device` |
//! | `espnow` | `espnow` | none |
//! | `ota` | `ota` | `heapless`, `sha2` |
//! | `provisioning` | `provisioning` | `heapless` (implies wifi+mqtt+lora) |
//! | `mock` | `wifi::mock`, `lora::mock`, `espnow::mock` | (implies wifi+lora+espnow) |
//!
//! ## Cargo features
//!
//! ### Domain features
//!
//! - **`wifi`** — Wi-Fi STA configuration, scanning, connection validation, and
//!   SoftAP lifecycle primitives. Zero external dependencies; `no_std` compatible.
//!
//! - **`mqtt`** — MQTT topic/client-ID validation and connection state machine.
//!   No external dependencies in the `no_std` subset. Requires `std` feature to
//!   unlock the `std`-specific helpers (see `std` feature below).
//!
//! - **`lora`** — LoRa radio primitives, regional config, and LoRaWAN Class A
//!   protocol state machine. Requires `heapless`, `nb`, and `lorawan-device`.
//!
//! - **`espnow`** — ESP-NOW peer tracking, command parsing, and liveness
//!   detection. Zero external dependencies.
//!
//! - **`ota`** — OTA metadata parsing, streaming SHA-256 verification, version
//!   comparison. Requires `heapless` and `sha2`.
//!
//! - **`provisioning`** — SoftAP captive-portal form parsing, field validation,
//!   and provisioning state machine. **This is a meta-feature that enables
//!   `wifi`, `mqtt`, and `lora`** because the two shipped `SchemaProfile`s
//!   — `WifiMqttDevice` and `LorawanFieldDevice` — reuse validation types from
//!   those domains and are part of the public provisioning API. This coupling is
//!   intentional; consumers may instead depend on only the domain features they
//!   need. Requires `heapless`.
//!
//! ### Utility features
//!
//! - **`std`** — Enables full MQTT helpers that require the standard library:
//!   [`mqtt::spawn_subscriber_thread`], [`mqtt::SubscribeClient`] trait,
//!   [`mqtt::QoS`] enum, and [`mqtt::format_broker_url`]. These functions need
//!   `std::thread`, `std::sync`, and the `anyhow` crate. **Implies `mqtt` feature.**
//!   Everything else in `juggler` is `no_std`; this flag does not affect other
//!   domains. Use this only if you need the thread-based subscriber helpers;
//!   pure MQTT validation and state machines compile under `mqtt` alone.
//!
//! - **`mock`** — Exposes test-double implementations for unit testing:
//!   [`wifi::mock::MockWifiDriver`], [`lora::mock::MockLoraRadio`], and
//!   [`espnow::mock::MockEspNowDriver`]. These mocks implement the driver traits
//!   so your application code can be tested against a deterministic, hardware-free
//!   environment on the host. **Implies `wifi`, `lora`, and `espnow` features.**
//!   Part of the public, semver-tracked API; safe for downstream production tests.
//!
//! ## Re-export patterns
//!
//! Each domain module re-exports its primary types at the crate root for ergonomic imports:
//! - `wifi::*` exports [`WiFiConfig`](wifi::WiFiConfig), [`WifiDriver`](wifi::WifiDriver), etc.
//! - `mqtt::*` exports [`MqttConnectionState`](mqtt::MqttConnectionState), validation functions, etc.
//! - `lora::*` exports [`LoraRadio`](lora::LoraRadio), [`LorawanDevice`](lora::LorawanDevice), etc.
//! - `espnow::*` exports [`EspNowDriver`](espnow::EspNowDriver), [`PeerTracker`](espnow::PeerTracker), etc.
//! - `ota::*` exports [`OtaError`](ota::OtaError), [`StreamingVerifier`](ota::StreamingVerifier), etc.
//! - `provisioning::*` exports [`ProvisioningState`](provisioning::ProvisioningState), [`SchemaProfile`](provisioning::SchemaProfile), etc.

#![cfg_attr(not(feature = "std"), no_std)]

// alloc is needed in tests (provisioning + espnow mocks use alloc::format! etc.)
#[cfg(test)]
extern crate alloc;

// ── Always-available (no feature gate) ──────────────────────────────────────
// backoff and status_colors are used across wifi+mqtt and have zero external deps.
pub mod backoff;
pub mod status_colors;

// ── Domain modules (feature-gated) ──────────────────────────────────────────

#[cfg(feature = "wifi")]
pub mod wifi;

#[cfg(any(feature = "mqtt", feature = "std"))]
pub mod mqtt;

#[cfg(feature = "lora")]
pub mod lora;

#[cfg(feature = "espnow")]
pub mod espnow;

#[cfg(feature = "ota")]
pub mod ota;

#[cfg(feature = "provisioning")]
pub mod provisioning;
