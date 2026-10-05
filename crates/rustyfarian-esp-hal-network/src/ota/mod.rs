//! Bare-metal OTA driver for ESP32-C3/C6/ESP32.
//!
//! Wraps `esp_bootloader_esp_idf::OtaUpdater` and provides streaming
//! firmware download with strict HTTP/1.1 GET over `embassy-net`.
//!
//! All public APIs are experimental.
//!
//! # Firmware metadata (re-exported)
//!
//! The cooperative download deadline ([`Deadline`], [`ActivationPermit`]) and the read-error classifier ([`classify_read_error`]) are re-exported from `juggler::ota` as well.
//!
//! With the opt-in `ota-wire` feature (serde + `alloc`; the consumer must provide a global allocator), the OTA wire contract (`OtaCommand`, `Manifest`, `OtaStatus`, `FailReason`, `RollbackReason`, and their error types and size limits) is re-exported from `juggler::ota` as well.
//! The persisted-record store (`OtaStore`, `OtaKv`, `reconcile_boot`, `note_boot_slot`, and their outcome and error types) is re-exported with `ota-wire` too; this crate ships no `OtaKv` backend; a bare-metal consumer implements `OtaKv` over its own flash storage.
//! The consumer runtime's pure state machines are reachable as the re-exported [`runtime`] module with `ota-wire`; this crate ships no runtime glue (no MQTT on this tier, ADR 015), and the runtime's configuration and verdict types (`OtaSettings`, `OtaTimings`, `OtaStacks`, `DeadlineAction`, `HealthVerdict`, `ConfigError`) are re-exported flat.
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

// Internal HTTP/1.1 GET client — implementation detail per ADR 011 §2.
// Module is private; no item in this module is part of the public API.
mod http;

// Re-export the full public surface of `juggler::ota` for domain parity with the
// `wifi`/`espnow` modules, so OTA consumers import metadata/version types from
// this crate rather than adding a redundant direct `juggler` dependency.
pub use juggler::ota::{
    bytes_to_hex, classify_read_error, decide_offer, decide_update, hex_to_bytes, reconcile,
    ActivationPermit, Admission, AttemptRecord, BlockedBy, BootFacts, Deadline, ImageMetadata,
    OfferDecision, OtaError, OtaState, ReconcileAction, SlotId, SlotState, StreamingVerifier,
    UpdateDecision, Version,
};

// The wire contract needs `alloc` + serde, so on this tier it is opt-in via `ota-wire`.
#[cfg(any(feature = "ota-wire", test))]
pub use juggler::ota::{
    note_boot_slot, reconcile_boot, slot_for_partition, slot_from_label, slot_label,
    BootDisposition, BootFault, BootOutcome, BootWarnings, BusyTally, CommandError, CorruptRecord,
    Delivery, FailReason, FailedDownload, HardwareFacts, HardwareReadError, KvError, KvStr,
    Manifest, ManifestError, NoteBoot, NoteBootError, OtaCommand, OtaKv, OtaStatus, OtaStore,
    PendingReport, PendingReportView, ReasonNote, RefusalNote, RepairedRecord, ReportRecord,
    RollbackNoted, RollbackReason, RollbackRequest, SettleOutcome, StatusError, StoreError,
    StoredAttempt, UpdateSlot, FACTORY_SLOT, MAX_COMMAND_BYTES, MAX_MANIFEST_BYTES, NVS_NAMESPACE,
};

// The consumer runtime's pure state machines and configuration (feature `ota-wire`).
#[cfg(any(feature = "ota-wire", test))]
pub use juggler::ota::runtime;
#[cfg(any(feature = "ota-wire", test))]
pub use juggler::ota::{
    ConfigError, DeadlineAction, HealthVerdict, OtaSettings, OtaStacks, OtaTimings,
    MIN_STACK_BYTES, MIN_WORKER_STACK_BYTES,
};

// One const per supported chip feature. The bare-metal OTA tier supports `esp32`, `esp32c3` and
// `esp32c6` (`esp32s3` has no OTA storage support, see the feature table in `Cargo.toml`).
// No chip feature is on for host checks and the `ota` stub build; the const is then absent on
// purpose, so nothing can compare manifests against a made-up chip name.
// esp-hal itself rejects enabling two chips at once, so at most one of these exists.

/// Experimental: API may change before 1.0.
///
/// The chip this firmware runs on, in the spelling a manifest's `target` uses (`"esp32"`, `"esp32c3"`, `"esp32c6"`).
///
/// Pass it to `Manifest::check_target` (needs the `ota-wire` feature).
/// Selected by the crate's chip feature; absent when no chip feature is enabled.
///
/// ```ignore
/// use rustyfarian_esp_hal_network::ota::{Manifest, TARGET_CHIP};
///
/// let manifest = Manifest::parse(body)?;
/// manifest.check_target(TARGET_CHIP)?;
/// ```
#[cfg(feature = "esp32c3")]
pub const TARGET_CHIP: &str = "esp32c3";

