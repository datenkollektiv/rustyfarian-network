//! The health policy of the running image as pure decisions, and the record upkeep that follows it (completion retry, repair), shared by the MQTT runtime (`ota-wire`) and the DDI runtime (`ota-ddi`).
//!
//! The policy decides when a freshly activated image proves itself healthy, and what happens when it does not.
//! It never touches a slot that is not pending verification, so a normally booted image cannot restart itself because the broker happens to be down.
//!
//! The ESP-IDF driver runs it on one thread (the app's main thread) as flat logic (ADR 023 decision 3): it reads the slot, sleeps, asks the app predicate, marks valid, records the attempt, rolls back or restarts in plain sequential code, and asks these functions only for the decisions.
//! [`SlotReads::busy`] and [`SlotReads::failed`] decide whether an unreadable slot is retried or ends the policy, [`poll_decision`] and [`after_unhealthy`] decide whether to evaluate, wait or give up at the deadline, and [`deadline_plan`] says what the deadline does.
//! [`HealthFlow`] composes the same decisions in the same order as an action machine (rung 3) for a tier that runs the policy as an async task (esp-hal), where flat blocking code does not fit (ADR 023 decision 2).
//! Elapsed time is measured from the boot instant the app captured at the very start of `main`, not from Wi-Fi association.

use core::time::Duration;

use crate::ota::persist::{
    reconcile_boot, BootDisposition, BootOutcome, HardwareFacts, HardwareReadError, OtaKv,
    OtaStore, ReasonNote, StoreError,
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

/// Experimental: API may change before 1.0.
///
/// The input to [`HealthFlow::poll`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthInput {
    /// Starts the policy; the first input.
    Start,
    /// The answer to [`HealthAction::ReadSlot`]: one read of the running slot.
    Slot(SlotReading),
    /// The wait of a [`HealthAction::Wait`] ended.
    Waited,
    /// The answer of the app predicate to [`HealthAction::AskHealthy`].
    Healthy(bool),
    /// The answer to [`HealthAction::MarkValid`]: whether the running image was marked valid.
    MarkedValid(bool),
    /// The rollback of [`HealthAction::Rollback`] returned: whether it reported success.
    RolledBack(bool),
    /// The app's restart callback of [`HealthAction::Restart`] returned.
    RestartReturned,
}

/// Experimental: API may change before 1.0.
///
/// What the tier must do next for the health policy ([`HealthFlow`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthAction {
    /// Read the running slot once, then answer with [`HealthInput::Slot`].
    ReadSlot,
    /// Wait this long, then answer with [`HealthInput::Waited`].
    Wait(Duration),
    /// Ask the app predicate, then answer with [`HealthInput::Healthy`].
    AskHealthy,
    /// Mark the running image valid, then answer with [`HealthInput::MarkedValid`].
    MarkValid,
    /// Roll back to the other slot; answer with [`HealthInput::RolledBack`] if the call returns.
    Rollback,
    /// Call the app's restart callback; answer with [`HealthInput::RestartReturned`] if it returns.
    Restart,
    /// The policy ended with this verdict; the tier settles it and feeds no further input.
    Done(HealthVerdict),
    /// The input did not answer the pending action; nothing changed (ADR 023 decision 5).
    Ignored,
}

