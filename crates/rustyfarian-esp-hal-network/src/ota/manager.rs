//! [`EspHalOtaManager`] — bare-metal OTA manager backed by
//! `esp_bootloader_esp_idf::OtaUpdater` and `esp-storage`.
//!
//! This module is compiled only when both a chip feature (`esp32c3`,
//! `esp32c6`, `esp32`, or `esp32s3`) **and** the `embassy` feature are active.

use core::time::Duration as CoreDuration;

use core::net::Ipv4Addr;
use embassy_net::tcp::TcpSocket;
use embassy_net::{IpAddress, IpEndpoint};
use embassy_time::{with_timeout, Duration, Instant};
use embedded_storage::nor_flash::NorFlash;
use esp_bootloader_esp_idf::ota_updater::OtaUpdater;
use esp_bootloader_esp_idf::partitions::{
    read_partition_table, AppPartitionSubType, DataPartitionSubType, FlashRegion, NorFlashRegion,
    PartitionType, PARTITION_TABLE_MAX_LEN,
};
use esp_storage::FlashStorage;
use juggler::ota::download::{
    DownloadAction, DownloadInput, DownloadMachine, ImageChip, IMAGE_HEADER_LEN,
};
use juggler::ota::{
    classify_read_error, round_up, ActivationPermit, ChunkAligner, Deadline, OtaError, SlotId,
    StreamingVerifier,
};

use super::activation::{self, Entry, OtadataSectors};
use super::cancel::OtaCancel;
#[cfg(feature = "ota-ddi")]
use super::ddi::plan::DdiGate;
#[cfg(feature = "ota-ddi")]
use super::ddi::resolve::{resolve, ResolveError};
#[cfg(feature = "ota-ddi")]
use super::flash::SharedFlash;
use super::http::async_client::fetch_get;
#[cfg(feature = "ota-ddi")]
use super::http::async_client::send_and_read_head;
#[cfg(feature = "ota-ddi")]
use super::http::ddi::{download_headers, request_head, Method, ACCEPT_FIRMWARE};
use super::http::parse_url;
#[cfg(feature = "ota-ddi")]
use juggler::ota::ddi::{CheckedUrl, RetryAfter};

/// Converts an `embassy_time` duration (microsecond-exact) to `core`.
pub(crate) fn to_core(d: Duration) -> CoreDuration {
    CoreDuration::from_micros(d.as_micros())
}

/// Converts a `core` duration to `embassy_time`, saturating at the maximum.
pub(crate) fn to_embassy(d: CoreDuration) -> Duration {
    // Capped well below `u64::MAX` so `Instant::now() + d` in `with_timeout`
    // cannot overflow.
    let micros = u64::try_from(d.as_micros()).unwrap_or(u64::MAX);
    Duration::from_micros(micros.min(u64::MAX / 2))
}

/// Bytes requested per socket read.
const CHUNK_LEN: usize = 512;

/// How long a failed download waits for the RST of the aborted socket to leave before it returns.
const ABORT_FLUSH_WAIT: Duration = Duration::from_millis(500);

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
    /// The firmware's [`SharedFlash`], borrowed for one write at a time; erased once by the
    /// caller, written at `base` plus the image offset (absolute flash offsets).
    #[cfg(feature = "ota-ddi")]
    Shared {
        flash: &'static SharedFlash,
        base: u32,
    },
}

