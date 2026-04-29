//! Firmware flasher wrapping `EspOta` / `EspOtaUpdate`.
//!
//! `FirmwareFlasher::begin()` returns an `OtaWriter` that implements `std::io::Write`
//! so it can be composed with any `Write`-based download loop.

use esp_idf_svc::ota::{EspOta, EspOtaUpdate};

use ota_pure::OtaError;

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

    /// Begin an OTA write session.
    ///
    /// Returns an [`OtaWriter`] that must be either completed with
    /// [`OtaWriter::complete`] or aborted with [`OtaWriter::abort`].
    pub fn begin(&mut self) -> Result<OtaWriter<'_>, OtaError> {
        let update = self.ota.initiate_update().map_err(|e| {
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
    /// aborted state until the next `begin()` call erases it.
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