/// Experimental: API may change before 1.0.
///
/// What a [`HealthFlow`] step did that a tier logs, taken with [`HealthFlow::take_note`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthNote {
    /// A failed slot read fell on the warning cadence: this many failed reads so far.
    SlotUnreadable(u32),
    /// Recording the completed attempt failed; it is tried again within `complete_attempts`.
    CompleteFailed(StoreError),
    /// The rollback reason was recorded (`written`), or an earlier reason of this boot was kept.
    RollbackNoted {
        /// The reason of the deadline plan.
        reason: RollbackReason,
        /// `false` when an earlier reason was kept.
        written: bool,
    },
    /// The rollback reason could not be recorded.
    RollbackNotNoted {
        /// The reason of the deadline plan.
        reason: RollbackReason,
        /// The store error.
        error: StoreError,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HealthPhase {
    Idle,
    ReadingSlot,
    SlotWait,
    Asking,
    Marking,
    Waiting,
    CompleteWait,
    Restarting,
    RollingBack,
    RollbackWait,
    Done,
}

/// Experimental: API may change before 1.0.
///
/// The health policy of the running image as an action machine (ADR 023 rung 3), composed from the pure decisions above in the order the ESP-IDF driver runs them.
///
/// The same policy runs on a thread (ESP-IDF) and as a task (esp-hal), so it is a machine here rather than one tier's sequential code (ADR 023 decision 2).
/// The tier performs each [`HealthAction`] and feeds the result back as a [`HealthInput`], with `elapsed`, the time since the app's boot instant, and the tier's `refuse_mark_valid` flag read at each call.
/// The machine reads no clock; it calls the store for the two records it owns, [`OtaStore::complete_attempt`] and [`OtaStore::note_rollback`].
///
/// The order:
/// - [`HealthAction::ReadSlot`] until the slot is known: a busy or failed read is retried every `slot_retry` ([`SlotReads`]), and gives up with [`HealthVerdict::SlotUnreadable`] only after `slot_min_attempts` reads and the failure deadline;
/// - a slot that is not pending verification ends with [`HealthVerdict::NothingToVerify`], touching nothing;
/// - a pending slot is decided by [`poll_decision`]: once the dwell has passed and nothing refuses, [`HealthAction::AskHealthy`] then [`HealthAction::MarkValid`], then `complete_attempt` with `complete_attempts` tries `complete_retry` apart ([`HealthVerdict::Applied`] or [`HealthVerdict::ApplyNotRecorded`]); otherwise [`after_unhealthy`] waits `poll_interval`;
/// - at the failure deadline the reason of [`deadline_plan`] is noted, then [`HealthAction::Restart`] or [`HealthAction::Rollback`] with `rollback_attempts` tries `rollback_retry` apart.
///
/// An input that does not answer the pending action returns [`HealthAction::Ignored`] and changes nothing.
#[derive(Debug, Clone)]
pub struct HealthFlow {
    cfg: HealthConfig,
    running: Version,
    phase: HealthPhase,
    reads: SlotReads,
    slot: SlotId,
    tries: u32,
    note: Option<HealthNote>,
}

impl HealthFlow {
    /// A policy with `cfg` for the running image `running`; nothing happens before [`HealthInput::Start`].
    pub fn new(cfg: HealthConfig, running: Version) -> Self {
        Self {
            cfg,
            running,
            phase: HealthPhase::Idle,
            reads: SlotReads::default(),
            slot: SlotId(0),
            tries: 0,
            note: None,
        }
    }

    /// What the last step did that a tier logs, if anything; cleared by the call.
    pub fn take_note(&mut self) -> Option<HealthNote> {
        self.note.take()
    }

    /// Advances the policy; `elapsed` is the time since the app's boot instant and `refuse_mark_valid` the tier's refusal flag now.
    pub fn poll<K: OtaKv>(
        &mut self,
        store: &mut OtaStore<K>,
        input: HealthInput,
        elapsed: Duration,
        refuse_mark_valid: bool,
    ) -> HealthAction {
        match (self.phase, input) {
            (HealthPhase::Idle, HealthInput::Start)
            | (HealthPhase::SlotWait, HealthInput::Waited) => {
                self.phase = HealthPhase::ReadingSlot;
                HealthAction::ReadSlot
            }
            (HealthPhase::ReadingSlot, HealthInput::Slot(reading)) => match reading {
                SlotReading::PendingVerify(slot) => {
                    self.slot = slot;
                    self.decide(store, elapsed, refuse_mark_valid)
                }
                SlotReading::NotPending => self.finish(HealthVerdict::NothingToVerify),
                SlotReading::Busy => {
                    let retry = self.reads.busy(&self.cfg, elapsed);
                    self.slot_retry(retry)
                }
                SlotReading::Failed => {
                    let retry = self.reads.failed(&self.cfg, elapsed);
                    self.slot_retry(retry)
                }
            },
            (HealthPhase::Waiting, HealthInput::Waited) => {
                self.decide(store, elapsed, refuse_mark_valid)
            }
            (HealthPhase::Asking, HealthInput::Healthy(true)) => {
                self.phase = HealthPhase::Marking;
                HealthAction::MarkValid
            }
            (HealthPhase::Asking, HealthInput::Healthy(false))
            | (HealthPhase::Marking, HealthInput::MarkedValid(false)) => {
                self.step(store, after_unhealthy(&self.cfg, elapsed))
            }
            (HealthPhase::Marking, HealthInput::MarkedValid(true)) => {
                self.tries = 0;
                self.complete(store)
            }
            (HealthPhase::CompleteWait, HealthInput::Waited) => self.complete(store),
            (HealthPhase::Restarting, HealthInput::RestartReturned) => {
                self.finish(HealthVerdict::RestartReturned)
            }
            (HealthPhase::RollingBack, HealthInput::RolledBack(true)) => {
                self.finish(HealthVerdict::DeadlineRolledBack)
            }
            (HealthPhase::RollingBack, HealthInput::RolledBack(false)) => {
                self.tries = self.tries.saturating_add(1);
                if self.tries >= self.cfg.rollback_attempts {
                    return self.finish(HealthVerdict::DeadlineRollbackFailed);
                }
                self.phase = HealthPhase::RollbackWait;
                HealthAction::Wait(self.cfg.rollback_retry)
            }
            (HealthPhase::RollbackWait, HealthInput::Waited) => {
                self.phase = HealthPhase::RollingBack;
                HealthAction::Rollback
            }
            _ => HealthAction::Ignored,
        }
    }

    fn decide<K: OtaKv>(
        &mut self,
        store: &mut OtaStore<K>,
        elapsed: Duration,
        refuse_mark_valid: bool,
    ) -> HealthAction {
        let step = poll_decision(&self.cfg, elapsed, refuse_mark_valid);
        self.step(store, step)
    }

    fn step<K: OtaKv>(&mut self, store: &mut OtaStore<K>, step: PollStep) -> HealthAction {
        match step {
            PollStep::Evaluate => {
                self.phase = HealthPhase::Asking;
                HealthAction::AskHealthy
            }
            PollStep::Wait(pause) => {
                self.phase = HealthPhase::Waiting;
                HealthAction::Wait(pause)
            }
            PollStep::Deadline => self.at_deadline(store),
        }
    }

    fn slot_retry(&mut self, retry: SlotRetry) -> HealthAction {
        match retry {
            SlotRetry::Sleep { pause, warn } => {
                self.note = warn.map(HealthNote::SlotUnreadable);
                self.phase = HealthPhase::SlotWait;
                HealthAction::Wait(pause)
            }
            SlotRetry::GiveUp => self.finish(HealthVerdict::SlotUnreadable),
        }
    }

    fn complete<K: OtaKv>(&mut self, store: &mut OtaStore<K>) -> HealthAction {
        match store.complete_attempt(self.running) {
            Ok(()) => self.finish(HealthVerdict::Applied),
            Err(e) => {
                self.note = Some(HealthNote::CompleteFailed(e));
                self.tries = self.tries.saturating_add(1);
                if self.tries >= self.cfg.complete_attempts {
                    return self.finish(HealthVerdict::ApplyNotRecorded);
                }
                self.phase = HealthPhase::CompleteWait;
                HealthAction::Wait(self.cfg.complete_retry)
            }
        }
    }

    fn at_deadline<K: OtaKv>(&mut self, store: &mut OtaStore<K>) -> HealthAction {
        let plan = deadline_plan(&self.cfg);
        let reason = plan.reason;
        self.note = Some(match store.note_rollback(reason, self.slot) {
            Ok(note) => HealthNote::RollbackNoted {
                reason,
                written: matches!(note, ReasonNote::Written(_)),
            },
            Err(error) => HealthNote::RollbackNotNoted { reason, error },
        });
        if plan.restart {
            self.phase = HealthPhase::Restarting;
            return HealthAction::Restart;
        }
        self.tries = 0;
        self.phase = HealthPhase::RollingBack;
        HealthAction::Rollback
    }

    fn finish(&mut self, verdict: HealthVerdict) -> HealthAction {
        self.phase = HealthPhase::Done;
        HealthAction::Done(verdict)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

#[cfg(test)]
mod flow_tests {
    use super::*;
    use crate::ota::persist::fault_kv::FaultKv;

    const SLOT: SlotId = SlotId(1);

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn running() -> Version {
        Version::new(1, 1, 0)
    }

    fn store() -> (FaultKv, OtaStore<FaultKv>) {
        let kv = FaultKv::new();
        let s = OtaStore::open(kv.clone(), || 7);
        (kv, s)
    }

    fn flow() -> HealthFlow {
        HealthFlow::new(HealthConfig::default(), running())
    }

    /// Feeds `input` at `t` seconds since boot, with no refusal.
    fn at(
        f: &mut HealthFlow,
        s: &mut OtaStore<FaultKv>,
        input: HealthInput,
        t: u64,
    ) -> HealthAction {
        f.poll(s, input, secs(t), false)
    }

    /// Starts the flow at `t` and answers the first read with a pending slot.
    fn pending_at(f: &mut HealthFlow, s: &mut OtaStore<FaultKv>, t: u64) -> HealthAction {
        assert_eq!(at(f, s, HealthInput::Start, t), HealthAction::ReadSlot);
        at(f, s, HealthInput::Slot(SlotReading::PendingVerify(SLOT)), t)
    }

    #[test]
    fn a_slot_that_is_not_pending_ends_at_once_and_touches_nothing() {
        let (kv, mut s) = store();
        let mut f = flow();
        assert_eq!(
            at(&mut f, &mut s, HealthInput::Start, 0),
            HealthAction::ReadSlot
        );
        assert_eq!(
            at(
                &mut f,
                &mut s,
                HealthInput::Slot(SlotReading::NotPending),
                0
            ),
            HealthAction::Done(HealthVerdict::NothingToVerify)
        );
        assert_eq!(kv.commits(), 0);
    }

    #[test]
    fn a_healthy_pending_image_is_marked_valid_after_the_dwell_and_recorded() {
        let (_kv, mut s) = store();
        let mut f = flow();
        assert_eq!(pending_at(&mut f, &mut s, 0), HealthAction::Wait(secs(1)));
        for t in 1..30 {
            assert_eq!(
                at(&mut f, &mut s, HealthInput::Waited, t),
                HealthAction::Wait(secs(1)),
                "{t} s: the dwell has not passed"
            );
        }
        assert_eq!(
            at(&mut f, &mut s, HealthInput::Waited, 30),
            HealthAction::AskHealthy
        );
        assert_eq!(
            at(&mut f, &mut s, HealthInput::Healthy(true), 30),
            HealthAction::MarkValid
        );
        assert_eq!(
            at(&mut f, &mut s, HealthInput::MarkedValid(true), 30),
            HealthAction::Done(HealthVerdict::Applied)
        );
    }

    #[test]
    fn a_refused_image_is_never_asked_and_meets_the_deadline() {
        let (_kv, mut s) = store();
        let mut f = flow();
        assert_eq!(
            f.poll(&mut s, HealthInput::Start, secs(40), true),
            HealthAction::ReadSlot
        );
        assert_eq!(
            f.poll(
                &mut s,
                HealthInput::Slot(SlotReading::PendingVerify(SLOT)),
                secs(40),
                true
            ),
            HealthAction::Wait(secs(1))
        );
        assert_eq!(
            f.poll(&mut s, HealthInput::Waited, secs(120), true),
            HealthAction::Rollback
        );
        assert_eq!(
            f.take_note(),
            Some(HealthNote::RollbackNoted {
                reason: RollbackReason::HealthDeadline,
                written: true
            })
        );
    }

    #[test]
    fn an_unhealthy_answer_or_a_failed_mark_valid_waits_then_rolls_back_with_bounded_tries() {
        let (_kv, mut s) = store();
        let mut f = flow();
        assert_eq!(pending_at(&mut f, &mut s, 30), HealthAction::AskHealthy);
        assert_eq!(
            at(&mut f, &mut s, HealthInput::Healthy(false), 30),
            HealthAction::Wait(secs(1))
        );
        assert_eq!(
            at(&mut f, &mut s, HealthInput::Waited, 31),
            HealthAction::AskHealthy
        );
        assert_eq!(
            at(&mut f, &mut s, HealthInput::Healthy(true), 31),
            HealthAction::MarkValid
        );
        assert_eq!(
            at(&mut f, &mut s, HealthInput::MarkedValid(false), 31),
            HealthAction::Wait(secs(1))
        );
        assert_eq!(
            at(&mut f, &mut s, HealthInput::Waited, 120),
            HealthAction::AskHealthy
        );
        assert_eq!(
            at(&mut f, &mut s, HealthInput::Healthy(false), 120),
            HealthAction::Rollback
        );
        assert_eq!(
            at(&mut f, &mut s, HealthInput::RolledBack(false), 120),
            HealthAction::Wait(secs(5))
        );
        assert_eq!(
            at(&mut f, &mut s, HealthInput::Waited, 125),
            HealthAction::Rollback
        );
        assert_eq!(
            at(&mut f, &mut s, HealthInput::RolledBack(false), 125),
            HealthAction::Wait(secs(5))
        );
        assert_eq!(
            at(&mut f, &mut s, HealthInput::Waited, 130),
            HealthAction::Rollback
        );
        assert_eq!(
            at(&mut f, &mut s, HealthInput::RolledBack(false), 130),
            HealthAction::Done(HealthVerdict::DeadlineRollbackFailed)
        );
    }

    #[test]
    fn a_rollback_that_returns_success_ends_the_policy() {
        let (_kv, mut s) = store();
        let mut f = flow();
        assert_eq!(pending_at(&mut f, &mut s, 120), HealthAction::AskHealthy);
        assert_eq!(
            at(&mut f, &mut s, HealthInput::Healthy(false), 120),
            HealthAction::Rollback
        );
        assert_eq!(
            at(&mut f, &mut s, HealthInput::RolledBack(true), 120),
            HealthAction::Done(HealthVerdict::DeadlineRolledBack)
        );
    }

    #[test]
    fn note_and_restart_calls_the_restart_callback_after_noting() {
        let (_kv, mut s) = store();
        let cfg = HealthConfig {
            deadline_action: DeadlineAction::NoteAndRestart(RollbackReason::Unhealthy),
            ..HealthConfig::default()
        };
        let mut f = HealthFlow::new(cfg, running());
        assert_eq!(pending_at(&mut f, &mut s, 120), HealthAction::AskHealthy);
        assert_eq!(
            at(&mut f, &mut s, HealthInput::Healthy(false), 120),
            HealthAction::Restart
        );
        assert_eq!(
            f.take_note(),
            Some(HealthNote::RollbackNoted {
                reason: RollbackReason::Unhealthy,
                written: true
            })
        );
        assert_eq!(
            at(&mut f, &mut s, HealthInput::RestartReturned, 121),
            HealthAction::Done(HealthVerdict::RestartReturned)
        );
    }

    #[test]
    fn a_failing_completion_is_retried_then_ends_apply_not_recorded() {
        let (kv, mut s) = store();
        kv.corrupt("rej_ver");
        let mut f = flow();
        assert_eq!(pending_at(&mut f, &mut s, 30), HealthAction::AskHealthy);
        at(&mut f, &mut s, HealthInput::Healthy(true), 30);
        assert_eq!(
            at(&mut f, &mut s, HealthInput::MarkedValid(true), 30),
            HealthAction::Wait(secs(1))
        );
        assert!(matches!(f.take_note(), Some(HealthNote::CompleteFailed(_))));
        assert_eq!(
            at(&mut f, &mut s, HealthInput::Waited, 31),
            HealthAction::Wait(secs(1))
        );
        assert_eq!(
            at(&mut f, &mut s, HealthInput::Waited, 32),
            HealthAction::Done(HealthVerdict::ApplyNotRecorded)
        );

        let (kv, mut s) = store();
        kv.corrupt("rej_ver");
        let mut f = flow();
        pending_at(&mut f, &mut s, 30);
        at(&mut f, &mut s, HealthInput::Healthy(true), 30);
        at(&mut f, &mut s, HealthInput::MarkedValid(true), 30);
        kv.revive();
        assert_eq!(
            at(&mut f, &mut s, HealthInput::Waited, 31),
            HealthAction::Done(HealthVerdict::Applied),
            "a later try succeeds"
        );
    }

    #[test]
    fn unreadable_slot_reads_retry_warn_and_give_up_after_the_deadline() {
        let (_kv, mut s) = store();
        let mut f = flow();
        assert_eq!(
            at(&mut f, &mut s, HealthInput::Start, 0),
            HealthAction::ReadSlot
        );
        assert_eq!(
            at(&mut f, &mut s, HealthInput::Slot(SlotReading::Failed), 0),
            HealthAction::Wait(Duration::from_millis(200))
        );
        assert_eq!(f.take_note(), Some(HealthNote::SlotUnreadable(1)));
        assert_eq!(
            at(&mut f, &mut s, HealthInput::Waited, 0),
            HealthAction::ReadSlot
        );
        assert_eq!(
            at(&mut f, &mut s, HealthInput::Slot(SlotReading::Busy), 0),
            HealthAction::Wait(Duration::from_millis(200))
        );
        assert_eq!(f.take_note(), None, "a busy read never warns");
        for _ in 0..3 {
            at(&mut f, &mut s, HealthInput::Waited, 200);
            at(&mut f, &mut s, HealthInput::Slot(SlotReading::Failed), 200);
        }
        assert_eq!(
            at(&mut f, &mut s, HealthInput::Waited, 200),
            HealthAction::ReadSlot
        );
        assert_eq!(
            at(&mut f, &mut s, HealthInput::Slot(SlotReading::Failed), 200),
            HealthAction::Done(HealthVerdict::SlotUnreadable)
        );
    }

    #[test]
    fn out_of_phase_input_is_ignored_and_changes_nothing() {
        let (_kv, mut s) = store();
        let mut f = flow();
        for input in [
            HealthInput::Waited,
            HealthInput::Healthy(true),
            HealthInput::MarkedValid(true),
            HealthInput::RolledBack(true),
            HealthInput::RestartReturned,
            HealthInput::Slot(SlotReading::NotPending),
        ] {
            assert_eq!(
                at(&mut f, &mut s, input, 0),
                HealthAction::Ignored,
                "{input:?}"
            );
        }
        assert_eq!(
            at(&mut f, &mut s, HealthInput::Start, 0),
            HealthAction::ReadSlot
        );
        assert_eq!(
            at(&mut f, &mut s, HealthInput::Start, 0),
            HealthAction::Ignored
        );
        assert_eq!(
            at(&mut f, &mut s, HealthInput::Waited, 0),
            HealthAction::Ignored
        );
        assert_eq!(
            at(
                &mut f,
                &mut s,
                HealthInput::Slot(SlotReading::NotPending),
                0
            ),
            HealthAction::Done(HealthVerdict::NothingToVerify)
        );
        for input in [
            HealthInput::Start,
            HealthInput::Waited,
            HealthInput::Healthy(true),
        ] {
            assert_eq!(
                at(&mut f, &mut s, input, 1),
                HealthAction::Ignored,
                "after Done: {input:?}"
            );
        }
    }
}
