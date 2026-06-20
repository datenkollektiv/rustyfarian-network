//! Bare-metal (esp-hal) network drivers for ESP32-C3, ESP32-C6, ESP32-S3, and ESP32.
//!
//! This crate consolidates four formerly-separate bare-metal crates into one,
//! with per-domain + chip feature gates:
//!
//! - [`wifi`] — async STA + SoftAP Wi-Fi via `esp-radio 0.18`
//!   (requires `embassy`; supported on `esp32c3`, `esp32c6`)
//! - [`lora`] — synchronous LoRa stub via embedded-hal SPI + GPIO
//!   (all chips; `esp32s3` uses hardware integration)
//! - [`ota`] — async OTA download + verify + swap
//!   (requires `embassy`; supported on `esp32c3`, `esp32c6`, `esp32`)
//! - [`provisioning`] — SoftAP captive-portal credential provisioning
//!   (requires `wifi` + `embassy`; supported on `esp32c3`, `esp32c6`)
//!
//! # Feature flags
//!
//! | Flag | Description |
//! |:-----|:------------|
//! | `wifi` | Wi-Fi STA + SoftAP (implies `embassy`) |
//! | `lora` | LoRa radio stub |
//! | `ota` | OTA manager (implies `embassy`) |
//! | `provisioning` | SoftAP captive portal (implies `wifi` + `embassy`) |
//! | `embassy` | Async executor + embassy-net stack |
//! | `esp32c3` | Target chip ESP32-C3 |
//! | `esp32c6` | Target chip ESP32-C6 |
//! | `esp32` | Target chip ESP32 (lora + ota only) |
//! | `esp32s3` | Target chip ESP32-S3 (lora only) |
//! | `unstable` | Forward `esp-hal/unstable` |
//! | `rt` | Forward `esp-hal/rt` |
//! | `ws2812` | Enable `rustyfarian-esp-hal-ws2812` LED dep |

#![no_std]

// ── Belt-and-braces compile-time guard ──────────────────────────────────────
#[cfg(all(feature = "provisioning", not(feature = "wifi")))]
compile_error!("the `provisioning` feature requires `wifi`");

// ── Always-on re-exports ─────────────────────────────────────────────────────
//
// `NoLed`, `SimpleLed`, and `StatusLed` are re-exported at the crate root so
// callers that used `rustyfarian_esp_hal_network::wifi::{NoLed, SimpleLed, StatusLed}`
// (or the lora crate equivalents) continue to work unchanged.
pub use pennant::{NoLed, SimpleLed, StatusLed};

// ── Domain modules (feature-gated) ──────────────────────────────────────────
//
// Each domain module is compiled when its feature is active OR when running
// tests (`cfg(test)`). The `cfg(test)` arm allows host-only unit tests
// embedded in the module files to run via `--no-default-features` without
// pulling in chip-specific hardware dependencies.

#[cfg(any(feature = "wifi", test))]
pub mod wifi;

#[cfg(any(feature = "lora", test))]
pub mod lora;

#[cfg(any(feature = "ota", test))]
pub mod ota;

#[cfg(any(feature = "provisioning", test))]
pub mod provisioning;
