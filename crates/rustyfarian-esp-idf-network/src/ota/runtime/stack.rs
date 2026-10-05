//! Stack high-water marks, in bytes.

use std::sync::atomic::{AtomicU32, Ordering};

/// The least free stack each runtime thread has seen, in bytes; `None` until the thread has sampled once.
///
/// The health policy runs on the app's own thread and is not tracked here.
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
pub(super) fn high_water_bytes() -> u32 {
    // SAFETY: `uxTaskGetStackHighWaterMark` only reads the stack bookkeeping of a task; a null handle selects the calling task, which is running and therefore alive for the whole call.
    let free: u32 = unsafe { esp_idf_svc::sys::uxTaskGetStackHighWaterMark(core::ptr::null_mut()) };
    free
}

/// Records the calling thread's mark in `cell` (keeping the lowest) and logs it.
pub(super) fn sample(cell: &AtomicU32, thread: &str) {
    let free = high_water_bytes();
    let lowest = cell.fetch_min(free, Ordering::Relaxed).min(free);
    log::debug!("[ota] {thread} stack: {free} bytes free now, {lowest} at the lowest");
}

pub(super) fn read(cell: &AtomicU32) -> Option<u32> {
    match cell.load(Ordering::Relaxed) {
        u32::MAX => None,
        free => Some(free),
    }
}
