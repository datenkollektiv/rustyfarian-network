//! [`EspHalOtaManager`] — bare-metal OTA manager backed by
//! `esp_bootloader_esp_idf::OtaUpdater` and `esp-storage`.
//!
//! This module is compiled only when both a chip feature (`esp32c3`,
//! `esp32c6`, or `esp32`) **and** the `embassy` feature are active.

use embassy_net::tcp::TcpSocket;
use embassy_time::{with_timeout, Duration};
use embedded_storage::ReadStorage;
use embedded_storage::Storage;
use esp_bootloader_esp_idf::ota::OtaImageState;
use esp_bootloader_esp_idf::ota_updater::OtaUpdater;
use esp_bootloader_esp_idf::partitions::PARTITION_TABLE_MAX_LEN;
use esp_storage::FlashStorage;
use ota_pure::{OtaError, StreamingVerifier};

use crate::http::async_client::fetch_get;
use crate::http::parse_url;

/// Experimental: API may change before 1.0.
///
/// Configuration for [`EspHalOtaManager`].
#[derive(Debug, Clone, Copy)]
pub struct OtaManagerConfig {
    /// Per-operation timeout (in seconds) applied to the HTTP header read and
    /// to each individual body-chunk socket read.
    ///
    /// A value of `0` is permitted but probably not useful — `embassy_time`
    /// will return `TimeoutError` on the first poll yield, mapped to
    /// [`OtaError::DownloadTimeout`].
    /// The total wall-clock duration of [`fetch_and_apply`] is unbounded and
    /// scales with the firmware size; the timeout caps individual stalls.
    ///
    /// [`fetch_and_apply`]: EspHalOtaManager::fetch_and_apply
    pub timeout_secs: u64,
}

/// Experimental: API may change before 1.0.
///
/// Async bare-metal OTA manager.
///
/// Streams firmware from a plain `http://` URL over an
/// `embassy_net::TcpSocket`, verifies the SHA-256 digest, writes to the
/// inactive OTA partition via `esp_bootloader_esp_idf::OtaUpdater`, and
/// swaps slots by activating the next partition.
///
/// # URL format
///
/// Only `http://` URLs with IP-literal hosts are supported for the MVP.
/// DNS resolution is the caller's responsibility; pass an IP address in
/// the URL (e.g. `http://192.168.1.100/firmware.bin`).
/// `https://` is rejected per ADR 011 §2.
///
/// # Flash peripheral
///
/// The `FLASH` peripheral is consumed at construction time via `FlashStorage::new()`.
/// In `esp-storage 0.9.0`, `FlashStorage::new()` takes ownership of the
/// `esp_hal::peripherals::FLASH` peripheral and panics if called more than once
/// per boot.  The manager stores the resulting `FlashStorage` and re-uses it
/// for every OTA operation.
///
/// # Deviation from the design doc
///
/// The public signature of `new()` takes an additional
/// `esp_hal::peripherals::FLASH<'d>` argument compared to the design doc,
/// which assumed `FlashStorage::new()` was zero-argument (as in older versions
/// of `esp-storage`).  The design doc has `&self` for `mark_valid`/`rollback`;
/// the implementation uses `&mut self` because `OtaUpdater` mutably borrows
/// the flash on every call.
///
/// # Usage
///
/// ```ignore
/// let peripherals = esp_hal::init(esp_hal::Config::default());
/// let mut manager = EspHalOtaManager::new(
///     OtaManagerConfig { timeout_secs: 60 },
///     peripherals.FLASH,
/// )?;
/// manager.fetch_and_apply(&mut socket, url, &expected_sha256).await?;
/// esp_hal::system::software_reset();
/// // After reboot, call mark_valid() once the health check passes.
/// ```
pub struct EspHalOtaManager<'d> {
    config: OtaManagerConfig,
    flash: FlashStorage<'d>,
}

