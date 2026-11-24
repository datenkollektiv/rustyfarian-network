//! The DDI worker composition as one action machine (ADR 023 rung 3): the poll loop with its admission, feedback, verdict, cancel, nudge and completion-retry work, in the order the ESP-IDF `ota-worker` thread runs them.
//!
//! The same composition runs on a thread (ESP-IDF) and as an async task (esp-hal), so it lives here once (ADR 023 decision 2).
//! [`DdiWorker`] owns the [`PollMachine`], the [`CancelMachine`] and the [`VerdictSender`]; a tier performs each [`WorkerAction`] (a request, a hardware read, a flag update, a download start, stop or join, a sleep) and feeds the result back as a [`WorkerInput`], with a [`Stamp`]: the monotonic time, the wall clock and a random value, all read by the tier.
//! The machine calls the store for every record decision itself, so a tier holds the store borrow for one [`DdiWorker::poll`] call and never across an action.
//!
//! Wake-ups ([`Wake`]: the MQTT nudge, the health verdict, an ended download) arrive only while the worker sleeps, as [`WorkerInput::Woken`]; a tier keeps them until then.
//! What a tier logs is returned as data: [`DdiWorker::drain_events`] after each call.
//!
//! The download of an admitted offer is the tier's ([`WorkerAction::StartDownload`]); its retry flow is [`DownloadFlow`](super::DownloadFlow).

use alloc::vec::Vec;
use core::time::Duration;

use super::admission::{
    admit_retrieved, AdmissionFacts, AdmittedOffer, DeploymentVerdict, Refusal,
};
use super::cancel::{CancelInput, CancelMachine, CancelResult, CancelStep, CancelTarget};
use super::download_flow::{settle_failed, SettleNote};
use super::endpoint::DdiEndpoint;
use super::feedback::{FeedbackPost, SendOutcome, VerdictSender, VerdictStep};
use super::poller::{
    Nudge, OfferGate, PollAction, PollAnswer, PollInput, PollMachine, PollVerdict, Transient,
    WorkOutcome,
};
use super::response::RequestOutcome;
use super::schedule::MIN_POLL_INTERVAL;
use super::settings::DdiSettings;
use super::wire::{ActionId, Board, Chip, ConfigData, Href};
use crate::ota::health::{
    completion_step, refresh_after_repair, retry_completion, CompletionBackoff,
    CompletionRetryConfig, CompletionStep, CompletionTry, HealthVerdict, RefreshedFlags,
    RuntimeFlags,
};
use crate::ota::persist::{
    DdiVerdict, HardwareFacts, HardwareReadError, OtaKv, OtaStore, StoreError, UpdateSlot,
    VerdictDelivery, VerdictNote,
};
use crate::ota::{BusyRetry, CancelRequest, FailReason, SlotState, Version};

/// Experimental: API may change before 1.0.
///
/// The longest pause between two background attempts to clear the attempt record after `ApplyNotRecorded`: the MQTT runtime's default (`report_retry`).
pub const COMPLETION_RETRY_CAP: Duration = Duration::from_secs(10);

/// Experimental: API may change before 1.0.
///
/// What a tier read for one call of [`DdiWorker::poll`] or [`DownloadFlow::poll`](super::DownloadFlow::poll): the machines read no clock and draw no random number.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stamp {
    /// The monotonic time since the runtime started, the one origin of every machine.
    pub now: Duration,
    /// Milliseconds since the Unix epoch, `None` while the wall clock is not set (below [`WALL_CLOCK_FLOOR_SECS`](super::WALL_CLOCK_FLOOR_SECS)); feedback timestamps and `Retry-After` dates use it.
    pub wall_millis: Option<u64>,
    /// A fresh random value for the back-off jitter.
    pub random: u32,
}

impl Stamp {
    /// Seconds since the Unix epoch, from [`wall_millis`](Self::wall_millis).
    pub const fn wall_secs(&self) -> Option<u64> {
        match self.wall_millis {
            Some(millis) => Some(millis / 1000),
            None => None,
        }
    }
}

/// Experimental: API may change before 1.0.
///
/// What woke the worker out of a [`WorkerAction::Sleep`].
///
/// A tier collects wake-ups while the worker is busy and hands them over, merged ([`Wake::merge`]), with the next [`WorkerInput::Woken`]; a timer that ran out is [`Wake::default`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Wake {
    /// The MQTT nudge topic was received.
    pub nudge: bool,
    /// The health policy ended with this verdict; it already persisted the success verdict, if any.
    pub health: Option<HealthVerdict>,
    /// A download ended, or may have.
    pub download_ended: bool,
}

impl Wake {
    /// Adds `other` to the wake-ups kept so far; the later health verdict wins.
    pub fn merge(&mut self, other: Wake) {
        self.nudge |= other.nudge;
        self.download_ended |= other.download_ended;
        if other.health.is_some() {
            self.health = other.health;
        }
    }

    /// Any wake-up is set.
    pub fn any(&self) -> bool {
        self.nudge || self.download_ended || self.health.is_some()
    }
}

/// Experimental: API may change before 1.0.
///
/// A request of [`WorkerAction::Send`]; the tier's DDI transport performs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DdiRequest {
    /// GET the controller base: answer with [`WorkerInput::PollAnswered`].
    Poll,
    /// PUT [`DdiWorker::config_data`] to the configData URL built from the base.
    ConfigData,
    /// GET this `deploymentBase` link (the transport checks its origin).
    Deployment(Href),
    /// GET this `cancelAction` link (the transport checks its origin).
    CancelAction(Href),
    /// POST this feedback: a `proceeding` or a cancel answer.
    Feedback(FeedbackPost<'static>),
    /// POST the closing feedback of this persisted verdict ([`FeedbackPost::verdict`]).
    Verdict {
        /// The store's pending verdict.
        verdict: DdiVerdict,
        /// The feedback timestamp.
        timestamp_ms: Option<u64>,
    },
}

impl DdiRequest {
    /// The feedback POST of a [`DdiRequest::Feedback`] or [`DdiRequest::Verdict`], `None` for a GET or the PUT.
    pub fn feedback(&self) -> Option<FeedbackPost<'_>> {
        match self {
            DdiRequest::Feedback(post) => Some(post.clone()),
            DdiRequest::Verdict {
                verdict,
                timestamp_ms,
            } => Some(FeedbackPost::verdict(verdict, *timestamp_ms)),
            _ => None,
        }
    }
}

/// Experimental: API may change before 1.0.
///
/// The input to [`DdiWorker::poll`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerInput {
    /// Starts the worker; the first input. `health_settled` is the tier's flag now: an unsettled boot of a DDI image sends the post-boot `proceeding`.
    Start {
        /// The health policy has settled (or never had to run).
        health_settled: bool,
    },
    /// The sleep of [`WorkerAction::Sleep`] ended, by its timer or by these wake-ups.
    Woken(Wake),
    /// The poll GET ended; `admission_open` is the tier's flag now.
    PollAnswered {
        /// How the GET ended.
        outcome: RequestOutcome,
        /// The library's admission flag.
        admission_open: bool,
    },
    /// Any other request of [`WorkerAction::Send`] ended.
    Answered(RequestOutcome),
    /// The answer to [`WorkerAction::ReadHardware`].
    Hardware(Result<HardwareFacts, HardwareReadError>),
    /// The tier applied [`WorkerAction::SetFlags`].
    FlagsSet,
    /// The answer to [`WorkerAction::StartDownload`]: `false` when the download could not be started.
    DownloadStarted(bool),
    /// The answer to [`WorkerAction::CheckDownload`]: whether the download still runs.
    DownloadRunning(bool),
    /// The answer of the download's cancel switch to [`WorkerAction::RequestStop`].
    StopRequested(CancelRequest),
    /// The download returned after [`WorkerAction::AwaitDownload`] and settled itself.
    DownloadEnded,
}

