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
//! [`ImageMetadata`], [`Version`], [`OtaState`], [`StreamingVerifier`], the
//! hex helpers ([`bytes_to_hex`], [`hex_to_bytes`]), and the update decision
//! policy ([`decide_update`], [`UpdateDecision`]), the offer decision with its
//! refused-version guard and admission gate ([`decide_offer`],
//! [`OfferDecision`], [`Admission`]), and boot
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

mod downloader;
mod flasher;

// Re-export the full public surface of `juggler::ota` for domain parity with the
// `wifi`/`espnow` modules, so OTA consumers import metadata/version types from
// this crate rather than adding a redundant direct `juggler` dependency.
// `StreamingVerifier` is also used internally below (via this same import).
pub use juggler::ota::{
    bytes_to_hex, decide_offer, decide_update, hex_to_bytes, reconcile, Admission, AttemptRecord,
    BootFacts, ImageMetadata, OfferDecision, OtaError, OtaState, ReconcileAction, SlotId,
    SlotState, StreamingVerifier, UpdateDecision, Version,
};

use std::io::Write;
use std::time::Duration;

use downloader::{check_content_length, FirmwareDownloader};
use flasher::{FirmwareFlasher, OtaWriter};

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
}

impl OtaSession {
    /// Experimental: API may change before 1.0.
    ///
    /// Create a new OTA session.
    pub fn new(config: OtaSessionConfig) -> Result<Self, OtaError> {
        Ok(Self { config })
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
    /// Only plain `http://` URLs are supported (see ADR 011).
    pub fn fetch_and_apply(
        &mut self,
        url: &str,
        expected_sha256: &[u8; 32],
    ) -> Result<(), OtaError> {
        let downloader = FirmwareDownloader::new(url)
            .with_timeout(Duration::from_secs(self.config.timeout_secs));

        let response = downloader.connect()?;

        let mut flasher = FirmwareFlasher::new()?;
        let image_len = check_content_length(
            response.content_length(),
            flasher.update_partition_capacity()?,
        )?;
        let mut ota_writer = flasher.begin(image_len)?;
        let mut verifier = StreamingVerifier::new();

        let result = {
            let mut verifying_writer = VerifyingWriter {
                inner: &mut ota_writer,
                verifier: &mut verifier,
            };
            response.stream(image_len, &mut verifying_writer, |downloaded, total| {
                log::debug!("OTA progress: {}/{} bytes", downloaded, total);
            })
        };

        match result {
            Err(e) => {
                log::error!("Firmware download failed: {}", e);
                let _ = ota_writer.abort();
                return Err(e);
            }
            Ok(bytes) => {
                log::info!("Download complete, verifying {} bytes", bytes);
            }
        }

        let computed = verifier.finalize();
        if &computed != expected_sha256 {
            log::error!("SHA-256 mismatch — aborting OTA, boot slot unchanged");
            let _ = ota_writer.abort();
            return Err(OtaError::ChecksumMismatch);
        }

        log::info!("SHA-256 verified — completing OTA partition swap");
        ota_writer.complete()
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
