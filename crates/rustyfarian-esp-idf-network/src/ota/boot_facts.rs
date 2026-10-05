//! Hardware facts for boot reconciliation and the early-boot slot note.
//!
//! Hardware only: running slot and state, update slot and state.
//! The runtime adds the running version and the report fact (see `juggler::ota::persist::reconcile_boot`).
//! Nothing here holds `EspOta` across calls, and nothing here changes the boot slot or an image state.
//!
//! Busy detection: `EspOta` is a process singleton and `EspOta::new()` fails with `ESP_ERR_INVALID_STATE` exactly when another handle is alive (esp-idf-svc 0.53.0, `src/ota.rs` lines 455 to 461: `if *taken { return Err(EspError::from_infallible::<ESP_ERR_INVALID_STATE>()) }`).
//! Only the error of `EspOta::new()` is read as "busy"; the same code from a later read of an open handle is a plain failure.
//! The constants this relies on are pinned by compile-time assertions below, but an esp-idf-svc bump that moves the busy signal to another code or API still needs this module re-checked.
//!
//! v1 two-slot assumption: partition labels are mapped with `slot_for_partition`, which knows only `ota_0` and `ota_1`.
//! Any other label, including `ota_2` and up, maps to `FACTORY_SLOT`, so a partition table with more than two OTA slots is misreported (a running `ota_2` looks like the factory image).

use std::time::Duration;

use embedded_svc::ota::SlotState as SvcSlotState;
use esp_idf_svc::ota::EspOta;
use esp_idf_svc::sys::{EspError, ESP_ERR_INVALID_STATE, ESP_ERR_NOT_FOUND};
use juggler::ota::persist::{
    note_boot_slot, slot_for_partition, slot_label, BusyTally, HardwareFacts, HardwareReadError,
    NoteBoot, NoteBootError, OtaStore, UpdateSlot,
};
use juggler::ota::{SlotId, SlotState};

use super::store::EspNvsKv;

/// How often and how patiently to retry when the `EspOta` singleton is in use.
///
/// `EspOta` is a process singleton, so a download in progress makes it busy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BusyRetry {
    /// Number of tries; values below 1 count as 1.
    pub tries: u8,
    /// Pause between two tries.
    pub delay: Duration,
}

impl Default for BusyRetry {
    /// Five tries, 200 ms apart (about 800 ms in the worst case).
    fn default() -> Self {
        Self {
            tries: 5,
            delay: Duration::from_millis(200),
        }
    }
}

fn map_state(state: SvcSlotState) -> SlotState {
    match state {
        SvcSlotState::Valid => SlotState::Valid,
        SvcSlotState::Unverified => SlotState::PendingVerify,
        SvcSlotState::Invalid => SlotState::Invalid,
        SvcSlotState::Factory | SvcSlotState::Unknown => SlotState::Unknown,
    }
}

/// `esp_err_t` of "the `EspOta` singleton is in use": what esp-idf-svc 0.53.0 `EspOta::new()` returns while another handle exists (`src/ota.rs` line 459).
const BUSY_CODE: i32 = ESP_ERR_INVALID_STATE;

/// `esp_err_t` of "no update partition": what esp-idf-svc 0.53.0 `EspOta::get_update_slot()` returns when `esp_ota_get_next_update_partition` finds none (`src/ota.rs` line 503).
const NO_UPDATE_PARTITION_CODE: i32 = ESP_ERR_NOT_FOUND;

// Pinned to the ESP-IDF values (`esp_err.h`): a changed constant fails `just check-ota-idf` and `just clippy-ota-tests`.
const _: () = assert!(BUSY_CODE == 0x103);
const _: () = assert!(NO_UPDATE_PARTITION_CODE == 0x105);

/// Whether the error code of `EspOta::new()` means "another handle is alive".
fn is_busy_code(code: i32) -> bool {
    code == BUSY_CODE
}

/// The update slot of a failed `get_update_slot()`: "no partition" is [`UpdateSlot::Absent`], anything else [`UpdateSlot::Unreadable`].
fn update_slot_of_failure(code: i32) -> UpdateSlot {
    if code == NO_UPDATE_PARTITION_CODE {
        UpdateSlot::Absent
    } else {
        UpdateSlot::Unreadable(code)
    }
}

