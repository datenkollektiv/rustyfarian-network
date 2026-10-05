//! State shared between the MQTT callback, the worker, the reporter and the health policy.
//!
//! Everything here is a flag or a counter; the store lives behind [`Records`](super::Records).

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;

/// Flags of the runtime that the app may read but never write.
///
/// The library owns them because it owns both sides of each coupling: boot reconciliation, the worker and the health policy.
#[derive(Debug, Clone)]
pub struct OtaFlags {
    cells: Arc<FlagCells>,
}

#[derive(Debug)]
struct FlagCells {
    admission_open: AtomicBool,
    refuse_mark_valid: AtomicBool,
    health_settled: AtomicBool,
    records_suspect: AtomicBool,
    roll_back_now_at_boot: AtomicBool,
}

impl OtaFlags {
    pub(super) fn new() -> Self {
        Self {
            cells: Arc::new(FlagCells {
                admission_open: AtomicBool::new(false),
                refuse_mark_valid: AtomicBool::new(false),
                health_settled: AtomicBool::new(false),
                records_suspect: AtomicBool::new(false),
                roll_back_now_at_boot: AtomicBool::new(false),
            }),
        }
    }

    /// Updates are admitted: set at start on a boot whose running slot is already valid (not refused, not failed closed), otherwise once the health policy reached a verdict for which `HealthVerdict::opens_admission` is true.
    ///
    /// Otherwise stays `false` until then and after `SlotUnreadable`, so a command that arrives early can never compete with the health policy for the OTA handle.
    /// For a pending slot it stays closed until mark-valid or rollback concludes, and it also opens for terminal verdicts that recorded no attempt (`NothingToVerify`, `DeadlineRollbackFailed`, `RestartReturned`).
    /// Update and rollback additionally check the live slot state (`PendingVerify`) on their own.
    pub fn admission_open(&self) -> bool {
        self.cells.admission_open.load(Ordering::Acquire)
    }

    /// The running image must not be marked valid (set by boot reconciliation on a version mismatch or a store fault).
    pub fn refuse_mark_valid(&self) -> bool {
        self.cells.refuse_mark_valid.load(Ordering::Acquire)
    }

    /// The health policy reached a terminal verdict, or (set at start) the running slot was already valid so the policy has nothing to verify.
    pub fn health_settled(&self) -> bool {
        self.cells.health_settled.load(Ordering::Acquire)
    }

    /// Boot reconciliation failed closed on a store fault and no repair has cleared it yet.
    pub fn records_suspect(&self) -> bool {
        self.cells.records_suspect.load(Ordering::Acquire)
    }

    pub(super) fn roll_back_now_at_boot(&self) -> bool {
        self.cells.roll_back_now_at_boot.load(Ordering::Acquire)
    }

    pub(super) fn set_admission_open(&self, value: bool) {
        self.cells.admission_open.store(value, Ordering::Release);
    }

    pub(super) fn set_refuse_mark_valid(&self, value: bool) {
        self.cells.refuse_mark_valid.store(value, Ordering::Release);
    }

    pub(super) fn set_health_settled(&self, value: bool) {
        self.cells.health_settled.store(value, Ordering::Release);
    }

    pub(super) fn set_records_suspect(&self, value: bool) {
        self.cells.records_suspect.store(value, Ordering::Release);
    }

    pub(super) fn set_roll_back_now_at_boot(&self, value: bool) {
        self.cells
            .roll_back_now_at_boot
            .store(value, Ordering::Release);
    }
}

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
