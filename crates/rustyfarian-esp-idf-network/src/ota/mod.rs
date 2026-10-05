//! ESP-IDF OTA driver — streaming download + SHA-256 verify + partition swap.
//!
//! All public APIs are experimental; stabilisation is deferred to the `ota-library` feature.
//!
//! # Quick Start
//!
//! ```rust,no_run
//! use rustyfarian_esp_idf_network::ota::{OtaSession, OtaSessionConfig};
//!
//! let mut session = OtaSession::new(OtaSessionConfig { timeout_secs: 60 }).unwrap();
//! let expected_sha256 = [0u8; 32]; // replace with real digest
//! session.fetch_and_apply("http://192.168.1.1/firmware.bin", &expected_sha256).unwrap();
//! esp_idf_svc::hal::reset::restart();
//! ```
//!
//! # Firmware metadata (re-exported)
//!
//! The cooperative download deadline ([`Deadline`], [`ActivationPermit`]) and the read-error classifier ([`classify_read_error`]) are re-exported from `juggler::ota` as well.
//!
//! The persisted OTA records ([`OtaStore`], [`OtaKv`], [`reconcile_boot`], [`note_boot_slot`], and their outcome and error types) are re-exported from `juggler::ota` as well; [`EspNvsKv`], [`open_store`] and [`open_store_in`] are the NVS backend, [`read_hardware_facts`] and [`note_boot_slot_idf`] read the bootloader.
//!
//! The OTA wire contract ([`OtaCommand`], [`Manifest`], [`OtaStatus`], [`FailReason`], [`RollbackReason`], and their error types and size limits) is re-exported from `juggler::ota` as well.
//!
//! [`ImageMetadata`], [`Version`], [`OtaState`], [`StreamingVerifier`], the
//! hex helpers ([`bytes_to_hex`], [`hex_to_bytes`]), and the update decision
//! policy ([`decide_update`], [`UpdateDecision`]), the offer decision with its
//! refused-version guard and admission gate ([`decide_offer`],
//! [`OfferDecision`], [`Admission`], [`BlockedBy`]), and boot
//! reconciliation ([`reconcile`], [`AttemptRecord`], [`BootFacts`],
//! [`ReconcileAction`], [`SlotId`], [`SlotState`]) are re-exported from
//! `juggler::ota` here — matching the `wifi`/`espnow` domains — so a
//! version-gated updater needs only this crate, with no separate `juggler`
//! dependency to parse a sidecar digest + version and decide whether to apply it:
//!
//! ```
//! use rustyfarian_esp_idf_network::ota::{decide_update, ImageMetadata, UpdateDecision, Version};
//!
//! let meta = ImageMetadata::parse(
//!     "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
//!     "1.4.0",
//! )
//! .unwrap();
//! assert_eq!(meta.version, Version::new(1, 4, 0));
//! assert_eq!(
//!     decide_update(Version::new(1, 3, 0), meta.version),
//!     UpdateDecision::Apply
//! );
//! ```

mod boot_facts;
mod downloader;
mod flasher;
#[cfg(all(feature = "ota", feature = "mqtt"))]
pub mod runtime;
mod store;

pub use boot_facts::{note_boot_slot_idf, read_hardware_facts, slot_evidence_line, BusyRetry};
pub use store::{open_store, open_store_in, EspNvsKv};

// The runtime's configuration and verdict types, flat like the rest of the `juggler::ota` surface.
// The state machines stay under `juggler::ota::runtime`; this crate's own `runtime` module is the glue.
pub use juggler::ota::{
    ConfigError, DeadlineAction, HealthVerdict, OtaSettings, OtaStacks, OtaTimings,
    MIN_STACK_BYTES, MIN_WORKER_STACK_BYTES,
};

// Re-export the full public surface of `juggler::ota` for domain parity with the
// `wifi`/`espnow` modules, so OTA consumers import metadata/version types from
// this crate rather than adding a redundant direct `juggler` dependency.
// `StreamingVerifier` is also used internally below (via this same import).
pub use juggler::ota::{
    bytes_to_hex, classify_read_error, decide_offer, decide_update, hex_to_bytes, note_boot_slot,
    reconcile, reconcile_boot, slot_for_partition, slot_from_label, slot_label, ActivationPermit,
    Admission, AttemptRecord, BlockedBy, BootDisposition, BootFacts, BootFault, BootOutcome,
    BootWarnings, BusyTally, CommandError, CorruptRecord, Deadline, Delivery, FailReason,
    FailedDownload, HardwareFacts, HardwareReadError, ImageMetadata, KvError, KvStr, Manifest,
    ManifestError, NoteBoot, NoteBootError, OfferDecision, OtaCommand, OtaError, OtaKv, OtaState,
    OtaStatus, OtaStore, PendingReport, PendingReportView, ReasonNote, ReconcileAction,
    RefusalNote, RepairedRecord, ReportRecord, RollbackNoted, RollbackReason, RollbackRequest,
    SettleOutcome, SlotId, SlotState, StatusError, StoreError, StoredAttempt, StreamingVerifier,
    UpdateDecision, UpdateSlot, Version, FACTORY_SLOT, MAX_COMMAND_BYTES, MAX_MANIFEST_BYTES,
    NVS_NAMESPACE,
};