/// The flash an [`EspHalOtaManager`] works on.
enum FlashHandle<'d> {
    /// Owned since [`EspHalOtaManager::new`]; a download borrows it from start to end.
    Owned(FlashStorage<'d>),
    /// The firmware's [`SharedFlash`] ([`EspHalOtaManager::new_shared`]), borrowed for one flash
    /// operation at a time and never across an `.await`.
    #[cfg(feature = "ota-ddi")]
    Shared(&'static SharedFlash),
}

/// What a download fetches.
enum Source<'s> {
    /// The URL of a public `fetch_and_apply*` call: a plain GET to an IPv4 literal.
    Plain(&'s str),
    /// A DDI artifact ([`EspHalOtaManager::fetch_ddi`]).
    #[cfg(feature = "ota-ddi")]
    Ddi(DdiFetch<'s>),
}

/// One DDI artifact fetch, the crate-private seam of the DDI transport's `download`.
///
/// The [`DdiGate`] carries the artifact's size and the chip the image must name; both are required, so this seam cannot fetch without the chip check.
#[cfg(feature = "ota-ddi")]
pub(crate) struct DdiFetch<'a> {
    /// The station stack, for the DNS lookup of the server name.
    pub(crate) stack: embassy_net::Stack<'static>,
    /// The artifact link, after the origin check.
    pub(crate) url: &'a CheckedUrl,
    /// The `Authorization` value; never logged.
    pub(crate) authorization: &'a str,
    /// The size and chip gates.
    pub(crate) gate: DdiGate,
    /// Filled with the `Retry-After` of a non-200 answer, for the retry policy.
    pub(crate) retry_after: &'a mut Option<RetryAfter>,
}

/// Number of OTA app slots in the partition table.
pub(super) fn ota_app_count(
    table: &esp_bootloader_esp_idf::partitions::PartitionTable<'_>,
) -> usize {
    table
        .iter()
        .filter(|p| {
            matches!(
                p.partition_type(),
                PartitionType::App(sub)
                    if sub != AppPartitionSubType::Factory && sub != AppPartitionSubType::Test
            )
        })
        .count()
}

/// Selects OTA slot `next_app` for boot in state `New` with one otadata entry write.
///
/// The only place that changes the boot selection; the permit parameter makes
/// a timed-out download unable to reach it.
/// `pt_buf` is free again once the `OtaUpdater` that borrowed it is dropped.
fn activate(
    flash: &mut FlashStorage<'_>,
    pt_buf: &mut [u8; PARTITION_TABLE_MAX_LEN],
    next_app: u8,
    _permit: ActivationPermit,
) -> Result<(), OtaError> {
    let table = read_partition_table(flash, pt_buf).map_err(|e| {
        log::error!(
            "OTA: activation could not read the partition table: {:?}",
            e
        );
        OtaError::PartitionNotFound
    })?;
    let ota_count = ota_app_count(&table);
    let otadata = table
        .find_partition(PartitionType::Data(DataPartitionSubType::Ota))
        .ok()
        .flatten()
        .ok_or_else(|| {
            log::error!("OTA: activation found no otadata partition");
            OtaError::PartitionNotFound
        })?;
    let mut region = otadata.as_flash_region(flash);
    if region.capacity() < OTADATA_SECTOR_OFFSETS[1] as usize + activation::ENTRY_LEN {
        log::error!(
            "OTA: otadata partition is too small: {} bytes",
            region.capacity()
        );
        return Err(OtaError::PartitionNotFound);
    }
    activation::activate_as_new(&mut Otadata(&mut region), ota_count, next_app).map_err(|f| {
        log::error!(
            "OTA: activating the next slot failed: {:?}; restore of the otadata sector: {:?}; \
             the previous selection was never overwritten",
            f.cause,
            f.restore
        );
        OtaError::FlashWriteFailed
    })
}

/// Sets the state of the otadata entry the bootloader selects.
///
/// Used by `mark_valid` and `rollback` instead of
/// `OtaUpdater::set_current_ota_state`, which writes to the highest sequence
/// even when that entry is `Aborted` or `Invalid` (the situation after a
/// rollback) and so would mark the aborted image `Valid`.
fn set_selected_state(
    flash: &mut FlashStorage<'_>,
    state: u32,
    what: &str,
) -> Result<(), OtaError> {
    let mut pt_buf = [0u8; PARTITION_TABLE_MAX_LEN];
    let table = read_partition_table(flash, &mut pt_buf).map_err(|e| {
        log::error!("{what}: could not read the partition table: {:?}", e);
        OtaError::PartitionNotFound
    })?;
    let otadata = table
        .find_partition(PartitionType::Data(DataPartitionSubType::Ota))
        .ok()
        .flatten()
        .ok_or_else(|| {
            log::error!("{what}: found no otadata partition");
            OtaError::PartitionNotFound
        })?;
    let mut region = otadata.as_flash_region(flash);
    if region.capacity() < OTADATA_SECTOR_OFFSETS[1] as usize + activation::ENTRY_LEN {
        log::error!("{what}: otadata partition is too small");
        return Err(OtaError::PartitionNotFound);
    }
    activation::set_selected_state(&mut Otadata(&mut region), state).map_err(|f| {
        log::error!(
            "{what}: setting the otadata state failed: {:?}; restore of the selected otadata \
             sector: {:?}",
            f.cause,
            f.restore
        );
        OtaError::FlashWriteFailed
    })
}

/// Start of the two otadata select entries, relative to the partition.
const OTADATA_SECTOR_OFFSETS: [u32; 2] = [0x0000, 0x1000];

/// [`OtadataSectors`] over the otadata partition's flash region.
struct Otadata<'r, 'a, 'd>(&'r mut FlashRegion<'a, 'd>);

impl OtadataSectors for Otadata<'_, '_, '_> {
    type Error = esp_bootloader_esp_idf::partitions::Error;

    fn read_entry(&mut self, index: usize) -> Result<Entry, Self::Error> {
        let mut entry = [0u8; activation::ENTRY_LEN];
        self.0.read(OTADATA_SECTOR_OFFSETS[index], &mut entry)?;
        Ok(entry)
    }

    fn write_entry(&mut self, index: usize, entry: &Entry) -> Result<(), Self::Error> {
        self.0.write(OTADATA_SECTOR_OFFSETS[index], entry)
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
        #[cfg(feature = "ota-ddi")]
        Sink::Shared { flash, base } => {
            let mut nor: &SharedFlash = flash;
            aligner
                .push(offset, chunk, |at, data| nor.write(*base + at, data))
                .map_err(|e| {
                    log::error!("Flash write failed: {:?}", e);
                    OtaError::FlashWriteFailed
                })
        }
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
        #[cfg(feature = "ota-ddi")]
        Sink::Shared { flash, base } => {
            let mut nor: &SharedFlash = flash;
            aligner
                .finish(len, |at, data| nor.write(*base + at, data))
                .map_err(|e| {
                    log::error!("OTA: final flash write failed: {:?}", e);
                    OtaError::FlashWriteFailed
                })
        }
        Sink::ReadModifyWrite(_) => Ok(()),
    }
}

