//! The library-owned flags of an OTA runtime, shared by the MQTT and the DDI runtime.

use std::sync::atomic::{AtomicBool, Ordering};
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
    pub(crate) fn new() -> Self {
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

    pub(crate) fn roll_back_now_at_boot(&self) -> bool {
        self.cells.roll_back_now_at_boot.load(Ordering::Acquire)
    }

    pub(crate) fn set_admission_open(&self, value: bool) {
        self.cells.admission_open.store(value, Ordering::Release);
    }

    pub(crate) fn set_refuse_mark_valid(&self, value: bool) {
        self.cells.refuse_mark_valid.store(value, Ordering::Release);
    }

    pub(crate) fn set_health_settled(&self, value: bool) {
        self.cells.health_settled.store(value, Ordering::Release);
    }

    pub(crate) fn set_records_suspect(&self, value: bool) {
        self.cells.records_suspect.store(value, Ordering::Release);
    }

    pub(crate) fn set_roll_back_now_at_boot(&self, value: bool) {
        self.cells
            .roll_back_now_at_boot
            .store(value, Ordering::Release);
    }
}
