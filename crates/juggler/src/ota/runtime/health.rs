//! The health policy of the running image as a state machine.
//!
//! The policy decides when a freshly activated image proves itself healthy, and what happens when it does not.
//! It never touches a slot that is not pending verification, so a normally booted image cannot restart itself because the broker happens to be down.
//!
//! [`HealthMachine::step`] is a function of the elapsed time, the refusal flag and the outcome of the previous action.
//! The driver (on the app's main thread) executes the returned [`HealthAction`] and feeds the result back; the machine never sleeps, reads a slot, marks valid, rolls back or restarts by itself.
//! Elapsed time is measured from the boot instant the app captured at the very start of `main`, not from Wi-Fi association.

use core::time::Duration;

use crate::ota::persist::{BootDisposition, BootOutcome};
use crate::ota::runtime::config::{DeadlineAction, OtaTimings};
use crate::ota::RollbackReason;

/// The slice of [`OtaTimings`] the policy uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthConfig {
    /// Time since boot before a healthy image is marked valid.
    pub healthy_dwell: Duration,
    /// Time since boot after which an unhealthy image gives up.
    pub failure_deadline: Duration,
    /// Poll interval while verifying.
    pub poll_interval: Duration,
    /// Interval between slot reads at start.
    pub slot_retry: Duration,
    /// Failed (not busy) slot reads before the deadline may end verification.
    pub slot_min_attempts: u32,
    /// Failed slot reads between two warnings.
    pub slot_warn_every: u32,
    /// Attempts to record the completed attempt.
    pub complete_attempts: u32,
    /// Pause between those attempts.
    pub complete_retry: Duration,
    /// Rollback attempts at the deadline.
    pub rollback_attempts: u32,
    /// Pause between those attempts.
    pub rollback_retry: Duration,
    /// What happens at the deadline.
    pub deadline_action: DeadlineAction,
}

impl From<&OtaTimings> for HealthConfig {
    fn from(t: &OtaTimings) -> Self {
        Self {
            healthy_dwell: t.healthy_dwell,
            failure_deadline: t.failure_deadline,
            poll_interval: t.poll_interval,
            slot_retry: t.slot_retry,
            slot_min_attempts: t.slot_min_attempts,
            slot_warn_every: t.slot_warn_every,
            complete_attempts: t.complete_attempts,
            complete_retry: t.complete_retry,
            rollback_attempts: t.rollback_attempts,
            rollback_retry: t.rollback_retry,
            deadline_action: t.deadline_action,
        }
    }
}

/// What reading the running slot returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotReading {
    /// The running slot is pending verification.
    PendingVerify,
    /// The running slot is in any other state.
    NotPending,
    /// The OTA handle is held by another thread (the worker); says nothing about the slot.
    Busy,
    /// The read failed for another reason.
    Failed,
}

/// The outcome of the previous action, fed to [`HealthMachine::step`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthEvent {
    /// The first call.
    Begin,
    /// A [`HealthAction::Sleep`] elapsed.
    Woke,
    /// Result of [`HealthAction::ReadSlot`].
    Slot(SlotReading),
    /// Result of [`HealthAction::Evaluate`]: the app predicate.
    Healthy(bool),
    /// Result of [`HealthAction::MarkValid`]: `true` when it succeeded.
    Marked(bool),
    /// Result of [`HealthAction::CompleteAttempt`]: `true` when it succeeded.
    Completed(bool),
    /// [`HealthAction::NoteRollback`] was attempted (its outcome is only logged).
    Noted,
    /// [`HealthAction::Rollback`] returned: `true` for `Ok(())` (unexpected, the device normally reboots), `false` for a failure.
    RollbackReturned(bool),
    /// [`HealthAction::Restart`] returned (the app's callback came back).
    RestartReturned,
}