/// Experimental: API may change before 1.0.
///
/// What the tier must do next for the worker.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkerAction {
    /// Perform this request, then answer with [`WorkerInput::PollAnswered`] for [`DdiRequest::Poll`] and [`WorkerInput::Answered`] for every other.
    ///
    /// A transport error raised before any request was sent reads as [`RequestOutcome::Unreachable`], so every step is answered.
    Send(DdiRequest),
    /// Read the hardware facts with this retry, then answer with [`WorkerInput::Hardware`].
    ReadHardware(BusyRetry),
    /// Copy these two flags into the tier's flags (the re-run of the boot reconciliation after a repair), then answer with [`WorkerInput::FlagsSet`].
    SetFlags {
        /// The records still fail closed on a store fault.
        records_suspect: bool,
        /// The running image must not be marked valid.
        refuse_mark_valid: bool,
    },
    /// Start the download of this admitted offer with a fresh cancel switch (`for_retries`), then answer with [`WorkerInput::DownloadStarted`].
    StartDownload(AdmittedOffer),
    /// Say whether the download started last still runs, with [`WorkerInput::DownloadRunning`].
    CheckDownload,
    /// Request the stop on the running download's cancel switch, then answer with [`WorkerInput::StopRequested`].
    RequestStop,
    /// Wait until the running download returned, then answer with [`WorkerInput::DownloadEnded`].
    AwaitDownload,
    /// Sleep until `now` reaches `until` (for ever with `None`) or a wake-up comes, then answer with [`WorkerInput::Woken`].
    Sleep {
        /// The monotonic time to wake at.
        until: Option<Duration>,
    },
    /// The input did not answer the pending action; nothing changed (ADR 023 decision 5).
    Ignored,
}

/// Experimental: API may change before 1.0.
///
/// Something a step of the worker did that a tier logs (see [`DdiWorker::drain_events`]).
///
/// Server-supplied values never appear here, only ids, codes, statuses and store results.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum WorkerEvent {
    /// The new image of this action booted before its health check; the post-boot `proceeding` is sent.
    BootedForAction(ActionId),
    /// The running DDI action could not be read; no post-boot `proceeding`.
    RunningActionUnreadable(StoreError),
    /// A poll answer was judged.
    PollAnswered(PollVerdict),
    /// The poll machine ignored its input (an internal error); the worker sleeps [`MIN_POLL_INTERVAL`].
    PollIgnored,
    /// The server answered 410: polling has ended until the next boot. Once per boot.
    PollingStopped,
    /// A nudge after polling had ended; not applied.
    NudgeAfterStop,
    /// A kept nudge was offered to the poll machine with this result.
    Nudge(Nudge),
    /// The pending verdict could not be read for a poll answer.
    PendingVerdictUnreadable(StoreError),
    /// The configData PUT was answered (`accepted` for a 2xx).
    ConfigData {
        /// A 2xx answer.
        accepted: bool,
    },
    /// The deployment GET was not answered normally.
    NotRetrieved(Transient),
    /// The deployment was refused and carries no action id to report to.
    Unreportable(FailReason),
    /// The server deferred the deployment (`skip`).
    Deferred(ActionId),
    /// The deployment was refused.
    Refused(Refusal),
    /// The deployment passed admission.
    Admitted {
        /// Its action.
        action: ActionId,
        /// The offered version.
        version: Version,
        /// The artifact size.
        size: u64,
    },
    /// The hardware facts could not be read.
    HardwareUnreadable(HardwareReadError),
    /// The running image is pending verification; the offer is deferred.
    PendingVerify(ActionId),
    /// The update slot is absent, unreadable or invalid; the offer is reported failed.
    SlotUnusable(ActionId),
    /// The attempt began with this id.
    AttemptBegun {
        /// Its action.
        action: ActionId,
        /// The attempt id.
        id: u32,
    },
    /// The action is closed; it is never applied again.
    ActionClosed(ActionId),
    /// An earlier attempt or verdict is open; the offer is deferred.
    OfferBlocked(ActionId),
    /// The attempt could not begin.
    AttemptNotBegun {
        /// Its action.
        action: ActionId,
        /// The store error.
        error: StoreError,
    },
    /// The download could not be started; the attempt was settled with [`FailReason::Busy`].
    DownloadNotStarted {
        /// Its action.
        action: ActionId,
        /// What the settle did.
        settled: SettleNote,
    },
    /// This failure has no profile code; it is never reported.
    NoProfileCode(FailReason),
    /// A verdict was persisted (or not).
    VerdictPersisted {
        /// Its action.
        action: ActionId,
        /// What the store returned.
        note: Result<VerdictNote, StoreError>,
    },
    /// The pending verdict could not be read for a delivery.
    VerdictUnreadable(StoreError),
    /// The verdict is sent again no sooner than `at`.
    VerdictNotDue {
        /// The monotonic time of the next allowed send.
        at: Duration,
    },
    /// Verdict sending is suspended until a poll is accepted.
    VerdictSuspended,
    /// A verdict send is still in flight (an internal error).
    VerdictInFlight,
    /// The verdict was delivered and marked (or not).
    VerdictDelivered {
        /// Its action.
        action: ActionId,
        /// What the store returned.
        marked: Result<VerdictDelivery, StoreError>,
    },
    /// The verdict was not delivered; it is sent again in 30 s.
    VerdictRetry(ActionId),
    /// The third consecutive 401 to this verdict: sending is suspended until a poll is accepted.
    VerdictSuspendedNow(ActionId),
    /// A verdict answer arrived with no send in flight (an internal error).
    VerdictAnswerIgnored,
    /// The 1005 `records_unreadable` verdict for this action was persisted (or not).
    RecordsUnreadableReported {
        /// The offered action.
        action: ActionId,
        /// What the store returned.
        persisted: Result<VerdictNote, StoreError>,
    },
    /// The boot reconciliation was re-run after the repair.
    Refreshed(RefreshedFlags),
    /// A cancel looked up its target.
    CancelTarget {
        /// The cancel's `stopId`.
        stop_id: ActionId,
        /// What the device knows of it.
        target: CancelTarget,
    },
    /// The download's cancel switch answered a stop.
    CancelStopRequested(CancelRequest),
    /// A cancel ended.
    CancelEnded(Option<CancelResult>),
    /// The cancel machine ignored its input (an internal error).
    CancelIgnored,
    /// A background attempt to clear the attempt record ran.
    Completion(CompletionTry),
}

/// Experimental: API may change before 1.0.
///
/// What the worker is built from: the identity of the running image, the admission facts that do not change during a boot and the boot repair's result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerConfig {
    /// The running version.
    pub running: Version,
    /// The version floor of the settings.
    pub floor: Option<Version>,
    /// The profile chip name of this target.
    pub chip: Chip,
    /// The board name, for configData.
    pub board: Board,
    /// The size of the update partition.
    pub partition_capacity: u64,
    /// The hardware read before an admission: `1 + session_busy_retries` tries, `session_busy_delay` apart.
    pub admission_read: BusyRetry,
    /// The backoff of the background completion retry.
    pub completion: CompletionRetryConfig,
    /// The boot demanded a rollback; a repair re-run keeps the image refused.
    pub roll_back_now_at_boot: bool,
    /// The boot repair owes the server a 1005 report ([`AfterRepair::report_owed`](super::AfterRepair::report_owed)).
    pub unreported_corruption: bool,
}

impl WorkerConfig {
    /// The configuration from the runtime settings, the target's chip, the update partition size, the boot flags and the repair's report debt.
    pub fn new(
        settings: &DdiSettings,
        chip: Chip,
        partition_capacity: u64,
        flags: &RuntimeFlags,
        report_owed: bool,
    ) -> Self {
        let t = &settings.timings;
        let initial = t.health.complete_retry;
        Self {
            running: settings.running_version,
            floor: settings.floor,
            chip,
            board: settings.board.clone(),
            partition_capacity,
            admission_read: BusyRetry {
                tries: u8::try_from(t.session_busy_retries.saturating_add(1)).unwrap_or(u8::MAX),
                delay: t.session_busy_delay,
            },
            completion: CompletionRetryConfig {
                initial,
                cap: COMPLETION_RETRY_CAP.max(initial),
            },
            roll_back_now_at_boot: flags.roll_back_now_at_boot,
            unreported_corruption: report_owed,
        }
    }
}

