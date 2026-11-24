//! The download of one admitted DDI offer as an action machine (ADR 023 rung 3): the retry and give-up flow around the tier's artifact download, the settle of a failure, and the steps into the new image after a success.
//!
//! On ESP-IDF this runs on the `ota-download` thread, on esp-hal as the download half of the runtime task, so the composition lives here once (ADR 023 decision 2).
//! The tier performs each [`DownloadFlowAction`] and feeds the result back as a [`DownloadFlowInput`] with a [`Stamp`]; the machine reads no clock.
//! It calls the store for the records it owns: [`OtaStore::mark_boot_selected`] after a success and [`OtaStore::settle_failed_ddi_download`] after the final failure.
//!
//! One cancel switch (`CancelGate::for_retries`, on ESP-IDF `OtaCancel::for_retries`) spans every attempt and is the tier's: the machine asks the tier to close it when the download gives up ([`DownloadFlowAction::Close`]), and a cancel accepted before that wins over the last error.

use core::time::Duration;

use super::feedback::FeedbackPost;
use super::response::{RequestOutcome, RetryAfter};
use super::retry::{decide_download_retry, DownloadRetry};
use super::settings::DdiSettings;
use super::wire::ActionId;
use super::worker::Stamp;
use crate::ota::persist::{
    FailedDownload, HardwareFacts, HardwareReadError, OtaKv, OtaStore, StoreError,
};
use crate::ota::{BusyRetry, FailReason, OtaError, Version};

/// Experimental: API may change before 1.0.
///
/// The input to [`DownloadFlow::poll`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadFlowInput {
    /// Starts the download; the first input.
    Start,
    /// The attempt of [`DownloadFlowAction::Attempt`] ended, with the `Retry-After` of a non-200 answer when it carried one.
    AttemptEnded {
        /// `Ok` when the image is written, verified and selected for the next boot.
        result: Result<(), OtaError>,
        /// The answer's `Retry-After`, if any.
        retry_after: Option<RetryAfter>,
    },
    /// The wait of [`DownloadFlowAction::Wait`] ended, early when a cancel was requested.
    WaitEnded {
        /// The cancel switch reports a requested cancel.
        cancel_requested: bool,
    },
    /// The answer to [`DownloadFlowAction::Close`]: whether a cancel was accepted before the switch closed.
    Closed {
        /// `true` when a cancel came first.
        cancel_first: bool,
    },
    /// The answer to [`DownloadFlowAction::ReadHardware`].
    Hardware(Result<HardwareFacts, HardwareReadError>),
    /// The `proceeding` POST of [`DownloadFlowAction::Selected`] ended; its outcome is not acted on.
    Answered(RequestOutcome),
    /// The app's restart callback of [`DownloadFlowAction::RestartAt`] returned.
    RestartReturned,
}

/// Experimental: API may change before 1.0.
///
/// How a failed download was settled in the store, for logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettleNote {
    /// [`OtaStore::settle_failed_ddi_download`] returned this.
    Settled(FailedDownload),
    /// The hardware could not be read: the attempt is kept for the next boot.
    HardwareUnreadable(HardwareReadError),
    /// The store refused the settle: the attempt is kept for the next boot.
    Store(StoreError),
}

/// Experimental: API may change before 1.0.
///
/// The end of a failed download, returned once by [`DownloadFlowAction::Ended`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadEnd {
    /// The reason the attempt was settled with: [`OtaError::Cancelled`] when a cancel came first, else the last error.
    pub reason: OtaError,
    /// What the settle did.
    pub settled: SettleNote,
}

