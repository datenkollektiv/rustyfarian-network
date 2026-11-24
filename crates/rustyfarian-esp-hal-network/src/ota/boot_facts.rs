//! Hardware facts for boot reconciliation and the early-boot slot note, read from the otadata through the [`SharedFlash`](super::SharedFlash).
//!
//! Hardware only: running slot and state, update slot and state; the runtime adds the running version and the report fact (see `juggler::ota::persist::reconcile_boot`).
//! The running slot is the partition the MMU maps (`PartitionTable::booted_partition`), as `esp_ota_get_running_partition` on the ESP-IDF tier, not the otadata selection, which already names the new slot after an activation.
//! Nothing here changes the boot slot or an image state.
//! Nothing here can be busy: a read borrows the shared flash for one synchronous call, so a [`HardwareReadError::Busy`] never comes from this tier.
//!
//! Error codes of [`HardwareReadError::Read`] on this tier:
//!
//! | Code | Meaning |
//! |:-----|:--------|
//! | `-0x7101` | no [`SharedFlash`](super::SharedFlash) was created yet |
//! | `-0x7102` | the shared flash was lent out (a nested call) |
//! | `-0x7103` | the partition table or the running partition could not be read |
//! | `-0x7104` | the otadata could not be read |
//!
//! v1 two-slot assumption: OTA apps other than `ota_0` and `ota_1` map to `FACTORY_SLOT`, as `slot_for_partition` maps labels on the ESP-IDF tier.

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::String;

use juggler::ota::persist::{slot_label, HardwareFacts, HardwareReadError, UpdateSlot};
use juggler::ota::{SlotId, SlotState};

/// No [`SharedFlash`](super::SharedFlash) was created yet.
const NO_SHARED_FLASH: i32 = -0x7101;
/// The shared flash was lent out (a nested call).
const FLASH_BUSY: i32 = -0x7102;
/// The partition table or the running partition could not be read.
#[cfg_attr(test, allow(dead_code))]
const PARTITION_TABLE: i32 = -0x7103;
/// The otadata could not be read.
#[cfg_attr(test, allow(dead_code))]
const OTADATA: i32 = -0x7104;

fn slot_name(slot: SlotId) -> &'static str {
    slot_label(slot).unwrap_or("unknown")
}

fn state_name(state: SlotState) -> &'static str {
    match state {
        SlotState::Valid => "Valid",
        SlotState::PendingVerify => "PendingVerify",
        SlotState::Invalid => "Invalid",
        SlotState::Unknown => "Unknown",
    }
}

fn unreadable(e: HardwareReadError) -> String {
    match e {
        HardwareReadError::Busy => "unreadable(busy)".to_owned(),
        HardwareReadError::Read(code) => format!("unreadable({code})"),
    }
}

/// Formats the slot evidence line from already-read facts; the same line as on the ESP-IDF tier.
fn format_slot_evidence(
    hardware: &Result<HardwareFacts, HardwareReadError>,
    next_boot: Result<SlotId, HardwareReadError>,
) -> String {
    let next = match next_boot {
        Ok(slot) => slot_name(slot).to_owned(),
        Err(e) => unreadable(e),
    };
    let (running, update) = match hardware {
        Ok(facts) => {
            let update = match facts.update_slot {
                UpdateSlot::Present(slot, state) => {
                    format!("{} ({})", slot_name(slot), state_name(state))
                }
                UpdateSlot::Absent => "absent".to_owned(),
                UpdateSlot::Unreadable(code) => format!("unreadable({code})"),
            };
            (
                format!(
                    "{} ({})",
                    slot_name(facts.running_slot),
                    state_name(facts.running_state)
                ),
                update,
            )
        }
        Err(e) => (unreadable(*e), "unknown".to_owned()),
    };
    format!("[ota] slots: running={running}, next boot={next}, update={update}")
}

#[cfg(all(
    feature = "ota-ddi",
    any(
        feature = "esp32c3",
        feature = "esp32c6",
        feature = "esp32",
        feature = "esp32s3"
    )
))]
mod chip {
    use alloc::string::String;