/// What the driver must do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthAction {
    /// Read the running slot once, without retrying a busy handle, and answer with [`HealthEvent::Slot`].
    ReadSlot,
    /// Sleep, then answer with [`HealthEvent::Woke`].
    Sleep(Duration),
    /// Ask the app predicate and answer with [`HealthEvent::Healthy`].
    Evaluate,
    /// Mark the running image valid and answer with [`HealthEvent::Marked`].
    MarkValid,
    /// Record the completed attempt for the running version and answer with [`HealthEvent::Completed`].
    CompleteAttempt,
    /// Note this rollback reason (the first reason wins) and answer with [`HealthEvent::Noted`].
    NoteRollback(RollbackReason),
    /// Roll back once and answer with [`HealthEvent::RollbackReturned`].
    Rollback,
    /// Call the app's restart callback and answer with [`HealthEvent::RestartReturned`].
    Restart,
    /// The policy is finished; apply the verdict.
    Finish(HealthVerdict),
}

/// How the policy ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthVerdict {
    /// The image was marked valid and the attempt was recorded; `applied` is published.
    Applied,
    /// The image was marked valid but the attempt could not be recorded within the bounded retries; `applied` is published anyway and the reporter thread keeps retrying the record in the background ([`CompletionRetry`](crate::ota::runtime::CompletionRetry)).
    ///
    /// Until the record is cleared every offer answers `attempt_unresolved`.
    ApplyNotRecorded,
    /// The running slot was not pending verification; nothing was touched.
    NothingToVerify,
    /// The deadline passed and the rollback reported success (unexpected: it normally reboots).
    DeadlineRolledBack,
    /// The deadline passed and every rollback attempt failed.
    DeadlineRollbackFailed,
    /// The deadline passed, the reason was noted and the restart callback returned.
    RestartReturned,
    /// The slot could not be read until the deadline; nothing was touched and updates stay refused.
    SlotUnreadable,
    /// The policy had already been started.
    AlreadyRunning,
}

impl HealthVerdict {
    /// `applied` is published for this verdict.
    ///
    /// Also for [`HealthVerdict::ApplyNotRecorded`]: the image IS valid, so the success is reported as soon as `mark_valid` succeeded, whether or not the attempt record could be cleared.
    pub const fn publishes_applied(self) -> bool {
        matches!(
            self,
            HealthVerdict::Applied | HealthVerdict::ApplyNotRecorded
        )
    }

    /// The reporter thread must keep retrying `complete_attempt` in the background after this verdict.
    pub const fn arms_completion_retry(self) -> bool {
        matches!(self, HealthVerdict::ApplyNotRecorded)
    }

    /// The library may admit updates after this verdict.
    ///
    /// Not after [`HealthVerdict::SlotUnreadable`]: the slot state is unknown, so an update could overwrite the only rollback target.
    pub const fn opens_admission(self) -> bool {
        matches!(
            self,
            HealthVerdict::Applied
                | HealthVerdict::ApplyNotRecorded
                | HealthVerdict::NothingToVerify
                | HealthVerdict::DeadlineRolledBack
                | HealthVerdict::DeadlineRollbackFailed
                | HealthVerdict::RestartReturned
        )
    }
}

/// Whether updates, rollbacks and repairs are admitted from the moment the runtime starts, before the health policy has read the slot.
///
/// True only when boot reconciliation released the slot (the hardware read succeeded and the running image is not pending verification), the image is not refused or about to be rolled back, and the records did not fail closed.
/// A pending-verify or unreadable slot stays closed: the health policy is the only opener there.
/// The same value is the initial "health settled" state, so the gates that key on it (early update block, repair) agree.
/// The health policy never touches a slot that is not pending and never closes admission, so this cannot conflict with it.
pub fn initial_admission(outcome: &BootOutcome) -> bool {
    outcome.slot_released
        && !outcome.refuse_mark_valid
        && !outcome.roll_back_now
        && !matches!(outcome.disposition, BootDisposition::FailedClosed(_))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    ReadSlot,
    Poll,
    Evaluating,
    Marking,
    Completing,
    CompleteWait,
    Noting { restart: bool },
    RollingBack,
    RollbackWait,
    Restarting,
    Done(HealthVerdict),
}

/// The health policy.
#[derive(Debug, Clone, Copy)]
pub struct HealthMachine {
    cfg: HealthConfig,
    phase: Phase,
    pending: HealthAction,
    failed_reads: u32,
    warn: Option<u32>,
    complete_tries: u32,
    rollback_tries: u32,
}

