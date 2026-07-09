//! Bare-metal OTA driver for ESP32-C3/C6/ESP32.
//!
//! Wraps `esp_bootloader_esp_idf::OtaUpdater` and provides streaming
//! firmware download with strict HTTP/1.1 GET over `embassy-net`.
//!
//! All public APIs are experimental.
//!
//! # Firmware metadata (re-exported)
//!
//! [`ImageMetadata`], [`Version`], [`OtaState`], [`StreamingVerifier`], and the
//! hex helpers ([`bytes_to_hex`], [`hex_to_bytes`]) are re-exported from
//! `juggler::ota` here — matching the `wifi`/`espnow` domains — so a
//! version-gated updater (parse a sidecar digest + version via
//! [`ImageMetadata::parse`], compare [`Version`]) needs only this crate and no
//! separate `juggler` dependency.

// Internal HTTP/1.1 GET client — implementation detail per ADR 011 §2.
// Module is private; no item in this module is part of the public API.
mod http;

// Re-export the full public surface of `juggler::ota` for domain parity with the
// `wifi`/`espnow` modules, so OTA consumers import metadata/version types from
// this crate rather than adding a redundant direct `juggler` dependency.
pub use juggler::ota::{
    bytes_to_hex, hex_to_bytes, ImageMetadata, OtaError, OtaState, StreamingVerifier, Version,
};

// Parity guard: every public `juggler::ota` type must stay re-exported from this
// crate's `ota` module (like `wifi`/`espnow`), so OTA consumers never need a
// direct `juggler` dependency. A dropped re-export fails to compile here (E0432).
// Runs on the host via `just test-ota-hal` (`cargo test --no-default-features`,
// which sets `cfg(test)` and so pulls this module in without the ESP deps).
#[cfg(test)]
mod reexport_parity_guard {
    #[test]
    fn ota_public_surface_is_reexported_from_this_crate() {
        use crate::ota::{
            bytes_to_hex, hex_to_bytes, ImageMetadata, OtaError, OtaState, StreamingVerifier,
            Version,
        };

        fn assert_exported<T>() {}
        assert_exported::<OtaError>();
        assert_exported::<OtaState>();
        assert_exported::<StreamingVerifier>();
        // Reference the hex helpers as function items (no call → no alloc needed).
        let _hex_helpers = (bytes_to_hex, hex_to_bytes);

        let meta = ImageMetadata::parse(
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "1.4.0",
        )
        .expect("valid sidecar metadata");
        assert_eq!(meta.version, Version::new(1, 4, 0));
    }
}

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
