//! [`EspHalOtaManager`] — bare-metal OTA manager backed by
//! `esp_bootloader_esp_idf::OtaUpdater` and `esp-storage`.
//!
//! This module is compiled only when both a chip feature (`esp32c3`,
//! `esp32c6`, or `esp32`) **and** the `embassy` feature are active.

use core::time::Duration as CoreDuration;

use embassy_net::tcp::TcpSocket;
use embassy_time::{with_timeout, Duration, Instant};
use esp_bootloader_esp_idf::ota::OtaImageState;
use esp_bootloader_esp_idf::ota_updater::OtaUpdater;
use esp_bootloader_esp_idf::partitions::PARTITION_TABLE_MAX_LEN;
use esp_storage::FlashStorage;
use juggler::ota::{classify_read_error, ActivationPermit, Deadline, OtaError, StreamingVerifier};

use super::http::async_client::fetch_get;
use super::http::parse_url;

/// Converts an `embassy_time` duration (microsecond-exact) to `core`.
fn to_core(d: Duration) -> CoreDuration {
    CoreDuration::from_micros(d.as_micros())
}

/// Converts a `core` duration to `embassy_time`, saturating at the maximum.
fn to_embassy(d: CoreDuration) -> Duration {
    // Capped well below `u64::MAX` so `Instant::now() + d` in `with_timeout`
    // cannot overflow.
    let micros = u64::try_from(d.as_micros()).unwrap_or(u64::MAX);
    Duration::from_micros(micros.min(u64::MAX / 2))
}