macro_rules! target_chip {
    ($($cfg:ident => $chip:literal),* $(,)?) => {
        $(
            /// Experimental: API may change before 1.0.
            ///
            /// The chip this firmware runs on, in the spelling a manifest's `target` uses (`"esp32"`, `"esp32c3"`, `"esp32c6"`, `"esp32s3"`, ...).
            ///
            /// Pass it to `Manifest::check_target`.
            /// Derived from the ESP-IDF target cfg (`esp_idf_idf_target_<chip>`), which `esp-idf-sys` propagates to this crate through `embuild::espidf::sysenv::output()` in `build.rs`, the same way the SoftAP cfg is.
            /// It is not defined when no known chip cfg is active, so a build for an unlisted chip fails with "cannot find `TARGET_CHIP`" instead of silently comparing manifests against a wrong chip name.
            ///
            /// ```ignore
            /// use rustyfarian_esp_idf_network::ota::{Manifest, TARGET_CHIP};
            ///
            /// let manifest = Manifest::parse(body)?;
            /// manifest.check_target(TARGET_CHIP)?;
            /// ```
            #[cfg($cfg)]
            pub const TARGET_CHIP: &str = $chip;
        )*
    };
}

target_chip! {
    esp_idf_idf_target_esp32 => "esp32",
    esp_idf_idf_target_esp32s2 => "esp32s2",
    esp_idf_idf_target_esp32s3 => "esp32s3",
    esp_idf_idf_target_esp32c2 => "esp32c2",
    esp_idf_idf_target_esp32c3 => "esp32c3",
    esp_idf_idf_target_esp32c5 => "esp32c5",
    esp_idf_idf_target_esp32c6 => "esp32c6",
    esp_idf_idf_target_esp32h2 => "esp32h2",
    esp_idf_idf_target_esp32p4 => "esp32p4",
}

#[cfg(test)]
mod target_chip_guard {
    use super::{Manifest, TARGET_CHIP};

    /// Fails to compile if no chip cfg reaches this crate (the const would be absent).
    #[test]
    fn target_chip_is_a_manifest_target_name() {
        assert!(TARGET_CHIP.starts_with("esp32"));
        let body = format!(
            r#"{{"version":"1.0.0","sha256":"{}","url":"http://h/fw.bin","target":"{TARGET_CHIP}"}}"#,
            "0".repeat(64)
        );
        let manifest = Manifest::parse(body.as_bytes()).expect("valid manifest");
        assert!(manifest.check_target(TARGET_CHIP).is_ok());
    }
}

use std::io::Write;
use std::time::{Duration, Instant};

use downloader::{check_content_length, FirmwareDownloader};
use flasher::{FirmwareFlasher, OtaWriter};

pub use downloader::url_for_log;

use esp_idf_svc::ota::EspOta;

/// Experimental: API may change before 1.0.
///
/// Configuration for an [`OtaSession`].
#[derive(Debug, Clone, Copy)]
pub struct OtaSessionConfig {
    /// HTTP connection + read timeout in seconds.
    pub timeout_secs: u64,
}

/// Experimental: API may change before 1.0.
///
/// Single-use OTA session that streams firmware from a plain HTTP server,
/// verifies the SHA-256 digest, and writes to the inactive OTA partition.
///
/// # Example
///
/// ```rust,no_run
/// # use rustyfarian_esp_idf_network::ota::{OtaSession, OtaSessionConfig};
/// let mut session = OtaSession::new(OtaSessionConfig { timeout_secs: 60 }).unwrap();
/// ```
#[derive(Debug)]
pub struct OtaSession {
    config: OtaSessionConfig,
    deadline: Deadline,
}

