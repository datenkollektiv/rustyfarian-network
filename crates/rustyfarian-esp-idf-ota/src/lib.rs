//! ESP-IDF OTA driver — streaming download + SHA-256 verify + partition swap.
//!
//! All public APIs are experimental; stabilisation is deferred to the `ota-library` feature.
//!
//! # Quick Start
//!
//! ```rust,no_run
//! use rustyfarian_esp_idf_ota::{OtaSession, OtaSessionConfig};
//!
//! let mut session = OtaSession::new(OtaSessionConfig { timeout_secs: 60 }).unwrap();
//! let expected_sha256 = [0u8; 32]; // replace with real digest
//! session.fetch_and_apply("http://192.168.1.1/firmware.bin", &expected_sha256).unwrap();
//! esp_idf_svc::hal::reset::restart();
//! ```

mod downloader;
mod flasher;

pub use ota_pure::OtaError;

use std::io::Write;
use std::time::Duration;

use downloader::FirmwareDownloader;
use flasher::{FirmwareFlasher, OtaWriter};
use ota_pure::StreamingVerifier;

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
/// # use rustyfarian_esp_idf_ota::{OtaSession, OtaSessionConfig};
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
    /// The operation is streaming: the full image is never held in RAM.
    /// Bytes are written to the inactive partition as they arrive — flash
    /// **is** modified during the download, even on the failure path.
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

        let mut flasher = FirmwareFlasher::new()?;
        let mut ota_writer = flasher.begin()?;
        let mut verifier = StreamingVerifier::new();

        let result = {
            let mut verifying_writer = VerifyingWriter {
                inner: &mut ota_writer,
                verifier: &mut verifier,
            };
            downloader.download(&mut verifying_writer, |downloaded, total| {
                if let Some(total) = total {
                    log::debug!("OTA progress: {}/{} bytes", downloaded, total);
                }
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
