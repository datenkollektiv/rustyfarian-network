//! What the MQTT callback does with one command payload.
//!
//! The callback must never block or call the client (ADR 017), so the whole decision is: parse, check the runtime is alive, claim the single busy slot, hand the command to a bounded channel.
//! The effects are passed in as closures so the order is part of the tested contract:
//! the `closed` check comes BEFORE the busy claim, so a dead worker never leaves a stuck busy flag behind.

use crate::ota::{CommandError, FailReason, OtaCommand};

/// The result of handing a command to the worker queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendResult {
    /// The command is queued.
    Sent,
    /// The queue is full.
    Full,
    /// The worker has gone away (its receiver was dropped).
    Disconnected,
}

/// What the callback must do after [`decide_intake`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntakeDecision {
    /// The command is queued and the busy flag stays set until the worker finishes it.
    Queued,
    /// The command was not queued: report `reason` through the reporter, and apply the two flags.
    Rejected {
        /// The status reason to report.
        reason: FailReason,
        /// The busy flag was claimed by this call and must be released.
        release_busy: bool,
        /// The worker is gone: set the shared `closed` flag so later commands answer `worker_unavailable` without touching busy.
        set_closed: bool,
    },
}

impl IntakeDecision {
    const fn reject(reason: FailReason) -> Self {
        IntakeDecision::Rejected {
            reason,
            release_busy: false,
            set_closed: false,
        }
    }
}

/// Decides what to do with one parsed command.
///
/// Order, each step only if the previous one passed:
///
/// 1. a parse error is `command_invalid`;
/// 2. `closed` is `worker_unavailable`, `claim_busy` is not called;
/// 3. `claim_busy()` returns whether this call won the busy flag (a compare-and-swap in the driver); losing is `busy`;
/// 4. `send(command)` queues the command: a full queue is `busy` and releases the flag, a gone worker is `worker_unavailable`, releases the flag and sets `closed`.
///
/// No payload text is part of the decision.
pub fn decide_intake(
    parsed: Result<OtaCommand, CommandError>,
    closed: bool,
    claim_busy: impl FnOnce() -> bool,
    send: impl FnOnce(OtaCommand) -> SendResult,
) -> IntakeDecision {
    let command = match parsed {
        Ok(command) => command,
        Err(e) => return IntakeDecision::reject(e.reason()),
    };
    if closed {
        return IntakeDecision::reject(FailReason::WorkerUnavailable);
    }
    if !claim_busy() {
        return IntakeDecision::reject(FailReason::Busy);
    }
    match send(command) {
        SendResult::Sent => IntakeDecision::Queued,
        SendResult::Full => IntakeDecision::Rejected {
            reason: FailReason::Busy,
            release_busy: true,
            set_closed: false,
        },
        SendResult::Disconnected => IntakeDecision::Rejected {
            reason: FailReason::WorkerUnavailable,
            release_busy: true,
            set_closed: true,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::Cell;

    fn update() -> Result<OtaCommand, CommandError> {
        OtaCommand::parse(br#"{"manifest_url":"http://h/m.json"}"#)
    }

    struct Probe {
        claimed: Cell<u32>,
        sent: Cell<u32>,
    }

    impl Probe {
        fn new() -> Self {
            Self {
                claimed: Cell::new(0),
                sent: Cell::new(0),
            }
        }

        fn run(
            &self,
            parsed: Result<OtaCommand, CommandError>,
            closed: bool,
            win: bool,
            send: SendResult,
        ) -> IntakeDecision {
            decide_intake(
                parsed,
                closed,
                || {
                    self.claimed.set(self.claimed.get() + 1);
                    win
                },
                |_| {
                    self.sent.set(self.sent.get() + 1);
                    send
                },
            )
        }
    }

    #[test]
    fn a_parse_error_is_command_invalid_and_touches_nothing() {
        let p = Probe::new();
        for e in [
            CommandError::TooLarge,
            CommandError::Malformed { line: 1, column: 1 },
            CommandError::UnsupportedShape,
            CommandError::InvalidUrl,
            CommandError::InvalidVersion,
        ] {
            let d = p.run(Err(e), false, true, SendResult::Sent);
            assert_eq!(d, IntakeDecision::reject(FailReason::CommandInvalid));
        }
        assert_eq!((p.claimed.get(), p.sent.get()), (0, 0));
    }

    #[test]
    fn closed_is_checked_before_the_busy_claim() {
        let p = Probe::new();
        let d = p.run(update(), true, true, SendResult::Sent);
        assert_eq!(d, IntakeDecision::reject(FailReason::WorkerUnavailable));
        assert_eq!(
            (p.claimed.get(), p.sent.get()),
            (0, 0),
            "busy stays untouched"
        );
    }

    #[test]
    fn losing_the_claim_is_busy_and_releases_nothing() {
        let p = Probe::new();
        let d = p.run(update(), false, false, SendResult::Sent);
        assert_eq!(d, IntakeDecision::reject(FailReason::Busy));
        assert_eq!((p.claimed.get(), p.sent.get()), (1, 0));
    }

    #[test]
    fn a_queued_command_keeps_the_busy_flag() {
        let p = Probe::new();
        assert_eq!(
            p.run(update(), false, true, SendResult::Sent),
            IntakeDecision::Queued
        );
        assert_eq!((p.claimed.get(), p.sent.get()), (1, 1));
    }

    #[test]
    fn a_full_queue_is_busy_and_releases_the_flag() {
        let p = Probe::new();
        assert_eq!(
            p.run(update(), false, true, SendResult::Full),
            IntakeDecision::Rejected {
                reason: FailReason::Busy,
                release_busy: true,
                set_closed: false
            }
        );
    }

    #[test]
    fn a_gone_worker_closes_the_runtime_and_releases_the_flag() {
        let p = Probe::new();
        assert_eq!(
            p.run(update(), false, true, SendResult::Disconnected),
            IntakeDecision::Rejected {
                reason: FailReason::WorkerUnavailable,
                release_busy: true,
                set_closed: true
            }
        );
    }

    #[test]
    fn a_command_queued_before_the_worker_died_is_followed_by_the_closed_path() {
        let p = Probe::new();
        assert_eq!(
            p.run(update(), false, true, SendResult::Sent),
            IntakeDecision::Queued
        );
        let d = p.run(update(), true, true, SendResult::Sent);
        assert_eq!(d, IntakeDecision::reject(FailReason::WorkerUnavailable));
        assert_eq!(p.claimed.get(), 1, "the second call never claims busy");
    }

    #[test]
    fn every_command_shape_is_queued_not_interpreted() {
        for payload in [
            &br#"{"manifest_url":"http://h/m.json"}"#[..],
            br#"{"action":"rollback","from":"1.0.0"}"#,
            br#"{"action":"repair"}"#,
        ] {
            let p = Probe::new();
            let d = p.run(OtaCommand::parse(payload), false, true, SendResult::Sent);
            assert_eq!(d, IntakeDecision::Queued);
        }
    }

    #[test]
    fn decisions_carry_no_payload_text() {
        let secret = br#"{"manifest_url":"http://user:secretpass@h/m.json","bogus":1}"#;
        let p = Probe::new();
        let d = p.run(OtaCommand::parse(secret), false, true, SendResult::Sent);
        assert!(!alloc::format!("{d:?}").contains("secretpass"));
    }
}
