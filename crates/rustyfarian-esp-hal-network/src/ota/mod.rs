//! Bare-metal OTA driver for ESP32-C3/C6/ESP32.
//!
//! Wraps `esp_bootloader_esp_idf::OtaUpdater`: boot-slot control ([`EspHalOtaManager`]) with the base `ota` feature, and with `ota-ddi` the DDI client, which streams firmware over `embassy-net` with its own HTTP/1.1 exchange.
//!
//! All public APIs are experimental.
//!
//! # Firmware metadata (re-exported)
//!
//! The cooperative download deadline ([`Deadline`], [`ActivationPermit`]) and the read-error classifier ([`classify_read_error`]) are re-exported from `juggler::ota` as well.
//!
//! The reason codes ([`FailReason`], [`RollbackReason`]) are re-exported from `juggler::ota` as well.
//! The persisted-record store (`OtaStore`, `OtaKv`, `reconcile_boot`, `note_boot_slot`, and their outcome and error types) is re-exported with `ota-ddi`.
//! With `ota-ddi` the [`ddi`] module holds the async DDI transport (`DdiTransport`) and the DDI runtime (`DdiRuntime`, `DdiHandle`, `DdiRunner`, `DdiNudge`, `OtaFlags`) under the ESP-IDF tier's names.
//! With `ota-ddi` this crate ships the `OtaKv` backend over raw flash (`FlashKv`), the shared flash handle (`SharedFlash`), the store opener (`open_records`) and the boot-fact reads (`read_hardware_facts`, `slot_evidence_line`, `note_boot_slot_hal`).
//! The health verdict types (`DeadlineAction`, `HealthVerdict`) are re-exported flat.
//!
//! [`ImageMetadata`], [`Version`], [`OtaState`], [`StreamingVerifier`], the
//! hex helpers ([`bytes_to_hex`], [`hex_to_bytes`]), and the update decision
//! policy ([`decide_update`], [`UpdateDecision`]), the offer decision with its
//! refused-version guard and admission gate ([`decide_offer`],
//! [`OfferDecision`], [`Admission`], [`BlockedBy`]), and boot
//! reconciliation ([`reconcile`], [`AttemptRecord`], [`BootFacts`],
//! [`ReconcileAction`], [`SlotId`], [`SlotState`]) are re-exported from
//! `juggler::ota` here — matching the `wifi`/`espnow` domains — so a
//! version-gated updater (parse a sidecar digest + version via
//! [`ImageMetadata::parse`], compare [`Version`] via [`decide_update`]) needs
//! only this crate and no separate `juggler` dependency.

// Internal HTTP/1.1 exchange of the DDI client.
// Module is private; no item in this module is part of the public API.
#[cfg(any(feature = "ota-ddi", test))]
mod http;

mod cancel;
pub use cancel::OtaCancel;

// Activation order (host-tested); used by the manager.
#[cfg(any(
    test,
    all(
        feature = "embassy",
        any(
            feature = "esp32c3",
            feature = "esp32c6",
            feature = "esp32",
            feature = "esp32s3"
        )
    )
))]
mod activation;

// Re-export the full public surface of `juggler::ota` for domain parity with the
// `wifi`/`espnow` modules, so OTA consumers import metadata/version types from
// this crate rather than adding a redundant direct `juggler` dependency.
pub use juggler::ota::{
    bytes_to_hex, check_content_length, classify_read_error, decide_offer, decide_update,
    hex_to_bytes, reconcile, ActivationPermit, Admission, AttemptRecord, BlockedBy, BootFacts,
    CancelRequest, Deadline, FailReason, ImageChip, ImageMetadata, OfferDecision, OtaError,
    OtaState, ReconcileAction, RollbackReason, SlotId, SlotState, StreamingVerifier,
    UpdateDecision, Version,
};