impl HealthMachine {
    /// A machine that starts by reading the slot.
    pub const fn new(cfg: HealthConfig) -> Self {
        Self {
            cfg,
            phase: Phase::ReadSlot,
            pending: HealthAction::ReadSlot,
            failed_reads: 0,
            warn: None,
            complete_tries: 0,
            rollback_tries: 0,
        }
    }

    /// The number of failed slot reads when the last step recorded one at the warning cadence, once.
    pub fn take_warning(&mut self) -> Option<u32> {
        self.warn.take()
    }

    /// Advances the policy.
    ///
    /// `elapsed` is the time since boot, `refuse` the library's refusal flag.
    /// An event that does not answer the pending action repeats that action, so a confused driver cannot skip a step.
    pub fn step(&mut self, elapsed: Duration, refuse: bool, event: HealthEvent) -> HealthAction {
        let action = self.advance(elapsed, refuse, event);
        self.pending = action;
        action
    }

    fn advance(&mut self, elapsed: Duration, refuse: bool, event: HealthEvent) -> HealthAction {
        match (self.phase, event) {
            (Phase::ReadSlot, HealthEvent::Begin | HealthEvent::Woke) => HealthAction::ReadSlot,
            (Phase::ReadSlot, HealthEvent::Slot(reading)) => self.on_slot(elapsed, refuse, reading),
            (Phase::Poll, HealthEvent::Woke) => self.poll_tick(elapsed, refuse),
            (Phase::Evaluating, HealthEvent::Healthy(true)) => {
                self.phase = Phase::Marking;
                HealthAction::MarkValid
            }
            (Phase::Evaluating, HealthEvent::Healthy(false)) => self.after_unhealthy(elapsed),
            (Phase::Marking, HealthEvent::Marked(true)) => {
                self.phase = Phase::Completing;
                HealthAction::CompleteAttempt
            }
            (Phase::Marking, HealthEvent::Marked(false)) => self.after_unhealthy(elapsed),
            (Phase::Completing, HealthEvent::Completed(true)) => {
                self.finish(HealthVerdict::Applied)
            }
            (Phase::Completing, HealthEvent::Completed(false)) => {
                self.complete_tries += 1;
                if self.complete_tries < self.cfg.complete_attempts {
                    self.phase = Phase::CompleteWait;
                    HealthAction::Sleep(self.cfg.complete_retry)
                } else {
                    self.finish(HealthVerdict::ApplyNotRecorded)
                }
            }
            (Phase::CompleteWait, HealthEvent::Woke) => {
                self.phase = Phase::Completing;
                HealthAction::CompleteAttempt
            }
            (Phase::Noting { restart }, HealthEvent::Noted) => {
                if restart {
                    self.phase = Phase::Restarting;
                    HealthAction::Restart
                } else {
                    self.phase = Phase::RollingBack;
                    HealthAction::Rollback
                }
            }
            (Phase::RollingBack, HealthEvent::RollbackReturned(true)) => {
                self.finish(HealthVerdict::DeadlineRolledBack)
            }
            (Phase::RollingBack, HealthEvent::RollbackReturned(false)) => {
                self.rollback_tries += 1;
                if self.rollback_tries < self.cfg.rollback_attempts {
                    self.phase = Phase::RollbackWait;
                    HealthAction::Sleep(self.cfg.rollback_retry)
                } else {
                    self.finish(HealthVerdict::DeadlineRollbackFailed)
                }
            }
            (Phase::RollbackWait, HealthEvent::Woke) => {
                self.phase = Phase::RollingBack;
                HealthAction::Rollback
            }
            (Phase::Restarting, HealthEvent::RestartReturned) => {
                self.finish(HealthVerdict::RestartReturned)
            }
            (Phase::Done(verdict), _) => HealthAction::Finish(verdict),
            _ => self.pending,
        }
    }

    fn finish(&mut self, verdict: HealthVerdict) -> HealthAction {
        self.phase = Phase::Done(verdict);
        HealthAction::Finish(verdict)
    }

