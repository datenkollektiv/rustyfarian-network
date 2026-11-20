//! The health policy driver shared by the MQTT and the DDI runtime: runs the policy on the app's main thread.
//!
//! One tier runs it on one thread, so it is plain sequential code (ADR 023 decision 3): `juggler` decides ([`SlotReads`], [`poll_decision`], [`after_unhealthy`], [`deadline_plan`]) and this file performs the steps (slot reads, sleeps, the app predicate, `mark_valid`, the attempt record, the rollback, the app's restart callback).
//! It never touches a slot that is not pending verification: it returns before the poll loop for any other state.
//!
//! The settle step after the verdict is not here, because it differs: the MQTT runtime publishes `applied` and arms the reporter's completion retry, the DDI runtime signals the `ota-worker` thread, which delivers the success verdict `complete_attempt` already persisted.

use std::thread;
use std::time::{Duration, Instant};

use juggler::ota::health::{
    after_unhealthy, deadline_plan, poll_decision, HealthConfig, HealthVerdict, PollStep,
    SlotReading, SlotReads, SlotRetry,
};
use juggler::ota::{HardwareReadError, ReasonNote, RollbackReason, SlotId, SlotState, Version};

use super::flags::OtaFlags;
use super::records::{Records, RestartFn};
use super::{read_hardware_facts, BusyRetry, OtaSession, OtaSessionConfig};

/// What the policy needs from a runtime: its timings, the record store, the flags, the restart callback and the identity of the running image.
pub(crate) struct HealthDriver<'a> {
    pub(crate) config: HealthConfig,
    pub(crate) records: &'a Records,
    pub(crate) flags: &'a OtaFlags,
    pub(crate) restart: &'a RestartFn,
    pub(crate) running: Version,
    pub(crate) timeout_secs: u64,
}

impl HealthDriver<'_> {
    /// Runs the policy and returns its verdict; the caller guards against a second run and settles the verdict.
    ///
    /// `boot` is the app's [`Instant`] from the start of `main`: the dwell and the failure deadline are measured from it.
    pub(crate) fn run<F: FnMut() -> bool>(&self, boot: Instant, mut healthy: F) -> HealthVerdict {
        let slot = match self.read_pending_slot(boot) {
            Ok(slot) => slot,
            Err(verdict) => return verdict,
        };

        loop {
            let step =
                match poll_decision(&self.config, boot.elapsed(), self.flags.refuse_mark_valid()) {
                    PollStep::Evaluate => {
                        if healthy() && self.mark_valid() {
                            return self.complete_attempt_with_retries();
                        }
                        after_unhealthy(&self.config, boot.elapsed())
                    }
                    other => other,
                };
            match step {
                PollStep::Wait(pause) => thread::sleep(pause),
                PollStep::Deadline => return self.at_deadline(slot),
                PollStep::Evaluate => {}
            }
        }
    }

    /// Reads the running slot until it is known: `Ok` with the slot when it is pending verification, `Err` with the verdict that ends the policy otherwise.
    ///
    /// A busy or failed read is retried every `slot_retry`; it gives up (`SlotUnreadable`) only after `slot_min_attempts` reads AND the failure deadline, touching nothing.
    fn read_pending_slot(&self, boot: Instant) -> Result<SlotId, HealthVerdict> {
        let single_read = BusyRetry {
            tries: 1,
            delay: Duration::ZERO,
        };
        let mut reads = SlotReads::default();
        loop {
            let reading = match read_hardware_facts(single_read) {
                Ok(hardware) if hardware.running_state == SlotState::PendingVerify => {
                    SlotReading::PendingVerify(hardware.running_slot)
                }
                Ok(_) => SlotReading::NotPending,
                Err(HardwareReadError::Busy) => SlotReading::Busy,
                Err(HardwareReadError::Read(_)) => SlotReading::Failed,
            };
            let retry = match reading {
                SlotReading::PendingVerify(slot) => return Ok(slot),
                SlotReading::NotPending => return Err(HealthVerdict::NothingToVerify),
                SlotReading::Busy => reads.busy(&self.config, boot.elapsed()),
                SlotReading::Failed => reads.failed(&self.config, boot.elapsed()),
            };
            match retry {
                SlotRetry::Sleep { pause, warn } => {
                    if let Some(failures) = warn {
                        log::warn!(
                            "[ota] cannot read the running OTA slot ({failures} failed reads)"
                        );
                    }
                    thread::sleep(pause);
                }
                SlotRetry::GiveUp => return Err(HealthVerdict::SlotUnreadable),
            }
        }
    }

    /// Records the completed attempt, retried `complete_attempts` times; `ApplyNotRecorded` when it never succeeds.
    fn complete_attempt_with_retries(&self) -> HealthVerdict {
        let mut tries = 0;
        loop {
            if self.complete_attempt() {
                return HealthVerdict::Applied;
            }
            tries += 1;
            if tries >= self.config.complete_attempts {
                return HealthVerdict::ApplyNotRecorded;
            }
            thread::sleep(self.config.complete_retry);
        }
    }

    /// The failure deadline: notes the reason, then restarts through the app's callback or rolls back with bounded retries.
    fn at_deadline(&self, slot: SlotId) -> HealthVerdict {
        let plan = deadline_plan(&self.config);
        self.note_rollback(slot, plan.reason);
        if plan.restart {
            log::error!("[ota] the image stayed unhealthy; calling the restart callback");
            (self.restart)();
            return HealthVerdict::RestartReturned;
        }
        let mut tries = 0;
        loop {
            if self.rollback() {
                return HealthVerdict::DeadlineRolledBack;
            }
            tries += 1;
            if tries >= self.config.rollback_attempts {
                return HealthVerdict::DeadlineRollbackFailed;
            }
            thread::sleep(self.config.rollback_retry);
        }
    }

    fn session(&self) -> Option<OtaSession> {
        match OtaSession::new(OtaSessionConfig {
            timeout_secs: self.timeout_secs,
        }) {
            Ok(session) => Some(session),
            Err(e) => {
                log::error!("[ota] no OTA session: {}", e.code());
                None
            }
        }
    }

    fn mark_valid(&self) -> bool {
        let Some(mut session) = self.session() else {
            return false;
        };
        match session.mark_valid() {
            Ok(()) => {
                log::info!(
                    "[ota] image {} is healthy; the slot is marked valid",
                    self.running
                );
                true
            }
            Err(e) => {
                log::warn!(
                    "[ota] mark_valid failed: {}; retrying until the deadline",
                    e.code()
                );
                false
            }
        }
    }

    fn complete_attempt(&self) -> bool {
        let running = self.running;
        match self.records.with(|store| store.complete_attempt(running)) {
            Ok(()) => true,
            Err(e) => {
                log::warn!("[ota] could not complete the update attempt: {e}");
                false
            }
        }
    }

    fn note_rollback(&self, slot: SlotId, reason: RollbackReason) {
        match self.records.with(|store| store.note_rollback(reason, slot)) {
            Ok(ReasonNote::Written(_)) => log::info!("[ota] rollback reason {reason:?} recorded"),
            Ok(ReasonNote::KeptFirst) => {
                log::info!("[ota] an earlier rollback reason of this boot is kept");
            }
            Err(e) => log::warn!("[ota] the rollback reason was not recorded: {e}"),
        }
    }

    fn rollback(&self) -> bool {
        let Some(mut session) = self.session() else {
            return false;
        };
        log::error!("[ota] health deadline reached with the slot still pending; rolling back");
        match session.rollback() {
            Ok(()) => true,
            Err(e) => {
                log::error!("[ota] the rollback failed: {}", e.code());
                false
            }
        }
    }
}