/// A failed read: the backend code, and whether it was the "singleton in use" failure of `EspOta::new()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Failure {
    code: i32,
    busy: bool,
}

impl Failure {
    /// The failure of `EspOta::new()`: busy when the code says so.
    fn of_new(e: &EspError) -> Self {
        Self {
            code: e.code(),
            busy: is_busy_code(e.code()),
        }
    }

    /// The failure of a read on an open handle: never busy.
    fn of_read(e: &EspError) -> Self {
        Self {
            code: e.code(),
            busy: false,
        }
    }
}

/// One short-lived `EspOta` handle: both slots are read on it and it is dropped before returning.
fn read_once() -> Result<HardwareFacts, Failure> {
    let ota = EspOta::new().map_err(|e| Failure::of_new(&e))?;
    let running = ota.get_running_slot().map_err(|e| Failure::of_read(&e))?;
    let update_slot = match ota.get_update_slot() {
        Ok(slot) => UpdateSlot::Present(slot_for_partition(&slot.label), map_state(slot.state)),
        Err(e) => update_slot_of_failure(e.code()),
    };
    Ok(HardwareFacts {
        running_slot: slot_for_partition(&running.label),
        running_state: map_state(running.state),
        update_slot,
    })
}

/// Reads the hardware facts, retrying a busy `EspOta` up to `retry.tries` times.
///
/// A running partition that is not `ota_0` or `ota_1` (the factory image) maps to `FACTORY_SLOT`; that is not an error.
/// v1 assumes exactly two OTA slots: with a partition table that has more (`ota_2` and up) those partitions are misreported as `FACTORY_SLOT`.
/// A failing update-slot read is soft: [`UpdateSlot::Absent`] when the partition table has no update partition (`ESP_ERR_NOT_FOUND`), [`UpdateSlot::Unreadable`] with the code otherwise; the decision core treats both as unknown.
/// "Busy" is `EspOta::new()` failing with `ESP_ERR_INVALID_STATE` (esp-idf-svc 0.53.0 `src/ota.rs` line 459), see the module docs.
/// Call it from a worker or the main thread: it sleeps between tries and may see `Busy` while a download holds the singleton.
///
/// # Errors
///
/// [`HardwareReadError::Busy`] when every failure was `EspOta::new()` reporting the singleton in use, otherwise [`HardwareReadError::Read`] with the last code.
pub fn read_hardware_facts(retry: BusyRetry) -> Result<HardwareFacts, HardwareReadError> {
    let mut tally = BusyTally::new();
    for attempt in 0..retry.tries.max(1) {
        if attempt > 0 {
            std::thread::sleep(retry.delay);
        }
        match read_once() {
            Ok(facts) => return Ok(facts),
            Err(failure) => tally.record(failure.code, failure.busy),
        }
    }
    Err(tally.into_error())
}

/// The slot the bootloader will select on the next boot, read on its own short-lived `EspOta` handle (dropped before returning).
fn read_next_boot_once() -> Result<SlotId, Failure> {
    let ota = EspOta::new().map_err(|e| Failure::of_new(&e))?;
    let boot = ota.get_boot_slot().map_err(|e| Failure::of_read(&e))?;
    Ok(slot_for_partition(&boot.label))
}

/// Reads the next-boot slot with the same busy handling as [`read_hardware_facts`].
fn read_next_boot(retry: BusyRetry) -> Result<SlotId, HardwareReadError> {
    let mut tally = BusyTally::new();
    for attempt in 0..retry.tries.max(1) {
        if attempt > 0 {
            std::thread::sleep(retry.delay);
        }
        match read_next_boot_once() {
            Ok(slot) => return Ok(slot),
            Err(failure) => tally.record(failure.code, failure.busy),
        }
    }
    Err(tally.into_error())
}

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

/// Formats the slot evidence line from already-read facts.
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

/// Builds the one-line slot evidence for hardware tests, read-only.
///
/// Shape: `[ota] slots: running=<label> (<state>), next boot=<label>, update=<label> (<state>)`.
/// `update=absent` or `update=unreadable(<code>)` replace the update part when the slot has no state; a failed next-boot read prints `next boot=unreadable(<code>)`.
/// If the running slot itself could not be read, `running=unreadable(<code>)` and `update=unknown` are printed.
/// The next-boot slot is read on its own short-lived `EspOta` handle; nothing is held across calls and nothing is changed.
pub fn slot_evidence_line(
    hardware: &Result<HardwareFacts, HardwareReadError>,
    retry: BusyRetry,
) -> String {
    format_slot_evidence(hardware, read_next_boot(retry))
}

