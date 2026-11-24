//! The health policy of the esp-hal DDI runtime: [`HealthFlow`] driven as an `async` loop on the app's task.
//!
//! `juggler` decides every step ([`HealthFlow`], ADR 023 rung 3); this file performs each one: a slot read, a timer, the app predicate, `mark_valid` and the rollback through the [`SharedFlash`], and the app's restart callback.
//! A rollback on this tier only writes the otadata, so a successful one is followed by the restart callback, where the ESP-IDF tier's rollback restarts by itself.

use core::time::Duration;

use embassy_time::{Instant, Timer};
use juggler::ota::health::{
    HealthAction, HealthConfig, HealthFlow, HealthInput, HealthNote, SlotReading,
};
use juggler::ota::{BusyRetry, HardwareReadError, HealthVerdict, SlotState, Version};

use super::runner::with_store;
use super::RestartFn;
use crate::ota::flags::OtaFlags;
use crate::ota::flash::SharedFlash;
use crate::ota::manager::{to_core, to_embassy};
use crate::ota::records::Records;
use crate::ota::{read_hardware_facts, EspHalOtaManager, OtaManagerConfig};

/// What the policy needs from the runtime: its timings, the record store, the flags, the restart callback, the running version and the flash.
pub(super) struct HealthDriver<'a> {
    pub(super) config: HealthConfig,
    pub(super) records: Records,
    pub(super) flags: &'a OtaFlags,
    pub(super) restart: RestartFn,
    pub(super) running: Version,
    pub(super) timeout_secs: u64,
    pub(super) flash: &'static SharedFlash,
}

impl HealthDriver<'_> {
    /// Runs the policy and returns its verdict; the caller guards against a second run and settles the verdict.
    ///
    /// `boot` is the app's [`Instant`] from the start of `main`: the dwell and the failure deadline are measured from it.
    pub(super) async fn run<F: FnMut() -> bool>(
        &self,
        boot: Instant,
        mut healthy: F,
    ) -> HealthVerdict {
        let mut flow = HealthFlow::new(self.config, self.running);
        let mut input = HealthInput::Start;
        loop {
            let elapsed = to_core(boot.elapsed());
            let refuse = self.flags.refuse_mark_valid();
            let action = with_store(self.records, |store| {
                flow.poll(store, input, elapsed, refuse)
            })
            .await;
            if let Some(note) = flow.take_note() {
                log_note(note);
            }
            input = match action {
                HealthAction::ReadSlot => HealthInput::Slot(read_slot()),
                HealthAction::Wait(pause) => {
                    Timer::after(to_embassy(pause)).await;
                    HealthInput::Waited
                }
                HealthAction::AskHealthy => HealthInput::Healthy(healthy()),
                HealthAction::MarkValid => HealthInput::MarkedValid(self.mark_valid()),
                HealthAction::Rollback => HealthInput::RolledBack(self.rollback()),
                HealthAction::Restart => {
                    log::error!("[ota] the image stayed unhealthy; calling the restart callback");
                    (self.restart)();
                    HealthInput::RestartReturned
                }
                HealthAction::Done(verdict) => return verdict,
                HealthAction::Ignored => {
                    log::error!("[ota] the health policy ignored its input; internal error");
                    return HealthVerdict::SlotUnreadable;
                }
            };
        }
    }

    fn manager(&self) -> Option<EspHalOtaManager<'static>> {
        let config = OtaManagerConfig {
            timeout_secs: self.timeout_secs,
        };
        match EspHalOtaManager::new_shared(config, self.flash) {
            Ok(manager) => Some(manager),
            Err(e) => {
                log::error!("[ota] no OTA manager: {}", e.code());
                None
            }
        }
    }

    fn mark_valid(&self) -> bool {
        let Some(mut manager) = self.manager() else {
            return false;
        };
        match manager.mark_valid() {
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

    /// Writes the rollback, then calls the restart callback; `true` when the rollback was written (the callback returned).
    fn rollback(&self) -> bool {
        let Some(mut manager) = self.manager() else {
            return false;
        };
        log::error!("[ota] health deadline reached with the slot still pending; rolling back");
        match manager.rollback() {
            Ok(()) => {
                log::warn!("[ota] the rollback is written; calling the restart callback");
                (self.restart)();
                true
            }
            Err(e) => {
                log::error!("[ota] the rollback failed: {}", e.code());
                false
            }
        }
    }
}

/// One read of the running slot, no retry (the flow retries).
fn read_slot() -> SlotReading {
    let once = BusyRetry {
        tries: 1,
        delay: Duration::ZERO,
    };
    match read_hardware_facts(once) {
        Ok(hardware) if hardware.running_state == SlotState::PendingVerify => {
            SlotReading::PendingVerify(hardware.running_slot)
        }
        Ok(_) => SlotReading::NotPending,
        Err(HardwareReadError::Busy) => SlotReading::Busy,
        Err(HardwareReadError::Read(_)) => SlotReading::Failed,
    }
}

fn log_note(note: HealthNote) {
    match note {
        HealthNote::SlotUnreadable(failures) => {
            log::warn!("[ota] cannot read the running OTA slot ({failures} failed reads)");
        }
        HealthNote::CompleteFailed(e) => {
            log::warn!("[ota] could not complete the update attempt: {e}");
        }
        HealthNote::RollbackNoted {
            reason,
            written: true,
        } => log::info!("[ota] rollback reason {reason:?} recorded"),
        HealthNote::RollbackNoted { written: false, .. } => {
            log::info!("[ota] an earlier rollback reason of this boot is kept");
        }
        HealthNote::RollbackNotNoted { error, .. } => {
            log::warn!("[ota] the rollback reason was not recorded: {error}");
        }
    }
}