/// Experimental: API may change before 1.0.
///
/// What the tier must do next for a download ([`DownloadFlow`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DownloadFlowAction {
    /// Run one download attempt with a fresh session that holds `remaining` of the download deadline, then answer with [`DownloadFlowInput::AttemptEnded`].
    Attempt {
        /// What is left of the one download deadline of this action.
        remaining: Duration,
    },
    /// The attempt failed transiently: wait until `until`, ending early when the cancel switch reports a request, then answer with [`DownloadFlowInput::WaitEnded`].
    Wait {
        /// The monotonic time the next attempt may begin.
        until: Duration,
        /// The error of the failed attempt.
        error: OtaError,
        /// The attempts made so far.
        attempts: u32,
    },
    /// The download gives up: close the cancel switch for good, then answer with [`DownloadFlowInput::Closed`].
    Close,
    /// Read the hardware facts with this retry, then answer with [`DownloadFlowInput::Hardware`].
    ReadHardware(BusyRetry),
    /// The download failed and is settled: wake the worker; the flow is over.
    Ended(DownloadEnd),
    /// The image is selected for the next boot and the selection was recorded (`recorded`): POST `proceeding`, then answer with [`DownloadFlowInput::Answered`].
    Selected {
        /// What [`OtaStore::mark_boot_selected`] returned; a failure is logged, the restart goes ahead.
        recorded: Result<(), StoreError>,
        /// The pre-reboot `proceeding`.
        proceeding: FeedbackPost<'static>,
    },
    /// Wait until `now` reaches this time (`restart_grace`), then call the app's restart callback; answer with [`DownloadFlowInput::RestartReturned`] if it returns.
    RestartAt(Duration),
    /// The restart callback returned: park the download for good.
    Parked,
    /// The input did not answer the pending action; nothing changed (ADR 023 decision 5).
    Ignored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Attempting,
    Waiting { error: OtaError },
    Closing(OtaError),
    Settling(OtaError),
    Proceeding,
    Restarting,
    Parked,
    Ended,
}

/// Experimental: API may change before 1.0.
///
/// The download of one admitted offer (see the [module docs](self)), in the order the ESP-IDF `ota-download` thread runs it:
///
/// - each attempt gets what is left of `firmware_deadline`, counted from [`DownloadFlowInput::Start`];
/// - a failed attempt is decided by [`decide_download_retry`] with the server's `Retry-After` read against the wall clock of the [`Stamp`]: a transient error waits, anything else gives up;
/// - after a wait, a requested cancel gives up with [`OtaError::Cancelled`], a passed deadline gives up with the last error, otherwise the next attempt starts;
/// - giving up closes the cancel switch ([`DownloadFlowAction::Close`]), then settles the attempt with [`OtaStore::settle_failed_ddi_download`] after a hardware read; a cancel writes no verdict, a reason without a profile code clears the attempt;
/// - a success records the boot selection, POSTs the pre-reboot `proceeding` and asks for the restart after `restart_grace`.
#[derive(Debug, Clone)]
pub struct DownloadFlow {
    action: ActionId,
    running: Version,
    deadline: Duration,
    grace: Duration,
    started: Duration,
    attempts: u32,
    phase: Phase,
}

impl DownloadFlow {
    /// The download of `action` under `settings` (the running version, `firmware_deadline` and `restart_grace`); nothing happens before [`DownloadFlowInput::Start`].
    pub fn new(action: ActionId, settings: &DdiSettings) -> Self {
        Self {
            action,
            running: settings.running_version,
            deadline: settings.timings.firmware_deadline,
            grace: settings.timings.restart_grace,
            started: Duration::ZERO,
            attempts: 0,
            phase: Phase::Idle,
        }
    }

    /// The action this download belongs to.
    pub fn action(&self) -> &ActionId {
        &self.action
    }

