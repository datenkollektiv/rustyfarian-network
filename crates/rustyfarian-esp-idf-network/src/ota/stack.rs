//! Stack high-water marks, in bytes, of the DDI runtime's threads.

use std::sync::atomic::{AtomicU32, Ordering};

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
