//! Stack high-water marks, in bytes, shared by the OTA runtimes.

use std::sync::atomic::{AtomicU32, Ordering};

/// The least free stack each MQTT OTA runtime thread has seen, in bytes; `None` until the thread has sampled once.
///
/// The health policy runs on the app's own thread and is not tracked here.
/// The DDI runtime reports its own threads in `ota::ddi::DdiStackMarks`.
#[cfg(all(feature = "ota-wire", feature = "mqtt"))]
#[deprecated(
    since = "0.8.0",
    note = "the MQTT OTA wire is deprecated and will be removed in a later release; use the DDI client (`ota-ddi`, ESP-IDF tier; esp-hal has no DDI client yet, ADR 022 gap)"
)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StackMarks {
    /// Least free bytes of the `ota-worker` thread.
    pub worker_min_free_bytes: Option<u32>,
    /// Least free bytes of the `ota-reporter` thread.
    pub reporter_min_free_bytes: Option<u32>,
}

/// Free stack bytes at the all-time low of the calling task.
///
/// On ESP-IDF the FreeRTOS high-water mark is already in bytes, not words.
pub(crate) fn high_water_bytes() -> u32 {
    // SAFETY: `uxTaskGetStackHighWaterMark` only reads the stack bookkeeping of a task; a null handle selects the calling task, which is running and therefore alive for the whole call.
    let free: u32 = unsafe { esp_idf_svc::sys::uxTaskGetStackHighWaterMark(core::ptr::null_mut()) };
    free
}

/// Records the calling thread's mark in `cell` (keeping the lowest) and logs it.
pub(crate) fn sample(cell: &AtomicU32, thread: &str) {
    let free = high_water_bytes();
    let lowest = cell.fetch_min(free, Ordering::Relaxed).min(free);
    log::debug!("[ota] {thread} stack: {free} bytes free now, {lowest} at the lowest");
}

pub(crate) fn read(cell: &AtomicU32) -> Option<u32> {
    match cell.load(Ordering::Relaxed) {
        u32::MAX => None,
        free => Some(free),
    }
}
