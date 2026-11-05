//! [`EspHalOtaManager`] — bare-metal OTA manager backed by
//! `esp_bootloader_esp_idf::OtaUpdater` and `esp-storage`.
//!
//! This module is compiled only when both a chip feature (`esp32c3`,
//! `esp32c6`, `esp32`, or `esp32s3`) **and** the `embassy` feature are active.

use core::time::Duration as CoreDuration;

use embassy_net::tcp::TcpSocket;
use embassy_time::{with_timeout, Duration, Instant};
use embedded_storage::nor_flash::NorFlash;
use esp_bootloader_esp_idf::ota::OtaImageState;
use esp_bootloader_esp_idf::ota_updater::OtaUpdater;
use esp_bootloader_esp_idf::partitions::{
    AppPartitionSubType, FlashRegion, NorFlashRegion, PARTITION_TABLE_MAX_LEN,
};
use esp_storage::FlashStorage;
use juggler::ota::download::{DownloadAction, DownloadInput, DownloadMachine};
use juggler::ota::{
    classify_read_error, round_up, ActivationPermit, ChunkAligner, Deadline, OtaError, SlotId,
    StreamingVerifier,
};

use super::activation::{activate_as_new, SlotControl};
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

/// Bytes requested per socket read.
const CHUNK_LEN: usize = 512;

/// A socket read buffer whose start is word-aligned, so a NOR driver writes
/// from it without copying through a sector-sized temporary.
#[repr(align(4))]
struct ChunkBuf([u8; CHUNK_LEN]);

/// Where downloaded bytes go.
///
/// `FlashRegion::write` is read-modify-erase-write: it reads the whole 4 KiB
/// sector, patches the chunk in, erases the sector and writes it back, so a
/// 512-byte chunk stream erased each sector eight times per image (bug 004).
/// An unencrypted partition therefore gets its sectors erased once up front and
/// is written through the non-erasing `NorFlash` view.
/// An encrypted partition keeps the read-modify-write path, because the ROM
/// encrypts whole sectors and the plain view is not available for it.
enum Sink<'r, 'a, 'd> {
    /// Read-modify-erase-write; accepts any offset and length.
    ReadModifyWrite(&'r mut FlashRegion<'a, 'd>),
    /// Erased once by the caller; needs word-aligned, contiguous writes.
    Nor(NorFlashRegion<'r, 'a, 'd>),
}

/// Selects the next OTA slot for boot.
///
/// The only call site of `activate_next_partition`; the permit parameter makes
/// a timed-out download unable to reach it.
fn activate(updater: &mut OtaUpdater<'_, '_>, _permit: ActivationPermit) -> Result<(), OtaError> {
    activate_as_new(&mut UpdaterControl(updater)).map_err(|e| {
        log::error!("activating the next OTA slot failed: {}", e);
        OtaError::FlashWriteFailed
    })
}

/// [`SlotControl`] over the real otadata writes.
struct UpdaterControl<'u, 'a, 'b>(&'u mut OtaUpdater<'a, 'b>);

impl SlotControl for UpdaterControl<'_, '_, '_> {
    type Error = &'static str;

    fn activate_next(&mut self) -> Result<(), Self::Error> {
        self.0.activate_next_partition().map_err(|e| {
            log::error!("activate_next_partition failed: {:?}", e);
            "activate_next_partition"
        })
    }

    fn mark_activated_new(&mut self) -> Result<(), Self::Error> {
        self.0
            .set_current_ota_state(OtaImageState::New)
            .map_err(|e| {
                log::error!("set_current_ota_state(New) failed: {:?}", e);
                "set_current_ota_state(New)"
            })
    }
}

/// Writes one chunk at `offset` through the selected path.
fn write_chunk(
    sink: &mut Sink<'_, '_, '_>,
    aligner: &mut ChunkAligner,
    offset: u32,
    chunk: &[u8],
) -> Result<(), OtaError> {
    match sink {
        // `write()` is the inherent `FlashRegion` method: it erases the
        // sector around the chunk (bug 004), which is required here
        // because the partition is encrypted.
        Sink::ReadModifyWrite(region) => region.write(offset, chunk).map_err(|e| {
            log::error!("Flash write failed: {:?}", e);
            OtaError::FlashWriteFailed
        }),
        // The partition is blank, so the chunk goes straight to flash;
        // the aligner holds back a partial word for the next chunk.
        Sink::Nor(nor) => aligner
            .push(offset, chunk, |at, data| nor.write(at, data))
            .map_err(|e| {
                log::error!("Flash write failed: {:?}", e);
                OtaError::FlashWriteFailed
            }),
    }
}

