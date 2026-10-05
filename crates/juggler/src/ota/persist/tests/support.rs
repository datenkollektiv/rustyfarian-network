//! Shared fixtures.

use alloc::vec::Vec;

use crate::ota::persist::fault_kv::FaultKv;
use crate::ota::persist::{HardwareFacts, OtaStore, UpdateSlot};
use crate::ota::{SlotId, SlotState, Version};

pub const V_OLD: Version = Version::new(1, 0, 0);
pub const V_NEW: Version = Version::new(2, 0, 0);
pub const EPOCH: u32 = 0x00C0_FFEE;
pub const SLOT_A: SlotId = SlotId(0);
pub const SLOT_B: SlotId = SlotId(1);

pub fn entropy() -> u32 {
    EPOCH
}

pub fn store(kv: &FaultKv) -> OtaStore<FaultKv> {
    OtaStore::open(kv.clone(), entropy)
}

pub fn hw(running: SlotId, state: SlotState, update: Option<(SlotId, SlotState)>) -> HardwareFacts {
    HardwareFacts {
        running_slot: running,
        running_state: state,
        update_slot: match update {
            Some((slot, state)) => UpdateSlot::Present(slot, state),
            None => UpdateSlot::Absent,
        },
    }
}

pub fn strs(items: &[&str]) -> Vec<alloc::string::String> {
    items
        .iter()
        .map(|s| alloc::string::String::from(*s))
        .collect()
}

/// Writes an attempt with all keys but `att_why`.
pub fn put_attempt(kv: &FaultKv, id: u32, version: &str, slot: &str, boot: bool, act: bool) {
    kv.put_u32("att_id", id)
        .put_u8("att_inv", 0)
        .put_str("att_ver", version)
        .put_str("att_slot", slot);
    if boot {
        kv.put_u8("att_boot", 1);
    }
    if act {
        kv.put_u8("att_act", 1);
    }
}

/// Counter and epoch as left by `id` handed-out ids.
pub fn put_counter(kv: &FaultKv, ctr: u32) {
    kv.put_u32("att_epoch", EPOCH).put_u32("att_ctr", ctr);
}

pub fn put_request(kv: &FaultKv, id: u32, why: &str, from: &str) {
    kv.put_u32("rq_id", id)
        .put_str("rq_why", why)
        .put_str("rq_from", from);
}

/// How a commit is made to misbehave.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fault {
    /// The commit applies, then the store is dead until revived (power loss).
    Crash,
    /// The commit fails and is not applied.
    Fail,
    /// A string set fails after its erase applied.
    FailAfterErase,
}

pub const FAULTS: [Fault; 3] = [Fault::Crash, Fault::Fail, Fault::FailAfterErase];

pub fn arm(kv: &FaultKv, fault: Fault, n: usize) {
    match fault {
        Fault::Crash => kv.crash_after(n),
        Fault::Fail => kv.fail_at(n),
        Fault::FailAfterErase => kv.fail_after_erase(n),
    }
}

/// Number of commits `run` performs on a world built by `setup`.
pub fn total_commits(setup: &dyn Fn(&FaultKv), run: &dyn Fn(&mut OtaStore<FaultKv>)) -> usize {
    let kv = FaultKv::new();
    setup(&kv);
    kv.reset_counters();
    run(&mut store(&kv));
    kv.commits()
}

/// Runs `run` once per (fault kind, commit index); `check` sees the revived backend afterwards.
pub fn each_fault(
    setup: &dyn Fn(&FaultKv),
    run: &dyn Fn(&mut OtaStore<FaultKv>),
    mut check: impl FnMut(&FaultKv, Fault, usize),
) {
    let total = total_commits(setup, run);
    assert!(total > 0, "the scenario must write something");
    for fault in FAULTS {
        for n in 1..=total {
            let kv = FaultKv::new();
            setup(&kv);
            kv.reset_counters();
            arm(&kv, fault, n);
            run(&mut store(&kv));
            kv.revive();
            check(&kv, fault, n);
        }
    }
}
