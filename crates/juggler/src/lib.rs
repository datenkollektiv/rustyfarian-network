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
//! | `std` | `mqtt` (full, incl. `CallbackScope`, `SubscribeClient`, `Outbox`, `PendingAcks`) | `anyhow` |
//! | `lora` | `lora` | `heapless`, `nb`, `lorawan-device`, `sx126x`, `embedded-hal` |
//! | `espnow` | `espnow` | none (implies wifi, for the country channel set) |
//! | `ota` | `ota` | `heapless`, `sha2` |
//! | `ota-wire` | `ota::wire` | `serde`, `serde_json` (implies `ota`, needs `alloc`) |
//! | `provisioning` | `provisioning` | `heapless`, `zeroize` (implies wifi+mqtt) |
//! | `flash-store` | `provisioning::flash_store` | `embedded-storage` (implies `provisioning`) |
//! | `embassy` | `espnow::EmbassyClock` | `embassy-time` (no time driver, ADR 021) |
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
//!   protocol state machine. Requires `heapless`, `nb`, `lorawan-device`,
//!   `sx126x`, and `embedded-hal`.
//!
//! - **`espnow`** — ESP-NOW peer tracking, command parsing, and liveness
//!   detection. Zero external dependencies.
//!
//! - **`ota`** — OTA metadata parsing, streaming SHA-256 verification, version
//!   comparison. Requires `heapless` and `sha2`.
//!
//! - **`ota-wire`** — OTA wire contract: command and manifest parsing, status
//!   JSON, and the frozen `failed` reason codes. Requires `serde` and
//!   `serde_json` (no default features, `alloc`); implies `ota`. The base `ota`
//!   feature stays free of both.
//!
//! - **`provisioning`** — SoftAP captive-portal form parsing, field validation,
//!   and provisioning state machine. **This is a meta-feature that enables
//!   `wifi` and `mqtt`** because the two shipped `SchemaProfile`s
//!   — `WifiMqttDevice` and `LorawanFieldDevice` — reuse validation types from
//!   those domains and are part of the public provisioning API. `lora` is not
//!   implied, so a Wi-Fi + MQTT device does not pull `lorawan-device`; enable it
//!   alongside for `LoraFields::to_lora_config`. Requires `heapless` and `zeroize`.
//!
//! ### Utility features
//!
//! - **`std`** — Enables the MQTT support that requires the standard library:
//!   [`mqtt::CallbackScope`] (the callback-context guard), the
//!   [`mqtt::SubscribeClient`] trait, [`mqtt::Outbox`] and [`mqtt::PendingAcks`]
//!   (the shared publish path with its PUBACK waiters), plus
//!   [`mqtt::format_broker_url`]. These need `std::sync`, `std::thread`, and the
//!   `anyhow` crate. The thread helpers `spawn_connect_thread` and
//!   `spawn_subscriber_thread` are also behind it, but both are deprecated and
//!   unused by either tier. **Implies `mqtt` feature.**
//!   Everything else in `juggler` is `no_std`; this flag does not affect other
//!   domains. Pure MQTT validation and state machines compile under `mqtt` alone.
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
//! - `lora::*` exports [`LoraRadio`](lora::LoraRadio), [`LorawanPhy`](lora::LorawanPhy), [`LorawanTimeline`](lora::LorawanTimeline), etc.
//! - `espnow::*` exports [`EspNowDriver`](espnow::EspNowDriver), [`PeerTracker`](espnow::PeerTracker), etc.
//! - `ota::*` exports [`OtaError`](ota::OtaError), [`StreamingVerifier`](ota::StreamingVerifier), etc.
//! - `provisioning::*` exports [`ProvisioningState`](provisioning::ProvisioningState), [`SchemaProfile`](provisioning::SchemaProfile), etc.

#![cfg_attr(not(feature = "std"), no_std)]

// alloc is needed in tests (provisioning + espnow mocks use alloc::format! etc.)
// and by the `ota-wire` JSON types (String, serde_json).
#[cfg(any(
    test,
    feature = "ota-wire",
    feature = "std",
    all(feature = "mock", feature = "mqtt")
))]
extern crate alloc;

// Shared test fixtures; every user sits under `wifi`, `mqtt` (provisioning implies both) or `espnow`.
#[cfg(all(test, any(feature = "wifi", feature = "mqtt", feature = "espnow")))]
pub(crate) mod test_support;

// ── Always-available (no feature gate) ──────────────────────────────────────
// backoff and status_colors are used across wifi+mqtt and have zero external deps.
pub mod backoff;
pub mod status_colors;
#[cfg(feature = "std")]
pub mod wait_list;
pub mod waker_slots;

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
