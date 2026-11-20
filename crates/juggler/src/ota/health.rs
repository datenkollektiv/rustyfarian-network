//! The health policy of the running image as pure decisions, and the record upkeep that follows it (completion retry, repair), shared by the MQTT runtime (`ota-wire`) and the DDI runtime (`ota-ddi`).
//!
//! The policy decides when a freshly activated image proves itself healthy, and what happens when it does not.
//! It never touches a slot that is not pending verification, so a normally booted image cannot restart itself because the broker happens to be down.
//!
//! One tier runs it, on one thread (the app's main thread), so it is flat logic (ADR 023 decision 3): the driver reads the slot, sleeps, asks the app predicate, marks valid, records the attempt, rolls back or restarts in plain sequential code, and asks these functions only for the decisions.
//! [`SlotReads::busy`] and [`SlotReads::failed`] decide whether an unreadable slot is retried or ends the policy, [`poll_decision`] and [`after_unhealthy`] decide whether to evaluate, wait or give up at the deadline, and [`deadline_plan`] says what the deadline does.
//! Elapsed time is measured from the boot instant the app captured at the very start of `main`, not from Wi-Fi association.
//!
//! [`HealthMachine`] is the earlier action machine over the same decisions; it is deprecated and kept for one release.

#![allow(deprecated)]

use core::time::Duration;

use crate::ota::persist::{
    reconcile_boot, BootDisposition, BootOutcome, HardwareFacts, HardwareReadError, OtaKv,
    OtaStore, StoreError,
};
use crate::ota::{FailReason, RollbackReason, SlotId, Version};

/// The timings of the health policy; the MQTT runtime builds it from its `OtaTimings` (`ota-wire`), the DDI runtime carries it in its settings.
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
    /// Slot reads before the deadline may end verification: failed reads for a failed read, failed and busy reads together for a busy one.
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

impl Default for HealthConfig {
    /// The documented defaults: 30 s dwell, 120 s deadline, 1 s poll, 200 ms slot retry, 5 slot reads, a warning every 25 failed reads, 3 completion attempts 1 s apart, 3 rollback attempts 5 s apart, [`DeadlineAction::Rollback`].
    fn default() -> Self {
        Self {
            healthy_dwell: Duration::from_secs(30),
            failure_deadline: Duration::from_secs(120),
            poll_interval: Duration::from_secs(1),
            slot_retry: Duration::from_millis(200),
            slot_min_attempts: 5,
            slot_warn_every: 25,
            complete_attempts: 3,
            complete_retry: Duration::from_secs(1),
            rollback_attempts: 3,
            rollback_retry: Duration::from_secs(5),
            deadline_action: DeadlineAction::Rollback,
        }
    }
}

/// What reading the running slot returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotReading {
    /// The running slot, which is pending verification.
    PendingVerify(SlotId),
    /// The running slot is in any other state.
    NotPending,
    /// The OTA handle is held by another thread (the worker); says nothing about the slot.
    ///
    /// Busy reads are retried, but once the failure deadline has passed and `slot_min_attempts` reads (busy or failed) were made, a busy read ends the policy with [`HealthVerdict::SlotUnreadable`] too.
    Busy,
    /// The read failed for another reason.
    Failed,
}

/// The outcome of the previous action, fed to [`HealthMachine::step`].
///
/// Deprecated with [`HealthMachine`].
#[deprecated(
    since = "0.7.0",
    note = "flat logic since ADR 023: drive the policy in sequential code with `poll_decision`, `after_unhealthy`, `SlotReads` and `deadline_plan`"
)]
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
///
/// Deprecated with [`HealthMachine`].
#[deprecated(
    since = "0.7.0",
    note = "flat logic since ADR 023: drive the policy in sequential code with `poll_decision`, `after_unhealthy`, `SlotReads` and `deadline_plan`"
)]
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
    /// Note this rollback reason for the running slot read at start (the first reason wins) and answer with [`HealthEvent::Noted`].
    NoteRollback(RollbackReason, SlotId),
    /// Roll back once and answer with [`HealthEvent::RollbackReturned`].
    Rollback,
    /// Call the app's restart callback and answer with [`HealthEvent::RestartReturned`].
    Restart,
    /// The policy is finished; apply the verdict.
    Finish(HealthVerdict),
    /// The event did not answer the pending action; nothing changed (ADR 023 decision 5).
    ///
    /// A driver that maps every action to its answer never sees this, so it treats it as an internal error.
    Ignored,
}

/// How the policy ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthVerdict {
    /// The image was marked valid and the attempt was recorded; `applied` is published.
    Applied,
    /// The image was marked valid but the attempt could not be recorded within the bounded retries; `applied` is published anyway and the runtime keeps retrying the record in the background (the MQTT runtime on its reporter thread, the DDI runtime on its worker); see [`retry_completion`].
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

