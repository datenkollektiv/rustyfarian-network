//! State shared between the MQTT callback, the worker, the reporter and the health policy.
//!
//! Everything here is a flag or a counter; the store lives behind [`Records`](super::Records), the library-owned flags are [`OtaFlags`](super::OtaFlags).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

/// Flags and counters of the command path.
#[derive(Debug)]
pub(super) struct Shared {
    busy: AtomicBool,
    closed: AtomicBool,
    completion_retry: AtomicBool,
    rejects_total: AtomicU32,
    pub(super) worker_free: AtomicU32,
    pub(super) reporter_free: AtomicU32,
}

impl Shared {
    pub(super) fn new() -> Self {
        Self {
            busy: AtomicBool::new(false),
            closed: AtomicBool::new(false),
            completion_retry: AtomicBool::new(false),
            rejects_total: AtomicU32::new(0),
            worker_free: AtomicU32::new(u32::MAX),
            reporter_free: AtomicU32::new(u32::MAX),
        }
    }

    /// Claims the single busy slot (queued or executing); `false` when it is taken.
    pub(super) fn claim_busy(&self) -> bool {
        self.busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(super) fn release_busy(&self) {
        self.busy.store(false, Ordering::Release);
    }

    pub(super) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    pub(super) fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    /// The health policy asks the reporter to keep retrying `complete_attempt` in the background.
    pub(super) fn request_completion_retry(&self) {
        self.completion_retry.store(true, Ordering::Release);
    }

    pub(super) fn completion_retry_requested(&self) -> bool {
        self.completion_retry.load(Ordering::Acquire)
    }

    /// The retry ended (completed, or the attempt is gone).
    pub(super) fn clear_completion_retry(&self) {
        self.completion_retry.store(false, Ordering::Release);
    }

    /// Counts one rejection that could not be queued for the reporter; wraps at `u32::MAX`.
    pub(super) fn count_dropped_reject(&self) {
        self.rejects_total.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn rejects_total(&self) -> u32 {
        self.rejects_total.load(Ordering::Relaxed)
    }
}