impl OtaSession {
    /// Experimental: API may change before 1.0.
    ///
    /// Create a new OTA session.
    pub fn new(config: OtaSessionConfig) -> Result<Self, OtaError> {
        Ok(Self {
            config,
            deadline: Deadline::none(),
        })
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Limit the whole [`fetch_and_apply`](Self::fetch_and_apply) call to `total`.
    ///
    /// The deadline is **cooperative**: the clock starts on entry to
    /// `fetch_and_apply` and is checked at operation boundaries, namely after
    /// connect and headers, after partition preparation (the `begin()` erase),
    /// after every chunk read and every flash write, after SHA-256
    /// verification, and immediately before the call that selects the new boot
    /// slot.
    /// A failed check aborts the flash session and returns
    /// [`OtaError::DownloadTimeout`].
    ///
    /// Blocking operations cannot be interrupted; expiry is detected when they
    /// return.
    /// Finalization is the exception: once the permit is granted and
    /// finalization starts, its result is returned even if it finishes after
    /// the deadline.
    /// Network waits are bounded only by the per-read timeout
    /// (`OtaSessionConfig::timeout_secs`) fixed at connection creation, so the
    /// worst-case overrun is one network phase (connect plus headers, which
    /// includes DNS resolution not covered by that timeout, or one read) or one
    /// flash operation (erase, write, finalization), whichever is longer.
    ///
    /// The deadline is expired when `elapsed >= total`.
    /// `Duration::ZERO` therefore fails with `DownloadTimeout` before any
    /// network or flash access.
    /// Not calling this method means no total limit; apart from the read-error
    /// mapping below, behaviour is unchanged from v0.5.0.
    ///
    /// An operation that fails reports its own error (`ChecksumMismatch`,
    /// `FlashWriteFailed`, `InsufficientSpace`, ...); `DownloadTimeout` from the
    /// deadline is reported only when the operation at that boundary succeeded
    /// but the deadline has elapsed.
    /// A single read that times out on its own (`ESP_ERR_HTTP_EAGAIN`) is
    /// `DownloadTimeout` regardless of the deadline; any other read error is
    /// `ServerUnreachable`.
    ///
    /// Invariant: a timeout never selects the new boot slot.
    /// Finalization requires an [`ActivationPermit`] that only a successful
    /// pre-activation deadline check can produce; if that check fails the flash
    /// session is aborted and the boot slot is unchanged.
    ///
    /// ```rust,no_run
    /// # use std::time::Duration;
    /// # use rustyfarian_esp_idf_network::ota::{OtaSession, OtaSessionConfig};
    /// let mut session = OtaSession::new(OtaSessionConfig { timeout_secs: 30 })
    ///     .unwrap()
    ///     .with_deadline(Duration::from_secs(300));
    /// ```
    #[must_use]
    pub fn with_deadline(mut self, total: Duration) -> Self {
        self.deadline = Deadline::after(total);
        self
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Fetch firmware from `url`, verify its SHA-256 against `expected_sha256`,
    /// write it to the inactive OTA slot, and set that slot as the boot partition.
    ///
    /// The server is contacted before any flash is touched: a connection or
    /// HTTP-status failure, a response without a `Content-Length` (including
    /// chunked transfer), and an image larger than the inactive partition
    /// ([`OtaError::InsufficientSpace`]) all fail without erasing anything.
    /// A body that ends before `Content-Length` bytes arrive fails with
    /// `DownloadFailed { status: 0 }`, as on the esp-hal tier.
    ///
    /// The operation is streaming: the full image is never held in RAM.
    /// Once the headers pass, only the sectors the image needs are erased and
    /// bytes are written to the inactive partition as they arrive — flash
    /// **is** modified during the download, even on the failure path.
    /// The connection stays open, unread, during that erase; a server whose
    /// send timeout is shorter than the erase drops it, which fails safely as
    /// `ServerUnreachable` with the boot slot unchanged.
    /// On verification failure (or any download / flash error), the OTA write
    /// session is aborted and the **boot slot is left unchanged**, so the
    /// device continues to boot the running image; the inactive partition is
    /// left in an undefined state and will be overwritten on the next OTA
    /// attempt. Power loss between mid-download and slot activation likewise
    /// leaves the boot slot unchanged.
    ///
    /// With [`with_deadline`](Self::with_deadline) set, the whole call is bounded
    /// by that total (see its documentation for the checkpoints and overrun
    /// limits).
    ///
    /// Only plain `http://` URLs are supported (see ADR 011).
    pub fn fetch_and_apply(
        &mut self,
        url: &str,
        expected_sha256: &[u8; 32],
    ) -> Result<(), OtaError> {
        let start = Instant::now();
        let deadline = self.deadline;

        deadline.check(start.elapsed())?;

        let downloader = FirmwareDownloader::new(url)
            .with_timeout(Duration::from_secs(self.config.timeout_secs));

        let response = downloader.connect()?;
        deadline.check(start.elapsed())?;

        let mut flasher = FirmwareFlasher::new()?;
        let image_len = check_content_length(
            response.content_length(),
            flasher.update_partition_capacity()?,
        )?;
        let mut ota_writer = flasher.begin(image_len)?;
        if let Err(e) = deadline.check(start.elapsed()) {
            return Err(abort_with(ota_writer, e));
        }
        let mut verifier = StreamingVerifier::new();

        let result = {
            let mut verifying_writer = VerifyingWriter {
                inner: &mut ota_writer,
                verifier: &mut verifier,
            };
            response.stream(
                image_len,
                &mut verifying_writer,
                |downloaded, total| {
                    log::debug!("OTA progress: {}/{} bytes", downloaded, total);
                },
                || deadline.check(start.elapsed()),
            )
        };

        match result {
            Err(e) => {
                log::error!("Firmware download failed: {}", e);
                return Err(abort_with(ota_writer, e));
            }
            Ok(bytes) => {
                log::info!("Download complete, verifying {} bytes", bytes);
            }
        }

        let computed = verifier.finalize();
        if &computed != expected_sha256 {
            log::error!("SHA-256 mismatch — aborting OTA, boot slot unchanged");
            return Err(abort_with(ota_writer, OtaError::ChecksumMismatch));
        }
        log::info!("SHA-256 verified — completing OTA partition swap");
        if let Err(e) = deadline.check(start.elapsed()) {
            log::error!("OTA deadline exceeded after verification — aborting, boot slot unchanged");
            return Err(abort_with(ota_writer, e));
        }

        let permit = match deadline.permit_activation(start.elapsed()) {
            Ok(permit) => permit,
            Err(e) => {
                log::error!(
                    "OTA deadline exceeded before activation — aborting, boot slot unchanged"
                );
                return Err(abort_with(ota_writer, e));
            }
        };
        ota_writer.complete(permit)
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Mark the running slot valid, cancelling the bootloader's automatic rollback.
    ///
    /// Call this after the new firmware has passed its health check.
    pub fn mark_valid(&mut self) -> Result<(), OtaError> {
        let mut ota = EspOta::new().map_err(|e| {
            log::error!("Failed to acquire EspOta handle for mark_valid: {:?}", e);
            OtaError::FlashWriteFailed
        })?;

        ota.mark_running_slot_valid().map_err(|e| {
            log::error!("mark_running_slot_valid failed: {:?}", e);
            OtaError::FlashWriteFailed
        })
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Revert to the previous OTA slot and reboot.
    ///
    /// This function does not return on success — the device reboots into the
    /// previous firmware.
    /// Returns `Err` only if rollback is not possible (e.g. no valid previous slot).
    pub fn rollback(&mut self) -> Result<(), OtaError> {
        let mut ota = EspOta::new().map_err(|e| {
            log::error!("Failed to acquire EspOta handle for rollback: {:?}", e);
            OtaError::FlashWriteFailed
        })?;

        // `mark_running_slot_invalid_and_reboot` never returns on success (device reboots).
        // It only returns on failure — surface as `FlashWriteFailed` so callers
        // can distinguish "no rollback target" (would surface as
        // `PartitionNotFound` from `EspOta::new` above) from "rollback path
        // failed for some other reason".
        let err = ota.mark_running_slot_invalid_and_reboot();
        log::error!("Rollback failed (no valid previous slot?): {:?}", err);
        Err(OtaError::FlashWriteFailed)
    }
}

/// Adapter that feeds every write to both an `OtaWriter` (flash) and a
/// `StreamingVerifier` (SHA-256) without any intermediate allocation.
struct VerifyingWriter<'a, 'b> {
    inner: &'a mut OtaWriter<'b>,
    verifier: &'a mut StreamingVerifier,
}

impl Write for VerifyingWriter<'_, '_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.verifier.update(buf);
        self.inner.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Abort the flash session and hand back the error that ended it.
///
/// The abort result is deliberately ignored: the original error is the one the
/// caller needs, and `OtaWriter::abort` already logs its own failure.
fn abort_with(writer: OtaWriter<'_>, err: OtaError) -> OtaError {
    let _ = writer.abort();
    err
}