/// What the poll loop does next while the image is pending verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollStep {
    /// Dwell passed and nothing refuses: ask the app predicate.
    Evaluate,
    /// Not yet, and the deadline is not reached: sleep this long, then decide again.
    Wait(Duration),
    /// The failure deadline is reached: apply [`deadline_plan`].
    Deadline,
}

/// The next step of the poll loop at `elapsed` since boot, with `refuse` the library's refusal flag.
///
/// The image is evaluated only once the healthy dwell has passed and nothing refuses it; otherwise the loop waits for `poll_interval`, until the failure deadline.
pub fn poll_decision(cfg: &HealthConfig, elapsed: Duration, refuse: bool) -> PollStep {
    if !refuse && elapsed >= cfg.healthy_dwell {
        PollStep::Evaluate
    } else {
        after_unhealthy(cfg, elapsed)
    }
}

/// The next step after the predicate said no or `mark_valid` failed: wait, or the deadline.
///
/// Never returns [`PollStep::Evaluate`].
pub fn after_unhealthy(cfg: &HealthConfig, elapsed: Duration) -> PollStep {
    if elapsed < cfg.failure_deadline {
        PollStep::Wait(cfg.poll_interval)
    } else {
        PollStep::Deadline
    }
}

/// What to do after a slot read that did not say whether the slot is pending.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotRetry {
    /// Read again after `pause`; `warn` is `Some(failed reads)` when this failure falls on the warning cadence.
    Sleep {
        /// How long to wait before the next read.
        pause: Duration,
        /// Log a warning with this many failed reads.
        warn: Option<u32>,
    },
    /// Give up: end the policy with [`HealthVerdict::SlotUnreadable`], touching nothing.
    GiveUp,
}

/// The unreadable-slot reads of the start-up loop.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SlotReads {
    failed: u32,
    busy: u32,
}

impl SlotReads {
    /// A busy read (the OTA handle is held by another thread): says nothing about the slot.
    ///
    /// Gives up once the failure deadline has passed and `slot_min_attempts` reads (busy or failed) were made.
    pub fn busy(&mut self, cfg: &HealthConfig, elapsed: Duration) -> SlotRetry {
        self.busy = self.busy.saturating_add(1);
        let reads = self.busy.saturating_add(self.failed);
        if reads >= cfg.slot_min_attempts && elapsed >= cfg.failure_deadline {
            SlotRetry::GiveUp
        } else {
            SlotRetry::Sleep {
                pause: cfg.slot_retry,
                warn: None,
            }
        }
    }

    /// A failed read.
    ///
    /// Gives up once the failure deadline has passed and `slot_min_attempts` failed reads were made; busy reads do not count here.
    pub fn failed(&mut self, cfg: &HealthConfig, elapsed: Duration) -> SlotRetry {
        self.failed = self.failed.saturating_add(1);
        if self.failed >= cfg.slot_min_attempts && elapsed >= cfg.failure_deadline {
            return SlotRetry::GiveUp;
        }
        let every = cfg.slot_warn_every.max(1);
        SlotRetry::Sleep {
            pause: cfg.slot_retry,
            warn: (self.failed % every == 1 % every).then_some(self.failed),
        }
    }
}

/// What the failure deadline does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeadlinePlan {
    /// The reason to note for the running slot (the first reason of a boot wins).
    pub reason: RollbackReason,
    /// `true` to call the app's restart callback after noting, `false` to roll back with bounded retries.
    pub restart: bool,
}