    /// Advances the download.
    pub fn poll<K: OtaKv>(
        &mut self,
        store: &mut OtaStore<K>,
        input: DownloadFlowInput,
        at: Stamp,
    ) -> DownloadFlowAction {
        match (self.phase, input) {
            (Phase::Idle, DownloadFlowInput::Start) => {
                self.started = at.now;
                self.next_attempt(at)
            }
            (Phase::Attempting, DownloadFlowInput::AttemptEnded { result: Ok(()), .. }) => {
                let recorded = store.mark_boot_selected();
                self.phase = Phase::Proceeding;
                DownloadFlowAction::Selected {
                    recorded,
                    proceeding: FeedbackPost::proceeding(self.action.clone(), at.wall_millis),
                }
            }
            (
                Phase::Attempting,
                DownloadFlowInput::AttemptEnded {
                    result: Err(error),
                    retry_after,
                },
            ) => {
                let asked = retry_after.and_then(|r| r.delay(at.wall_secs()));
                let elapsed = at.now.saturating_sub(self.started);
                match decide_download_retry(&error, self.attempts, elapsed, self.deadline, asked) {
                    DownloadRetry::Retry { wait } => {
                        self.phase = Phase::Waiting { error };
                        DownloadFlowAction::Wait {
                            until: at.now.saturating_add(wait),
                            error,
                            attempts: self.attempts,
                        }
                    }
                    DownloadRetry::GiveUp => self.give_up(error),
                }
            }
            (Phase::Waiting { error }, DownloadFlowInput::WaitEnded { cancel_requested }) => {
                if cancel_requested {
                    self.give_up(OtaError::Cancelled)
                } else if at.now.saturating_sub(self.started) >= self.deadline {
                    self.give_up(error)
                } else {
                    self.next_attempt(at)
                }
            }
            (Phase::Closing(error), DownloadFlowInput::Closed { cancel_first }) => {
                let reason = if cancel_first {
                    OtaError::Cancelled
                } else {
                    error
                };
                self.phase = Phase::Settling(reason);
                DownloadFlowAction::ReadHardware(BusyRetry::default())
            }
            (Phase::Settling(reason), DownloadFlowInput::Hardware(hw)) => {
                let settled = settle_failed(store, hw, self.running, FailReason::Ota(reason));
                self.phase = Phase::Ended;
                DownloadFlowAction::Ended(DownloadEnd { reason, settled })
            }
            (Phase::Proceeding, DownloadFlowInput::Answered(_)) => {
                self.phase = Phase::Restarting;
                DownloadFlowAction::RestartAt(at.now.saturating_add(self.grace))
            }
            (Phase::Restarting, DownloadFlowInput::RestartReturned) => {
                self.phase = Phase::Parked;
                DownloadFlowAction::Parked
            }
            _ => DownloadFlowAction::Ignored,
        }
    }

    fn next_attempt(&mut self, at: Stamp) -> DownloadFlowAction {
        self.attempts = self.attempts.saturating_add(1);
        self.phase = Phase::Attempting;
        let elapsed = at.now.saturating_sub(self.started);
        DownloadFlowAction::Attempt {
            remaining: self.deadline.saturating_sub(elapsed),
        }
    }

    fn give_up(&mut self, error: OtaError) -> DownloadFlowAction {
        self.phase = Phase::Closing(error);
        DownloadFlowAction::Close
    }
}

