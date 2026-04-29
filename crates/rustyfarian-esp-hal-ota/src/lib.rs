#![no_std]
//! Bare-metal OTA driver for ESP32-C3/C6/ESP32.
//!
//! Wraps `esp_bootloader_esp_idf::OtaUpdater` and provides streaming
//! firmware download with strict HTTP/1.1 GET over `embassy-net`.
//!
//! All public APIs are experimental.

// Internal HTTP/1.1 GET client — implementation detail per ADR 011 §2.
// Module is private; no item in this module is part of the public API.
mod http;

pub use ota_pure::OtaError;

#[cfg(all(
    feature = "embassy",
    any(feature = "esp32c3", feature = "esp32c6", feature = "esp32")
))]
mod manager;
#[cfg(all(
    feature = "embassy",
    any(feature = "esp32c3", feature = "esp32c6", feature = "esp32")
))]
pub use manager::{EspHalOtaManager, OtaManagerConfig};

#[cfg(not(all(
    feature = "embassy",
    any(feature = "esp32c3", feature = "esp32c6", feature = "esp32")
)))]
mod stub;
#[cfg(not(all(
    feature = "embassy",
    any(feature = "esp32c3", feature = "esp32c6", feature = "esp32")
)))]
pub use stub::{EspHalOtaManager, OtaManagerConfig};