/// The deadline behaviour configured in `cfg`.
pub fn deadline_plan(cfg: &HealthConfig) -> DeadlinePlan {
    match cfg.deadline_action {
        DeadlineAction::Rollback => DeadlinePlan {
            reason: RollbackReason::HealthDeadline,
            restart: false,
        },
        DeadlineAction::NoteAndRestart(reason) => DeadlinePlan {
            reason,
            restart: true,
        },
    }
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

/// The health policy as an action machine.
///
/// Deprecated: the policy runs on one thread of one tier, so the decisions are pure functions and the driver is sequential (ADR 023 decisions 2 and 3).
#[deprecated(
    since = "0.7.0",
    note = "flat logic since ADR 023: drive the policy in sequential code with `poll_decision`, `after_unhealthy`, `SlotReads` and `deadline_plan`"
)]
#[derive(Debug, Clone, Copy)]
pub struct HealthMachine {
    cfg: HealthConfig,
    phase: Phase,
    slot: Option<SlotId>,
    reads: SlotReads,
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
            slot: None,
            reads: SlotReads { failed: 0, busy: 0 },
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
    /// An event that does not answer the pending action returns [`HealthAction::Ignored`] and changes nothing, so a confused driver cannot skip a step; the pending action is still answered by its own event.
    /// A finished policy answers every event with its [`HealthAction::Finish`].
    pub fn step(&mut self, elapsed: Duration, refuse: bool, event: HealthEvent) -> HealthAction {
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
            _ => HealthAction::Ignored,
        }
    }

    fn finish(&mut self, verdict: HealthVerdict) -> HealthAction {
        self.phase = Phase::Done(verdict);
        HealthAction::Finish(verdict)
    }

    fn on_slot(&mut self, elapsed: Duration, refuse: bool, reading: SlotReading) -> HealthAction {
        let retry = match reading {
            SlotReading::NotPending => return self.finish(HealthVerdict::NothingToVerify),
            SlotReading::PendingVerify(slot) => {
                self.slot = Some(slot);
                self.phase = Phase::Poll;
                return self.poll_tick(elapsed, refuse);
            }
            SlotReading::Busy => self.reads.busy(&self.cfg, elapsed),
            SlotReading::Failed => self.reads.failed(&self.cfg, elapsed),
        };
        match retry {
            SlotRetry::Sleep { pause, warn } => {
                if warn.is_some() {
                    self.warn = warn;
                }
                HealthAction::Sleep(pause)
            }
            SlotRetry::GiveUp => self.finish(HealthVerdict::SlotUnreadable),
        }
    }

    fn poll_tick(&mut self, elapsed: Duration, refuse: bool) -> HealthAction {
        let step = poll_decision(&self.cfg, elapsed, refuse);
        self.apply(step)
    }

    fn after_unhealthy(&mut self, elapsed: Duration) -> HealthAction {
        let step = after_unhealthy(&self.cfg, elapsed);
        self.apply(step)
    }

    fn apply(&mut self, step: PollStep) -> HealthAction {
        match step {
            PollStep::Evaluate => {
                self.phase = Phase::Evaluating;
                HealthAction::Evaluate
            }
            PollStep::Wait(pause) => {
                self.phase = Phase::Poll;
                HealthAction::Sleep(pause)
            }
            PollStep::Deadline => {
                // Only a pending slot reaches the deadline, and it was read at start.
                let Some(slot) = self.slot else {
                    return self.finish(HealthVerdict::SlotUnreadable);
                };
                let plan = deadline_plan(&self.cfg);
                self.phase = Phase::Noting {
                    restart: plan.restart,
                };
                HealthAction::NoteRollback(plan.reason, slot)
            }
        }
    }
}

/// What the health policy does when the failure deadline passes with the image still pending verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeadlineAction {
    /// Note `health_deadline`, then roll back with bounded retries (the production behaviour).
    Rollback,
    /// Note the given reason, then call the app's restart callback and let the bootloader abort the unverified image.
    ///
    /// The unhealthy demo build uses this with [`RollbackReason::Unhealthy`].
    NoteAndRestart(RollbackReason),
}

/// The library flags at runtime start, derived from boot reconciliation.
///
/// The tier copies them into its flags and performs the rollback when `roll_back_now_at_boot` is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeFlags {
    /// Updates, rollbacks and repairs are admitted from the start (see [`initial_admission`]).
    pub admission_open: bool,
    /// The health policy counts as settled from the start; the same value as `admission_open`.
    pub health_settled: bool,
    /// The running image must not be marked valid.
    pub refuse_mark_valid: bool,
    /// The boot demanded a rollback now.
    pub roll_back_now_at_boot: bool,
    /// The boot ended fail-closed on a store fault (see [`BootOutcome::records_suspect`]).
    pub records_suspect: bool,
}

impl RuntimeFlags {
    /// Derives the flags from the boot reconciliation outcome.
    pub fn from_boot(outcome: &BootOutcome) -> Self {
        let admit = initial_admission(outcome);
        Self {
            admission_open: admit,
            health_settled: admit,
            refuse_mark_valid: outcome.refuse_mark_valid,
            roll_back_now_at_boot: outcome.roll_back_now,
            records_suspect: outcome.records_suspect(),
        }
    }
}

/// The library flags after a repair, and what the re-run of boot reconciliation said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefreshedFlags {
    /// The re-run still ends fail-closed on a store fault.
    pub records_suspect: bool,
    /// The running image must still not be marked valid.
    pub refuse_mark_valid: bool,
    /// The full outcome, for logging; a `roll_back_now` here is only logged, never acted on.
    pub outcome: BootOutcome,
}

/// Re-runs boot reconciliation after a repair and derives the flags.
///
/// Reconciliation is built to be repeated after any crash, so a second run with unchanged facts changes no record.
/// `refuse_mark_valid` stays set when the boot demanded a rollback (`roll_back_now_at_boot`: a version mismatch); otherwise it follows the new outcome, which clears a refusal that only came from the unreadable records.
/// Returns `None` when the hardware facts could not be read: nothing is re-run and the flags stay as they are.
pub fn refresh_after_repair<K: OtaKv>(
    store: &mut OtaStore<K>,
    hw: Result<HardwareFacts, HardwareReadError>,
    running_version: Option<Version>,
    roll_back_now_at_boot: bool,
) -> Option<RefreshedFlags> {
    let hw = hw.ok()?;
    let outcome = reconcile_boot(store, Ok(hw), running_version);
    Some(RefreshedFlags {
        records_suspect: outcome.records_suspect(),
        refuse_mark_valid: roll_back_now_at_boot || outcome.refuse_mark_valid,
        outcome,
    })
}

