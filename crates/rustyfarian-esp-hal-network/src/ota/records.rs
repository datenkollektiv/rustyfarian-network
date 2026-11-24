//! The record store of the DDI runtime, and the pre-Wi-Fi hook that opens it ([`open_records`]).
//!
//! The store keeps its records in the `nvs` data partition through `juggler::ota::persist::FlashKv` (an append-only snapshot log, decision D1 of the DDI round on this tier).
//!
//! Named gaps of that region:
//! - an ESP-IDF image flashed later onto the same board treats the region as foreign NVS (and may erase it), and this store in turn treats NVS pages left by an earlier ESP-IDF image as foreign data: it opens empty and erases them as its log reaches them;
//! - whether `esp-radio` persists PHY calibration data in the `nvs` partition is unverified; nothing in this workspace asks it to.

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embedded_storage::nor_flash::NorFlash;
use esp_bootloader_esp_idf::partitions::{
    read_partition_table, DataPartitionSubType, PartitionType, PARTITION_TABLE_MAX_LEN,
};
use esp_storage::FlashStorage;
use juggler::ota::persist::{FlashKv, KvError, OtaStore, StoreError};

use super::boot_facts::note_boot_slot_hal;
use super::flash::SharedFlash;

/// The record store of this tier: [`FlashKv`] over the [`SharedFlash`].
pub(crate) type Store = OtaStore<FlashKv<&'static SharedFlash>>;

/// The cell of the store: a lock that masks interrupts only while it is taken or released, never while the store is borrowed.
///
/// A store call may commit several times, and each commit may erase a sector; a `critical_section::Mutex` here would mask interrupts (Wi-Fi, the timer) for all of them together.
/// Each flash operation still masks interrupts on its own, inside [`SharedFlash`].
type StoreCell = Mutex<CriticalSectionRawMutex, Option<Store>>;

/// The only store; empty until [`open_records`] succeeds.
static RECORDS: StoreCell = Mutex::new(None);

/// Experimental: API may change before 1.0.
///
/// The record store, shared by every task of the DDI runtime; a `Copy` handle to the one store of this boot.
///
/// The app only passes it from [`open_records`] to the runtime's `start`.
/// The ESP-IDF tier has the same type (there behind an `Arc<Mutex<..>>`; this tier has no `Arc`, since the ESP32-C3 has no atomic compare-and-swap).
#[derive(Clone, Copy)]
pub struct Records(&'static StoreCell);

impl core::fmt::Debug for Records {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Records").finish_non_exhaustive()
    }
}

impl Records {
    /// Runs `f` with the store borrowed; `None` when it is not open or lent out already (a nested call).
    ///
    /// Interrupts stay enabled while `f` runs; only each flash operation masks them, one at a time.
    /// `f` must contain store calls only: nothing in `f` may wait.
    pub(crate) fn with<R>(&self, f: impl FnOnce(&mut Store) -> R) -> Option<R> {
        let mut slot = self.0.try_lock().ok()?;
        slot.as_mut().map(f)
    }
}

/// Offset and size of the `nvs` data partition.
fn nvs_region(flash: &mut FlashStorage<'_>) -> Result<(u32, u32), KvError> {
    let mut pt_buf = [0u8; PARTITION_TABLE_MAX_LEN];
    let table = read_partition_table(flash, &mut pt_buf).map_err(|e| {
        log::error!("[ota] records: could not read the partition table: {:?}", e);
        KvError::new(KvError::CODE_FLASH)
    })?;
    let nvs = table
        .find_partition(PartitionType::Data(DataPartitionSubType::Nvs))
        .ok()
        .flatten()
        .ok_or_else(|| {
            log::error!("[ota] records: the partition table has no `nvs` data partition");
            KvError::new(KvError::CODE_GEOMETRY)
        })?;
    Ok((nvs.offset(), nvs.len()))
}

/// The install epoch's entropy, and the DDI runtime's back-off jitter: the hardware RNG.
pub(crate) fn random_u32() -> u32 {
    esp_hal::rng::Rng::new().random()
}

/// Experimental: API may change before 1.0.
///
/// Hook 1 of the DDI runtime, early in `main` before Wi-Fi: opens the record store and records the boot slot.
///
/// The store lives in the `nvs` data partition of the partition table (see the module documentation for the named gaps of that region); it needs at least two 4 KiB sectors.
/// A failure of the boot-slot note is only logged; boot reconciliation in the runtime's `start` is the gate.
/// The ESP-IDF tier has the same call on its NVS partition.
///
/// # Errors
///
/// [`StoreError::Kv`] with [`KvError::CODE_ALREADY_OPEN`] when the store was opened before in this boot (nothing panics), [`KvError::CODE_GEOMETRY`] when there is no usable `nvs` partition, or [`KvError::CODE_FLASH`] when the flash cannot be read.
pub fn open_records(flash: &'static SharedFlash) -> Result<Records, StoreError> {
    let already = || StoreError::Kv(KvError::new(KvError::CODE_ALREADY_OPEN));
    if RECORDS.try_lock().map_or(true, |slot| slot.is_some()) {
        return Err(already());
    }
    let (offset, size) = flash
        .with(|storage| nvs_region(storage))
        .unwrap_or(Err(KvError::new(KvError::CODE_FLASH)))
        .map_err(StoreError::Kv)?;
    log::info!(
        "[ota] records: {} sectors of the nvs partition at {:#x}",
        size / <&SharedFlash as NorFlash>::ERASE_SIZE as u32,
        offset
    );
    let kv = FlashKv::open(flash, offset, size).map_err(StoreError::Kv)?;
    {
        let Ok(mut slot) = RECORDS.try_lock() else {
            return Err(already());
        };
        if slot.is_some() {
            return Err(already());
        }
        *slot = Some(OtaStore::open(kv, random_u32));
    }
    let records = Records(&RECORDS);
    match records.with(note_boot_slot_hal) {
        Some(Ok(note)) => log::info!("[ota] boot slot noted: {note:?}"),
        Some(Err(e)) => log::warn!("[ota] the boot slot was not noted: {e}"),
        None => log::warn!("[ota] the boot slot was not noted: the store is busy"),
    }
    Ok(records)
}
