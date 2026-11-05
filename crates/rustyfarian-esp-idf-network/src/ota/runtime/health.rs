//! The health policy driver: runs the policy on the app's main thread.
//!
//! One tier runs it on one thread, so it is plain sequential code (ADR 023 decision 3): `juggler` decides ([`SlotReads`], [`poll_decision`], [`after_unhealthy`], [`deadline_plan`]) and this file performs the steps (slot reads, sleeps, the app predicate, `mark_valid`, the attempt record, the rollback, the app's restart callback).
//! It never touches a slot that is not pending verification: it returns before the poll loop for any other state.

use std::sync::atomic::Ordering;
use std::thread;
use std::time::{Duration, Instant};

use juggler::ota::runtime::{
    after_unhealthy, deadline_plan, poll_decision, HealthConfig, HealthVerdict, PollStep,
    SlotReading, SlotReads, SlotRetry,
};
use juggler::ota::{HardwareReadError, OtaStatus, ReasonNote, SlotId, SlotState};

use super::publish::publish_status;
use super::OtaHandle;
use crate::ota::{read_hardware_facts, BusyRetry, OtaSession, OtaSessionConfig};

impl OtaHandle {
    /// Runs the health policy of the running image on the calling thread and returns its verdict.
    ///
    /// Call it once from the app's main thread after the MQTT client and the runtime are started, where the app would otherwise park.
    /// `boot` MUST be the [`Instant`] captured at the very start of `main`, before Wi-Fi: the dwell and the failure deadline are measured from it, not from association.
    /// `healthy` is the app's predicate (for example a fresh tick, MQTT connected, an IPv4 address); the library adds the minimum dwell, the refusal flag and the deadline.
    ///
    /// The policy:
    /// - reads the running slot every `slot_retry`; it gives up (`SlotUnreadable`) only after `slot_min_attempts` reads AND the failure deadline, touching nothing (failed reads count for a failed read, failed and busy reads together for a busy one);
    /// - does nothing for a slot that is not pending verification (`NothingToVerify`) and opens update admission;
    /// - for a pending slot polls every `poll_interval`: once the dwell has passed, the refusal flag is clear and `healthy()` is true, it marks the image valid, records the completed attempt (retried `complete_attempts` times) and publishes `applied`; if the record still cannot be cleared `applied` is published anyway (`ApplyNotRecorded`) and the reporter thread keeps retrying with backoff until the record is cleared;
    /// - at the failure deadline notes the rollback reason, then rolls back with bounded retries (`DeadlineAction::Rollback`), or notes the reason and calls the app's restart callback (`DeadlineAction::NoteAndRestart`).
    ///
    /// A second call returns [`HealthVerdict::AlreadyRunning`] without doing anything.
    ///
    /// If `open_records` or `OtaRuntime::start` failed there is no handle and no health policy: a pending image is then NOT marked valid and the bootloader aborts it at the next reset.
    /// That is deliberate; a degraded policy that marked the image valid without attempt bookkeeping would hide the failure.
    pub fn run_health_policy<F: FnMut() -> bool>(
        &self,
        boot: Instant,
        mut healthy: F,
    ) -> HealthVerdict {
        if self.health_started.swap(true, Ordering::AcqRel) {
            return HealthVerdict::AlreadyRunning;
        }
        let config = HealthConfig::from(&self.cfg.settings.timings);
        let slot = match self.read_pending_slot(&config, boot) {
            Ok(slot) => slot,
            Err(verdict) => return self.finish(verdict),
        };

        loop {
            let step = match poll_decision(&config, boot.elapsed(), self.flags.refuse_mark_valid())
            {
                PollStep::Evaluate => {
                    if healthy() && self.mark_valid() {
                        let verdict = self.complete_attempt_with_retries(&config);
                        return self.finish(verdict);
                    }
                    after_unhealthy(&config, boot.elapsed())
                }
                other => other,
            };
            match step {
                PollStep::Wait(pause) => thread::sleep(pause),
                PollStep::Deadline => {
                    let verdict = self.at_deadline(&config, slot);
                    return self.finish(verdict);
                }
                PollStep::Evaluate => {}
            }
        }
    }

    /// Reads the running slot until it is known: `Ok` with the slot when it is pending verification, `Err` with the verdict that ends the policy otherwise.
    ///
    /// A busy or failed read is retried every `slot_retry`; it gives up (`SlotUnreadable`) only after `slot_min_attempts` reads AND the failure deadline, touching nothing.
    fn read_pending_slot(
        &self,
        config: &HealthConfig,
        boot: Instant,
    ) -> Result<SlotId, HealthVerdict> {
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
                SlotReading::Busy => reads.busy(config, boot.elapsed()),
                SlotReading::Failed => reads.failed(config, boot.elapsed()),
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
    fn complete_attempt_with_retries(&self, config: &HealthConfig) -> HealthVerdict {
        let mut tries = 0;
        loop {
            if self.complete_attempt() {
                return HealthVerdict::Applied;
            }
            tries += 1;
            if tries >= config.complete_attempts {
                return HealthVerdict::ApplyNotRecorded;
            }
            thread::sleep(config.complete_retry);
        }
    }

    /// The failure deadline: notes the reason, then restarts through the app's callback or rolls back with bounded retries.
    fn at_deadline(&self, config: &HealthConfig, slot: SlotId) -> HealthVerdict {
        let plan = deadline_plan(config);
        self.note_rollback(slot, plan.reason);
        if plan.restart {
            log::error!("[ota] the image stayed unhealthy; calling the restart callback");
            (self.cfg.restart)();
            return HealthVerdict::RestartReturned;
        }
        let mut tries = 0;
        loop {
            if self.rollback() {
                return HealthVerdict::DeadlineRolledBack;
            }
            tries += 1;
            if tries >= config.rollback_attempts {
                return HealthVerdict::DeadlineRollbackFailed;
            }
            thread::sleep(config.rollback_retry);
        }
    }

    fn finish(&self, verdict: HealthVerdict) -> HealthVerdict {
        self.settle(verdict);
        verdict
    }

    fn session(&self) -> Option<OtaSession> {
        match OtaSession::new(OtaSessionConfig {
            timeout_secs: self.cfg.settings.timings.firmware_timeout_secs,
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
                    self.cfg.settings.running_version
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
        let running = self.cfg.settings.running_version;
        match self.records.with(|store| store.complete_attempt(running)) {
            Ok(()) => true,
            Err(e) => {
                log::warn!("[ota] could not complete the update attempt: {e}");
                false
            }
        }
    }

    fn note_rollback(&self, slot: SlotId, reason: juggler::ota::RollbackReason) {
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

    /// Applies a terminal verdict: publishes `applied` first (also when the attempt record could not be cleared), arms the background completion retry if needed, then opens admission, and marks the policy settled.
    fn settle(&self, verdict: HealthVerdict) {
        log::info!("[ota] health policy finished: {verdict:?}");
        if verdict.publishes_applied() {
            let version = self.cfg.settings.running_version.to_string();
            publish_status(
                &self.mqtt,
                &self.cfg.settings.status_topic,
                &OtaStatus::Applied { version: &version },
                &self.cfg.settings.timings,
            );
        }
        if verdict.arms_completion_retry() {
            log::warn!(
                "[ota] the attempt record was not cleared; the reporter keeps retrying in the background"
            );
            self.shared.request_completion_retry();
        }
        if verdict.opens_admission() {
            self.flags.set_admission_open(true);
        }
        self.flags.set_health_settled(true);
    }
}