/// The result of one background attempt to record the completed attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionTry {
    /// The attempt record was cleared; admission reopens by itself.
    Completed,
    /// No attempt for the running version remains (cleared by a repair, or it belongs to another version); stop retrying.
    Gone,
    /// The attempt is still present and could not be cleared (or read); retry later.
    StillFailing,
}

/// One background retry of `complete_attempt(running)` after `ApplyNotRecorded`, inside ONE store lock.
///
/// The caller guarantees the running slot is known valid (`mark_valid` succeeded) and holds no lock across the backoff sleeps.
/// It only completes an attempt for `running`: an attempt of another version, or one without a readable id, is left to boot reconciliation and ends the retries.
pub fn retry_completion<K: OtaKv>(store: &mut OtaStore<K>, running: Version) -> CompletionTry {
    match store.read_attempt() {
        Ok(None) => CompletionTry::Gone,
        Ok(Some(stored)) if stored.record.version == Some(running) => {
            match store.complete_attempt(running) {
                Ok(()) => CompletionTry::Completed,
                Err(_) => CompletionTry::StillFailing,
            }
        }
        Ok(Some(_)) => CompletionTry::Gone,
        Err(_) => CompletionTry::StillFailing,
    }
}

/// Why [`repair_all`] did not finish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairError {
    /// The cap was reached and unreadable records remain.
    Capped,
    /// A transient storage error.
    Store(StoreError),
}

impl RepairError {
    /// Both causes are reported as `repair_failed`.
    pub const fn reason(&self) -> FailReason {
        FailReason::RepairFailed
    }
}

/// Repairs record groups until none is left, at most `max_iterations` of them.
///
/// Returns how many groups were repaired (zero means nothing was unreadable).
/// Reaching the cap with records still unreadable is [`RepairError::Capped`], never a success with the cap as the count.
///
/// # Errors
///
/// [`RepairError::Store`] on a transient failure (records repaired so far stay repaired), [`RepairError::Capped`] at the cap.
pub fn repair_all<K: OtaKv>(
    store: &mut OtaStore<K>,
    max_iterations: u32,
) -> Result<u32, RepairError> {
    let mut repaired = 0;
    while repaired < max_iterations {
        match store.repair_corrupt() {
            Ok(Some(_)) => repaired += 1,
            Ok(None) => return Ok(repaired),
            Err(e) => return Err(RepairError::Store(e)),
        }
    }
    match store.corrupt_present() {
        Ok(false) => Ok(repaired),
        Ok(true) => Err(RepairError::Capped),
        Err(e) => Err(RepairError::Store(e)),
    }
}

/// Experimental: API may change before 1.0.
///
/// Whether the boot repair leaves a report owed: the next offered DDI action goes out `closed` + `failure` + 1005 `records_unreadable` instead of being applied.
///
/// A transient read error also fails the boot reconciliation closed on a store fault, yet nothing is corrupt and [`repair_all`] finds nothing; reporting it would fail a valid action for one flaky read.
/// A report is owed when the repair dropped at least one group (`Ok` with a count above zero), could not finish (`Err`), or the re-run of the reconciliation still ends on a store fault (`still_suspect`).
/// `still_suspect` is the flag after that re-run; when the hardware facts could not be read the re-run is skipped and the flag is the boot's, so an unconfirmed fault stays owed.
pub const fn repair_report_owed(repaired: Result<u32, RepairError>, still_suspect: bool) -> bool {
    match repaired {
        Ok(0) => still_suspect,
        Ok(_) | Err(_) => true,
    }
}

/// What to do after a background completion attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionStep {
    /// The attempt still failed: the retry stays armed with a longer delay.
    Retry,
    /// The attempt record is cleared or gone: the retry is disarmed.
    Stop,
    /// The retry was not armed, so no attempt was due; the machine is unchanged.
    Ignored,
}

/// The backoff of the background completion retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompletionRetryConfig {
    /// Delay before the first background attempt, and the base of the backoff.
    pub initial: Duration,
    /// The backoff doubles per failed attempt up to this delay.
    pub cap: Duration,
}

/// The schedule of the background retry of `complete_attempt` after the health policy ended with `ApplyNotRecorded`.
///
/// The image is already valid and `applied` already published; the retry only keeps trying to clear the attempt record so admission reopens without a reset.
/// The reporter thread owns the loop (no sleep happens under the store lock) and keeps an `Option<CompletionBackoff>`: `None` while not armed.
/// Time is `Duration` since the reporter started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompletionBackoff {
    next_due: Duration,
    delay: Duration,
}