/// Where the worker waits for the tier.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    Idle,
    BootProceeding,
    Sleeping { until: Duration },
    Stopped,
    Polling,
    ConfigData,
    Deployment(Href),
    AdmitHardware(AdmittedOffer),
    AdmitProceeding(AdmittedOffer),
    Starting(ActionId),
    StartFailed(ActionId),
    Verdict(ActionId),
    UnreadableHardware,
    UnreadableFlags,
    CancelGet,
    CancelCheck(ActionId),
    CancelStop,
    CancelAwait,
    CancelSend,
}

/// The end of one piece of poll work: an action for the tier, or the hand-back for [`PollInput::Performed`].
enum Step {
    Act(WorkerAction),
    Done(Option<WorkOutcome>),
}

/// What [`VerdictSender::next`] decided, without the borrow of the verdict.
enum Next {
    Send,
    NotDue(Duration),
    Suspended,
    InFlight,
}

/// Experimental: API may change before 1.0.
///
/// The DDI worker (see the [module docs](self)), in the order of the ESP-IDF `ota-worker` thread:
///
/// - [`WorkerInput::Start`]: a boot whose health policy has not settled and whose attempt is bound to a DDI action sends the post-boot `proceeding`, then the first poll;
/// - each poll answer reads the offer gate: [`OfferGate::RecordsUnreadable`] while the boot repair owes a report, [`OfferGate::Blocked`] while the tier's admission flag is closed, the store's gate otherwise;
/// - a deployment goes through [`admit_retrieved`]; a refusal is persisted and sent, an admitted offer reads the hardware, begins the attempt ([`OtaStore::begin_ddi_attempt`]), sends `proceeding` and starts the download;
/// - a cancel runs the [`CancelMachine`] to its end, asking whether the download still runs before the look-up;
/// - a records-unreadable report persists the 1005 verdict, re-runs the boot reconciliation and sends it;
/// - verdicts go out through the [`VerdictSender`], and a delivered one is marked;
/// - while sleeping it applies kept nudges through [`PollMachine::nudge`] and retries `complete_attempt` after an `ApplyNotRecorded` health verdict ([`retry_completion`]).
///
/// An input that does not answer the pending action returns [`WorkerAction::Ignored`] and changes nothing.
#[derive(Debug)]
pub struct DdiWorker {
    config: WorkerConfig,
    endpoint: DdiEndpoint,
    poller: PollMachine,
    cancel: CancelMachine,
    sender: VerdictSender,
    phase: Phase,
    /// The action of the download started last, until the tier says it no longer runs ([`WorkerInput::DownloadRunning`], [`WorkerInput::DownloadEnded`]).
    ///
    /// A [`Wake::download_ended`] does not clear it: a wake-up may be late and belong to an earlier download, so the cancel look-up asks the tier ([`WorkerAction::CheckDownload`]).
    downloading: Option<ActionId>,
    /// A poll was asked for (nudge, health verdict, ended download) and `PollMachine::nudge` has not accepted or dropped it yet.
    nudge_pending: bool,
    /// The health policy asked for the background completion retry.
    completion_requested: bool,
    /// The background retry of `complete_attempt`; `None` while not armed.
    completion: Option<CompletionBackoff>,
    unreported_corruption: bool,
    stopped_noted: bool,
    events: Vec<WorkerEvent>,
}

impl DdiWorker {
    /// A worker polling `endpoint` with `config`; nothing happens before [`WorkerInput::Start`].
    pub fn new(endpoint: DdiEndpoint, config: WorkerConfig) -> Self {
        let unreported_corruption = config.unreported_corruption;
        Self {
            poller: PollMachine::new(endpoint.clone()),
            endpoint,
            config,
            cancel: CancelMachine::new(),
            sender: VerdictSender::new(),
            phase: Phase::Idle,
            downloading: None,
            nudge_pending: false,
            completion_requested: false,
            completion: None,
            unreported_corruption,
            stopped_noted: false,
            events: Vec::new(),
        }
    }

    /// The endpoint every URL is built from.
    pub fn endpoint(&self) -> &DdiEndpoint {
        &self.endpoint
    }