/// Experimental: API may change before 1.0.
///
/// Settles a failed download in the store with `reason`, given the hardware read `hw`: [`OtaStore::settle_failed_ddi_download`] for the running image `running`.
///
/// Unreadable hardware settles nothing; the next boot reconciles the attempt.
pub fn settle_failed<K: OtaKv>(
    store: &mut OtaStore<K>,
    hw: Result<HardwareFacts, HardwareReadError>,
    running: Version,
    reason: FailReason,
) -> SettleNote {
    match hw {
        Err(e) => SettleNote::HardwareUnreadable(e),
        Ok(hw) => match store.settle_failed_ddi_download(hw, Some(running), reason) {
            Ok(settled) => SettleNote::Settled(settled),
            Err(e) => SettleNote::Store(e),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ota::ddi::wire::Execution;
    use crate::ota::persist::fault_kv::FaultKv;
    use crate::ota::{SlotId, SlotState, UpdateSlot};

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn at(t: u64) -> Stamp {
        Stamp {
            now: secs(t),
            wall_millis: None,
            random: 0,
        }
    }

    fn settings() -> DdiSettings {
        DdiSettings::new(Version::new(1, 0, 0), "board").expect("settings")
    }

    fn flow() -> DownloadFlow {
        DownloadFlow::new(ActionId::parse("7").expect("id"), &settings())
    }

    fn store() -> OtaStore<FaultKv> {
        OtaStore::open(FaultKv::new(), || 1)
    }

    fn hw() -> Result<HardwareFacts, HardwareReadError> {
        Ok(HardwareFacts {
            running_slot: SlotId(0),
            running_state: SlotState::Valid,
            update_slot: UpdateSlot::Present(SlotId(1), SlotState::Unknown),
        })
    }

    fn failed(e: OtaError) -> DownloadFlowInput {
        DownloadFlowInput::AttemptEnded {
            result: Err(e),
            retry_after: None,
        }
    }

    #[test]
    fn a_success_records_the_selection_sends_proceeding_and_asks_for_the_restart() {
        let mut s = store();
        let mut f = flow();
        assert_eq!(
            f.poll(&mut s, DownloadFlowInput::Start, at(10)),
            DownloadFlowAction::Attempt {
                remaining: secs(300)
            }
        );
        let action = f.poll(
            &mut s,
            DownloadFlowInput::AttemptEnded {
                result: Ok(()),
                retry_after: None,
            },
            Stamp {
                wall_millis: Some(1_700_000_000_123),
                ..at(20)
            },
        );
        let DownloadFlowAction::Selected {
            recorded,
            proceeding,
        } = action
        else {
            panic!("{action:?}");
        };
        assert_eq!(recorded, Ok(()));
        assert_eq!(proceeding.execution, Execution::Proceeding);
        assert_eq!(proceeding.timestamp, Some(1_700_000_000_123));
        assert_eq!(
            f.poll(
                &mut s,
                DownloadFlowInput::Answered(RequestOutcome::Unreachable),
                at(21)
            ),
            DownloadFlowAction::RestartAt(secs(22))
        );
        assert_eq!(
            f.poll(&mut s, DownloadFlowInput::RestartReturned, at(22)),
            DownloadFlowAction::Parked
        );
        assert_eq!(
            f.poll(&mut s, DownloadFlowInput::RestartReturned, at(23)),
            DownloadFlowAction::Ignored
        );
    }

    #[test]
    fn a_transient_error_waits_and_retries_with_the_deadline_left() {
        let mut s = store();
        let mut f = flow();
        f.poll(&mut s, DownloadFlowInput::Start, at(0));
        assert_eq!(
            f.poll(&mut s, failed(OtaError::ServerUnreachable), at(2)),
            DownloadFlowAction::Wait {
                until: secs(7),
                error: OtaError::ServerUnreachable,
                attempts: 1
            }
        );
        assert_eq!(
            f.poll(
                &mut s,
                DownloadFlowInput::WaitEnded {
                    cancel_requested: false
                },
                at(7)
            ),
            DownloadFlowAction::Attempt {
                remaining: secs(293)
            }
        );
        assert_eq!(
            f.poll(&mut s, failed(OtaError::DownloadTimeout), at(9)),
            DownloadFlowAction::Wait {
                until: secs(19),
                error: OtaError::DownloadTimeout,
                attempts: 2
            },
            "the back-off doubles"
        );
    }

    #[test]
    fn retry_after_sets_a_longer_wait() {
        let mut s = store();
        let mut f = flow();
        f.poll(&mut s, DownloadFlowInput::Start, at(0));
        assert_eq!(
            f.poll(
                &mut s,
                DownloadFlowInput::AttemptEnded {
                    result: Err(OtaError::DownloadFailed { status: 503 }),
                    retry_after: RetryAfter::capture("40"),
                },
                at(1)
            ),
            DownloadFlowAction::Wait {
                until: secs(41),
                error: OtaError::DownloadFailed { status: 503 },
                attempts: 1
            }
        );
    }

    #[test]
    fn a_final_error_gives_up_and_settles_with_it() {
        let mut s = store();
        let mut f = flow();
        f.poll(&mut s, DownloadFlowInput::Start, at(0));
        assert_eq!(
            f.poll(&mut s, failed(OtaError::ChecksumMismatch), at(1)),
            DownloadFlowAction::Close
        );
        assert_eq!(
            f.poll(
                &mut s,
                DownloadFlowInput::Closed {
                    cancel_first: false
                },
                at(1)
            ),
            DownloadFlowAction::ReadHardware(BusyRetry::default())
        );
        assert_eq!(
            f.poll(&mut s, DownloadFlowInput::Hardware(hw()), at(1)),
            DownloadFlowAction::Ended(DownloadEnd {
                reason: OtaError::ChecksumMismatch,
                settled: SettleNote::Settled(FailedDownload::NoAttempt),
            })
        );
    }

    #[test]
    fn a_cancel_accepted_before_the_give_up_wins_over_the_error() {
        let mut s = store();
        let mut f = flow();
        f.poll(&mut s, DownloadFlowInput::Start, at(0));
        f.poll(&mut s, failed(OtaError::ChecksumMismatch), at(1));
        f.poll(
            &mut s,
            DownloadFlowInput::Closed { cancel_first: true },
            at(1),
        );
        let DownloadFlowAction::Ended(end) =
            f.poll(&mut s, DownloadFlowInput::Hardware(hw()), at(1))
        else {
            panic!("no end");
        };
        assert_eq!(end.reason, OtaError::Cancelled);
    }

    #[test]
    fn a_cancel_during_the_wait_gives_up_with_cancelled() {
        let mut s = store();
        let mut f = flow();
        f.poll(&mut s, DownloadFlowInput::Start, at(0));
        f.poll(&mut s, failed(OtaError::ServerUnreachable), at(1));
        assert_eq!(
            f.poll(
                &mut s,
                DownloadFlowInput::WaitEnded {
                    cancel_requested: true
                },
                at(3)
            ),
            DownloadFlowAction::Close
        );
        f.poll(
            &mut s,
            DownloadFlowInput::Closed { cancel_first: true },
            at(3),
        );
        let DownloadFlowAction::Ended(end) = f.poll(
            &mut s,
            DownloadFlowInput::Hardware(Err(HardwareReadError::Read(-1))),
            at(3),
        ) else {
            panic!("no end");
        };
        assert_eq!(end.reason, OtaError::Cancelled);
        assert_eq!(
            end.settled,
            SettleNote::HardwareUnreadable(HardwareReadError::Read(-1))
        );
    }

    #[test]
    fn a_wait_that_ends_past_the_deadline_gives_up_with_the_last_error() {
        let mut s = store();
        let mut f = flow();
        f.poll(&mut s, DownloadFlowInput::Start, at(0));
        assert!(matches!(
            f.poll(&mut s, failed(OtaError::DownloadTimeout), at(290)),
            DownloadFlowAction::Wait { until, .. } if until == secs(295)
        ));
        f.poll(
            &mut s,
            DownloadFlowInput::WaitEnded {
                cancel_requested: false,
            },
            at(300),
        );
        f.poll(
            &mut s,
            DownloadFlowInput::Closed {
                cancel_first: false,
            },
            at(300),
        );
        let DownloadFlowAction::Ended(end) =
            f.poll(&mut s, DownloadFlowInput::Hardware(hw()), at(300))
        else {
            panic!("no end");
        };
        assert_eq!(end.reason, OtaError::DownloadTimeout);
    }

    #[test]
    fn out_of_phase_input_is_ignored_and_changes_nothing() {
        let mut s = store();
        let mut f = flow();
        for input in [
            failed(OtaError::ServerUnreachable),
            DownloadFlowInput::WaitEnded {
                cancel_requested: true,
            },
            DownloadFlowInput::Closed { cancel_first: true },
            DownloadFlowInput::Hardware(hw()),
            DownloadFlowInput::Answered(RequestOutcome::Timeout),
            DownloadFlowInput::RestartReturned,
        ] {
            assert_eq!(
                f.poll(&mut s, input.clone(), at(0)),
                DownloadFlowAction::Ignored,
                "{input:?}"
            );
        }
        assert!(matches!(
            f.poll(&mut s, DownloadFlowInput::Start, at(0)),
            DownloadFlowAction::Attempt { .. }
        ));
        assert_eq!(
            f.poll(&mut s, DownloadFlowInput::Start, at(0)),
            DownloadFlowAction::Ignored
        );
        assert_eq!(
            f.poll(
                &mut s,
                DownloadFlowInput::WaitEnded {
                    cancel_requested: true
                },
                at(0)
            ),
            DownloadFlowAction::Ignored,
            "no wait is running"
        );
    }
}
