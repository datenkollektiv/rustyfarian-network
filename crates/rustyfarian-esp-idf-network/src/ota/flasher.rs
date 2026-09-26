//! Firmware flasher wrapping `EspOta` / `EspOtaUpdate`.
//!
//! `FirmwareFlasher::begin()` returns an `OtaWriter` that implements `std::io::Write`
//! so it can be composed with any `Write`-based download loop.

use esp_idf_svc::ota::{EspOta, EspOtaUpdate};
use esp_idf_svc::sys::esp_ota_get_next_update_partition;

use juggler::ota::OtaError;

/// Manages the OTA flash partition handle.
pub struct FirmwareFlasher {
    ota: EspOta,
}

impl FirmwareFlasher {
    /// Create a new flasher.
    ///
    /// Acquires the singleton `EspOta` handle.
    pub fn new() -> Result<Self, OtaError> {
        let ota = EspOta::new().map_err(|e| {
            log::error!("Failed to initialise OTA partition handle: {:?}", e);
            OtaError::PartitionNotFound
        })?;

        log::info!("OTA flasher initialised");
        Ok(Self { ota })
    }

    /// Size in bytes of the partition the next update will be written to.
    ///
    /// This is the same partition [`begin`](Self::begin) opens, so an image no
    /// larger than this fits.
    pub fn update_partition_capacity(&self) -> Result<usize, OtaError> {
        // SAFETY: `esp_ota_get_next_update_partition` only reads the partition
        // table; a null `start_from` selects the slot after the running one. A
        // non-null result points at a partition-table entry that lives for the
        // whole program, so borrowing it via `as_ref` is sound.
        let partition = unsafe { esp_ota_get_next_update_partition(core::ptr::null()).as_ref() };
        match partition {
            Some(partition) => Ok(partition.size as usize),
            None => {
                log::error!("No OTA update partition found");
                Err(OtaError::PartitionNotFound)
            }
        }
    }

    /// Begin an OTA write session for an image of exactly `image_len` bytes.
    ///
    /// Only the sectors covering `image_len` are erased (instead of the whole
    /// partition). The caller must have checked `image_len` against
    /// [`update_partition_capacity`](Self::update_partition_capacity) and must
    /// reject a body shorter than `image_len` before completing the update.
    ///
    /// Returns an [`OtaWriter`] that must be either completed with
    /// [`OtaWriter::complete`] or aborted with [`OtaWriter::abort`].
    pub fn begin(&mut self, image_len: usize) -> Result<OtaWriter<'_>, OtaError> {
        let update = self
            .ota
            .initiate_update_with_known_size(image_len)
            .map_err(|e| {
                log::error!("Failed to initiate OTA update: {:?}", e);
                OtaError::FlashWriteFailed
            })?;

        log::info!("OTA write session started");
        Ok(OtaWriter {
            update,
            bytes_written: 0,
        })
    }
}

/// Active OTA write session.
///
/// Write firmware data in chunks using the `std::io::Write` impl.
/// Call [`complete`](OtaWriter::complete) when all data has been written to
/// set the new partition as the boot partition, or call
/// [`abort`](OtaWriter::abort) to cancel without changing the boot slot.
pub struct OtaWriter<'a> {
    update: EspOtaUpdate<'a>,
    bytes_written: usize,
}

impl<'a> OtaWriter<'a> {
    /// Write a chunk of firmware data to flash.
    pub fn write_chunk(&mut self, data: &[u8]) -> Result<(), OtaError> {
        self.update.write(data).map_err(|e| {
            log::error!(
                "Flash write failed at offset {}: {:?}",
                self.bytes_written,
                e
            );
            OtaError::FlashWriteFailed
        })?;

        self.bytes_written += data.len();
        Ok(())
    }

    /// Complete the OTA update.
    ///
    /// Sets the new partition as the boot partition.
    /// The device will boot from the new firmware on the next reboot.
    pub fn complete(self) -> Result<(), OtaError> {
        self.update.complete().map_err(|e| {
            log::error!("Failed to complete OTA update: {:?}", e);
            OtaError::FlashWriteFailed
        })?;

        log::info!("OTA update completed: {} bytes written", self.bytes_written);
        Ok(())
    }

    /// Abort the OTA update.
    ///
    /// The previous firmware remains active; the inactive slot is left in an
    /// aborted state until the next `begin()` call erases the sectors the next
    /// image needs and overwrites them.
    pub fn abort(self) -> Result<(), OtaError> {
        self.update.abort().map_err(|e| {
            log::error!("Failed to abort OTA update: {:?}", e);
            OtaError::FlashWriteFailed
        })?;

        log::info!("OTA update aborted after {} bytes", self.bytes_written);
        Ok(())
    }

    /// Return the number of bytes written so far.
    #[allow(dead_code)]
    pub fn bytes_written(&self) -> usize {
        self.bytes_written
    }
}

impl std::io::Write for OtaWriter<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.write_chunk(buf)
            .map_err(|e| std::io::Error::other(format!("{e}")))?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
