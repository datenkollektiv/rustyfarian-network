//! The library-owned flags of the DDI runtime, the esp-hal twin of the ESP-IDF tier's `OtaFlags`.
//!
//! Each flag is an `AtomicBool` that is only loaded and stored, never swapped: the ESP32-C3 has no atomic read-modify-write.
//! One writer sets each flag (the runtime's start, its worker or the health policy), so a plain store is enough.

use core::sync::atomic::{AtomicBool, Ordering};

/// The cells behind [`OtaFlags`]; the runtime keeps them for the whole program.
#[derive(Debug)]
pub(crate) struct FlagCells {
    admission_open: AtomicBool,
    refuse_mark_valid: AtomicBool,
    health_settled: AtomicBool,
    records_suspect: AtomicBool,
    roll_back_now_at_boot: AtomicBool,
}

impl FlagCells {
    /// Every flag clear.
    pub(crate) const fn new() -> Self {
        Self {
            admission_open: AtomicBool::new(false),
            refuse_mark_valid: AtomicBool::new(false),
            health_settled: AtomicBool::new(false),
            records_suspect: AtomicBool::new(false),
            roll_back_now_at_boot: AtomicBool::new(false),
        }
    }
}

/// Experimental: API may change before 1.0.
///
/// Flags of the runtime that the app may read but never write, with the getters of the ESP-IDF tier's `OtaFlags`.
///
/// The library owns them because it owns both sides of each coupling: boot reconciliation, the worker and the health policy.
/// The handle is `Copy`; on this tier it points at cells the runtime keeps for the whole program (the ESP-IDF tier shares them through an `Arc`, which the ESP32-C3 cannot use).
#[derive(Debug, Clone, Copy)]
pub struct OtaFlags {
    cells: &'static FlagCells,
}

impl OtaFlags {
    pub(crate) const fn new(cells: &'static FlagCells) -> Self {
        Self { cells }
    }

    /// Updates are admitted: set at start on a boot whose running slot is already valid (not refused, not failed closed), otherwise once the health policy reached a verdict for which `HealthVerdict::opens_admission` is true.
    ///
    /// Otherwise stays `false` until then and after `SlotUnreadable`, so an offer that arrives early can never compete with the health policy for the OTA slot.
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

#[cfg(test)]
mod tests {
    use super::{FlagCells, OtaFlags};

    #[test]
    fn each_setter_reaches_its_getter_only() {
        static CELLS: FlagCells = FlagCells::new();
        let flags = OtaFlags::new(&CELLS);
        let copy = flags;
        assert!(!flags.admission_open() && !flags.health_settled());
        flags.set_admission_open(true);
        flags.set_records_suspect(true);
        flags.set_roll_back_now_at_boot(true);
        assert!(copy.admission_open());
        assert!(copy.records_suspect());
        assert!(CELLS
            .roll_back_now_at_boot
            .load(core::sync::atomic::Ordering::Acquire));
        assert!(!copy.refuse_mark_valid());
        assert!(!copy.health_settled());
        flags.set_refuse_mark_valid(true);
        flags.set_health_settled(true);
        flags.set_admission_open(false);
        assert!(copy.refuse_mark_valid() && copy.health_settled() && !copy.admission_open());
    }
}