    /// The body of [`DdiRequest::ConfigData`].
    pub fn config_data(&self) -> ConfigData<'_> {
        ConfigData {
            chip: &self.config.chip,
            board: &self.config.board,
            version: self.config.running,
        }
    }

    /// The configuration, for the scenario harness, whose reference composition reads the version floor live.
    #[cfg(test)]
    pub(crate) fn config_mut(&mut self) -> &mut WorkerConfig {
        &mut self.config
    }

    /// The events of the calls since the last drain, oldest first.
    pub fn drain_events(&mut self) -> Vec<WorkerEvent> {
        core::mem::take(&mut self.events)
    }

    /// Advances the worker; `at` carries the time, the wall clock and a random value read by the tier for this call.
    pub fn poll<K: OtaKv>(
        &mut self,
        store: &mut OtaStore<K>,
        input: WorkerInput,
        at: Stamp,
    ) -> WorkerAction {
        let phase = core::mem::replace(&mut self.phase, Phase::Idle);
        let step = match (phase, input) {
            (Phase::Idle, WorkerInput::Start { health_settled }) => {
                return self.start(store, health_settled, at);
            }
            (Phase::BootProceeding, WorkerInput::Answered(_)) => {
                return self.drive(store, PollInput::Tick, at);
            }
            (Phase::Sleeping { until }, WorkerInput::Woken(wake)) => {
                self.apply_wake(wake);
                if at.now >= until {
                    self.nudge_pending = false;
                    return self.drive(store, PollInput::Tick, at);
                }
                return self.sleep_turn(store, until, at);
            }
            (Phase::Stopped, WorkerInput::Woken(wake)) => {
                self.apply_wake(wake);
                if wake.nudge {
                    self.events.push(WorkerEvent::NudgeAfterStop);
                }
                self.nudge_pending = false;
                return self.drive(store, PollInput::Tick, at);
            }
            (
                Phase::Polling,
                WorkerInput::PollAnswered {
                    outcome,
                    admission_open,
                },
            ) => {
                let answer = self.answer(store, outcome, admission_open, at);
                return self.drive(store, PollInput::Answered(answer), at);
            }
            (Phase::ConfigData, WorkerInput::Answered(outcome)) => {
                let accepted = is_2xx(&outcome);
                self.events.push(WorkerEvent::ConfigData { accepted });
                Step::Done((!accepted).then(|| work(outcome, at)))
            }
            (Phase::Deployment(href), WorkerInput::Answered(outcome)) => {
                self.retrieved(store, &href, outcome, at)
            }
            (Phase::AdmitHardware(offer), WorkerInput::Hardware(hw)) => {
                self.admit(store, offer, hw, at)
            }
            (Phase::AdmitProceeding(offer), WorkerInput::Answered(_)) => {
                self.phase = Phase::Starting(offer.action.clone());
                Step::Act(WorkerAction::StartDownload(offer))
            }
            (Phase::Starting(action), WorkerInput::DownloadStarted(true)) => {
                self.downloading = Some(action);
                Step::Done(None)
            }
            (Phase::Starting(action), WorkerInput::DownloadStarted(false)) => {
                self.phase = Phase::StartFailed(action);
                Step::Act(WorkerAction::ReadHardware(BusyRetry::default()))
            }
            (Phase::StartFailed(action), WorkerInput::Hardware(hw)) => {
                let settled = settle_failed(store, hw, self.config.running, FailReason::Busy);
                self.events
                    .push(WorkerEvent::DownloadNotStarted { action, settled });
                Step::Done(None)
            }
            (Phase::Verdict(action), WorkerInput::Answered(outcome)) => {
                self.verdict_answered(store, action, outcome, at)
            }
            (Phase::UnreadableHardware, WorkerInput::Hardware(hw)) => self.refresh(store, hw, at),
            (Phase::UnreadableFlags, WorkerInput::FlagsSet) => self.deliver_verdict(store, at),
            (Phase::CancelGet, WorkerInput::Answered(outcome)) => {
                let step = self.cancel.poll(
                    CancelInput::Retrieved(work(outcome, at)),
                    at.now,
                    at.wall_millis,
                );
                self.cancel_step(store, step, at)
            }
            (Phase::CancelCheck(stop_id), WorkerInput::DownloadRunning(running)) => {
                // The early poll for an ended download comes from its wake-up, which the tier sends for every ended download.
                if !running {
                    self.downloading = None;
                }
                let step = self.look_up(store, stop_id, at);
                self.cancel_step(store, step, at)
            }
            (Phase::CancelStop, WorkerInput::StopRequested(request)) => {
                self.events.push(WorkerEvent::CancelStopRequested(request));
                let step =
                    self.cancel
                        .poll(CancelInput::StopRequested(request), at.now, at.wall_millis);
                self.cancel_step(store, step, at)
            }
            (Phase::CancelAwait, WorkerInput::DownloadEnded) => {
                self.downloading = None;
                let step = self
                    .cancel
                    .poll(CancelInput::DownloadReturned, at.now, at.wall_millis);
                self.cancel_step(store, step, at)
            }
            (Phase::CancelSend, WorkerInput::Answered(outcome)) => {
                let step =
                    self.cancel
                        .poll(CancelInput::Sent(work(outcome, at)), at.now, at.wall_millis);
                self.cancel_step(store, step, at)
            }
            (phase, _) => {
                self.phase = phase;
                return WorkerAction::Ignored;
            }
        };
        match step {
            Step::Act(action) => action,
            Step::Done(hand_back) => self.drive(store, PollInput::Performed(hand_back), at),
        }
    }

    // ── the poll loop ────────────────────────────────────────────────────────────

    /// Feedback sequence point 3: the new image booted, before its health check.
    fn start<K: OtaKv>(
        &mut self,
        store: &mut OtaStore<K>,
        health_settled: bool,
        at: Stamp,
    ) -> WorkerAction {
        if !health_settled {
            match store.running_ddi_action() {
                Ok(Some(action)) => {
                    self.events
                        .push(WorkerEvent::BootedForAction(action.clone()));
                    self.phase = Phase::BootProceeding;
                    let post = FeedbackPost::proceeding(action, at.wall_millis);
                    return WorkerAction::Send(DdiRequest::Feedback(post));
                }
                Ok(None) => {}
                Err(e) => self.events.push(WorkerEvent::RunningActionUnreadable(e)),
            }
        }
        self.drive(store, PollInput::Tick, at)
    }

    /// Runs the poll machine until it hands the tier an action.
    fn drive<K: OtaKv>(
        &mut self,
        store: &mut OtaStore<K>,
        mut input: PollInput,
        at: Stamp,
    ) -> WorkerAction {
        loop {
            let answered = matches!(input, PollInput::Answered(_));
            let action = self.poller.poll(input, at.now);
            if answered {
                if let Some(verdict) = self.poller.verdict() {
                    self.sender.poll_answered(verdict);
                    self.events.push(WorkerEvent::PollAnswered(verdict));
                }
            }
            let step = match action {
                PollAction::Sleep { until } => return self.sleep_turn(store, until, at),
                PollAction::Poll => {
                    self.phase = Phase::Polling;
                    return WorkerAction::Send(DdiRequest::Poll);
                }
                PollAction::PutConfigData => {
                    self.phase = Phase::ConfigData;
                    return WorkerAction::Send(DdiRequest::ConfigData);
                }
                PollAction::RetrieveCancel(href) => {
                    self.phase = Phase::CancelGet;
                    return WorkerAction::Send(DdiRequest::CancelAction(href));
                }
                PollAction::RetrieveDeployment(href) => {
                    self.phase = Phase::Deployment(href.clone());
                    return WorkerAction::Send(DdiRequest::Deployment(href));
                }
                PollAction::ReportUnreadable(action) => self.report_unreadable(store, action),
                PollAction::DeliverVerdict => self.deliver_verdict(store, at),
                PollAction::Stopped => return self.stopped(store, at),
                PollAction::Ignored => {
                    self.events.push(WorkerEvent::PollIgnored);
                    let until = at.now.saturating_add(MIN_POLL_INTERVAL);
                    self.phase = Phase::Sleeping { until };
                    return WorkerAction::Sleep { until: Some(until) };
                }
            };
            match step {
                Step::Act(action) => return action,
                Step::Done(hand_back) => input = PollInput::Performed(hand_back),
            }
        }
    }

    /// One turn of the sleep until `until`: the completion retry, a kept nudge, then the wake time.
    ///
    /// A nudge either moves the next poll to now (the sleep ends), is kept until 10 s after the last poll have passed, or is dropped.
    fn sleep_turn<K: OtaKv>(
        &mut self,
        store: &mut OtaStore<K>,
        until: Duration,
        at: Stamp,
    ) -> WorkerAction {
        self.service_completion(store, at.now);
        if self.nudge_pending {
            let nudge = self.poller.nudge(at.now);
            if nudge != Nudge::InFlight {
                self.events.push(WorkerEvent::Nudge(nudge));
            }
            match nudge {
                Nudge::Moved | Nudge::AlreadyDue => {
                    self.nudge_pending = false;
                    return self.drive(store, PollInput::Tick, at);
                }
                Nudge::TooSoon | Nudge::InFlight => {}
                Nudge::Throttled | Nudge::Stopped => self.nudge_pending = false,
            }
        }
        let mut wake_at = until;
        if self.nudge_pending {
            if let Some(allowed) = self.poller.nudge_allowed_at() {
                wake_at = wake_at.min(allowed);
            }
        }
        if let Some(backoff) = self.completion {
            wake_at = wake_at.min(at.now.saturating_add(backoff.wait(at.now)));
        }
        self.phase = Phase::Sleeping { until };
        WorkerAction::Sleep {
            until: Some(wake_at),
        }
    }

    /// Polling has ended for this boot (410 Gone): sleep, keeping the record upkeep alive.
    fn stopped<K: OtaKv>(&mut self, store: &mut OtaStore<K>, at: Stamp) -> WorkerAction {
        if !self.stopped_noted {
            self.stopped_noted = true;
            self.events.push(WorkerEvent::PollingStopped);
        }
        self.service_completion(store, at.now);
        self.phase = Phase::Stopped;
        WorkerAction::Sleep {
            until: self
                .completion
                .map(|backoff| at.now.saturating_add(backoff.wait(at.now))),
        }
    }

    fn apply_wake(&mut self, wake: Wake) {
        if wake.any() {
            self.nudge_pending = true;
        }
        if wake
            .health
            .is_some_and(HealthVerdict::arms_completion_retry)
        {
            self.completion_requested = true;
        }
    }

    /// Retries `complete_attempt` for the running version when the retry is armed and due: after `ApplyNotRecorded` the success verdict exists only once this succeeds.
    ///
    /// A cleared record asks for a poll so the success verdict goes out promptly.
    fn service_completion<K: OtaKv>(&mut self, store: &mut OtaStore<K>, now: Duration) {
        if self.completion.is_none() && self.completion_requested {
            self.completion = Some(CompletionBackoff::armed(&self.config.completion, now));
        }
        let Some(backoff) = self.completion else {
            return;
        };
        if !backoff.due(now) {
            return;
        }
        let result = retry_completion(store, self.config.running);
        self.events.push(WorkerEvent::Completion(result));
        if completion_step(result) == CompletionStep::Retry {
            self.completion = Some(backoff.failed(&self.config.completion, now));
        } else {
            self.completion = None;
            self.completion_requested = false;
            if result == CompletionTry::Completed {
                self.nudge_pending = true;
            }
        }
    }

    fn answer<K: OtaKv>(
        &mut self,
        store: &mut OtaStore<K>,
        outcome: RequestOutcome,
        admission_open: bool,
        at: Stamp,
    ) -> PollAnswer {
        let store_gate = store.offer_gate();
        let pending = store.pending_verdict();
        if let Err(e) = pending {
            self.events.push(WorkerEvent::PendingVerdictUnreadable(e));
        }
        let gate = if self.unreported_corruption {
            OfferGate::RecordsUnreadable
        } else if !admission_open {
            OfferGate::Blocked
        } else {
            store_gate
        };
        PollAnswer {
            outcome,
            gate,
            wall_clock: at.wall_secs(),
            random: at.random,
            verdict_pending: matches!(pending, Ok(Some(_))),
        }
    }

    // ── deployments ──────────────────────────────────────────────────────────────

    fn retrieved<K: OtaKv>(
        &mut self,
        store: &mut OtaStore<K>,
        href: &Href,
        outcome: RequestOutcome,
        at: Stamp,
    ) -> Step {
        let facts = AdmissionFacts {
            running: self.config.running,
            rolled_back: store.refused().ok().flatten(),
            floor: self.config.floor,
            partition_capacity: self.config.partition_capacity,
            chip: self.config.chip.clone(),
        };
        match admit_retrieved(href, &outcome, &facts, &self.endpoint) {
            DeploymentVerdict::NotRetrieved(transient) => {
                self.events.push(WorkerEvent::NotRetrieved(transient));
                Step::Done(Some(work(outcome, at)))
            }
            DeploymentVerdict::Unreportable(reason) => {
                self.events.push(WorkerEvent::Unreportable(reason));
                Step::Done(None)
            }
            DeploymentVerdict::Deferred(action) => {
                self.events.push(WorkerEvent::Deferred(action));
                Step::Done(None)
            }
            DeploymentVerdict::Refused(refusal) => {
                let verdict = DdiVerdict::refused(&refusal);
                self.events.push(WorkerEvent::Refused(refusal));
                self.persist_and_deliver(store, &verdict, at)
            }
            DeploymentVerdict::Admitted(offer) => {
                self.events.push(WorkerEvent::Admitted {
                    action: offer.action.clone(),
                    version: offer.version,
                    size: offer.size,
                });
                self.phase = Phase::AdmitHardware(offer);
                Step::Act(WorkerAction::ReadHardware(self.config.admission_read))
            }
        }
    }

    /// Begins the attempt of an admitted offer on the update slot the hardware names, then sends `proceeding`.
    fn admit<K: OtaKv>(
        &mut self,
        store: &mut OtaStore<K>,
        offer: AdmittedOffer,
        hw: Result<HardwareFacts, HardwareReadError>,
        at: Stamp,
    ) -> Step {
        let hw = match hw {
            Ok(hw) => hw,
            Err(e) => {
                self.events.push(WorkerEvent::HardwareUnreadable(e));
                return Step::Done(None);
            }
        };
        if hw.running_state == SlotState::PendingVerify {
            self.events.push(WorkerEvent::PendingVerify(offer.action));
            return Step::Done(None);
        }
        let (slot, slot_was_invalid) = match hw.update_slot {
            UpdateSlot::Present(slot, state) => (slot, state == SlotState::Invalid),
            UpdateSlot::Absent | UpdateSlot::Unreadable(_) => {
                return self.slot_unusable(store, offer.action, at);
            }
        };
        match store.begin_ddi_attempt(&offer, slot, slot_was_invalid) {
            Ok(id) => {
                self.events.push(WorkerEvent::AttemptBegun {
                    action: offer.action.clone(),
                    id,
                });
                let post = FeedbackPost::proceeding(offer.action.clone(), at.wall_millis);
                self.phase = Phase::AdmitProceeding(offer);
                Step::Act(WorkerAction::Send(DdiRequest::Feedback(post)))
            }
            Err(StoreError::ActionClosed) => {
                self.events.push(WorkerEvent::ActionClosed(offer.action));
                Step::Done(None)
            }
            Err(StoreError::NotAdmitted(_) | StoreError::VerdictPending) => {
                self.events.push(WorkerEvent::OfferBlocked(offer.action));
                Step::Done(None)
            }
            Err(StoreError::InvalidSlot) => self.slot_unusable(store, offer.action, at),
            Err(error) => {
                self.events.push(WorkerEvent::AttemptNotBegun {
                    action: offer.action,
                    error,
                });
                Step::Done(None)
            }
        }
    }

    fn slot_unusable<K: OtaKv>(
        &mut self,
        store: &mut OtaStore<K>,
        action: ActionId,
        at: Stamp,
    ) -> Step {
        self.events.push(WorkerEvent::SlotUnusable(action.clone()));
        let reason = FailReason::PartitionNotFound;
        match DdiVerdict::failed(action, reason) {
            Some(verdict) => self.persist_and_deliver(store, &verdict, at),
            None => {
                self.events.push(WorkerEvent::NoProfileCode(reason));
                Step::Done(None)
            }
        }
    }

    // ── verdicts ─────────────────────────────────────────────────────────────────

    /// Persists an attempt-less verdict and sends it at once; a pending verdict of another action wins and the server offers this action again.
    fn persist_and_deliver<K: OtaKv>(
        &mut self,
        store: &mut OtaStore<K>,
        verdict: &DdiVerdict,
        at: Stamp,
    ) -> Step {
        let note = store.persist_verdict(verdict);
        self.events.push(WorkerEvent::VerdictPersisted {
            action: verdict.action.clone(),
            note,
        });
        match note {
            Ok(VerdictNote::Written | VerdictNote::KeptPending) => self.deliver_verdict(store, at),
            _ => Step::Done(None),
        }
    }

    /// Sends the store's pending verdict when it is due.
    fn deliver_verdict<K: OtaKv>(&mut self, store: &mut OtaStore<K>, at: Stamp) -> Step {
        let verdict = match store.pending_verdict() {
            Ok(Some(verdict)) => verdict,
            Ok(None) => return Step::Done(None),
            Err(e) => {
                self.events.push(WorkerEvent::VerdictUnreadable(e));
                return Step::Done(None);
            }
        };
        let next = match self.sender.next(&verdict, at.now, at.wall_millis) {
            VerdictStep::Send(_) => Next::Send,
            VerdictStep::NotDue { at } => Next::NotDue(at),
            VerdictStep::Suspended => Next::Suspended,
            VerdictStep::InFlight => Next::InFlight,
        };
        let event = match next {
            Next::Send => {
                self.phase = Phase::Verdict(verdict.action.clone());
                return Step::Act(WorkerAction::Send(DdiRequest::Verdict {
                    verdict,
                    timestamp_ms: at.wall_millis,
                }));
            }
            Next::NotDue(at) => WorkerEvent::VerdictNotDue { at },
            Next::Suspended => WorkerEvent::VerdictSuspended,
            Next::InFlight => WorkerEvent::VerdictInFlight,
        };
        self.events.push(event);
        Step::Done(None)
    }

    fn verdict_answered<K: OtaKv>(
        &mut self,
        store: &mut OtaStore<K>,
        action: ActionId,
        outcome: RequestOutcome,
        at: Stamp,
    ) -> Step {
        match self.sender.answered(work(outcome, at)) {
            SendOutcome::Delivered(delivered) => {
                let marked = store.mark_verdict_delivered(&delivered);
                self.events.push(WorkerEvent::VerdictDelivered {
                    action: delivered,
                    marked,
                });
                Step::Done(None)
            }
            SendOutcome::Retry(hand_back) => {
                self.events.push(WorkerEvent::VerdictRetry(action));
                Step::Done(hand_back)
            }
            SendOutcome::Suspended(hand_back) => {
                self.events.push(WorkerEvent::VerdictSuspendedNow(action));
                Step::Done(Some(hand_back))
            }
            SendOutcome::Ignored => {
                self.events.push(WorkerEvent::VerdictAnswerIgnored);
                Step::Done(None)
            }
        }
    }

    /// Reports the next offered action `closed` + `failure` + 1005 without applying it, after the records proved unreadable at boot.
    fn report_unreadable<K: OtaKv>(&mut self, store: &mut OtaStore<K>, action: ActionId) -> Step {
        let persisted = store.persist_verdict(&DdiVerdict::records_unreadable(action.clone()));
        self.events
            .push(WorkerEvent::RecordsUnreadableReported { action, persisted });
        if persisted.is_err() {
            return Step::Done(None);
        }
        self.unreported_corruption = false;
        self.phase = Phase::UnreadableHardware;
        Step::Act(WorkerAction::ReadHardware(BusyRetry::default()))
    }

    /// Re-runs the boot reconciliation after the 1005 verdict write repaired the records, then sends the verdict.
    fn refresh<K: OtaKv>(
        &mut self,
        store: &mut OtaStore<K>,
        hw: Result<HardwareFacts, HardwareReadError>,
        at: Stamp,
    ) -> Step {
        if let Err(e) = hw {
            self.events.push(WorkerEvent::HardwareUnreadable(e));
        }
        let refreshed = refresh_after_repair(
            store,
            hw,
            Some(self.config.running),
            self.config.roll_back_now_at_boot,
        );
        match refreshed {
            Some(refreshed) => {
                let action = WorkerAction::SetFlags {
                    records_suspect: refreshed.records_suspect,
                    refuse_mark_valid: refreshed.refuse_mark_valid,
                };
                self.events.push(WorkerEvent::Refreshed(refreshed));
                self.phase = Phase::UnreadableFlags;
                Step::Act(action)
            }
            None => self.deliver_verdict(store, at),
        }
    }

    // ── cancels ──────────────────────────────────────────────────────────────────

    /// Runs the cancel machine until it needs the tier or ends.
    fn cancel_step<K: OtaKv>(
        &mut self,
        store: &mut OtaStore<K>,
        mut step: CancelStep,
        at: Stamp,
    ) -> Step {
        loop {
            step = match step {
                CancelStep::LookUp(stop_id) => {
                    if self.downloading.is_some() {
                        self.phase = Phase::CancelCheck(stop_id);
                        return Step::Act(WorkerAction::CheckDownload);
                    }
                    self.look_up(store, stop_id, at)
                }
                CancelStep::RequestStop => {
                    if self.downloading.is_some() {
                        self.phase = Phase::CancelStop;
                        return Step::Act(WorkerAction::RequestStop);
                    }
                    let request = CancelRequest::TooLate;
                    self.events.push(WorkerEvent::CancelStopRequested(request));
                    self.cancel
                        .poll(CancelInput::StopRequested(request), at.now, at.wall_millis)
                }
                CancelStep::AwaitDownload => {
                    if self.downloading.is_some() {
                        self.phase = Phase::CancelAwait;
                        return Step::Act(WorkerAction::AwaitDownload);
                    }
                    self.cancel
                        .poll(CancelInput::DownloadReturned, at.now, at.wall_millis)
                }
                CancelStep::Send(post) => {
                    self.phase = Phase::CancelSend;
                    return Step::Act(WorkerAction::Send(DdiRequest::Feedback(post)));
                }
                CancelStep::Done(hand_back) => {
                    self.events
                        .push(WorkerEvent::CancelEnded(self.cancel.result()));
                    return Step::Done(hand_back);
                }
                CancelStep::Ignored => {
                    self.events.push(WorkerEvent::CancelIgnored);
                    return Step::Done(None);
                }
            };
        }
    }

    fn look_up<K: OtaKv>(
        &mut self,
        store: &mut OtaStore<K>,
        stop_id: ActionId,
        at: Stamp,
    ) -> CancelStep {
        let target = store.cancel_target(&stop_id, self.downloading.as_ref());
        self.events
            .push(WorkerEvent::CancelTarget { stop_id, target });
        self.cancel
            .poll(CancelInput::Target(target), at.now, at.wall_millis)
    }
}