    fn on_slot(&mut self, elapsed: Duration, refuse: bool, reading: SlotReading) -> HealthAction {
        match reading {
            SlotReading::NotPending => self.finish(HealthVerdict::NothingToVerify),
            SlotReading::PendingVerify => {
                self.phase = Phase::Poll;
                self.poll_tick(elapsed, refuse)
            }
            SlotReading::Busy => HealthAction::Sleep(self.cfg.slot_retry),
            SlotReading::Failed => {
                self.failed_reads += 1;
                if self.failed_reads % self.cfg.slot_warn_every.max(1)
                    == 1 % self.cfg.slot_warn_every.max(1)
                {
                    self.warn = Some(self.failed_reads);
                }
                if self.failed_reads >= self.cfg.slot_min_attempts
                    && elapsed >= self.cfg.failure_deadline
                {
                    self.finish(HealthVerdict::SlotUnreadable)
                } else {
                    HealthAction::Sleep(self.cfg.slot_retry)
                }
            }
        }
    }

    fn poll_tick(&mut self, elapsed: Duration, refuse: bool) -> HealthAction {
        if !refuse && elapsed >= self.cfg.healthy_dwell {
            self.phase = Phase::Evaluating;
            HealthAction::Evaluate
        } else {
            self.after_unhealthy(elapsed)
        }
    }

