//! The health policy hook of the MQTT runtime.
//!
//! The policy itself is the shared `HealthDriver` (`ota::health_driver`); this file guards against a second run and performs the MQTT-specific settle step: it publishes `applied` and arms the reporter's completion retry.

use std::sync::atomic::Ordering;
use std::time::Instant;

use juggler::ota::runtime::{HealthConfig, HealthVerdict};
use juggler::ota::OtaStatus;

use super::publish::publish_status;
use super::OtaHandle;
use crate::ota::health_driver::HealthDriver;

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
        healthy: F,
    ) -> HealthVerdict {
        if self.health_started.swap(true, Ordering::AcqRel) {
            return HealthVerdict::AlreadyRunning;
        }
        let timings = &self.cfg.settings.timings;
        let verdict = HealthDriver {
            config: HealthConfig::from(timings),
            records: &self.records,
            flags: &self.flags,
            restart: &self.cfg.restart,
            running: self.cfg.settings.running_version,
            timeout_secs: timings.firmware_timeout_secs,
        }
        .run(boot, healthy);
        self.settle(verdict);
        verdict
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