/// Writes the tail the aligner held back (the `NorFlash` path only).
fn flush_tail(
    sink: &mut Sink<'_, '_, '_>,
    aligner: &mut ChunkAligner,
    len: u32,
) -> Result<(), OtaError> {
    match sink {
        Sink::Nor(nor) => aligner
            .finish(len, |at, data| nor.write(at, data))
            .map_err(|e| {
                log::error!("OTA: final flash write failed: {:?}", e);
                OtaError::FlashWriteFailed
            }),
        Sink::ReadModifyWrite(_) => Ok(()),
    }
}

/// Feeds the last outcome and returns the machine's final result.
fn finish(
    machine: &mut DownloadMachine,
    input: DownloadInput,
    elapsed: CoreDuration,
) -> Result<(), OtaError> {
    match machine.poll_bounded(input, elapsed)? {
        DownloadAction::Done(result) => result,
        other => Err(unexpected(&other)),
    }
}

/// An action the loop's current phase cannot perform: an internal error.
fn unexpected(action: &DownloadAction) -> OtaError {
    log::error!("OTA: unexpected download step {:?}", action);
    OtaError::DownloadFailed { status: 0 }
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
    /// `fetch_and_apply` and is checked at operation boundaries, namely on
    /// entry, after connect and headers, after partition preparation (the
    /// erase), after every chunk read and every flash write, and immediately
    /// before `activate_next_partition`.
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
    /// The server is contacted before the partition is prepared: a connection
    /// or HTTP-status failure, a response without a `Content-Length` or with a
    /// `Transfer-Encoding` (`DownloadFailed { status: 0 }`), and an image larger
    /// than the inactive partition ([`OtaError::InsufficientSpace`]) all fail
    /// without erasing anything; the rule is
    /// [`check_content_length`](juggler::ota::check_content_length), and the
    /// order of the steps is `juggler::ota::download::DownloadMachine`, shared
    /// with the ESP-IDF tier.
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
        let elapsed = || to_core(start.elapsed());
        let per_op = CoreDuration::from_secs(self.config.timeout_secs);
        let mut machine = DownloadMachine::new(self.deadline, per_op, *expected_sha256);

        // 1. Connect, read the headers and size the partition; nothing is
        //    prepared yet, so an error returns without an abort.
        let mut input = DownloadInput::Begin;
        let len = loop {
            let action = machine
                .poll_bounded(input, elapsed())
                .inspect_err(|e| log::error!("OTA: download stopped: {}", e))?;
            input = match action {
                DownloadAction::Connect { budget } => self.connect(socket, url, budget).await,
                DownloadAction::Prepare { len } => break len,
                DownloadAction::Done(result) => return result,
                other => return Err(unexpected(&other)),
            };
        };
        log::info!("OTA: downloading {} bytes", len);

        // 2. Only now is the partition table read for writing.
        //    `OtaUpdater::new` reads it to locate ota_0/ota_1.
        let mut pt_buf = [0u8; PARTITION_TABLE_MAX_LEN];
        let mut updater = match OtaUpdater::new(&mut self.flash, &mut pt_buf) {
            Ok(updater) => updater,
            Err(e) => {
                log::error!("OtaUpdater::new failed: {:?}", e);
                return finish(
                    &mut machine,
                    DownloadInput::PrepareFailed(OtaError::PartitionNotFound),
                    elapsed(),
                );
            }
        };

        // The region borrows `updater`, which activation needs back, so the
        // stream runs in its own scope and hands out either the permit or the
        // final result.
        let streamed = {
            let mut region = match updater.next_partition() {
                Ok((region, _slot)) => region,
                Err(e) => {
                    log::error!("next_partition failed: {:?}", e);
                    return finish(
                        &mut machine,
                        DownloadInput::PrepareFailed(OtaError::PartitionNotFound),
                        elapsed(),
                    );
                }
            };

            // 2a. Pick the write path, erasing an unencrypted partition's
            //     sectors once here instead of once per chunk (bug 004). The
            //     span covers the image rounded up to whole sectors, which also
            //     leaves room for the aligner's final word padding; the machine
            //     admitted `len <= capacity`, and a partition is a whole number
            //     of sectors, so the span never leaves the partition.
            let sector = FlashStorage::SECTOR_SIZE;
            let erased_span = round_up(len, sector).min(region.capacity() as u32);
            let mut aligner =
                ChunkAligner::new(<NorFlashRegion<'_, '_, '_> as NorFlash>::WRITE_SIZE);
            // `as_nor_flash()` refuses an encrypted partition (the ROM encrypts
            // whole sectors, so that path must keep read-modify-erase-write).
            // It is probed first and taken again below: the borrow of a failed
            // probe would otherwise live through the arm that needs `region` back.
            let nor_capable = region.as_nor_flash().is_ok();
            let prepared = if nor_capable {
                match region.as_nor_flash() {
                    Err(e) => {
                        log::error!("OTA: partition is not writable through NorFlash: {:?}", e);
                        Err(OtaError::FlashWriteFailed)
                    }
                    Ok(mut nor) => {
                        log::info!("OTA: erasing {} bytes of the target partition", erased_span);
                        match nor.erase(0, erased_span) {
                            Ok(()) => Ok(Sink::Nor(nor)),
                            Err(e) => {
                                log::error!("OTA: erase of the target partition failed: {:?}", e);
                                Err(OtaError::FlashWriteFailed)
                            }
                        }
                    }
                }
            } else {
                Ok(Sink::ReadModifyWrite(&mut region))
            };
            let mut sink = match prepared {
                Ok(sink) => sink,
                Err(e) => {
                    return finish(&mut machine, DownloadInput::PrepareFailed(e), elapsed());
                }
            };

            // 3. Stream the body: each chunk feeds both the flash region and
            //    the verifier. Each socket read waits at most `min(per-read
            //    timeout, remaining deadline)`; a timed-out wait is
            //    `DownloadTimeout`, a socket read error is `ServerUnreachable`.
            let mut verifier = StreamingVerifier::new();
            // Word-aligned so `write_nor` writes from this buffer directly; a
            // misaligned slice would make it copy through its own 4 KiB sector
            // buffer, on the stack, once per chunk.
            let mut chunk_buf = ChunkBuf([0u8; CHUNK_LEN]);
            let mut written: u32 = 0;
            let mut input = DownloadInput::Prepared;
            loop {
                let action = match machine.poll_bounded(input, elapsed()) {
                    Ok(action) => action,
                    // Nothing on this tier needs undoing: the slot is simply
                    // never activated.
                    Err(e) => {
                        log::error!("OTA: download stopped: {} — boot slot unchanged", e);
                        break Err(e);
                    }
                };
                input = match action {
                    DownloadAction::Read { max, budget } => {
                        let to_read = (max as usize).min(CHUNK_LEN);
                        match with_timeout(
                            to_embassy(budget),
                            socket.read(&mut chunk_buf.0[..to_read]),
                        )
                        .await
                        {
                            Err(_) => {
                                log::error!(
                                    "OTA: body chunk read timed out (per-read {}s or deadline)",
                                    self.config.timeout_secs
                                );
                                DownloadInput::ReadFailed(classify_read_error(true))
                            }
                            Ok(Err(e)) => {
                                log::error!("OTA: body chunk read failed: {:?}", e);
                                DownloadInput::ReadFailed(classify_read_error(false))
                            }
                            Ok(Ok(n)) => DownloadInput::Read(n),
                        }
                    }
                    DownloadAction::Write { n } => {
                        let chunk = &chunk_buf.0[..n];
                        verifier.update(chunk);
                        match write_chunk(&mut sink, &mut aligner, written, chunk) {
                            Ok(()) => {
                                written += n as u32;
                                DownloadInput::Written
                            }
                            Err(e) => DownloadInput::WriteFailed(e),
                        }
                    }
                    // Write the tail the aligner held back, padded with 0xFF
                    // inside the erased span, then hand over the digest.
                    DownloadAction::Finalize => match flush_tail(&mut sink, &mut aligner, len) {
                        Err(e) => DownloadInput::WriteFailed(e),
                        Ok(()) => {
                            log::info!("OTA: download complete, verifying SHA-256");
                            DownloadInput::Digest(
                                core::mem::replace(&mut verifier, StreamingVerifier::new())
                                    .finalize(),
                            )
                        }
                    },
                    // Nothing on this tier needs undoing: the slot is simply
                    // never activated, so the boot slot stays unchanged.
                    DownloadAction::Abort(e) => {
                        log::error!("OTA: download failed: {} — boot slot unchanged", e);
                        DownloadInput::Aborted
                    }
                    DownloadAction::Activate(permit) => break Ok(permit),
                    // The end of an aborted download.
                    DownloadAction::Done(Err(e)) => break Err(e),
                    other => break Err(unexpected(&other)),
                };
            }
        };

        // 4. Activate the new slot, with nothing between the permit and this call.
        //    API mapping (esp-bootloader-esp-idf 0.6.0, unchanged since 0.5.0):
        //      activate_next_partition() = commit / finalize — writes the OTA data
        //        partition so the bootloader boots the new slot on next reset.
        //      set_current_ota_state(OtaImageState::Valid)   = mark_valid / cancel rollback
        //      set_current_ota_state(OtaImageState::Invalid) = signal bootloader to rollback
        match streamed {
            Ok(permit) => {
                log::info!("OTA: SHA-256 verified — activating next partition");
                let result = activate(&mut updater, permit);
                if result.is_ok() {
                    log::info!("OTA: partition swap complete — reboot to apply");
                }
                finish(&mut machine, DownloadInput::Activated(result), elapsed())
            }
            Err(e) => Err(e),
        }
    }

    /// Performs [`DownloadAction::Connect`]: the GET, the headers, and the
    /// size of the inactive partition, read only after the server answered.
    async fn connect(
        &mut self,
        socket: &mut TcpSocket<'_>,
        url: &str,
        budget: CoreDuration,
    ) -> DownloadInput {
        let parsed = match parse_url(url) {
            Ok(parsed) => parsed,
            Err(e) => return DownloadInput::ConnectFailed(OtaError::from(e)),
        };
        // The wait is `min(per-read timeout, remaining deadline)` so a stalled
        // server cannot hang the OTA path or outlast the deadline.
        let headers = match with_timeout(to_embassy(budget), fetch_get(socket, &parsed)).await {
            Err(_) => {
                log::error!(
                    "OTA: HTTP header phase timed out (per-read {}s or deadline)",
                    self.config.timeout_secs
                );
                return DownloadInput::ConnectFailed(classify_read_error(true));
            }
            Ok(Err(e)) => return DownloadInput::ConnectFailed(e),
            Ok(Ok(headers)) => headers,
        };
        match self.partition_capacity() {
            Ok(capacity) => DownloadInput::Headers {
                content_length: headers.content_length,
                chunked: headers.chunked,
                capacity,
            },
            Err(e) => DownloadInput::ConnectFailed(e),
        }
    }

    /// Size in bytes of the inactive OTA partition, from a short-lived
    /// `OtaUpdater` (it only reads the partition table).
    fn partition_capacity(&mut self) -> Result<u64, OtaError> {
        let mut pt_buf = [0u8; PARTITION_TABLE_MAX_LEN];
        let mut updater = OtaUpdater::new(&mut self.flash, &mut pt_buf).map_err(|e| {
            log::error!("OtaUpdater::new failed: {:?}", e);
            OtaError::PartitionNotFound
        })?;
        let (region, _slot) = updater.next_partition().map_err(|e| {
            log::error!("next_partition failed: {:?}", e);
            OtaError::PartitionNotFound
        })?;
        // `capacity()` is an inherent `FlashRegion` method (the `embedded-storage`
        // trait impls are opt-in since esp-bootloader-esp-idf 0.6).
        Ok(region.capacity() as u64)
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
    /// The slot the bootloader selected for this boot: `ota_0` is `SlotId(0)`,
    /// `ota_1` is `SlotId(1)`, anything else (factory) is `SlotId(0xFF)`, the `FACTORY_SLOT` of the `ota-wire` feature.
    ///
    /// It is the *selected* slot, which is the running slot on every normal
    /// boot; right after [`fetch_and_apply`](Self::fetch_and_apply) it is
    /// already the new slot, before the reboot.
    pub fn selected_slot(&mut self) -> Result<SlotId, OtaError> {
        let mut pt_buf = [0u8; PARTITION_TABLE_MAX_LEN];
        let mut updater = OtaUpdater::new(&mut self.flash, &mut pt_buf).map_err(|e| {
            log::error!("selected_slot: OtaUpdater::new failed: {:?}", e);
            OtaError::PartitionNotFound
        })?;
        let subtype = updater.selected_partition().map_err(|e| {
            log::error!("selected_slot: selected_partition failed: {:?}", e);
            OtaError::PartitionNotFound
        })?;
        Ok(match subtype {
            AppPartitionSubType::Ota0 => SlotId(0),
            AppPartitionSubType::Ota1 => SlotId(1),
            _ => SlotId(0xFF),
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