impl CompletionBackoff {
    /// Arms the retry; the first attempt is due after `initial`.
    pub fn armed(cfg: &CompletionRetryConfig, now: Duration) -> Self {
        Self {
            next_due: now + cfg.initial,
            delay: cfg.initial,
        }
    }

    /// How long the loop may wait before the next attempt.
    pub fn wait(&self, now: Duration) -> Duration {
        self.next_due.saturating_sub(now)
    }

    /// An attempt is due.
    pub fn due(&self, now: Duration) -> bool {
        now >= self.next_due
    }

    /// An attempt failed: the delay doubles up to the cap.
    #[must_use]
    pub fn failed(self, cfg: &CompletionRetryConfig, now: Duration) -> Self {
        let delay = self.delay.saturating_mul(2).min(cfg.cap);
        Self {
            next_due: now + delay,
            delay,
        }
    }
}

/// What to do after a background completion attempt: success or a vanished attempt stops the retry ([`CompletionStep::Stop`]), a failure keeps it armed ([`CompletionStep::Retry`]).
pub fn completion_step(result: CompletionTry) -> CompletionStep {
    match result {
        CompletionTry::Completed | CompletionTry::Gone => CompletionStep::Stop,
        CompletionTry::StillFailing => CompletionStep::Retry,
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
        HealthConfig::default()
    }

    fn at_boot_plus(t: u64) -> Duration {
        secs(t)
    }

    #[test]
    fn poll_decision_evaluates_only_after_the_dwell_and_without_a_refusal() {
        let c = HealthConfig {
            healthy_dwell: secs(30),
            failure_deadline: secs(120),
            poll_interval: secs(1),
            ..cfg()
        };
        assert_eq!(
            poll_decision(&c, at_boot_plus(29), false),
            PollStep::Wait(secs(1))
        );
        assert_eq!(
            poll_decision(&c, at_boot_plus(30), false),
            PollStep::Evaluate
        );
        assert_eq!(
            poll_decision(&c, at_boot_plus(60), true),
            PollStep::Wait(secs(1)),
            "a refusal keeps the image unevaluated"
        );
        assert_eq!(
            poll_decision(&c, at_boot_plus(120), true),
            PollStep::Deadline
        );
    }

    #[test]
    fn after_unhealthy_waits_until_the_deadline_and_never_evaluates() {
        let c = HealthConfig {
            failure_deadline: secs(120),
            poll_interval: secs(2),
            ..cfg()
        };
        assert_eq!(
            after_unhealthy(&c, at_boot_plus(119)),
            PollStep::Wait(secs(2))
        );
        assert_eq!(after_unhealthy(&c, at_boot_plus(120)), PollStep::Deadline);
        assert_eq!(after_unhealthy(&c, at_boot_plus(900)), PollStep::Deadline);
    }

    #[test]
    fn unreadable_slot_gives_up_only_after_the_deadline_and_enough_reads() {
        let c = HealthConfig {
            failure_deadline: secs(120),
            slot_min_attempts: 3,
            slot_retry: Duration::from_millis(200),
            ..cfg()
        };
        let mut reads = SlotReads::default();
        let sleep = |warn| SlotRetry::Sleep {
            pause: Duration::from_millis(200),
            warn,
        };
        assert_eq!(
            reads.busy(&c, secs(500)),
            sleep(None),
            "one read is not enough"
        );
        assert_eq!(reads.busy(&c, secs(500)), sleep(None));
        assert_eq!(
            reads.busy(&c, secs(119)),
            sleep(None),
            "deadline not reached"
        );
        assert_eq!(reads.busy(&c, secs(120)), SlotRetry::GiveUp);
    }

    #[test]
    fn busy_reads_count_toward_a_failed_give_up_but_not_the_reverse() {
        let c = HealthConfig {
            failure_deadline: secs(10),
            slot_min_attempts: 3,
            ..cfg()
        };
        let sleeps = |r: SlotRetry| matches!(r, SlotRetry::Sleep { .. });

        let mut reads = SlotReads::default();
        assert!(sleeps(reads.failed(&c, secs(100))));
        assert!(sleeps(reads.busy(&c, secs(100))));
        assert_eq!(
            reads.busy(&c, secs(100)),
            SlotRetry::GiveUp,
            "a busy read counts the failed reads too"
        );

        let mut reads = SlotReads::default();
        assert!(sleeps(reads.busy(&c, secs(100))));
        assert!(sleeps(reads.busy(&c, secs(100))));
        assert!(
            sleeps(reads.failed(&c, secs(100))),
            "a failed read does not count the busy ones"
        );
        assert!(sleeps(reads.failed(&c, secs(100))));
        assert_eq!(reads.failed(&c, secs(100)), SlotRetry::GiveUp);
    }

    #[test]
    fn failed_reads_warn_on_the_first_and_every_nth() {
        let c = HealthConfig {
            slot_warn_every: 3,
            slot_min_attempts: 1_000,
            ..cfg()
        };
        let mut reads = SlotReads::default();
        let mut warns = [None; 7];
        for slot in &mut warns {
            *slot = match reads.failed(&c, secs(0)) {
                SlotRetry::Sleep { warn, .. } => warn,
                SlotRetry::GiveUp => panic!("must not give up"),
            };
        }
        assert_eq!(warns, [Some(1), None, None, Some(4), None, None, Some(7)]);
    }

    #[test]
    fn deadline_plan_follows_the_configured_action() {
        let rollback = HealthConfig {
            deadline_action: DeadlineAction::Rollback,
            ..cfg()
        };
        assert_eq!(
            deadline_plan(&rollback),
            DeadlinePlan {
                reason: RollbackReason::HealthDeadline,
                restart: false
            }
        );
        let restart = HealthConfig {
            deadline_action: DeadlineAction::NoteAndRestart(RollbackReason::Unhealthy),
            ..cfg()
        };
        assert_eq!(
            deadline_plan(&restart),
            DeadlinePlan {
                reason: RollbackReason::Unhealthy,
                restart: true
            }
        );
    }

    fn started() -> HealthMachine {
        let mut m = HealthMachine::new(cfg());
        assert_eq!(m.step(Duration::ZERO, false, E::Begin), A::ReadSlot);
        m
    }

    const SLOT: SlotId = SlotId(1);

    fn pending_at(m: &mut HealthMachine, t: Duration) -> HealthAction {
        m.step(t, false, E::Slot(SlotReading::PendingVerify(SLOT)))
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
    fn busy_before_deadline_sleeps() {
        let mut m = started();
        for i in 0..200 {
            let a = m.step(secs(i % 119), false, E::Slot(SlotReading::Busy));
            assert_eq!(a, A::Sleep(Duration::from_millis(200)));
            assert_eq!(m.step(secs(i % 119), false, E::Woke), A::ReadSlot);
        }
        assert_eq!(m.take_warning(), None);
    }

    #[test]
    fn permanent_busy_reaches_slot_unreadable_after_deadline() {
        let mut m = started();
        for i in 1..=4 {
            let a = m.step(secs(500), false, E::Slot(SlotReading::Busy));
            assert!(
                matches!(a, A::Sleep(_)),
                "busy read {i} is below the minimum"
            );
            m.step(secs(500), false, E::Woke);
        }
        let a = m.step(secs(500), false, E::Slot(SlotReading::Busy));
        assert_eq!(a, A::Finish(HealthVerdict::SlotUnreadable));
        assert!(!HealthVerdict::SlotUnreadable.opens_admission());
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
    fn busy_and_failed_reads_count_together_only_for_a_busy_read() {
        let mut m = started();
        for _ in 0..2 {
            m.step(secs(300), false, E::Slot(SlotReading::Failed));
            m.step(secs(300), false, E::Woke);
            m.step(secs(300), false, E::Slot(SlotReading::Busy));
            m.step(secs(300), false, E::Woke);
        }
        let a = m.step(secs(300), false, E::Slot(SlotReading::Failed));
        assert!(
            matches!(a, A::Sleep(_)),
            "three failed reads are not enough"
        );
        m.step(secs(300), false, E::Woke);
        let a = m.step(secs(300), false, E::Slot(SlotReading::Busy));
        assert_eq!(a, A::Finish(HealthVerdict::SlotUnreadable));
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
            m.step(secs(60), true, E::Slot(SlotReading::PendingVerify(SLOT))),
            A::Sleep(secs(1))
        );
        assert_eq!(m.step(secs(61), true, E::Woke), A::Sleep(secs(1)));
        assert_eq!(
            m.step(secs(120), true, E::Woke),
            A::NoteRollback(RollbackReason::HealthDeadline, SLOT)
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
            A::NoteRollback(RollbackReason::HealthDeadline, SLOT)
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
            A::NoteRollback(RollbackReason::HealthDeadline, SLOT)
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
            A::NoteRollback(RollbackReason::Unhealthy, SLOT)
        );
        assert_eq!(m.step(secs(15), false, E::Noted), A::Restart);
        let end = m.step(secs(15), false, E::RestartReturned);
        assert_eq!(end, A::Finish(HealthVerdict::RestartReturned));
        assert!(HealthVerdict::RestartReturned.opens_admission());
    }

    #[test]
    fn out_of_phase_event_is_ignored_and_pending_kept() {
        let mut m = started();
        assert_eq!(m.step(secs(1), false, E::Marked(true)), A::Ignored);
        assert_eq!(m.step(secs(1), false, E::RestartReturned), A::Ignored);
        assert_eq!(pending_at(&mut m, secs(31)), A::Evaluate);
        assert_eq!(m.step(secs(31), false, E::Completed(true)), A::Ignored);
        assert_eq!(
            m.step(secs(31), false, E::RollbackReturned(true)),
            A::Ignored
        );
        assert_eq!(m.step(secs(31), false, E::Woke), A::Ignored);
        assert_eq!(
            m.step(secs(31), false, E::Healthy(true)),
            A::MarkValid,
            "the pending Evaluate is still answered by its own event"
        );

        let mut m = started();
        pending_at(&mut m, secs(120));
        m.step(secs(120), false, E::Healthy(false));
        assert_eq!(m.step(secs(120), false, E::Noted), A::Rollback);
        assert_eq!(
            m.step(secs(120), false, E::Noted),
            A::Ignored,
            "a stray event never repeats the rollback"
        );
        assert_eq!(m.step(secs(120), false, E::Woke), A::Ignored);
        assert_eq!(
            m.step(secs(120), false, E::RollbackReturned(true)),
            A::Finish(HealthVerdict::DeadlineRolledBack)
        );
    }

    #[test]
    fn note_rollback_carries_the_slot_read_at_start() {
        for slot in [SlotId(0), SlotId(1)] {
            let mut m = started();
            assert_eq!(
                m.step(secs(1), false, E::Slot(SlotReading::PendingVerify(slot))),
                A::Sleep(secs(1))
            );
            assert_eq!(
                m.step(secs(120), true, E::Woke),
                A::NoteRollback(RollbackReason::HealthDeadline, slot)
            );
        }
    }

    /// Drives a scripted driver until the policy finishes; panics on `Ignored` or after 5000 steps.
    fn drive(
        mut c: HealthConfig,
        restart: bool,
        mut answer: impl FnMut(A) -> E,
    ) -> (HealthVerdict, usize) {
        if restart {
            c.deadline_action = DeadlineAction::NoteAndRestart(RollbackReason::Unhealthy);
        }
        let mut m = HealthMachine::new(c);
        let mut t = Duration::ZERO;
        let mut event = E::Begin;
        for steps in 1..=5_000 {
            let a = m.step(t, false, event);
            match a {
                A::Finish(v) => return (v, steps),
                A::Ignored => panic!("ignored after {steps} steps"),
                A::Sleep(d) => t += d,
                _ => {}
            }
            event = answer(a);
        }
        panic!("no verdict within 5000 steps");
    }

    /// A scripted driver: the answer to one action, given the slot reading to report.
    type Script = fn(A, E) -> E;

    #[test]
    fn every_scripted_path_reaches_finish() {
        let slot = E::Slot(SlotReading::PendingVerify(SLOT));
        let cases: [(&str, bool, Script, HealthVerdict); 6] = [
            (
                "healthy",
                false,
                |a, slot| match a {
                    A::ReadSlot => slot,
                    A::Evaluate => E::Healthy(true),
                    A::MarkValid => E::Marked(true),
                    A::CompleteAttempt => E::Completed(true),
                    _ => E::Woke,
                },
                HealthVerdict::Applied,
            ),
            (
                "unhealthy",
                false,
                |a, slot| match a {
                    A::ReadSlot => slot,
                    A::Evaluate => E::Healthy(false),
                    A::NoteRollback(..) => E::Noted,
                    A::Rollback => E::RollbackReturned(true),
                    _ => E::Woke,
                },
                HealthVerdict::DeadlineRolledBack,
            ),
            (
                "mark-fail",
                false,
                |a, slot| match a {
                    A::ReadSlot => slot,
                    A::Evaluate => E::Healthy(true),
                    A::MarkValid => E::Marked(false),
                    A::NoteRollback(..) => E::Noted,
                    A::Rollback => E::RollbackReturned(false),
                    _ => E::Woke,
                },
                HealthVerdict::DeadlineRollbackFailed,
            ),
            (
                "rollback-fail",
                false,
                |a, slot| match a {
                    A::ReadSlot => slot,
                    A::Evaluate => E::Healthy(false),
                    A::NoteRollback(..) => E::Noted,
                    A::Rollback => E::RollbackReturned(false),
                    _ => E::Woke,
                },
                HealthVerdict::DeadlineRollbackFailed,
            ),
            (
                "restart",
                true,
                |a, slot| match a {
                    A::ReadSlot => slot,
                    A::Evaluate => E::Healthy(false),
                    A::NoteRollback(..) => E::Noted,
                    A::Restart => E::RestartReturned,
                    _ => E::Woke,
                },
                HealthVerdict::RestartReturned,
            ),
            (
                "busy-forever",
                false,
                |a, _| match a {
                    A::ReadSlot => E::Slot(SlotReading::Busy),
                    _ => E::Woke,
                },
                HealthVerdict::SlotUnreadable,
            ),
        ];
        for (name, restart, script, verdict) in cases {
            let (got, steps) = drive(cfg(), restart, |a| script(a, slot));
            assert_eq!(got, verdict, "{name}");
            assert!(steps < 5_000, "{name}");
        }
    }

    #[test]
    fn only_a_pending_slot_is_ever_marked_or_rolled_back() {
        let touching = |a: &A| {
            matches!(
                a,
                A::Evaluate
                    | A::MarkValid
                    | A::CompleteAttempt
                    | A::NoteRollback(..)
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
    fn runtime_flags_follow_initial_admission() {
        use crate::ota::BootFault;

        let open = RuntimeFlags::from_boot(&outcome(BootDisposition::Deferred, true));
        assert!(open.admission_open && open.health_settled);
        assert!(!open.records_suspect);

        let pending = RuntimeFlags::from_boot(&outcome(BootDisposition::AwaitHealthCheck, false));
        assert!(!pending.admission_open && !pending.health_settled);

        let suspect = RuntimeFlags::from_boot(&outcome(
            BootDisposition::FailedClosed(BootFault::Store(StoreError::InvalidSlot)),
            true,
        ));
        assert!(suspect.records_suspect);
        assert!(!suspect.admission_open);
    }

    #[test]
    fn refresh_after_repair_skips_unreadable_hardware() {
        use crate::ota::persist::fault_kv::FaultKv;

        let kv = FaultKv::new();
        let mut s = OtaStore::open(kv.clone(), || 0xC0FF_EE00);
        assert_eq!(
            refresh_after_repair(
                &mut s,
                Err(HardwareReadError::Busy),
                Some(Version::new(1, 0, 0)),
                false
            ),
            None
        );
        assert_eq!(kv.reads() + kv.commits(), 0);
    }

    #[test]
    fn a_report_is_owed_when_the_repair_dropped_or_failed_or_the_records_stay_suspect() {
        assert!(!repair_report_owed(Ok(0), false));
        assert!(repair_report_owed(Ok(0), true));
        assert!(repair_report_owed(Ok(1), false));
        assert!(repair_report_owed(Ok(3), true));
        assert!(repair_report_owed(Err(RepairError::Capped), false));
        assert!(repair_report_owed(
            Err(RepairError::Store(StoreError::InvalidSlot)),
            false
        ));
    }

    fn running_a() -> Result<HardwareFacts, HardwareReadError> {
        use crate::ota::persist::UpdateSlot;
        use crate::ota::SlotState;
        Ok(HardwareFacts {
            running_slot: SlotId(0),
            running_state: SlotState::Valid,
            update_slot: UpdateSlot::Absent,
        })
    }

    #[test]
    fn a_transient_read_error_at_boot_owes_no_report() {
        use crate::ota::persist::fault_kv::FaultKv;

        let running = Some(Version::new(1, 0, 0));
        let kv = FaultKv::new();
        let mut s = OtaStore::open(kv.clone(), || 0xC0FF_EE00);
        kv.reset_counters();
        let _ = reconcile_boot(&mut s, running_a(), running);
        let reads = kv.reads();
        assert!(reads > 0);

        let mut failed_closed = 0;
        for n in 1..=reads {
            let kv = FaultKv::new();
            let mut s = OtaStore::open(kv.clone(), || 0xC0FF_EE00);
            kv.reset_counters();
            kv.fail_read_at(n);
            let boot = reconcile_boot(&mut s, running_a(), running);
            kv.revive();
            if !boot.records_suspect() {
                continue;
            }
            failed_closed += 1;
            let repaired = repair_all(&mut s, 32);
            assert_eq!(repaired, Ok(0), "read {n}: nothing was corrupt");
            let refreshed =
                refresh_after_repair(&mut s, running_a(), running, false).expect("hardware read");
            assert!(!refreshed.records_suspect, "read {n}");
            assert!(
                !repair_report_owed(repaired, refreshed.records_suspect),
                "read {n}: a flaky read must not fail a valid action"
            );
        }
        assert!(failed_closed > 0, "some read fails the boot closed");
    }

    #[test]
    fn a_corrupt_record_at_boot_owes_a_report() {
        use crate::ota::persist::fault_kv::FaultKv;

        let running = Some(Version::new(1, 0, 0));
        let kv = FaultKv::new();
        kv.put_str("ddi_act", "042");
        let mut s = OtaStore::open(kv.clone(), || 0xC0FF_EE00);
        let boot = reconcile_boot(&mut s, running_a(), running);
        assert!(boot.records_suspect());
        let repaired = repair_all(&mut s, 32);
        assert!(matches!(repaired, Ok(n) if n > 0), "{repaired:?}");
        let refreshed =
            refresh_after_repair(&mut s, running_a(), running, false).expect("hardware read");
        assert!(!refreshed.records_suspect);
        assert!(repair_report_owed(repaired, refreshed.records_suspect));
    }
}
