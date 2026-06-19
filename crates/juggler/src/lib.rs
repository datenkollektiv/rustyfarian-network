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