    use esp_bootloader_esp_idf::partitions::{
        read_partition_table, AppPartitionSubType, PartitionType, PARTITION_TABLE_MAX_LEN,
    };
    use esp_storage::FlashStorage;
    use juggler::ota::persist::{
        note_boot_slot, BusyRetry, FlashKv, HardwareFacts, HardwareReadError, NoteBoot,
        NoteBootError, OtaStore, FACTORY_SLOT,
    };
    use juggler::ota::{OtaError, SlotId};

    use super::super::activation::{self, hardware_facts, OtadataRead};
    use super::super::flash::SharedFlash;
    use super::super::manager::{read_otadata, update_capacity};
    use super::{format_slot_evidence, FLASH_BUSY, NO_SHARED_FLASH, OTADATA, PARTITION_TABLE};

    /// The OTA app number of the partition the MMU maps, `None` for a factory or any other app.
    fn running_app(flash: &mut FlashStorage<'_>) -> Result<Option<u8>, i32> {
        let mut pt_buf = [0u8; PARTITION_TABLE_MAX_LEN];
        let table = read_partition_table(flash, &mut pt_buf).map_err(|e| {
            log::error!(
                "[ota] boot facts: could not read the partition table: {:?}",
                e
            );
            PARTITION_TABLE
        })?;
        let booted = table.booted_partition().map_err(|e| {
            log::error!(
                "[ota] boot facts: could not find the running partition: {:?}",
                e
            );
            PARTITION_TABLE
        })?;
        Ok(booted.and_then(|entry| match entry.partition_type() {
            // `Ota0 + n`; factory, test and anything else are not OTA apps.
            PartitionType::App(sub) => (sub as u8)
                .checked_sub(AppPartitionSubType::Ota0 as u8)
                .filter(|n| *n < 16),
            _ => None,
        }))
    }