    fn after_unhealthy(&mut self, elapsed: Duration) -> HealthAction {
        if elapsed < self.cfg.failure_deadline {
            self.phase = Phase::Poll;
            return HealthAction::Sleep(self.cfg.poll_interval);
        }
        match self.cfg.deadline_action {
            DeadlineAction::Rollback => {
                self.phase = Phase::Noting { restart: false };
                HealthAction::NoteRollback(RollbackReason::HealthDeadline)
            }
            DeadlineAction::NoteAndRestart(reason) => {
                self.phase = Phase::Noting { restart: true };
                HealthAction::NoteRollback(reason)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use HealthAction as A;
    use HealthEvent as E;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn cfg() -> HealthConfig {
        HealthConfig::from(&OtaTimings::default())
    }

    fn started() -> HealthMachine {
        let mut m = HealthMachine::new(cfg());
        assert_eq!(m.step(Duration::ZERO, false, E::Begin), A::ReadSlot);
        m
    }

    fn pending_at(m: &mut HealthMachine, t: Duration) -> HealthAction {
        m.step(t, false, E::Slot(SlotReading::PendingVerify))
    }

    #[test]
    fn a_slot_that_is_not_pending_is_never_touched() {
        let mut m = started();
        let a = m.step(secs(1), false, E::Slot(SlotReading::NotPending));
        assert_eq!(a, A::Finish(HealthVerdict::NothingToVerify));
        assert!(HealthVerdict::NothingToVerify.opens_admission());
        assert_eq!(m.step(secs(500), false, E::Woke), a, "terminal is sticky");
    }

    #[test]
    fn busy_reads_never_count_toward_giving_up() {
        let mut m = started();
        for i in 0..200 {
            let a = m.step(secs(500 + i), false, E::Slot(SlotReading::Busy));
            assert_eq!(a, A::Sleep(Duration::from_millis(200)));
            assert_eq!(m.step(secs(500 + i), false, E::Woke), A::ReadSlot);
        }
        assert_eq!(m.take_warning(), None);
    }

    #[test]
    fn giving_up_needs_enough_failed_reads_and_the_deadline() {
        let mut m = started();
        for i in 1..=4 {
            let a = m.step(secs(200), false, E::Slot(SlotReading::Failed));
            assert!(matches!(a, A::Sleep(_)), "failure {i} is below the minimum");
            m.step(secs(200), false, E::Woke);
        }
        let a = m.step(secs(119), false, E::Slot(SlotReading::Failed));
        assert!(
            matches!(a, A::Sleep(_)),
            "five failures but before the deadline"
        );
        m.step(secs(119), false, E::Woke);
        let a = m.step(secs(120), false, E::Slot(SlotReading::Failed));
        assert_eq!(a, A::Finish(HealthVerdict::SlotUnreadable));
        assert!(!HealthVerdict::SlotUnreadable.opens_admission());
    }

    #[test]
    fn busy_reads_between_failures_do_not_help_giving_up() {
        let mut m = started();
        for _ in 0..4 {
            m.step(secs(300), false, E::Slot(SlotReading::Failed));
            m.step(secs(300), false, E::Woke);
            m.step(secs(300), false, E::Slot(SlotReading::Busy));
            m.step(secs(300), false, E::Woke);
        }
        let a = m.step(secs(300), false, E::Slot(SlotReading::Busy));
        assert!(matches!(a, A::Sleep(_)));
    }

    #[test]
    fn read_failures_warn_at_the_configured_cadence() {
        let mut m = started();
        let mut warned = alloc::vec::Vec::new();
        for _ in 0..30 {
            m.step(secs(1), false, E::Slot(SlotReading::Failed));
            if let Some(n) = m.take_warning() {
                warned.push(n);
            }
            m.step(secs(1), false, E::Woke);
        }
        assert_eq!(warned, [1, 26]);
    }

    #[test]
    fn the_dwell_boundary_is_inclusive() {
        let mut m = started();
        let a = pending_at(&mut m, Duration::from_millis(29_999));
        assert_eq!(a, A::Sleep(secs(1)));
        assert_eq!(m.step(secs(30), false, E::Woke), A::Evaluate);
    }

    #[test]
    fn a_refusal_blocks_the_predicate_until_the_deadline_rolls_back() {
        let mut m = started();
        assert_eq!(
            m.step(secs(60), true, E::Slot(SlotReading::PendingVerify)),
            A::Sleep(secs(1))
        );
        assert_eq!(m.step(secs(61), true, E::Woke), A::Sleep(secs(1)));
        assert_eq!(
            m.step(secs(120), true, E::Woke),
            A::NoteRollback(RollbackReason::HealthDeadline)
        );
    }

    #[test]
    fn healthy_applies_in_order() {
        let mut m = started();
        assert_eq!(pending_at(&mut m, secs(31)), A::Evaluate);
        assert_eq!(m.step(secs(31), false, E::Healthy(true)), A::MarkValid);
        assert_eq!(m.step(secs(31), false, E::Marked(true)), A::CompleteAttempt);
        let end = m.step(secs(31), false, E::Completed(true));
        assert_eq!(end, A::Finish(HealthVerdict::Applied));
        assert!(HealthVerdict::Applied.publishes_applied());
        assert!(HealthVerdict::Applied.opens_admission());
    }

    #[test]
    fn a_failing_mark_valid_keeps_polling_with_the_deadline_armed() {
        let mut m = started();
        assert_eq!(pending_at(&mut m, secs(31)), A::Evaluate);
        assert_eq!(m.step(secs(31), false, E::Healthy(true)), A::MarkValid);
        assert_eq!(m.step(secs(31), false, E::Marked(false)), A::Sleep(secs(1)));
        assert_eq!(m.step(secs(32), false, E::Woke), A::Evaluate);
        assert_eq!(m.step(secs(32), false, E::Healthy(true)), A::MarkValid);
        assert_eq!(
            m.step(secs(120), false, E::Marked(false)),
            A::NoteRollback(RollbackReason::HealthDeadline)
        );
    }

    #[test]
    fn recording_the_attempt_is_retried_then_given_up_without_applied() {
        let mut m = started();
        pending_at(&mut m, secs(31));
        m.step(secs(31), false, E::Healthy(true));
        m.step(secs(31), false, E::Marked(true));
        assert_eq!(
            m.step(secs(31), false, E::Completed(false)),
            A::Sleep(secs(1))
        );
        assert_eq!(m.step(secs(32), false, E::Woke), A::CompleteAttempt);
        assert_eq!(
            m.step(secs(32), false, E::Completed(false)),
            A::Sleep(secs(1))
        );
        assert_eq!(m.step(secs(33), false, E::Woke), A::CompleteAttempt);
        let end = m.step(secs(33), false, E::Completed(false));
        assert_eq!(end, A::Finish(HealthVerdict::ApplyNotRecorded));
        assert!(HealthVerdict::ApplyNotRecorded.publishes_applied());
        assert!(HealthVerdict::ApplyNotRecorded.arms_completion_retry());
        assert!(!HealthVerdict::Applied.arms_completion_retry());
        assert!(HealthVerdict::ApplyNotRecorded.opens_admission());
    }

    #[test]
    fn a_late_success_of_the_record_still_applies() {
        let mut m = started();
        pending_at(&mut m, secs(31));
        m.step(secs(31), false, E::Healthy(true));
        m.step(secs(31), false, E::Marked(true));
        m.step(secs(31), false, E::Completed(false));
        m.step(secs(32), false, E::Woke);
        assert_eq!(
            m.step(secs(32), false, E::Completed(true)),
            A::Finish(HealthVerdict::Applied)
        );
    }

    #[test]
    fn at_the_deadline_the_rollback_notes_then_tries_three_times() {
        let mut m = started();
        assert_eq!(pending_at(&mut m, secs(40)), A::Evaluate);
        assert_eq!(
            m.step(secs(40), false, E::Healthy(false)),
            A::Sleep(secs(1))
        );
        assert_eq!(m.step(secs(119), false, E::Woke), A::Evaluate);
        assert_eq!(
            m.step(secs(119), false, E::Healthy(false)),
            A::Sleep(secs(1))
        );
        assert_eq!(m.step(secs(120), false, E::Woke), A::Evaluate);
        assert_eq!(
            m.step(secs(120), false, E::Healthy(false)),
            A::NoteRollback(RollbackReason::HealthDeadline)
        );
        let mut rollbacks = 0;
        let mut action = m.step(secs(120), false, E::Noted);
        loop {
            match action {
                A::Rollback => {
                    rollbacks += 1;
                    action = m.step(secs(120), false, E::RollbackReturned(false));
                }
                A::Sleep(d) => {
                    assert_eq!(d, secs(5));
                    action = m.step(secs(125), false, E::Woke);
                }
                A::Finish(v) => {
                    assert_eq!(v, HealthVerdict::DeadlineRollbackFailed);
                    break;
                }
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(rollbacks, 3);
        assert!(HealthVerdict::DeadlineRollbackFailed.opens_admission());
    }

    #[test]
    fn a_rollback_that_reports_success_is_a_verdict() {
        let mut m = started();
        pending_at(&mut m, secs(120));
        m.step(secs(120), false, E::Healthy(false));
        let a = m.step(secs(120), false, E::Noted);
        assert_eq!(a, A::Rollback);
        assert_eq!(
            m.step(secs(120), false, E::RollbackReturned(true)),
            A::Finish(HealthVerdict::DeadlineRolledBack)
        );
    }

    #[test]
    fn the_healthy_check_wins_at_the_deadline_tick() {
        let mut m = started();
        assert_eq!(pending_at(&mut m, secs(120)), A::Evaluate);
        assert_eq!(m.step(secs(120), false, E::Healthy(true)), A::MarkValid);
    }

    #[test]
    fn the_unhealthy_demo_notes_and_restarts_after_fifteen_seconds() {
        let mut c = cfg();
        c.failure_deadline = secs(15);
        c.deadline_action = DeadlineAction::NoteAndRestart(RollbackReason::Unhealthy);
        let mut m = HealthMachine::new(c);
        assert_eq!(m.step(Duration::ZERO, false, E::Begin), A::ReadSlot);
        assert_eq!(pending_at(&mut m, secs(2)), A::Sleep(secs(1)));
        for t in 3..15 {
            assert_eq!(m.step(secs(t), false, E::Woke), A::Sleep(secs(1)), "t={t}");
        }
        assert_eq!(
            m.step(secs(15), false, E::Woke),
            A::NoteRollback(RollbackReason::Unhealthy)
        );
        assert_eq!(m.step(secs(15), false, E::Noted), A::Restart);
        let end = m.step(secs(15), false, E::RestartReturned);
        assert_eq!(end, A::Finish(HealthVerdict::RestartReturned));
        assert!(HealthVerdict::RestartReturned.opens_admission());
    }

    #[test]
    fn an_unexpected_event_repeats_the_pending_action() {
        let mut m = started();
        assert_eq!(m.step(secs(1), false, E::Marked(true)), A::ReadSlot);
        assert_eq!(m.step(secs(1), false, E::RestartReturned), A::ReadSlot);
        pending_at(&mut m, secs(31));
        assert_eq!(m.step(secs(31), false, E::Completed(true)), A::Evaluate);
    }

    #[test]
    fn only_a_pending_slot_is_ever_marked_or_rolled_back() {
        let touching = |a: &A| {
            matches!(
                a,
                A::Evaluate
                    | A::MarkValid
                    | A::CompleteAttempt
                    | A::NoteRollback(_)
                    | A::Rollback
                    | A::Restart
            )
        };
        let events = [
            E::Begin,
            E::Woke,
            E::Slot(SlotReading::NotPending),
            E::Slot(SlotReading::Busy),
            E::Slot(SlotReading::Failed),
            E::Healthy(true),
            E::Healthy(false),
            E::Marked(true),
            E::Marked(false),
            E::Completed(true),
            E::Completed(false),
            E::Noted,
            E::RollbackReturned(false),
            E::RestartReturned,
        ];
        let mut seed = 0x9E37_79B9_u32;
        for _ in 0..300 {
            let mut m = HealthMachine::new(cfg());
            let mut t = 0u64;
            for _ in 0..60 {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                let ev = events[(seed >> 16) as usize % events.len()];
                t += u64::from((seed >> 8) % 40);
                let a = m.step(secs(t), seed & 1 == 0, ev);
                assert!(
                    !touching(&a),
                    "touched a slot that was never pending: {ev:?} -> {a:?}"
                );
            }
        }
    }

    #[test]
    fn every_terminal_verdict_is_sticky_and_classified() {
        for v in [
            HealthVerdict::Applied,
            HealthVerdict::ApplyNotRecorded,
            HealthVerdict::NothingToVerify,
            HealthVerdict::DeadlineRolledBack,
            HealthVerdict::DeadlineRollbackFailed,
            HealthVerdict::RestartReturned,
            HealthVerdict::SlotUnreadable,
            HealthVerdict::AlreadyRunning,
        ] {
            let opens = !matches!(
                v,
                HealthVerdict::SlotUnreadable | HealthVerdict::AlreadyRunning
            );
            assert_eq!(v.opens_admission(), opens, "{v:?}");
            assert_eq!(
                v.publishes_applied(),
                matches!(v, HealthVerdict::Applied | HealthVerdict::ApplyNotRecorded)
            );
            assert_eq!(
                v.arms_completion_retry(),
                v == HealthVerdict::ApplyNotRecorded
            );
        }
    }

    fn outcome(disposition: BootDisposition, released: bool) -> BootOutcome {
        BootOutcome {
            disposition,
            action: None,
            slot_released: released,
            admission_open_at_boot: false,
            refuse_mark_valid: false,
            roll_back_now: false,
            report_pending: false,
            warnings: Default::default(),
        }
    }

    #[test]
    fn a_valid_boot_admits_from_the_start() {
        use crate::ota::SettleOutcome;
        for d in [
            BootDisposition::Settled(SettleOutcome::NoRequest),
            BootDisposition::ReportedRollback,
            BootDisposition::Deferred,
        ] {
            assert!(initial_admission(&outcome(d.clone(), true)), "{d:?}");
        }
    }

    #[test]
    fn a_pending_or_unreadable_slot_stays_closed_until_the_policy_opens_it() {
        assert!(!initial_admission(&outcome(
            BootDisposition::AwaitHealthCheck,
            false
        )));
        assert!(!initial_admission(&outcome(
            BootDisposition::Deferred,
            false
        )));
    }

    #[test]
    fn a_failed_closed_boot_stays_closed_even_with_a_released_slot() {
        use crate::ota::{BootFault, StoreError};
        let o = outcome(
            BootDisposition::FailedClosed(BootFault::Store(StoreError::ReportPending)),
            true,
        );
        assert!(!initial_admission(&o));
    }

    #[test]
    fn a_refused_or_rolling_back_image_stays_closed() {
        let mut o = outcome(BootDisposition::Deferred, true);
        o.refuse_mark_valid = true;
        assert!(!initial_admission(&o));
        let mut o = outcome(BootDisposition::RollBackNow, true);
        o.roll_back_now = true;
        assert!(!initial_admission(&o));
    }

    #[test]
    fn the_config_follows_the_timings() {
        let t = OtaTimings {
            healthy_dwell: secs(5),
            rollback_attempts: 7,
            ..OtaTimings::default()
        };
        let c = HealthConfig::from(&t);
        assert_eq!((c.healthy_dwell, c.rollback_attempts), (secs(5), 7));
    }
}