fn work(outcome: RequestOutcome, at: Stamp) -> WorkOutcome {
    WorkOutcome {
        outcome,
        wall_clock: at.wall_secs(),
        random: at.random,
    }
}

fn is_2xx(outcome: &RequestOutcome) -> bool {
    matches!(outcome, RequestOutcome::Response(r) if (200..300).contains(&r.status))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ota::ddi::wire::{Execution, ProfileCode};
    use crate::ota::ddi::{ControllerId, DdiResponse};
    use crate::ota::persist::fault_kv::FaultKv;
    use crate::ota::SlotId;

    const BASE: &str = "http://ddi.lab:8080/DEFAULT";
    const POLL_EMPTY: &str = r#"{"config":{"polling":{"sleep":"00:00:30"}}}"#;
    const POLL_DEPLOY: &str = r#"{"config":{"polling":{"sleep":"00:00:30"}},"_links":{"deploymentBase":{"href":"http://ddi.lab:8080/DEFAULT/controller/v1/dev-1/deploymentBase/7"}}}"#;
    const POLL_CANCEL: &str = r#"{"config":{"polling":{"sleep":"00:00:30"}},"_links":{"cancelAction":{"href":"http://ddi.lab:8080/DEFAULT/controller/v1/dev-1/cancelAction/7"}}}"#;
    const DEPLOYMENT: &str = r#"{"id":"7","deployment":{"download":"forced","update":"forced","chunks":[{"version":"1.1.0","metadata":[{"key":"chip","value":"esp32c3"}],"artifacts":[{"filename":"fw.bin","size":1234,"hashes":{"sha256":"2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"},"_links":{"download-http":{"href":"http://ddi.lab:8080/DEFAULT/fw.bin"}}}]}]}}"#;
    const CANCEL: &str = r#"{"id":"7","cancelAction":{"stopId":"7"}}"#;

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

    fn status(code: u16, body: &str) -> RequestOutcome {
        RequestOutcome::Response(DdiResponse {
            status: code,
            retry_after: None,
            body: body.as_bytes().to_vec(),
        })
    }

    fn settings() -> DdiSettings {
        DdiSettings::new(Version::new(1, 0, 0), "board").expect("settings")
    }

    fn worker(report_owed: bool) -> DdiWorker {
        let id = ControllerId::parse("dev-1").expect("id");
        let endpoint = DdiEndpoint::new(BASE, &id).expect("base");
        let flags = RuntimeFlags {
            admission_open: true,
            health_settled: true,
            refuse_mark_valid: false,
            roll_back_now_at_boot: false,
            records_suspect: false,
        };
        let chip = Chip::parse("esp32c3").expect("chip");
        let config = WorkerConfig::new(&settings(), chip, 0x1E_0000, &flags, report_owed);
        DdiWorker::new(endpoint, config)
    }

    fn store() -> (FaultKv, OtaStore<FaultKv>) {
        let kv = FaultKv::new();
        let s = OtaStore::open(kv.clone(), || 1);
        (kv, s)
    }

    fn hw(running_state: SlotState, update_slot: UpdateSlot) -> WorkerInput {
        WorkerInput::Hardware(Ok(HardwareFacts {
            running_slot: SlotId(0),
            running_state,
            update_slot,
        }))
    }

    fn valid_hw() -> WorkerInput {
        hw(
            SlotState::Valid,
            UpdateSlot::Present(SlotId(1), SlotState::Unknown),
        )
    }

    /// Starts the worker and answers its first poll with `body` at 0 s, through the once-per-boot configData PUT.
    fn first_poll(w: &mut DdiWorker, s: &mut OtaStore<FaultKv>, body: &str) -> WorkerAction {
        let start = WorkerInput::Start {
            health_settled: true,
        };
        assert_eq!(
            w.poll(s, start, at(0)),
            WorkerAction::Send(DdiRequest::Poll)
        );
        let answered = WorkerInput::PollAnswered {
            outcome: status(200, body),
            admission_open: true,
        };
        assert_eq!(
            w.poll(s, answered, at(0)),
            WorkerAction::Send(DdiRequest::ConfigData)
        );
        w.poll(s, WorkerInput::Answered(status(200, "")), at(0))
    }

    /// Runs the first poll up to the hardware read of the admitted offer.
    fn admitted(w: &mut DdiWorker, s: &mut OtaStore<FaultKv>) {
        let link = first_poll(w, s, POLL_DEPLOY);
        assert!(
            matches!(link, WorkerAction::Send(DdiRequest::Deployment(_))),
            "{link:?}"
        );
        let read = w.poll(s, WorkerInput::Answered(status(200, DEPLOYMENT)), at(0));
        assert_eq!(
            read,
            WorkerAction::ReadHardware(BusyRetry {
                tries: 4,
                delay: Duration::from_millis(500)
            })
        );
    }

    /// Runs an admitted offer up to its running download.
    fn downloading(w: &mut DdiWorker, s: &mut OtaStore<FaultKv>) {
        admitted(w, s);
        let proceeding = w.poll(s, valid_hw(), at(0));
        let WorkerAction::Send(DdiRequest::Feedback(post)) = proceeding else {
            panic!("{proceeding:?}");
        };
        assert_eq!(post.execution, Execution::Proceeding);
        let start = w.poll(s, WorkerInput::Answered(status(200, "")), at(0));
        assert!(
            matches!(&start, WorkerAction::StartDownload(offer) if offer.action.as_str() == "7"),
            "{start:?}"
        );
        assert_eq!(
            w.poll(s, WorkerInput::DownloadStarted(true), at(0)),
            WorkerAction::Sleep {
                until: Some(secs(30))
            }
        );
    }

    #[test]
    fn stamp_wake_and_request_helpers() {
        let stamp = Stamp {
            wall_millis: Some(1_700_000_000_999),
            ..at(0)
        };
        assert_eq!(stamp.wall_secs(), Some(1_700_000_000));
        assert_eq!(at(0).wall_secs(), None);

        let mut wake = Wake::default();
        assert!(!wake.any());
        wake.merge(Wake {
            nudge: true,
            ..Wake::default()
        });
        wake.merge(Wake {
            health: Some(HealthVerdict::Applied),
            download_ended: true,
            ..Wake::default()
        });
        assert_eq!(
            wake,
            Wake {
                nudge: true,
                health: Some(HealthVerdict::Applied),
                download_ended: true
            }
        );

        assert_eq!(DdiRequest::Poll.feedback(), None);
        let action = ActionId::parse("7").expect("id");
        let verdict = DdiVerdict::records_unreadable(action.clone());
        let request = DdiRequest::Verdict {
            verdict: verdict.clone(),
            timestamp_ms: Some(5),
        };
        assert_eq!(
            request.feedback(),
            Some(FeedbackPost::verdict(&verdict, Some(5)))
        );
        let post = FeedbackPost::proceeding(action, None);
        assert_eq!(DdiRequest::Feedback(post.clone()).feedback(), Some(post));
    }

    #[test]
    fn an_admitted_offer_begins_its_attempt_sends_proceeding_and_starts_the_download() {
        let (_kv, mut s) = store();
        let mut w = worker(false);
        downloading(&mut w, &mut s);
        assert!(matches!(s.read_attempt(), Ok(Some(_))));
        let events = w.drain_events();
        assert!(events.contains(&WorkerEvent::PollAnswered(PollVerdict::Ok)));
        assert!(events.contains(&WorkerEvent::ConfigData { accepted: true }));
        assert!(events.iter().any(
            |e| matches!(e, WorkerEvent::AttemptBegun { action, .. } if action.as_str() == "7")
        ));
        assert!(w.drain_events().is_empty(), "drained");
    }

    #[test]
    fn a_download_that_cannot_start_is_settled_busy() {
        let (_kv, mut s) = store();
        let mut w = worker(false);
        admitted(&mut w, &mut s);
        w.poll(&mut s, valid_hw(), at(0));
        w.poll(&mut s, WorkerInput::Answered(status(200, "")), at(0));
        assert_eq!(
            w.poll(&mut s, WorkerInput::DownloadStarted(false), at(0)),
            WorkerAction::ReadHardware(BusyRetry::default())
        );
        assert_eq!(
            w.poll(&mut s, valid_hw(), at(0)),
            WorkerAction::Sleep {
                until: Some(secs(30))
            }
        );
        assert!(w.drain_events().iter().any(|e| matches!(
            e,
            WorkerEvent::DownloadNotStarted {
                settled: SettleNote::Settled(_),
                ..
            }
        )));
        assert!(
            matches!(s.read_attempt(), Ok(None)),
            "a busy failure clears the attempt"
        );
    }

    #[test]
    fn unreadable_hardware_or_a_pending_image_defers_the_offer() {
        let (_kv, mut s) = store();
        let mut w = worker(false);
        admitted(&mut w, &mut s);
        assert_eq!(
            w.poll(
                &mut s,
                WorkerInput::Hardware(Err(HardwareReadError::Busy)),
                at(0)
            ),
            WorkerAction::Sleep {
                until: Some(secs(30))
            }
        );
        assert!(w
            .drain_events()
            .contains(&WorkerEvent::HardwareUnreadable(HardwareReadError::Busy)));

        let (_kv, mut s) = store();
        let mut w = worker(false);
        admitted(&mut w, &mut s);
        w.poll(
            &mut s,
            hw(
                SlotState::PendingVerify,
                UpdateSlot::Present(SlotId(1), SlotState::Valid),
            ),
            at(0),
        );
        assert!(w
            .drain_events()
            .iter()
            .any(|e| matches!(e, WorkerEvent::PendingVerify(_))));
        assert!(matches!(s.read_attempt(), Ok(None)), "no attempt begun");
    }

    #[test]
    fn an_absent_update_slot_is_reported_failed_and_delivered() {
        let (_kv, mut s) = store();
        let mut w = worker(false);
        admitted(&mut w, &mut s);
        let send = w.poll(&mut s, hw(SlotState::Valid, UpdateSlot::Absent), at(0));
        let WorkerAction::Send(request @ DdiRequest::Verdict { .. }) = send else {
            panic!("{send:?}");
        };
        let post = request.feedback().expect("a feedback");
        assert_eq!(
            post.code,
            ProfileCode::for_reason(FailReason::PartitionNotFound)
        );
        assert_eq!(
            w.poll(&mut s, WorkerInput::Answered(status(200, "")), at(0)),
            WorkerAction::Sleep {
                until: Some(secs(30))
            }
        );
        assert_eq!(s.pending_verdict(), Ok(None), "delivered and marked");
        assert!(w
            .drain_events()
            .iter()
            .any(|e| matches!(e, WorkerEvent::SlotUnusable(_))));
    }

    #[test]
    fn a_cancel_asks_whether_the_download_runs_then_stops_and_awaits_it() {
        let (_kv, mut s) = store();
        let mut w = worker(false);
        downloading(&mut w, &mut s);
        assert_eq!(
            w.poll(&mut s, WorkerInput::Woken(Wake::default()), at(30)),
            WorkerAction::Send(DdiRequest::Poll)
        );
        let get = w.poll(
            &mut s,
            WorkerInput::PollAnswered {
                outcome: status(200, POLL_CANCEL),
                admission_open: true,
            },
            at(30),
        );
        assert!(
            matches!(get, WorkerAction::Send(DdiRequest::CancelAction(_))),
            "{get:?}"
        );
        assert_eq!(
            w.poll(&mut s, WorkerInput::Answered(status(200, CANCEL)), at(30)),
            WorkerAction::CheckDownload
        );
        assert_eq!(
            w.poll(&mut s, WorkerInput::DownloadRunning(true), at(30)),
            WorkerAction::RequestStop
        );
        assert_eq!(
            w.poll(
                &mut s,
                WorkerInput::StopRequested(CancelRequest::Accepted),
                at(30)
            ),
            WorkerAction::AwaitDownload
        );
        let answer = w.poll(&mut s, WorkerInput::DownloadEnded, at(31));
        let WorkerAction::Send(DdiRequest::Feedback(post)) = answer else {
            panic!("{answer:?}");
        };
        assert_eq!(
            post,
            FeedbackPost::cancel_acknowledged(post.action.clone(), None)
        );
        assert_eq!(
            w.poll(&mut s, WorkerInput::Answered(status(200, "")), at(31)),
            WorkerAction::Sleep {
                until: Some(secs(61))
            }
        );
        assert!(w
            .drain_events()
            .contains(&WorkerEvent::CancelEnded(Some(CancelResult::Acknowledged))));
    }

    #[test]
    fn a_cancel_after_the_download_ended_looks_up_without_it() {
        let (_kv, mut s) = store();
        let mut w = worker(false);
        downloading(&mut w, &mut s);
        w.poll(&mut s, WorkerInput::Woken(Wake::default()), at(30));
        w.poll(
            &mut s,
            WorkerInput::PollAnswered {
                outcome: status(200, POLL_CANCEL),
                admission_open: true,
            },
            at(30),
        );
        w.poll(&mut s, WorkerInput::Answered(status(200, CANCEL)), at(30));
        let next = w.poll(&mut s, WorkerInput::DownloadRunning(false), at(30));
        assert!(
            matches!(next, WorkerAction::Send(DdiRequest::Feedback(_))),
            "no stop is requested: {next:?}"
        );
        assert!(w.drain_events().iter().any(|e| matches!(
            e,
            WorkerEvent::CancelTarget {
                target: CancelTarget::Activated | CancelTarget::NotRunning,
                ..
            }
        )));
    }

    #[test]
    fn a_nudge_polls_at_once_but_never_within_10_s_of_the_last_poll() {
        let (_kv, mut s) = store();
        let mut w = worker(false);
        assert_eq!(
            first_poll(&mut w, &mut s, POLL_EMPTY),
            WorkerAction::Sleep {
                until: Some(secs(30))
            }
        );
        let nudge = Wake {
            nudge: true,
            ..Wake::default()
        };
        assert_eq!(
            w.poll(&mut s, WorkerInput::Woken(nudge), at(3)),
            WorkerAction::Sleep {
                until: Some(secs(10))
            }
        );
        assert_eq!(
            w.poll(&mut s, WorkerInput::Woken(Wake::default()), at(10)),
            WorkerAction::Send(DdiRequest::Poll)
        );
        let events = w.drain_events();
        assert!(events.contains(&WorkerEvent::Nudge(Nudge::TooSoon)));
        assert!(events.contains(&WorkerEvent::Nudge(Nudge::Moved)));
    }

    #[test]
    fn apply_not_recorded_arms_the_completion_retry_until_the_record_clears() {
        let (kv, mut s) = store();
        s.begin_attempt(Version::new(1, 0, 0), SlotId(1), false)
            .expect("attempt");
        kv.corrupt("rej_ver");
        let mut w = worker(false);
        first_poll(&mut w, &mut s, POLL_EMPTY);
        let health = Wake {
            health: Some(HealthVerdict::ApplyNotRecorded),
            ..Wake::default()
        };
        assert_eq!(
            w.poll(&mut s, WorkerInput::Woken(health), at(5)),
            WorkerAction::Sleep {
                until: Some(secs(6))
            },
            "armed: the first try is due after complete_retry"
        );
        assert_eq!(
            w.poll(&mut s, WorkerInput::Woken(Wake::default()), at(6)),
            WorkerAction::Sleep {
                until: Some(secs(8))
            },
            "still failing: the backoff doubles"
        );
        kv.revive();
        assert_eq!(
            w.poll(&mut s, WorkerInput::Woken(Wake::default()), at(8)),
            WorkerAction::Sleep {
                until: Some(secs(10))
            },
            "cleared: the kept poll request waits for the 10 s"
        );
        assert!(matches!(s.read_attempt(), Ok(None)));
        assert_eq!(
            w.poll(&mut s, WorkerInput::Woken(Wake::default()), at(10)),
            WorkerAction::Send(DdiRequest::Poll)
        );
        let events = w.drain_events();
        assert!(events.contains(&WorkerEvent::Completion(CompletionTry::StillFailing)));
        assert!(events.contains(&WorkerEvent::Completion(CompletionTry::Completed)));
    }

    #[test]
    fn records_unreadable_reports_1005_refreshes_the_flags_and_sends() {
        let (_kv, mut s) = store();
        let mut w = worker(true);
        assert_eq!(
            first_poll(&mut w, &mut s, POLL_DEPLOY),
            WorkerAction::ReadHardware(BusyRetry::default())
        );
        assert_eq!(
            w.poll(&mut s, valid_hw(), at(0)),
            WorkerAction::SetFlags {
                records_suspect: false,
                refuse_mark_valid: false
            }
        );
        let send = w.poll(&mut s, WorkerInput::FlagsSet, at(0));
        let WorkerAction::Send(request @ DdiRequest::Verdict { .. }) = send else {
            panic!("{send:?}");
        };
        assert_eq!(
            request.feedback().expect("post").code,
            Some(ProfileCode::Device)
        );
    }

    #[test]
    fn a_410_stops_polling_and_wakes_keep_only_the_record_upkeep() {
        let (_kv, mut s) = store();
        let mut w = worker(false);
        w.poll(
            &mut s,
            WorkerInput::Start {
                health_settled: true,
            },
            at(0),
        );
        assert_eq!(
            w.poll(
                &mut s,
                WorkerInput::PollAnswered {
                    outcome: status(410, ""),
                    admission_open: true
                },
                at(0)
            ),
            WorkerAction::Sleep { until: None }
        );
        let nudge = Wake {
            nudge: true,
            ..Wake::default()
        };
        assert_eq!(
            w.poll(&mut s, WorkerInput::Woken(nudge), at(50)),
            WorkerAction::Sleep { until: None }
        );
        let events = w.drain_events();
        assert_eq!(
            events
                .iter()
                .filter(|e| **e == WorkerEvent::PollingStopped)
                .count(),
            1
        );
        assert!(events.contains(&WorkerEvent::NudgeAfterStop));
    }

    #[test]
    fn out_of_phase_input_is_ignored_and_changes_nothing() {
        let (kv, mut s) = store();
        let mut w = worker(false);
        let strays = [
            WorkerInput::Woken(Wake::default()),
            WorkerInput::PollAnswered {
                outcome: status(200, POLL_EMPTY),
                admission_open: true,
            },
            WorkerInput::Answered(status(200, "")),
            valid_hw(),
            WorkerInput::FlagsSet,
            WorkerInput::DownloadStarted(true),
            WorkerInput::DownloadRunning(true),
            WorkerInput::StopRequested(CancelRequest::Accepted),
            WorkerInput::DownloadEnded,
        ];
        for input in strays.clone() {
            assert_eq!(
                w.poll(&mut s, input.clone(), at(0)),
                WorkerAction::Ignored,
                "before Start: {input:?}"
            );
        }
        assert_eq!(
            w.poll(
                &mut s,
                WorkerInput::Start {
                    health_settled: true
                },
                at(0)
            ),
            WorkerAction::Send(DdiRequest::Poll)
        );
        for input in strays
            .into_iter()
            .filter(|i| !matches!(i, WorkerInput::PollAnswered { .. }))
            .chain([WorkerInput::Start {
                health_settled: false,
            }])
        {
            assert_eq!(
                w.poll(&mut s, input.clone(), at(1)),
                WorkerAction::Ignored,
                "while polling: {input:?}"
            );
        }
        assert_eq!(kv.commits(), 0, "an ignored input writes nothing");
        assert!(w.drain_events().is_empty(), "an ignored input logs nothing");
        assert_eq!(
            w.poll(
                &mut s,
                WorkerInput::PollAnswered {
                    outcome: status(200, POLL_EMPTY),
                    admission_open: true
                },
                at(1)
            ),
            WorkerAction::Send(DdiRequest::ConfigData),
            "the pending poll is still answered"
        );
    }
}