/// Feeds the last outcome and returns the machine's final result.
fn finish(
    cancel: &OtaCancel,
    machine: &mut DownloadMachine,
    input: DownloadInput,
    elapsed: CoreDuration,
) -> Result<(), OtaError> {
    match cancel.poll(machine, input, elapsed)? {
        DownloadAction::Done(result) => result,
        other => Err(unexpected(&other)),
    }
}

/// An action the loop's current phase cannot perform: an internal error.
fn unexpected(action: &DownloadAction) -> OtaError {
    log::error!("OTA: unexpected download step {:?}", action);
    OtaError::DownloadFailed { status: 0 }
}

/// One download in progress: its machine, cancel switch, image length, per-read timeout and clock.
///
/// Bundled so the streaming helpers take few arguments (the Xtensa LLVM backend has failed on
/// functions with many, see `docs/project-lore.md` "Toolchain & Build").
struct Run<'r, E: Fn() -> CoreDuration> {
    machine: &'r mut DownloadMachine,
    cancel: &'r OtaCancel,
    len: u32,
    timeout_secs: u64,
    elapsed: &'r E,
}

/// Step 3 of a download, on both flash paths: streams the body into `sink` and returns the activation permit.
///
/// Each chunk feeds both the flash and the verifier. Each socket read waits at most `min(per-read
/// timeout, remaining deadline)`; a timed-out wait is `DownloadTimeout`, a socket read error is
/// `ServerUnreachable`.
async fn stream_body<E: Fn() -> CoreDuration>(
    run: &mut Run<'_, E>,
    sink: &mut Sink<'_, '_, '_>,
    aligner: &mut ChunkAligner,
    socket: &mut TcpSocket<'_>,
) -> Result<ActivationPermit, OtaError> {
    let mut verifier = StreamingVerifier::new();
    // Word-aligned so `write_nor` writes from this buffer directly; a
    // misaligned slice would make it copy through its own 4 KiB sector
    // buffer, on the stack, once per chunk.
    let mut chunk_buf = ChunkBuf([0u8; CHUNK_LEN]);
    let mut written: u32 = 0;
    let mut input = DownloadInput::Prepared;
    loop {
        let action = match run.cancel.poll(run.machine, input, (run.elapsed)()) {
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
                match with_timeout(to_embassy(budget), socket.read(&mut chunk_buf.0[..to_read]))
                    .await
                {
                    Err(_) => {
                        log::error!(
                            "OTA: body chunk read timed out (per-read {}s or deadline)",
                            run.timeout_secs
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
            // The header the machine read before the erase goes first.
            DownloadAction::WriteHeader(bytes) => {
                verifier.update(&bytes);
                match write_chunk(sink, aligner, written, &bytes) {
                    Ok(()) => {
                        written += IMAGE_HEADER_LEN as u32;
                        DownloadInput::Written
                    }
                    Err(e) => DownloadInput::WriteFailed(e),
                }
            }
            DownloadAction::Write { n } => {
                let chunk = &chunk_buf.0[..n];
                verifier.update(chunk);
                match write_chunk(sink, aligner, written, chunk) {
                    Ok(()) => {
                        written += n as u32;
                        DownloadInput::Written
                    }
                    Err(e) => DownloadInput::WriteFailed(e),
                }
            }
            // Write the tail the aligner held back, padded with 0xFF
            // inside the erased span, then hand over the digest.
            DownloadAction::Finalize => match flush_tail(sink, aligner, run.len) {
                Err(e) => DownloadInput::WriteFailed(e),
                Ok(()) => {
                    log::info!("OTA: download complete, verifying SHA-256");
                    DownloadInput::Digest(
                        core::mem::replace(&mut verifier, StreamingVerifier::new()).finalize(),
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
}

/// Where a shared-flash download writes: the target slot, resolved once.
#[cfg(feature = "ota-ddi")]
struct Target {
    /// Absolute flash offset of the target partition.
    offset: u32,
    /// Size of the target partition in bytes.
    capacity: u32,
    /// OTA app number of the target partition.
    next_app: u8,
    /// Whether the partition is encrypted (no plain `NorFlash` view).
    encrypted: bool,
}

/// Resolves the slot `OtaUpdater::next_partition` names, under one short flash borrow.
#[cfg(feature = "ota-ddi")]
fn resolve_target(flash: &mut FlashStorage<'_>) -> Result<Target, OtaError> {
    let mut pt_buf = [0u8; PARTITION_TABLE_MAX_LEN];
    let (capacity, slot, encrypted) = {
        let mut updater = OtaUpdater::new(&mut *flash, &mut pt_buf).map_err(|e| {
            log::error!("OtaUpdater::new failed: {:?}", e);
            OtaError::PartitionNotFound
        })?;
        let (mut region, slot) = updater.next_partition().map_err(|e| {
            log::error!("next_partition failed: {:?}", e);
            OtaError::PartitionNotFound
        })?;
        let encrypted = region.as_nor_flash().is_err();
        (region.capacity() as u32, slot, encrypted)
    };
    let table = read_partition_table(flash, &mut pt_buf).map_err(|e| {
        log::error!("OTA: could not read the partition table: {:?}", e);
        OtaError::PartitionNotFound
    })?;
    let entry = table
        .find_partition(PartitionType::App(slot))
        .ok()
        .flatten()
        .ok_or_else(|| {
            log::error!("OTA: the target partition is missing from the table");
            OtaError::PartitionNotFound
        })?;
    Ok(Target {
        offset: entry.offset(),
        capacity,
        // As in the owned path: `Ota0 + n`, anything else wraps out of range for the plan.
        next_app: (slot as u8).wrapping_sub(AppPartitionSubType::Ota0 as u8),
        encrypted,
    })
}

/// Steps 2 to 4 of a download on the [`SharedFlash`]: resolve the target, erase it sector by
/// sector with a yield after each, stream the body, activate.
///
/// Every flash operation borrows the shared flash on its own; no borrow lives across an `.await`.
#[cfg(feature = "ota-ddi")]
async fn stream_shared<E: Fn() -> CoreDuration>(
    shared: &'static SharedFlash,
    socket: &mut TcpSocket<'_>,
    mut run: Run<'_, E>,
) -> Result<(), OtaError> {
    let target = shared.with(resolve_target).unwrap_or_else(|| {
        log::error!("OTA: the shared flash is busy");
        Err(OtaError::PartitionNotFound)
    });
    let target = match target {
        Ok(target) if target.encrypted => {
            log::error!(
                "OTA: the target partition is encrypted; the shared-flash path writes plain sectors only"
            );
            Err(OtaError::FlashWriteFailed)
        }
        other => other,
    };
    let target = match target {
        Ok(target) => target,
        Err(e) => {
            return finish(
                run.cancel,
                run.machine,
                DownloadInput::PrepareFailed(e),
                (run.elapsed)(),
            )
        }
    };

    // 2a. Erase the image span (whole sectors, inside the partition, as in the owned path) one
    //     sector per borrow, yielding to the executor after each.
    let sector = FlashStorage::SECTOR_SIZE;
    let erased_span = round_up(run.len, sector).min(target.capacity);
    log::info!("OTA: erasing {} bytes of the target partition", erased_span);
    let mut at = 0;
    while at < erased_span {
        let from = target.offset + at;
        let mut nor: &SharedFlash = shared;
        if let Err(e) = nor.erase(from, from + sector) {
            log::error!("OTA: erase of the target partition failed: {:?}", e);
            return finish(
                run.cancel,
                run.machine,
                DownloadInput::PrepareFailed(OtaError::FlashWriteFailed),
                (run.elapsed)(),
            );
        }
        at += sector;
        embassy_futures::yield_now().await;
    }

    // 3. Stream the body.
    let mut sink = Sink::Shared {
        flash: shared,
        base: target.offset,
    };
    let mut aligner = ChunkAligner::new(<&SharedFlash as NorFlash>::WRITE_SIZE);
    let permit = stream_body(&mut run, &mut sink, &mut aligner, socket).await?;

    // 4. Activate the new slot, with nothing between the permit and this call.
    log::info!("OTA: SHA-256 verified — activating next partition");
    let result = shared
        .with(|flash| {
            let mut pt_buf = [0u8; PARTITION_TABLE_MAX_LEN];
            activate(flash, &mut pt_buf, target.next_app, permit)
        })
        .unwrap_or_else(|| {
            log::error!("OTA: the shared flash is busy; the boot slot is unchanged");
            Err(OtaError::FlashWriteFailed)
        });
    if result.is_ok() {
        log::info!("OTA: partition swap complete — reboot to apply");
    }
    finish(
        run.cancel,
        run.machine,
        DownloadInput::Activated(result),
        (run.elapsed)(),
    )
}

/// Size in bytes of the inactive OTA partition, from a short-lived
/// `OtaUpdater` (it only reads the partition table).
pub(crate) fn update_capacity(flash: &mut FlashStorage<'_>) -> Result<u64, OtaError> {
    let mut pt_buf = [0u8; PARTITION_TABLE_MAX_LEN];
    let mut updater = OtaUpdater::new(flash, &mut pt_buf).map_err(|e| {
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

/// Reads both otadata entries and the OTA app slot count; `what` names the caller in logs.
pub(super) fn read_otadata(
    flash: &mut FlashStorage<'_>,
    what: &str,
) -> Result<([activation::Entry; 2], usize), OtaError> {
    let mut pt_buf = [0u8; PARTITION_TABLE_MAX_LEN];
    let table = read_partition_table(&mut *flash, &mut pt_buf).map_err(|e| {
        log::error!("{}: could not read the partition table: {:?}", what, e);
        OtaError::PartitionNotFound
    })?;
    let ota_count = ota_app_count(&table);
    let otadata = table
        .find_partition(PartitionType::Data(DataPartitionSubType::Ota))
        .ok()
        .flatten()
        .ok_or_else(|| {
            log::error!("{}: found no otadata partition", what);
            OtaError::PartitionNotFound
        })?;
    let mut region = otadata.as_flash_region(flash);
    if region.capacity() < OTADATA_SECTOR_OFFSETS[1] as usize + activation::ENTRY_LEN {
        log::error!("{}: otadata partition is too small", what);
        return Err(OtaError::PartitionNotFound);
    }
    let mut otadata = Otadata(&mut region);
    let entries = [otadata.read_entry(0), otadata.read_entry(1)];
    let [Ok(first), Ok(second)] = entries else {
        log::error!("{}: reading the otadata failed", what);
        return Err(OtaError::PartitionNotFound);
    };
    Ok(([first, second], ota_count))
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
    flash: FlashHandle<'d>,
    deadline: Deadline,
    expected_chip: Option<ImageChip>,
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
            flash: FlashHandle::Owned(FlashStorage::new(flash)),
            deadline: Deadline::none(),
            expected_chip: None,
        })
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Create a manager on the firmware's [`SharedFlash`], which the OTA record store and the boot-fact reads use as well.
    ///
    /// The manager borrows the flash for one operation at a time and never across an `.await`, so the store can commit between two chunks of a download.
    /// The download erases the target sectors one by one and yields to the executor after each, so a long erase does not starve Wi-Fi and MQTT.
    /// An encrypted target partition is refused with [`OtaError::FlashWriteFailed`] before anything is erased: this path writes plain sectors only.
    /// Everything else behaves as with [`new`](Self::new).
    ///
    /// ```ignore
    /// let flash = SharedFlash::new(peripherals.FLASH)?;
    /// let mut manager = EspHalOtaManager::new_shared(OtaManagerConfig { timeout_secs: 30 }, flash)?;
    /// ```
    #[cfg(feature = "ota-ddi")]
    pub fn new_shared(
        config: OtaManagerConfig,
        flash: &'static SharedFlash,
    ) -> Result<Self, OtaError> {
        Ok(Self {
            config,
            flash: FlashHandle::Shared(flash),
            deadline: Deadline::none(),
            expected_chip: None,
        })
    }

    /// Runs `f` on the flash: the owned storage, or the shared one for this call only.
    ///
    /// `busy` is the error when the shared flash is already borrowed (a nested call).
    #[cfg_attr(
        not(feature = "ota-ddi"),
        allow(unused_variables, reason = "only the shared flash can be busy")
    )]
    fn with_flash<R>(
        &mut self,
        busy: OtaError,
        f: impl FnOnce(&mut FlashStorage<'_>) -> Result<R, OtaError>,
    ) -> Result<R, OtaError> {
        match &mut self.flash {
            FlashHandle::Owned(flash) => f(flash),
            #[cfg(feature = "ota-ddi")]
            FlashHandle::Shared(shared) => shared.with(|flash| f(flash)).unwrap_or_else(|| {
                log::error!("OTA: the shared flash is busy");
                Err(busy)
            }),
        }
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Limit the whole [`fetch_and_apply`](Self::fetch_and_apply) call to `total`.
    ///
    /// The deadline is **cooperative**: the clock starts on entry to
    /// `fetch_and_apply` and is checked at operation boundaries, namely on
    /// entry, after connect and headers, after partition preparation (the
    /// erase), after every chunk read and every flash write, and immediately
    /// before the otadata activation write.
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
    /// Require the image header to name `chip` before the partition is prepared.
    ///
    /// The first 16 body bytes are read before the erase and checked by `juggler::ota::download::check_image_header`: a different chip id fails with [`OtaError::TargetMismatch`], a missing `0xE9` magic byte or an image shorter than the header with `DownloadFailed { status: 0 }`, both without touching the partition.
    /// On a match those bytes are written first, so the image and its SHA-256 are unchanged.
    /// Not calling this method means no header check.
    /// The ESP-IDF tier has the same method on `OtaSession`.
    ///
    /// ```ignore
    /// let mut manager = EspHalOtaManager::new(
    ///     OtaManagerConfig { timeout_secs: 30 },
    ///     peripherals.FLASH,
    /// )?
    /// .with_expected_chip(ImageChip::Esp32C3);
    /// ```
    #[must_use]
    pub fn with_expected_chip(mut self, chip: ImageChip) -> Self {
        self.expected_chip = Some(chip);
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
    /// Power loss between mid-download and the activation likewise leaves the
    /// boot slot unchanged.
    ///
    /// Activation is a single 32-byte write of the new `ota_seq`, the `New`
    /// state and their CRC into the otadata sector that is *not* selected, so
    /// the boot selection changes only together with the armed rollback; a
    /// failed or interrupted write leaves the previous selection in place.
    /// After a write error or a read-back mismatch the previous bytes of that
    /// sector are written back, the original error is returned as
    /// [`OtaError::FlashWriteFailed`], and whether the restore worked is
    /// logged.
    ///
    /// After this function returns `Ok(())` the caller must reboot the device.
    /// Once the new image has passed its health check, call [`mark_valid`].
    ///
    /// With [`with_deadline`](Self::with_deadline) set, the whole call is bounded by that total.
    ///
    /// To cancel the download from another task, use
    /// [`fetch_and_apply_cancellable`](Self::fetch_and_apply_cancellable).
    ///
    /// [`mark_valid`]: EspHalOtaManager::mark_valid
    pub async fn fetch_and_apply(
        &mut self,
        socket: &mut TcpSocket<'_>,
        url: &str,
        expected_sha256: &[u8; 32],
    ) -> Result<(), OtaError> {
        self.fetch_and_apply_cancellable(socket, url, expected_sha256, &OtaCancel::new())
            .await
    }

    /// Experimental: API may change before 1.0.
    ///
    /// [`fetch_and_apply`](Self::fetch_and_apply) that another task can stop through `cancel`.
    ///
    /// Returns [`OtaError::Cancelled`] exactly when an [`OtaCancel::cancel`] was accepted; the boot slot is then unchanged.
    ///
    /// On any error the socket is aborted (a TCP RST), so a server that is still sending the image (after a refused header, say) stops; the call waits up to 500 ms for the RST to leave.
    pub async fn fetch_and_apply_cancellable(
        &mut self,
        socket: &mut TcpSocket<'_>,
        url: &str,
        expected_sha256: &[u8; 32],
        cancel: &OtaCancel,
    ) -> Result<(), OtaError> {
        self.run(socket, &mut Source::Plain(url), expected_sha256, cancel)
            .await
    }

    /// The DDI artifact fetch behind the DDI transport's `download`: the same streaming download, verification and slot swap as [`fetch_and_apply_cancellable`](Self::fetch_and_apply_cancellable), with the token sent, the server name resolved, and redirects refused ([`OtaError::Redirect`]).
    ///
    /// The declared `Content-Length` must equal the gate's size ([`OtaError::SizeMismatch`]) and the image header must name its chip ([`OtaError::TargetMismatch`]), both before any flash is touched; a non-200 answer hands its `Retry-After` back.
    #[cfg(feature = "ota-ddi")]
    pub(crate) async fn fetch_ddi(
        &mut self,
        socket: &mut TcpSocket<'_>,
        fetch: DdiFetch<'_>,
        expected_sha256: &[u8; 32],
        cancel: &OtaCancel,
    ) -> Result<(), OtaError> {
        self.run(socket, &mut Source::Ddi(fetch), expected_sha256, cancel)
            .await
    }

    /// [`stream`](Self::stream), with the socket aborted on error.
    async fn run(
        &mut self,
        socket: &mut TcpSocket<'_>,
        source: &mut Source<'_>,
        expected_sha256: &[u8; 32],
        cancel: &OtaCancel,
    ) -> Result<(), OtaError> {
        let result = self.stream(socket, source, expected_sha256, cancel).await;
        if result.is_err() {
            socket.abort();
            let _ = with_timeout(ABORT_FLUSH_WAIT, socket.flush()).await;
        }
        result
    }

    /// The body of [`fetch_and_apply_cancellable`](Self::fetch_and_apply_cancellable), without the socket abort on error.
    async fn stream(
        &mut self,
        socket: &mut TcpSocket<'_>,
        source: &mut Source<'_>,
        expected_sha256: &[u8; 32],
        cancel: &OtaCancel,
    ) -> Result<(), OtaError> {
        let start = Instant::now();
        let elapsed = || to_core(start.elapsed());
        let per_op = CoreDuration::from_secs(self.config.timeout_secs);
        let mut machine = match source {
            Source::Plain(_) => {
                let machine = DownloadMachine::new(self.deadline, per_op, *expected_sha256);
                match self.expected_chip {
                    Some(chip) => machine.with_expected_chip(chip),
                    None => machine,
                }
            }
            // The DDI gates replace the manager's own chip setting: the chip check is never off.
            #[cfg(feature = "ota-ddi")]
            Source::Ddi(fetch) => fetch.gate.machine(self.deadline, per_op, *expected_sha256),
        };

        // 1. Connect, read the headers and size the partition; nothing is
        //    prepared yet, so an error returns without an abort.
        let mut input = DownloadInput::Begin;
        let mut header = [0u8; IMAGE_HEADER_LEN];
        let len = loop {
            let action = cancel
                .poll(&mut machine, input, elapsed())
                .inspect_err(|e| log::error!("OTA: download stopped: {}", e))?;
            input = match action {
                DownloadAction::Connect { budget } => match source {
                    Source::Plain(url) => self.connect(socket, url, budget).await,
                    #[cfg(feature = "ota-ddi")]
                    Source::Ddi(fetch) => self.connect_ddi(socket, fetch, budget).await,
                },
                // The image header, read before the partition is erased; the machine keeps it.
                DownloadAction::ReadHeader { max, budget } => {
                    let to_read = (max as usize).min(IMAGE_HEADER_LEN);
                    match with_timeout(to_embassy(budget), socket.read(&mut header[..to_read]))
                        .await
                    {
                        Err(_) => {
                            log::error!(
                                "OTA: image header read timed out (per-read {}s or deadline)",
                                self.config.timeout_secs
                            );
                            DownloadInput::ReadFailed(classify_read_error(true))
                        }
                        Ok(Err(e)) => {
                            log::error!("OTA: image header read failed: {:?}", e);
                            DownloadInput::ReadFailed(classify_read_error(false))
                        }
                        Ok(Ok(n)) => DownloadInput::header_read(&header[..n]),
                    }
                }
                DownloadAction::Prepare { len } => break len,
                DownloadAction::Done(result) => return result,
                other => return Err(unexpected(&other)),
            };
        };
        log::info!("OTA: downloading {} bytes", len);

        let mut run = Run {
            machine: &mut machine,
            cancel,
            len,
            timeout_secs: self.config.timeout_secs,
            elapsed: &elapsed,
        };
        #[cfg_attr(
            not(feature = "ota-ddi"),
            allow(
                clippy::infallible_destructuring_match,
                reason = "the shared-flash arm exists only with `ota-ddi`"
            )
        )]
        let flash = match &mut self.flash {
            FlashHandle::Owned(flash) => flash,
            #[cfg(feature = "ota-ddi")]
            FlashHandle::Shared(shared) => return stream_shared(shared, socket, run).await,
        };

        // 2. Only now is the partition table read for writing.
        //    `OtaUpdater::new` reads it to locate ota_0/ota_1.
        let mut pt_buf = [0u8; PARTITION_TABLE_MAX_LEN];
        let mut updater = match OtaUpdater::new(flash, &mut pt_buf) {
            Ok(updater) => updater,
            Err(e) => {
                log::error!("OtaUpdater::new failed: {:?}", e);
                return finish(
                    run.cancel,
                    run.machine,
                    DownloadInput::PrepareFailed(OtaError::PartitionNotFound),
                    (run.elapsed)(),
                );
            }
        };

        // The region borrows `updater`, which activation needs back, so the
        // stream runs in its own scope and hands out either the permit or the
        // final result.
        let next_app: u8;
        let streamed = {
            let mut region = match updater.next_partition() {
                Ok((region, slot)) => {
                    // `ota_app_number` is crate-private; the discriminants are
                    // `Ota0 + n`, and anything else wraps out of range, which
                    // the activation plan refuses.
                    next_app = (slot as u8).wrapping_sub(AppPartitionSubType::Ota0 as u8);
                    region
                }
                Err(e) => {
                    log::error!("next_partition failed: {:?}", e);
                    return finish(
                        run.cancel,
                        run.machine,
                        DownloadInput::PrepareFailed(OtaError::PartitionNotFound),
                        (run.elapsed)(),
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
                    return finish(
                        run.cancel,
                        run.machine,
                        DownloadInput::PrepareFailed(e),
                        (run.elapsed)(),
                    );
                }
            };

            // 3. Stream the body (see `stream_body`).
            stream_body(&mut run, &mut sink, &mut aligner, socket).await
        };

        // 4. Activate the new slot, with nothing between the permit and this call.
        //    One otadata entry write carries the new sequence number, the `New`
        //    state and the CRC (see `activation`); the updater's own
        //    `activate_next_partition` + `set_current_ota_state(New)` pair is two
        //    writes and is not used.
        match streamed {
            Ok(permit) => {
                log::info!("OTA: SHA-256 verified — activating next partition");
                // The updater only located the slots and has no `Drop`; its
                // borrow of the flash and the table buffer ended with its
                // last use above.
                let result = activate(flash, &mut pt_buf, next_app, permit);
                if result.is_ok() {
                    log::info!("OTA: partition swap complete — reboot to apply");
                }
                finish(
                    run.cancel,
                    run.machine,
                    DownloadInput::Activated(result),
                    (run.elapsed)(),
                )
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
        // Open the TCP connection here: the caller hands over an unconnected
        // socket, and this tier resolves no names, so the host must be an
        // IPv4 literal. The connect shares the budget with the header phase.
        let ip: Ipv4Addr = match parsed.host.parse() {
            Ok(ip) => ip,
            Err(_) => {
                log::error!(
                    "OTA: host {:?} is not an IPv4 literal (no DNS on this tier)",
                    parsed.host
                );
                return DownloadInput::ConnectFailed(OtaError::ServerUnreachable);
            }
        };
        let endpoint = IpEndpoint::new(IpAddress::Ipv4(ip), parsed.port);
        let connect_started = Instant::now();
        match with_timeout(to_embassy(budget), socket.connect(endpoint)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                log::warn!("OTA: tcp connect to {}:{} failed: {:?}", ip, parsed.port, e);
                return DownloadInput::ConnectFailed(OtaError::ServerUnreachable);
            }
            Err(_) => {
                log::error!(
                    "OTA: tcp connect to {}:{} timed out (per-op {}s or deadline)",
                    ip,
                    parsed.port,
                    self.config.timeout_secs
                );
                return DownloadInput::ConnectFailed(classify_read_error(true));
            }
        }
        let budget = budget.saturating_sub(to_core(connect_started.elapsed()));
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

    /// Performs [`DownloadAction::Connect`] for a DDI artifact: the name lookup, the connect, the GET with the `Authorization` header, the head, and the size of the inactive partition.
    ///
    /// All of it shares `budget`; the URL and the token are never logged.
    #[cfg(feature = "ota-ddi")]
    async fn connect_ddi(
        &mut self,
        socket: &mut TcpSocket<'_>,
        fetch: &mut DdiFetch<'_>,
        budget: CoreDuration,
    ) -> DownloadInput {
        let parsed = match parse_url(fetch.url.as_str()) {
            Ok(parsed) => parsed,
            Err(e) => return DownloadInput::ConnectFailed(OtaError::from(e)),
        };
        let started = Instant::now();
        let left = |budget: CoreDuration| budget.saturating_sub(to_core(started.elapsed()));
        let address = match resolve(fetch.stack, parsed.host, budget).await {
            Ok(address) => address,
            Err(ResolveError::Timeout) => {
                return DownloadInput::ConnectFailed(classify_read_error(true))
            }
            Err(ResolveError::Unreachable) => {
                return DownloadInput::ConnectFailed(OtaError::ServerUnreachable)
            }
        };
        let endpoint = IpEndpoint::new(address, parsed.port);
        match with_timeout(to_embassy(left(budget)), socket.connect(endpoint)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                log::warn!("OTA: DDI download connect to {} failed: {:?}", endpoint, e);
                return DownloadInput::ConnectFailed(OtaError::ServerUnreachable);
            }
            Err(_) => {
                log::error!(
                    "OTA: DDI download connect to {} timed out (per-op {}s or deadline)",
                    endpoint,
                    self.config.timeout_secs
                );
                return DownloadInput::ConnectFailed(classify_read_error(true));
            }
        }
        let head = request_head(
            Method::Get,
            &parsed,
            fetch.authorization,
            ACCEPT_FIRMWARE,
            0,
        );
        let response = with_timeout(
            to_embassy(left(budget)),
            send_and_read_head(socket, &[&head]),
        )
        .await;
        drop(head);
        let headers = match response {
            Err(_) => {
                log::error!(
                    "OTA: DDI download header phase timed out (per-read {}s or deadline)",
                    self.config.timeout_secs
                );
                return DownloadInput::ConnectFailed(classify_read_error(true));
            }
            Ok(Err(e)) => {
                log::error!("OTA: DDI download response failed: {:?}", e);
                return DownloadInput::ConnectFailed(OtaError::ServerUnreachable);
            }
            Ok(Ok(head)) => match download_headers(head, fetch.retry_after) {
                Ok(headers) => headers,
                Err(e) => return DownloadInput::ConnectFailed(e),
            },
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
        self.with_flash(OtaError::PartitionNotFound, update_capacity)
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Mark the running slot valid, cancelling the bootloader's automatic rollback.
    ///
    /// Call after the new firmware has passed its health check (e.g. Wi-Fi
    /// associated + 30 s dwell — see ADR 011 §4).
    ///
    /// Sets the state of the otadata entry the bootloader selects (not the
    /// highest sequence, which can be an aborted entry after a rollback).
    pub fn mark_valid(&mut self) -> Result<(), OtaError> {
        self.with_flash(OtaError::FlashWriteFailed, |flash| {
            set_selected_state(flash, activation::STATE_VALID, "mark_valid")
        })
    }

    /// Experimental: API may change before 1.0.
    ///
    /// The slot the bootloader selected for this boot: `ota_0` is `SlotId(0)`,
    /// `ota_1` is `SlotId(1)`, anything else (factory) is `SlotId(0xFF)`, the `FACTORY_SLOT` of the `ota-wire` feature.
    ///
    /// It is the slot of the otadata entry the bootloader selects (an `Aborted`
    /// or `Invalid` entry is skipped, as after a rollback), which is the
    /// running slot on every normal boot; right after [`fetch_and_apply`](Self::fetch_and_apply) it is
    /// already the new slot, before the reboot.
    pub fn selected_slot(&mut self) -> Result<SlotId, OtaError> {
        let (entries, ota_count) = self.read_otadata("selected_slot")?;
        Ok(match activation::selected_app(&entries, ota_count) {
            Some(app) => SlotId(app),
            None => SlotId(0xFF),
        })
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Whether the previous update was rolled back: the bootloader selected a slot and the other
    /// slot's otadata entry is intact but `Aborted` or `Invalid`.
    ///
    /// A firmware that fetches a fixed target version calls this before fetching, so a rolled-back
    /// image is not downloaded again on every boot.
    /// The evidence stays until the next update overwrites that otadata sector.
    /// The ESP-IDF tier has the same method on `OtaSession` (`esp_ota_get_last_invalid_partition`).
    pub fn previous_update_rolled_back(&mut self) -> Result<bool, OtaError> {
        let (entries, _) = self.read_otadata("previous_update_rolled_back")?;
        Ok(activation::rolled_back(&entries))
    }

    /// Reads both otadata entries and the OTA app slot count; `what` names the caller in logs.
    fn read_otadata(&mut self, what: &str) -> Result<([activation::Entry; 2], usize), OtaError> {
        self.with_flash(OtaError::PartitionNotFound, |flash| {
            read_otadata(flash, what)
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
    /// Sets the state of the otadata entry the bootloader selects.
    pub fn rollback(&mut self) -> Result<(), OtaError> {
        self.with_flash(OtaError::FlashWriteFailed, |flash| {
            set_selected_state(flash, activation::STATE_INVALID, "rollback")
        })
    }
}
