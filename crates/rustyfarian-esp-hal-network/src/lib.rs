//! Bare-metal (esp-hal) network drivers for ESP32-C3, ESP32-C6, ESP32-S3, and ESP32.
//!
//! This crate consolidates four formerly-separate bare-metal crates into one,
//! with per-domain + chip feature gates:
//!
//! - [`wifi`] — async STA + SoftAP Wi-Fi via `esp-radio 1.0.0-beta.1` (pre-release, exact-pinned)
//!   (requires `embassy`; supported on `esp32c3`, `esp32c6`, `esp32s3`)
//! - [`mqtt`] — async MQTT 5 client over the Wi-Fi `embassy-net` stack (`minimq`, ADR 019)
//!   (requires `wifi` + `embassy`; supported on `esp32c3`, `esp32c6`, `esp32s3`)
//! - [`espnow`] — ESP-NOW over `esp-radio` `esp_now`, implementing `juggler::espnow::EspNowDriver` (ADR 020)
//!   (requires `wifi` + `embassy`; supported on `esp32c3`, `esp32c6`)
//! - [`lora`] — synchronous LoRa stub via embedded-hal SPI + GPIO
//!   (all chips; `esp32s3` uses hardware integration)
//! - [`ota`] — async OTA download + verify + swap
//!   (requires `embassy`; supported on `esp32c3`, `esp32c6`, `esp32`, `esp32s3`)
//! - [`provisioning`] — SoftAP captive-portal credential provisioning
//!   (requires `wifi` + `embassy`; supported on `esp32c3`, `esp32c6`, `esp32s3`)
//!
//! # Feature flags
//!
//! | Flag | Description |
//! |:-----|:------------|
//! | `wifi` | Wi-Fi STA + SoftAP (implies `embassy`) |
//! | `mqtt` | MQTT 5 client (implies `wifi` + `embassy`) |
//! | `espnow` | ESP-NOW driver (implies `wifi` + `embassy`; `esp32c3`, `esp32c6`) |
//! | `lora` | LoRa radio stub |
//! | `ota` | OTA manager (implies `embassy`) |
//! | `provisioning` | SoftAP captive portal (implies `wifi` + `embassy`) |
//! | `embassy` | Async executor + embassy-net stack |
//! | `esp32c3` | Target chip ESP32-C3 |
//! | `esp32c6` | Target chip ESP32-C6 |
//! | `esp32` | Target chip ESP32 (lora + ota only) |
//! | `esp32s3` | Target chip ESP32-S3 |
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

#[cfg(any(feature = "mqtt", test))]
pub mod mqtt;

#[cfg(any(feature = "espnow", test))]
pub mod espnow;

#[cfg(any(feature = "lora", test))]
pub mod lora;

#[cfg(any(feature = "ota", test))]
pub mod ota;

#[cfg(any(feature = "provisioning", test))]
pub mod provisioning;

// ── Host critical-section implementation (tests only) ───────────────────────
//
// On target, esp-hal provides the `critical-section` implementation. The host
// test binary needs one for the ESP-NOW cells. `critical-section/std` is not an
// option: it selects `restore-state-bool`, which cannot coexist with the
// `restore-state-none` that `esp-idf-hal/critical-section` selects when the
// whole workspace is linted in one cargo invocation. The implementation exists only in
// the lib unit-test binary: integration tests and doctests that touch the cells would not
// link, and a future dev-dependency enabling `critical-section/std` would clash with it.
// This implementation uses
// the `()` restore state and is re-entrant on a single thread.
#[cfg(test)]
mod host_critical_section {
    extern crate std;

    use std::sync::Mutex;
    use std::thread::{self, ThreadId};

    struct HostCriticalSection;
    critical_section::set_impl!(HostCriticalSection);

    /// Owning thread and its nesting depth while a critical section is held.
    static OWNER: Mutex<Option<(ThreadId, u32)>> = Mutex::new(None);

    unsafe impl critical_section::Impl for HostCriticalSection {
        unsafe fn acquire() -> critical_section::RawRestoreState {
            let me = thread::current().id();
            loop {
                let mut owner = OWNER.lock().unwrap_or_else(|e| e.into_inner());
                match owner.as_mut() {
                    None => {
                        *owner = Some((me, 1));
                        return;
                    }
                    Some((holder, depth)) if *holder == me => {
                        *depth += 1;
                        return;
                    }
                    Some(_) => {}
                }
                drop(owner);
                thread::yield_now();
            }
        }

        unsafe fn release(_restore_state: critical_section::RawRestoreState) {
            let mut owner = OWNER.lock().unwrap_or_else(|e| e.into_inner());
            if let Some((_, depth)) = owner.as_mut() {
                *depth -= 1;
                if *depth == 0 {
                    *owner = None;
                }
            }
        }
    }
}