// The persisted-record store, used by the DDI client.
#[cfg(any(feature = "ota-ddi", test))]
pub use juggler::ota::{
    note_boot_slot, read_with_busy_retry, reconcile_boot, slot_for_partition, slot_from_label,
    slot_label, BootDisposition, BootFault, BootOutcome, BootWarnings, BusyRetry, BusyTally,
    CorruptRecord, Delivery, FailedDownload, HardwareFacts, HardwareReadError, KvError, KvStr,
    NoteBoot, NoteBootError, OtaKv, OtaStore, PendingReport, PendingReportView, ReasonNote,
    RefusalNote, RepairedRecord, ReportRecord, RollbackNoted, RollbackRequest, SettleOutcome,
    StoreError, StoredAttempt, UpdateSlot, FACTORY_SLOT, NVS_NAMESPACE,
};

// The NOR-flash backend of the record store (`juggler/ota-flash-kv`, enabled by `ota-ddi`).
#[cfg(any(feature = "ota-ddi", test))]
pub use juggler::ota::FlashKv;

// The DDI client: the transport and the ESP-IDF tier's `ota::ddi` names (ADR 022).
#[cfg(any(
    test,
    all(
        feature = "ota-ddi",
        any(
            feature = "esp32c3",
            feature = "esp32c6",
            feature = "esp32",
            feature = "esp32s3"
        )
    )
))]
pub mod ddi;

// The library-owned flags of the DDI runtime (`ddi::OtaFlags`).
#[cfg(any(
    test,
    all(
        feature = "ota-ddi",
        any(
            feature = "esp32c3",
            feature = "esp32c6",
            feature = "esp32",
            feature = "esp32s3"
        )
    )
))]
mod flags;

// The shared flash handle, the record store opener and the boot-fact reads of the DDI client.
#[cfg(any(
    test,
    all(
        feature = "ota-ddi",
        any(
            feature = "esp32c3",
            feature = "esp32c6",
            feature = "esp32",
            feature = "esp32s3"
        )
    )
))]
mod boot_facts;
#[cfg(any(
    test,
    all(
        feature = "ota-ddi",
        any(
            feature = "esp32c3",
            feature = "esp32c6",
            feature = "esp32",
            feature = "esp32s3"
        )
    )
))]
mod flash;
#[cfg(all(
    feature = "ota-ddi",
    any(
        feature = "esp32c3",
        feature = "esp32c6",
        feature = "esp32",
        feature = "esp32s3"
    )
))]
mod records;
#[cfg(all(
    feature = "ota-ddi",
    any(
        feature = "esp32c3",
        feature = "esp32c6",
        feature = "esp32",
        feature = "esp32s3"
    )
))]
pub use boot_facts::{note_boot_slot_hal, read_hardware_facts, slot_evidence_line};
#[cfg(all(
    feature = "ota-ddi",
    any(
        feature = "esp32c3",
        feature = "esp32c6",
        feature = "esp32",
        feature = "esp32s3"
    )
))]
pub use flash::{SharedFlash, SharedFlashTaken};
#[cfg(all(
    feature = "ota-ddi",
    any(
        feature = "esp32c3",
        feature = "esp32c6",
        feature = "esp32",
        feature = "esp32s3"
    )
))]
pub use records::{open_records, Records};

// The health verdict types of the base `ota` feature (the policy is `juggler::ota::health`).
pub use juggler::ota::{DeadlineAction, HealthVerdict};

// One const per supported chip feature. The bare-metal OTA tier supports `esp32`, `esp32c3`,
// `esp32c6` and `esp32s3`.
// No chip feature is on for host checks and the `ota` stub build; the const is then absent on
// purpose, so nothing can compare an image against a made-up chip name.
// esp-hal itself rejects enabling two chips at once, so at most one of these exists.

/// Experimental: API may change before 1.0.
///
/// The chip this firmware runs on, in the spelling [`ImageChip::from_target`] takes (`"esp32"`, `"esp32c3"`, `"esp32c6"`, `"esp32s3"`).
///
/// Selected by the crate's chip feature; absent when no chip feature is enabled.
///
/// ```ignore
/// use rustyfarian_esp_hal_network::ota::{ImageChip, TARGET_CHIP};
///
/// let chip = ImageChip::from_target(TARGET_CHIP);
/// ```
#[cfg(feature = "esp32c3")]
pub const TARGET_CHIP: &str = "esp32c3";