fn read_running_slot() -> Result<SlotId, HardwareReadError> {
    let ota = EspOta::new().map_err(|e| {
        if is_busy_code(e.code()) {
            HardwareReadError::Busy
        } else {
            HardwareReadError::Read(e.code())
        }
    })?;
    let running = ota
        .get_running_slot()
        .map_err(|e| HardwareReadError::Read(e.code()))?;
    Ok(slot_for_partition(&running.label))
}

/// Early-boot step: marks the attempt activated when the running slot is the attempt's slot.
///
/// Takes the store the runtime later wraps in its `Mutex` (there is no second store) and must run before the runtime starts its threads.
/// `EspOta` is touched only when an unactivated attempt exists, once, with no retry.
/// The caller only logs a failure: `reconcile_boot` is the gate.
///
/// # Errors
///
/// [`NoteBootError::Store`] when a record cannot be read or written, [`NoteBootError::Hardware`] when the running slot cannot be read.
pub fn note_boot_slot_idf(store: &mut OtaStore<EspNvsKv>) -> Result<NoteBoot, NoteBootError> {
    note_boot_slot(store, read_running_slot)
}

#[cfg(test)]
mod tests {
    use super::*;
    use juggler::ota::persist::FACTORY_SLOT;

    #[test]
    fn default_retry_is_the_clock_bound() {
        let retry = BusyRetry::default();
        assert_eq!(retry.tries, 5);
        assert_eq!(retry.delay, Duration::from_millis(200));
    }

    #[test]
    fn only_the_singleton_in_use_code_is_busy() {
        assert!(is_busy_code(ESP_ERR_INVALID_STATE));
        assert!(is_busy_code(0x103));
        assert!(!is_busy_code(ESP_ERR_NOT_FOUND));
        assert!(!is_busy_code(0));
    }

    #[test]
    fn a_missing_update_partition_is_absent_and_any_other_failure_unreadable() {
        assert_eq!(
            update_slot_of_failure(ESP_ERR_NOT_FOUND),
            UpdateSlot::Absent
        );
        assert_eq!(
            update_slot_of_failure(ESP_ERR_INVALID_STATE),
            UpdateSlot::Unreadable(ESP_ERR_INVALID_STATE)
        );
    }

    #[test]
    fn only_a_failed_new_can_be_busy() {
        let busy = EspError::from_infallible::<ESP_ERR_INVALID_STATE>();
        assert!(Failure::of_new(&busy).busy);
        assert!(!Failure::of_read(&busy).busy);
    }

    #[test]
    fn slot_evidence_line_shapes() {
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
            running_slot: SlotId(1),
            running_state: SlotState::Valid,
            update_slot: UpdateSlot::Unreadable(5),
        });
        assert_eq!(
            format_slot_evidence(&facts, Err(HardwareReadError::Read(7))),
            "[ota] slots: running=ota_1 (Valid), next boot=unreadable(7), update=unreadable(5)"
        );
        let facts = Ok(HardwareFacts {
            running_slot: FACTORY_SLOT,
            running_state: SlotState::Unknown,
            update_slot: UpdateSlot::Absent,
        });
        assert_eq!(
            format_slot_evidence(&facts, Ok(FACTORY_SLOT)),
            "[ota] slots: running=factory (Unknown), next boot=factory, update=absent"
        );
    }

    #[test]
    fn every_svc_slot_state_maps() {
        assert_eq!(map_state(SvcSlotState::Valid), SlotState::Valid);
        assert_eq!(
            map_state(SvcSlotState::Unverified),
            SlotState::PendingVerify
        );
        assert_eq!(map_state(SvcSlotState::Invalid), SlotState::Invalid);
        assert_eq!(map_state(SvcSlotState::Factory), SlotState::Unknown);
        assert_eq!(map_state(SvcSlotState::Unknown), SlotState::Unknown);
    }
}
