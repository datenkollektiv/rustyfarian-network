//! Bare-metal OTA driver for ESP32-C3/C6/ESP32.
//!
//! Wraps `esp_bootloader_esp_idf::OtaUpdater` and provides streaming
//! firmware download with strict HTTP/1.1 GET over `embassy-net`.
//!
//! All public APIs are experimental.
//!
//! # Firmware metadata (re-exported)
//!
//! [`ImageMetadata`], [`Version`], [`OtaState`], [`StreamingVerifier`], the
//! hex helpers ([`bytes_to_hex`], [`hex_to_bytes`]), and the update decision
//! policy ([`decide_update`], [`UpdateDecision`]), the offer decision with its
//! refused-version guard ([`decide_offer`], [`OfferDecision`]), and boot
//! reconciliation ([`reconcile`], [`AttemptRecord`], [`BootFacts`],
//! [`ReconcileAction`], [`SlotId`], [`SlotState`]) are re-exported from
//! `juggler::ota` here — matching the `wifi`/`espnow` domains — so a
//! version-gated updater (parse a sidecar digest + version via
//! [`ImageMetadata::parse`], compare [`Version`] via [`decide_update`]) needs
//! only this crate and no separate `juggler` dependency.

// Internal HTTP/1.1 GET client — implementation detail per ADR 011 §2.
// Module is private; no item in this module is part of the public API.
mod http;

// Re-export the full public surface of `juggler::ota` for domain parity with the
// `wifi`/`espnow` modules, so OTA consumers import metadata/version types from
// this crate rather than adding a redundant direct `juggler` dependency.
pub use juggler::ota::{
    bytes_to_hex, decide_offer, decide_update, hex_to_bytes, reconcile, AttemptRecord, BootFacts,
    ImageMetadata, OfferDecision, OtaError, OtaState, ReconcileAction, SlotId, SlotState,
    StreamingVerifier, UpdateDecision, Version,
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
            bytes_to_hex, decide_offer, decide_update, hex_to_bytes, reconcile, AttemptRecord,
            BootFacts, ImageMetadata, OfferDecision, OtaError, OtaState, ReconcileAction, SlotId,
            SlotState, StreamingVerifier, UpdateDecision, Version,
        };

        fn assert_exported<T>() {}
        assert_exported::<OtaError>();
        assert_exported::<OtaState>();
        assert_exported::<StreamingVerifier>();
        assert_exported::<OfferDecision>();
        assert_exported::<AttemptRecord>();
        assert_exported::<BootFacts>();
        assert_exported::<ReconcileAction>();
        assert_exported::<SlotId>();
        assert_exported::<SlotState>();
        // Reference the hex helpers as function items (no call → no alloc needed).
        let _hex_helpers = (bytes_to_hex, hex_to_bytes);

        let meta = ImageMetadata::parse(
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
            "1.4.0",
        )
        .expect("valid sidecar metadata");
        assert_eq!(meta.version, Version::new(1, 4, 0));
        assert_eq!(
            decide_update(Version::new(1, 3, 0), meta.version),
            UpdateDecision::Apply
        );
        assert_eq!(
            decide_offer(Version::new(1, 3, 0), meta.version, None),
            OfferDecision::Apply
        );
        let facts = BootFacts {
            running_slot: SlotId(0),
            running_state: SlotState::Valid,
            running_version: Some(Version::new(1, 3, 0)),
            update_slot: Some((SlotId(1), SlotState::Unknown)),
            report_persisted: false,
        };
        assert_eq!(reconcile(None, &facts), ReconcileAction::NoAttempt);
        let _ = AttemptRecord {
            attempt_id: 1,
            version: Some(meta.version),
            slot: SlotId(1),
            boot_selected: false,
            activated: false,
            slot_was_invalid: false,
        };
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