    /// Runs `f` on the shared flash, mapping a missing or lent-out handle to its code.
    fn on_flash<R>(f: impl FnOnce(&mut FlashStorage<'static>) -> Result<R, i32>) -> Result<R, i32> {
        let shared = SharedFlash::get().ok_or(NO_SHARED_FLASH)?;
        shared.with(f).unwrap_or(Err(FLASH_BUSY))
    }

    fn read_once() -> Result<OtadataRead, i32> {
        on_flash(|flash| {
            let (sectors, ota_count) =
                read_otadata(flash, "read_hardware_facts").map_err(|_| OTADATA)?;
            Ok(OtadataRead {
                sectors,
                ota_count,
                running_app: running_app(flash)?,
            })
        })
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Reads the hardware facts from the otadata and the partition table, through the [`SharedFlash`].
    ///
    /// The same call as on the ESP-IDF tier; `retry` is accepted for that and unused, because nothing on this tier can be busy.
    /// A running partition that is not `ota_0` or `ota_1` (the factory image) maps to `FACTORY_SLOT`; that is not an error.
    /// The update slot is the OTA app after the running one (`ota_0` from the factory image), as `esp_ota_get_next_update_partition` names it.
    ///
    /// # Errors
    ///
    /// [`HardwareReadError::Read`] with one of the codes in the module documentation, when the shared flash is missing or lent out, or the partition table or the otadata cannot be read.
    pub fn read_hardware_facts(retry: BusyRetry) -> Result<HardwareFacts, HardwareReadError> {
        let _ = retry;
        hardware_facts(read_once())
    }

    /// The slot the bootloader selects on the next boot: the otadata selection.
    fn read_next_boot() -> Result<SlotId, HardwareReadError> {
        on_flash(|flash| read_otadata(flash, "slot_evidence_line").map_err(|_| OTADATA))
            .map(|(sectors, ota_count)| {
                activation::selected_app(&sectors, ota_count).map_or(FACTORY_SLOT, |app| {
                    if app < 2 {
                        SlotId(app)
                    } else {
                        FACTORY_SLOT
                    }
                })
            })
            .map_err(HardwareReadError::Read)
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Builds the one-line slot evidence for hardware tests, read-only; the same line as on the ESP-IDF tier.
    ///
    /// Shape: `[ota] slots: running=<label> (<state>), next boot=<label>, update=<label> (<state>)`.
    /// `update=absent` replaces the update part when the partition table has no update slot; a failed next-boot read prints `next boot=unreadable(<code>)`.
    /// If the running slot itself could not be read, `running=unreadable(<code>)` and `update=unknown` are printed.
    /// `retry` is accepted for parity and unused.
    pub fn slot_evidence_line(
        hardware: &Result<HardwareFacts, HardwareReadError>,
        retry: BusyRetry,
    ) -> String {
        let _ = retry;
        format_slot_evidence(hardware, read_next_boot())
    }

    fn read_running_slot() -> Result<SlotId, HardwareReadError> {
        on_flash(running_app)
            .map(|app| match app {
                Some(app) if app < 2 => SlotId(app),
                _ => FACTORY_SLOT,
            })
            .map_err(HardwareReadError::Read)
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Early-boot step: marks the attempt activated when the running slot is the attempt's slot.
    ///
    /// [`open_records`](super::super::open_records) calls it; the caller only logs a failure, because `reconcile_boot` is the gate.
    /// The partition table is read only when an unactivated attempt exists, once, with no retry.
    ///
    /// # Errors
    ///
    /// [`NoteBootError::Store`] when a record cannot be read or written, [`NoteBootError::Hardware`] when the running slot cannot be read.
    pub fn note_boot_slot_hal(
        store: &mut OtaStore<FlashKv<&'static SharedFlash>>,
    ) -> Result<NoteBoot, NoteBootError> {
        note_boot_slot(store, read_running_slot)
    }

    /// The size in bytes of the OTA update partition (the one after the running app), through the [`SharedFlash`].
    ///
    /// # Errors
    ///
    /// [`OtaError::PartitionNotFound`] when no shared flash exists, it is lent out, or the partition table names no update partition.
    pub(crate) fn update_partition_capacity() -> Result<u64, OtaError> {
        let shared = SharedFlash::get().ok_or(OtaError::PartitionNotFound)?;
        shared
            .with(|flash| update_capacity(flash))
            .unwrap_or(Err(OtaError::PartitionNotFound))
    }
}

#[cfg(all(
    feature = "ota-ddi",
    any(
        feature = "esp32c3",
        feature = "esp32c6",
        feature = "esp32",
        feature = "esp32s3"
    )
))]
pub(crate) use chip::update_partition_capacity;
#[cfg(all(
    feature = "ota-ddi",
    any(
        feature = "esp32c3",
        feature = "esp32c6",
        feature = "esp32",
        feature = "esp32s3"
    )
))]
pub use chip::{note_boot_slot_hal, read_hardware_facts, slot_evidence_line};

#[cfg(test)]
mod tests {
    use super::*;
    use juggler::ota::persist::FACTORY_SLOT;

    #[test]
    fn slot_evidence_line_shapes_match_the_esp_idf_tier() {
        let facts = Ok(HardwareFacts {
            running_slot: SlotId(0),
            running_state: SlotState::PendingVerify,
            update_slot: UpdateSlot::Present(SlotId(1), SlotState::Valid),
        });
        assert_eq!(
            format_slot_evidence(&facts, Ok(SlotId(0))),
            "[ota] slots: running=ota_0 (PendingVerify), next boot=ota_0, update=ota_1 (Valid)"
        );
        let facts = Ok(HardwareFacts {
            running_slot: FACTORY_SLOT,
            running_state: SlotState::Unknown,
            update_slot: UpdateSlot::Absent,
        });
        assert_eq!(
            format_slot_evidence(&facts, Err(HardwareReadError::Read(FLASH_BUSY))),
            "[ota] slots: running=factory (Unknown), next boot=unreadable(-28930), update=absent"
        );
        assert_eq!(
            format_slot_evidence(
                &Err(HardwareReadError::Read(NO_SHARED_FLASH)),
                Ok(FACTORY_SLOT)
            ),
            "[ota] slots: running=unreadable(-28929), next boot=factory, update=unknown"
        );
    }
}