/// Selects the next OTA slot for boot.
///
/// The only call site of `activate_next_partition`; the permit parameter makes
/// a timed-out download unable to reach it.
fn activate(updater: &mut OtaUpdater<'_, '_>, _permit: ActivationPermit) -> Result<(), OtaError> {
    updater.activate_next_partition().map_err(|e| {
        log::error!("activate_next_partition failed: {:?}", e);
        OtaError::FlashWriteFailed
    })
}

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
    /// Without [`with_deadline`], the total wall-clock duration of [`fetch_and_apply`] is unbounded and
    /// scales with the firmware size; the timeout caps individual stalls.
    ///
    /// [`fetch_and_apply`]: EspHalOtaManager::fetch_and_apply
    /// [`with_deadline`]: EspHalOtaManager::with_deadline
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
/// Since `esp-storage 0.9.0`, `FlashStorage::new()` takes ownership of the
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
    deadline: Deadline,
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
            deadline: Deadline::none(),
        })
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Limit the whole [`fetch_and_apply`](Self::fetch_and_apply) call to `total`.
    ///
    /// The deadline is **cooperative**: the clock starts on entry to
    /// `fetch_and_apply` and is checked at operation boundaries, namely after
    /// partition lookup, after connect and headers, after every chunk read and
    /// every flash write, after SHA-256 verification, and immediately before
    /// `activate_next_partition`.
    /// A failed check returns [`OtaError::DownloadTimeout`] and the new slot is
    /// not activated.
    ///
    /// Each network wait uses `min(per-read timeout, remaining deadline)`, so
    /// network waits never overrun the deadline; blocking flash operations
    /// (erase, write) cannot be interrupted and may overrun, with expiry
    /// detected when they return.
    /// Finalization is different: once the permit is granted and activation
    /// starts, its result is returned even if it finishes after the deadline.
    ///
    /// The deadline is expired when `elapsed >= total`.
    /// `Duration::ZERO` therefore fails with `DownloadTimeout` before any
    /// network or flash access.
    /// Not calling this method means no total limit: behaviour is unchanged
    /// apart from the read-error mapping noted below.
    ///
    /// An operation that fails reports its own error (`ChecksumMismatch`,
    /// `FlashWriteFailed`, `InsufficientSpace`, ...); `DownloadTimeout` from the
    /// deadline is reported only when the operation at that boundary succeeded
    /// but the deadline has elapsed.
    /// A body read whose wait times out is `DownloadTimeout` whether the
    /// per-read limit or the deadline was binding; a socket read *error* (for
    /// example a connection reset) is `ServerUnreachable`.
    ///
    /// Invariant: a timeout never selects the new boot slot.
    /// Activation requires an [`ActivationPermit`] that only a successful
    /// pre-activation deadline check can produce.
    ///
    /// ```ignore
    /// let mut manager = EspHalOtaManager::new(
    ///     OtaManagerConfig { timeout_secs: 30 },
    ///     peripherals.FLASH,
    /// )?
    /// .with_deadline(embassy_time::Duration::from_secs(300));
    /// ```
    #[must_use]
    pub fn with_deadline(mut self, total: Duration) -> Self {
        self.deadline = Deadline::after(to_core(total));
        self
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
    /// With [`with_deadline`](Self::with_deadline) set, the whole call is bounded by that total.
    ///
    /// [`mark_valid`]: EspHalOtaManager::mark_valid
    pub async fn fetch_and_apply(
        &mut self,
        socket: &mut TcpSocket<'_>,
        url: &str,
        expected_sha256: &[u8; 32],
    ) -> Result<(), OtaError> {
        let start = Instant::now();
        let deadline = self.deadline;
        let per_op = CoreDuration::from_secs(self.config.timeout_secs);
        let elapsed = || to_core(start.elapsed());
        let budget = || to_embassy(deadline.clip(per_op, elapsed()));

        // 0. A zero (or already-expired) deadline fails before any network or
        //    flash access.
        deadline.check(elapsed())?;

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
        // `capacity()` is an inherent `FlashRegion` method (the `embedded-storage`
        // trait impls are opt-in since esp-bootloader-esp-idf 0.6).
        let max_bytes = region.capacity() as u64;
        // Defensive: `region.write(offset: u32, ...)` and the per-chunk math
        // below cast the running offset to `u32`. The real ESP32-C3/-C6 OTA
        // partition capacity is well under 4 MiB, so this assertion is a
        // backstop against future hardware where partitions exceed `u32::MAX`.
        debug_assert!(
            max_bytes <= u32::MAX as u64,
            "OTA partition exceeds u32 offset range"
        );
        deadline.check(elapsed())?;

        // 4. Send GET request and parse headers.
        //    `fetch_get` validates status 200, exactly-one Content-Length,
        //    no Transfer-Encoding, and 0 < Content-Length <= max_bytes.
        //    The wait is `min(per-read timeout, remaining deadline)` so a
        //    stalled server cannot hang the OTA path or outlast the deadline.
        let http_resp = with_timeout(budget(), fetch_get(socket, &parsed, max_bytes))
            .await
            .map_err(|_| {
                log::error!(
                    "OTA: HTTP header phase timed out (per-read {}s or deadline)",
                    self.config.timeout_secs
                );
                classify_read_error(true)
            })??;
        deadline.check(elapsed())?;
        let content_length = http_resp.content_length;
        log::info!("OTA: downloading {} bytes", content_length);

        // 5. Stream body: each chunk feeds both the flash region and the verifier.
        //    Each socket read waits at most `min(per-read timeout, remaining
        //    deadline)`; a timed-out wait is `DownloadTimeout`, a socket read
        //    error is `ServerUnreachable`.
        let mut verifier = StreamingVerifier::new();
        let mut chunk_buf = [0u8; 512];
        let mut remaining = content_length;

        while remaining > 0 {
            let to_read = (remaining as usize).min(chunk_buf.len());
            let n = with_timeout(budget(), socket.read(&mut chunk_buf[..to_read]))
                .await
                .map_err(|_| {
                    log::error!(
                        "OTA: body chunk read timed out (per-read {}s or deadline)",
                        self.config.timeout_secs
                    );
                    classify_read_error(true)
                })?
                .map_err(|e| {
                    log::error!("OTA: body chunk read failed: {:?}", e);
                    classify_read_error(false)
                })?;
            if n == 0 {
                // EOF before Content-Length bytes received — peer closed the
                // socket mid-body. This is a protocol-shape failure (server
                // declared a length it did not deliver), not a stall, so use
                // the `status: 0` sentinel rather than `DownloadTimeout`.
                log::error!("OTA: short read — EOF before {} bytes remaining", remaining);
                return Err(OtaError::DownloadFailed { status: 0 });
            }
            deadline.check(elapsed())?;
            let chunk = &chunk_buf[..n];
            verifier.update(chunk);
            // `write()` is the inherent `FlashRegion` method (see `capacity()` above).
            region
                .write((content_length - remaining) as u32, chunk)
                .map_err(|e| {
                    log::error!("Flash write failed: {:?}", e);
                    OtaError::FlashWriteFailed
                })?;
            deadline.check(elapsed())?;
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
        deadline.check(elapsed())?;

        // 7. Pre-activation check: the permit is the only way to call `activate`.
        let permit = deadline.permit_activation(elapsed()).inspect_err(|_| {
            log::error!("OTA: deadline exceeded before activation — boot slot unchanged");
        })?;

        // 8. Activate the new slot, with nothing between the permit and this call.
        //    API mapping (esp-bootloader-esp-idf 0.5.0):
        //      activate_next_partition() = commit / finalize — writes the OTA data
        //        partition so the bootloader boots the new slot on next reset.
        //      set_current_ota_state(OtaImageState::Valid)   = mark_valid / cancel rollback
        //      set_current_ota_state(OtaImageState::Invalid) = signal bootloader to rollback
        activate(&mut updater, permit)?;

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