/// Experimental: API may change before 1.0.
///
/// The chip this firmware runs on (`"esp32c6"`); pass it to `Manifest::check_target`. Absent when no chip feature is enabled.
#[cfg(feature = "esp32c6")]
pub const TARGET_CHIP: &str = "esp32c6";

/// Experimental: API may change before 1.0.
///
/// The chip this firmware runs on (`"esp32"`); pass it to `Manifest::check_target`. Absent when no chip feature is enabled.
#[cfg(feature = "esp32")]
pub const TARGET_CHIP: &str = "esp32";

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
            bytes_to_hex, classify_read_error, decide_offer, decide_update, hex_to_bytes,
            reconcile, ActivationPermit, Admission, AttemptRecord, BlockedBy, BootFacts, Deadline,
            ImageMetadata, OfferDecision, OtaError, OtaState, ReconcileAction, SlotId, SlotState,
            StreamingVerifier, UpdateDecision, Version,
        };
        use crate::ota::{
            note_boot_slot, reconcile_boot, slot_for_partition, slot_from_label, slot_label,
            BootDisposition, BootFault, BootOutcome, BootWarnings, BusyTally, CorruptRecord,
            Delivery, FailedDownload, HardwareFacts, HardwareReadError, KvError, KvStr, NoteBoot,
            NoteBootError, OtaKv, OtaStore, PendingReport, PendingReportView, ReasonNote,
            RefusalNote, RepairedRecord, ReportRecord, RollbackNoted, RollbackRequest,
            SettleOutcome, StoreError, StoredAttempt, UpdateSlot, FACTORY_SLOT, NVS_NAMESPACE,
        };
        use crate::ota::{
            CommandError, FailReason, Manifest, ManifestError, OtaCommand, OtaStatus,
            RollbackReason, StatusError, MAX_COMMAND_BYTES, MAX_MANIFEST_BYTES,
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
        assert_eq!(classify_read_error(true), OtaError::DownloadTimeout);
        assert_eq!(classify_read_error(false), OtaError::ServerUnreachable);
        assert_exported::<OtaCommand>();
        assert_exported::<CommandError>();
        assert_exported::<Manifest>();
        assert_exported::<ManifestError>();
        assert_exported::<OtaStatus<'static>>();
        assert_exported::<StatusError>();
        assert_exported::<FailReason>();
        assert_exported::<RollbackReason>();
        assert_exported::<BlockedBy>();
        assert_eq!(
            RollbackReason::from_label("operator"),
            RollbackReason::Operator
        );
        #[cfg(any(feature = "esp32", feature = "esp32c3", feature = "esp32c6"))]
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
        use crate::ota::runtime::{
            decide_intake, HealthConfig, HealthEvent, HealthMachine, IntakeDecision, ManifestBody,
            ReporterConfig, ReporterMachine, Retention, SendResult,
        };
        use crate::ota::{
            ConfigError, DeadlineAction, HealthVerdict, OtaSettings, OtaStacks, OtaTimings,
            MIN_STACK_BYTES, MIN_WORKER_STACK_BYTES,
        };

        assert_exported::<ConfigError>();
        assert_exported::<DeadlineAction>();
        assert_exported::<HealthVerdict>();
        assert_exported::<OtaStacks>();
        assert_exported::<Retention>();
        assert_eq!(MIN_STACK_BYTES, 4096);
        assert_eq!(MIN_WORKER_STACK_BYTES, 12288);
        let settings = OtaSettings::new("dev/ota/command", "dev/ota/status", Version::new(1, 0, 0))
            .expect("valid topics");
        assert_eq!(settings.timings, OtaTimings::default());
        assert_eq!(settings.stacks, OtaStacks::default());
        assert!(matches!(
            decide_intake(
                OtaCommand::parse(br#"{"action":"repair"}"#),
                false,
                || true,
                |_| SendResult::Sent
            ),
            IntakeDecision::Queued
        ));
        let mut health = HealthMachine::new(HealthConfig::from(&settings.timings));
        let _first = health.step(core::time::Duration::ZERO, false, HealthEvent::Begin);
        assert!(HealthVerdict::Applied.publishes_applied());
        let _reporter = ReporterMachine::new(ReporterConfig::from(&settings.timings));
        let _body = ManifestBody::new(core::time::Duration::from_secs(10));

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
        assert_eq!(MAX_COMMAND_BYTES, 512);
        assert_eq!(MAX_MANIFEST_BYTES, 1024);
        assert!(matches!(
            OtaCommand::parse(br#"{"manifest_url":"http://h/m.json"}"#),
            Ok(OtaCommand::Update { .. })
        ));
        assert_eq!(
            OtaCommand::parse(b"{}").unwrap_err().reason(),
            FailReason::CommandInvalid
        );
        assert_eq!(
            OtaStatus::Failed {
                reason: FailReason::from_offer(OfferDecision::Skip, Admission::Open)
                    .expect("skip is a failure")
            }
            .to_json()
            .expect("serializes"),
            r#"{"status":"failed","reason":"up_to_date"}"#
        );
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