impl<'d> EspHalOtaManager<'d> {
    /// Experimental: API may change before 1.0.
    ///
    /// Create a new OTA manager.
    ///
    /// `flash` is the `FLASH` peripheral from `esp_hal::init()`.
    /// It is consumed to create the internal `FlashStorage`.
    pub fn new(
        config: OtaManagerConfig,
        flash: esp_hal::peripherals::FLASH<'d>,
    ) -> Result<Self, OtaError> {
        Ok(Self {
            config,
            flash: FlashStorage::new(flash),
        })
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Fetch firmware from `url`, verify its SHA-256 against `expected_sha256`,
    /// write it to the inactive OTA slot, and activate that slot for next boot.
    ///
    /// The operation is streaming: the full image is never held in RAM.
    /// Bytes are written to the inactive partition as they arrive — flash
    /// **is** modified during the download, even on the failure path.
    /// On checksum mismatch (or any download / flash error), the new slot is
    /// **not** activated and the boot slot is left unchanged, so the device
    /// continues to boot the running image; the inactive partition is left in
    /// an undefined state and will be overwritten on the next OTA attempt.
    /// Power loss between mid-download and `activate_next_partition` likewise
    /// leaves the boot slot unchanged.
    ///
    /// After this function returns `Ok(())` the caller must reboot the device.
    /// Once the new image has passed its health check, call [`mark_valid`].
    ///
    /// [`mark_valid`]: EspHalOtaManager::mark_valid
    pub async fn fetch_and_apply(
        &mut self,
        socket: &mut TcpSocket<'_>,
        url: &str,
        expected_sha256: &[u8; 32],
    ) -> Result<(), OtaError> {
        let timeout = Duration::from_secs(self.config.timeout_secs);

        // 1. Parse URL.
        let parsed = parse_url(url).map_err(OtaError::from)?;

        // 2. Create OtaUpdater borrowing the stored flash.
        //    `OtaUpdater::new` reads the partition table to locate ota_0/ota_1.
        let mut pt_buf = [0u8; PARTITION_TABLE_MAX_LEN];
        let mut updater = OtaUpdater::new(&mut self.flash, &mut pt_buf).map_err(|e| {
            log::error!("OtaUpdater::new failed: {:?}", e);
            OtaError::PartitionNotFound
        })?;

        // 3. Find the inactive partition and its size (used as max_bytes guard).
        let (mut region, _slot) = updater.next_partition().map_err(|e| {
            log::error!("next_partition failed: {:?}", e);
            OtaError::PartitionNotFound
        })?;
        // `capacity()` comes from `embedded_storage::ReadStorage`.
        let max_bytes = region.capacity() as u64;
        // Defensive: `region.write(offset: u32, ...)` and the per-chunk math
        // below cast the running offset to `u32`. The real ESP32-C3/-C6 OTA
        // partition capacity is well under 4 MiB, so this assertion is a
        // backstop against future hardware where partitions exceed `u32::MAX`.
        debug_assert!(
            max_bytes <= u32::MAX as u64,
            "OTA partition exceeds u32 offset range"
        );

        // 4. Send GET request and parse headers.
        //    `fetch_get` validates status 200, exactly-one Content-Length,
        //    no Transfer-Encoding, and Content-Length <= max_bytes.
        //    Wrapped in `with_timeout` so a stalled server cannot hang the
        //    OTA path indefinitely.
        let http_resp = with_timeout(timeout, fetch_get(socket, &parsed, max_bytes))
            .await
            .map_err(|_| {
                log::error!(
                    "OTA: HTTP header phase exceeded {}s timeout",
                    self.config.timeout_secs
                );
                OtaError::DownloadTimeout
            })??;
        let content_length = http_resp.content_length;
        log::info!("OTA: downloading {} bytes", content_length);

        // 5. Stream body: each chunk feeds both the flash region and the verifier.
        //    Each socket read is bounded by `timeout` — a peer that stops
        //    sending bytes mid-body fails the OTA attempt rather than hanging
        //    the firmware.
        let mut verifier = StreamingVerifier::new();
        let mut chunk_buf = [0u8; 512];
        let mut remaining = content_length;

        while remaining > 0 {
            let to_read = (remaining as usize).min(chunk_buf.len());
            let n = with_timeout(timeout, socket.read(&mut chunk_buf[..to_read]))
                .await
                .map_err(|_| {
                    log::error!(
                        "OTA: body chunk read exceeded {}s timeout",
                        self.config.timeout_secs
                    );
                    OtaError::DownloadTimeout
                })?
                .map_err(|_| OtaError::DownloadTimeout)?;
            if n == 0 {
                // EOF before Content-Length bytes received — peer closed the
                // socket mid-body. This is a protocol-shape failure (server
                // declared a length it did not deliver), not a stall, so use
                // the `status: 0` sentinel rather than `DownloadTimeout`.
                log::error!("OTA: short read — EOF before {} bytes remaining", remaining);
                return Err(OtaError::DownloadFailed { status: 0 });
            }
            let chunk = &chunk_buf[..n];
            verifier.update(chunk);
            // `write()` comes from `embedded_storage::Storage`.
            region
                .write((content_length - remaining) as u32, chunk)
                .map_err(|e| {
                    log::error!("Flash write failed: {:?}", e);
                    OtaError::FlashWriteFailed
                })?;
            remaining -= n as u64;
        }

        log::info!("OTA: download complete, verifying SHA-256");

        // 6. Verify digest.
        let computed = verifier.finalize();
        if &computed != expected_sha256 {
            log::error!("OTA: SHA-256 mismatch — boot slot unchanged");
            return Err(OtaError::ChecksumMismatch);
        }

        log::info!("OTA: SHA-256 verified — activating next partition");

        // 7. Activate the new slot.
        //    API mapping (esp-bootloader-esp-idf 0.5.0):
        //      activate_next_partition() = commit / finalize — writes the OTA data
        //        partition so the bootloader boots the new slot on next reset.
        //      set_current_ota_state(OtaImageState::Valid)   = mark_valid / cancel rollback
        //      set_current_ota_state(OtaImageState::Invalid) = signal bootloader to rollback
        updater.activate_next_partition().map_err(|e| {
            log::error!("activate_next_partition failed: {:?}", e);
            OtaError::FlashWriteFailed
        })?;

        log::info!("OTA: partition swap complete — reboot to apply");
        Ok(())
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Mark the running slot valid, cancelling the bootloader's automatic rollback.
    ///
    /// Call after the new firmware has passed its health check (e.g. Wi-Fi
    /// associated + 30 s dwell — see ADR 011 §4).
    ///
    /// Uses `OtaUpdater::set_current_ota_state(OtaImageState::Valid)`.
    pub fn mark_valid(&mut self) -> Result<(), OtaError> {
        let mut pt_buf = [0u8; PARTITION_TABLE_MAX_LEN];
        let mut updater = OtaUpdater::new(&mut self.flash, &mut pt_buf).map_err(|e| {
            log::error!("mark_valid: OtaUpdater::new failed: {:?}", e);
            OtaError::PartitionNotFound
        })?;
        updater
            .set_current_ota_state(OtaImageState::Valid)
            .map_err(|e| {
                log::error!("mark_valid: set_current_ota_state failed: {:?}", e);
                OtaError::FlashWriteFailed
            })
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Mark the running slot invalid, triggering bootloader rollback to the
    /// previous slot on next reset.
    ///
    /// This function returns `Ok(())` — the actual reboot is the caller's
    /// responsibility (use `esp_hal::system::software_reset()` or equivalent).
    ///
    /// Uses `OtaUpdater::set_current_ota_state(OtaImageState::Invalid)`.
    pub fn rollback(&mut self) -> Result<(), OtaError> {
        let mut pt_buf = [0u8; PARTITION_TABLE_MAX_LEN];
        let mut updater = OtaUpdater::new(&mut self.flash, &mut pt_buf).map_err(|e| {
            log::error!("rollback: OtaUpdater::new failed: {:?}", e);
            OtaError::PartitionNotFound
        })?;
        updater
            .set_current_ota_state(OtaImageState::Invalid)
            .map_err(|e| {
                log::error!("rollback: set_current_ota_state failed: {:?}", e);
                OtaError::FlashWriteFailed
            })
    }
}
