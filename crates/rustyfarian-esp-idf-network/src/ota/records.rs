//! The record store shared by every thread of an OTA runtime, and the pre-Wi-Fi hook that opens it.
//!
//! Both the MQTT OTA runtime (`ota-wire` and `mqtt`) and the DDI runtime (`ota-ddi`) take their [`Records`] from [`open_records`]; the types are re-exported from `ota::runtime` and `ota::ddi`.

use std::sync::{Arc, Mutex, PoisonError};

use esp_idf_svc::nvs::EspDefaultNvsPartition;
use juggler::ota::{OtaStore, StoreError};

use super::{note_boot_slot_idf, open_store, EspNvsKv};

/// Called to restart the device.
///
/// The app decides how: the worker calls it after a successful download, the health policy only for `DeadlineAction::NoteAndRestart`.
/// It normally never returns; if it does the worker parks (the MQTT runtime answers every later command `busy`, the DDI runtime starts nothing more) and the health policy returns `HealthVerdict::RestartReturned`.
pub type RestartFn = Arc<dyn Fn() + Send + Sync>;

/// The record store, shared by every runtime thread behind one mutex.
///
/// Clone it freely; the app only passes it from [`open_records`] to the runtime's `start`.
#[derive(Clone)]
pub struct Records(Arc<Mutex<OtaStore<EspNvsKv>>>);

impl Records {
    /// Runs `f` with the store locked; a poisoned lock is ignored (the store holds no invariant across a panic: every record write is one commit).
    ///
    /// `f` must contain store calls only.
    pub(crate) fn with<R>(&self, f: impl FnOnce(&mut OtaStore<EspNvsKv>) -> R) -> R {
        let mut guard = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut guard)
    }
}

/// Hook 1 of both OTA runtimes, early in `main` before Wi-Fi: opens the record store and records the boot slot.
///
/// A failure of the boot-slot note is only logged; boot reconciliation in the runtime's `start` is the gate.
///
/// # Errors
///
/// [`StoreError`] when the store cannot be opened.
pub fn open_records(nvs: EspDefaultNvsPartition) -> Result<Records, StoreError> {
    let mut store = open_store(nvs)?;
    match note_boot_slot_idf(&mut store) {
        Ok(note) => log::info!("[ota] boot slot noted: {note:?}"),
        Err(e) => log::warn!("[ota] the boot slot was not noted: {e}"),
    }
    Ok(Records(Arc::new(Mutex::new(store))))
}