/// Experimental: API may change before 1.0.
///
/// The chip this firmware runs on (`"esp32c6"`); pass it to [`ImageChip::from_target`]. Absent when no chip feature is enabled.
#[cfg(feature = "esp32c6")]
pub const TARGET_CHIP: &str = "esp32c6";

/// Experimental: API may change before 1.0.
///
/// The chip this firmware runs on (`"esp32"`); pass it to [`ImageChip::from_target`]. Absent when no chip feature is enabled.
#[cfg(feature = "esp32")]
pub const TARGET_CHIP: &str = "esp32";

/// Experimental: API may change before 1.0.
///
/// The chip this firmware runs on (`"esp32s3"`); pass it to [`ImageChip::from_target`]. Absent when no chip feature is enabled.
#[cfg(feature = "esp32s3")]
pub const TARGET_CHIP: &str = "esp32s3";

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
            bytes_to_hex, check_content_length, classify_read_error, decide_offer, decide_update,
            hex_to_bytes, reconcile, ActivationPermit, Admission, AttemptRecord, BlockedBy,
            BootFacts, CancelRequest, Deadline, FailReason, ImageChip, ImageMetadata,
            OfferDecision, OtaError, OtaState, ReconcileAction, RollbackReason, SlotId, SlotState,
            StreamingVerifier, UpdateDecision, Version,
        };
        use crate::ota::{
            note_boot_slot, read_with_busy_retry, reconcile_boot, slot_for_partition,
            slot_from_label, slot_label, BootDisposition, BootFault, BootOutcome, BootWarnings,
            BusyRetry, BusyTally, CorruptRecord, Delivery, FailedDownload, HardwareFacts,
            HardwareReadError, KvError, KvStr, NoteBoot, NoteBootError, OtaKv, OtaStore,
            PendingReport, PendingReportView, ReasonNote, RefusalNote, RepairedRecord,
            ReportRecord, RollbackNoted, RollbackRequest, SettleOutcome, StoreError, StoredAttempt,
            UpdateSlot, FACTORY_SLOT, NVS_NAMESPACE,
        };

        struct NeverKv;
        impl OtaKv for NeverKv {
            fn get_u8(&self, _: &str) -> Result<Option<u8>, KvError> {
                Ok(None)
            }
            fn get_u32(&self, _: &str) -> Result<Option<u32>, KvError> {
                Ok(None)
            }
            fn get_str(&self, _: &str) -> Result<Option<KvStr>, KvError> {
                Ok(None)
            }
            fn set_u8(&mut self, _: &str, _: u8) -> Result<(), KvError> {
                Ok(())
            }
            fn set_u32(&mut self, _: &str, _: u32) -> Result<(), KvError> {
                Ok(())
            }
            fn set_str(&mut self, _: &str, _: &str) -> Result<(), KvError> {
                Ok(())
            }
            fn remove(&mut self, _: &str) -> Result<(), KvError> {
                Ok(())
            }
        }

        fn assert_exported<T>() {}
        assert_exported::<OtaError>();
        assert_exported::<OtaState>();
        assert_exported::<StreamingVerifier>();
        assert_exported::<OfferDecision>();
        assert_exported::<Admission>();
        assert_exported::<AttemptRecord>();
        assert_exported::<BootFacts>();
        assert_exported::<ReconcileAction>();
        assert_exported::<SlotId>();
        assert_exported::<SlotState>();
        assert_exported::<CancelRequest>();
        assert_exported::<ImageChip>();
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
            decide_offer(Version::new(1, 3, 0), meta.version, None, Admission::Open),
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
        assert_exported::<ActivationPermit>();
        let _permit: ActivationPermit = Deadline::none()
            .permit_activation(core::time::Duration::ZERO)
            .expect("no deadline always permits");
        assert_eq!(
            Deadline::after(core::time::Duration::ZERO)
                .check(core::time::Duration::ZERO)
                .unwrap_err(),
            OtaError::DownloadTimeout
        );
        assert_eq!(
            check_content_length(None, false, 4096),
            Err(OtaError::DownloadFailed { status: 0 })
        );
        assert_eq!(classify_read_error(true), OtaError::DownloadTimeout);
        assert_eq!(classify_read_error(false), OtaError::ServerUnreachable);
        assert_exported::<FailReason>();
        assert_exported::<RollbackReason>();
        assert_exported::<BlockedBy>();
        assert_eq!(
            RollbackReason::from_label("operator"),
            RollbackReason::Operator
        );
        #[cfg(any(
            feature = "esp32",
            feature = "esp32c3",
            feature = "esp32c6",
            feature = "esp32s3"
        ))]
        assert!(crate::ota::TARGET_CHIP.starts_with("esp32"));
        assert_exported::<BootDisposition>();
        assert_exported::<BootFault>();
        assert_exported::<FailedDownload>();
        assert_exported::<UpdateSlot>();
        assert_exported::<BootOutcome>();
        assert_exported::<BootWarnings>();
        assert_exported::<Delivery>();
        assert_exported::<RollbackNoted>();
        assert_exported::<BusyTally>();
        assert_eq!(BusyRetry::default().tries, 5);
        assert_eq!(
            read_with_busy_retry(BusyRetry::default(), || Ok::<u8, (i32, bool)>(1), |_| {}),
            Ok(1)
        );
        assert_exported::<CorruptRecord>();
        assert_exported::<HardwareFacts>();
        assert_exported::<HardwareReadError>();
        assert_exported::<KvError>();
        assert_exported::<KvStr>();
        assert_exported::<NoteBoot>();
        assert_exported::<NoteBootError>();
        assert_exported::<PendingReport>();
        assert_exported::<PendingReportView>();
        assert_exported::<ReasonNote>();
        assert_exported::<RefusalNote>();
        assert_exported::<RepairedRecord>();
        assert_exported::<ReportRecord>();
        assert_exported::<RollbackRequest>();
        assert_exported::<SettleOutcome>();
        assert_exported::<StoreError>();
        assert_exported::<StoredAttempt>();
        use crate::ota::{DeadlineAction, HealthVerdict};

        assert_exported::<DeadlineAction>();
        assert!(HealthVerdict::Applied.publishes_applied());

        let mut store = OtaStore::open(NeverKv, || 1);
        assert_eq!(
            note_boot_slot(&mut store, || Ok(SlotId(0))),
            Ok(NoteBoot::NoAttempt)
        );
        let failed = reconcile_boot(&mut store, Err(HardwareReadError::Busy), None);
        assert!(failed.refuse_mark_valid);
        assert_eq!(slot_for_partition("factory"), FACTORY_SLOT);
        assert_eq!(slot_from_label("ota_1"), Some(SlotId(1)));
        assert_eq!(slot_label(SlotId(0)), Some("ota_0"));
        assert_eq!(NVS_NAMESPACE, "ota");
        assert_eq!(
            FailReason::from_offer(OfferDecision::Skip, Admission::Open),
            Some(FailReason::UpToDate)
        );
    }
}

#[cfg(all(
    feature = "embassy",
    any(
        feature = "esp32c3",
        feature = "esp32c6",
        feature = "esp32",
        feature = "esp32s3"
    )
))]
mod manager;
#[cfg(all(
    feature = "embassy",
    any(
        feature = "esp32c3",
        feature = "esp32c6",
        feature = "esp32",
        feature = "esp32s3"
    )
))]
pub use manager::{EspHalOtaManager, OtaManagerConfig};

#[cfg(not(all(
    feature = "embassy",
    any(
        feature = "esp32c3",
        feature = "esp32c6",
        feature = "esp32",
        feature = "esp32s3"
    )
)))]
mod stub;
#[cfg(not(all(
    feature = "embassy",
    any(
        feature = "esp32c3",
        feature = "esp32c6",
        feature = "esp32",
        feature = "esp32s3"
    )
)))]
pub use stub::{EspHalOtaManager, OtaManagerConfig};
