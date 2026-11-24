//! [`DdiRunner`]: the task of the esp-hal DDI runtime, the worker and the download of the ESP-IDF tier's two threads as two halves of one future.
//!
//! Every decision is made by the machines of `juggler::ota::ddi` ([`DdiWorker`], [`DownloadFlow`], ADR 023 rung 3); this file only performs each action they ask for with one request, one hardware read, one flag update or one wait, and logs the events they hand back (ADR 023 decision 7).
//! The two halves share the running download (its offer, its cancel switch and whether it still runs) through a critical-section mutex that is never held across an `.await`.
//! The store is borrowed for one machine call only ([`with_store`]), never across a request, a flash write of the download or a wait.
//! Nothing here restarts the device except the app's restart callback, after the new image was selected for the next boot.

use core::cell::RefCell;
use core::convert::Infallible;
use core::fmt;
use core::time::Duration;

use critical_section::Mutex;
use embassy_futures::join::join;
use embassy_futures::select::select;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use embassy_time::{Instant, Timer};
use juggler::ota::ddi::wire::{Execution, ProfileCode};
use juggler::ota::ddi::{
    AdmittedOffer, DdiRequest, DdiResponse, DdiSettings, DdiTransportError, DdiWorker,
    DownloadFlow, DownloadFlowAction, DownloadFlowInput, FeedbackPath, FeedbackPost, Nudge,
    PollVerdict, RequestOutcome, SettleNote, Stamp, Wake, WorkerAction, WorkerEvent, WorkerInput,
    MIN_POLL_INTERVAL,
};
use juggler::ota::health::CompletionTry;
use juggler::ota::persist::{VerdictDelivery, VerdictNote};
use juggler::ota::{CancelRequest, FailReason, FailedDownload, StoreError};

use super::handle::DdiShared;
use super::transport::DdiTransport;
use super::RestartFn;
use crate::ota::flags::OtaFlags;
use crate::ota::flash::SharedFlash;
use crate::ota::manager::{to_core, to_embassy};
use crate::ota::records::{random_u32, Records, Store};
use crate::ota::{read_hardware_facts, EspHalOtaManager, OtaCancel, OtaManagerConfig};

/// How often the wait between two download attempts looks at the cancel switch.
const CANCEL_POLL: embassy_time::Duration = embassy_time::Duration::from_millis(250);

/// The pause before a store call is tried again when the store was lent out (cannot happen in a correct firmware).
const STORE_RETRY: embassy_time::Duration = embassy_time::Duration::from_millis(10);

/// Runs `f` on the record store for one synchronous call; a store that is lent out (a nested borrow, which no correct caller makes) is tried again after a short pause instead of panicking.
pub(super) async fn with_store<R>(records: Records, mut f: impl FnMut(&mut Store) -> R) -> R {
    loop {
        if let Some(result) = records.with(&mut f) {
            return result;
        }
        log::error!("[ota] the OTA record store is lent out; retrying");
        Timer::after(STORE_RETRY).await;
    }
}

/// The download both halves share: the offer handed over, whether a download runs, and its cancel switch.
pub(super) struct Link {
    slot: Mutex<RefCell<Slot>>,
    /// One switch for every download of the boot, re-armed by the worker before each start.
    cancel: OtaCancel,
    /// Rung by the worker when it hands over an offer.
    posted: Signal<CriticalSectionRawMutex, ()>,
    /// Rung by the download half when a download returned.
    ended: Signal<CriticalSectionRawMutex, ()>,
}

#[derive(Default)]
struct Slot {
    offer: Option<AdmittedOffer>,
    running: bool,
}

impl Link {
    pub(super) fn new() -> Self {
        Self {
            slot: Mutex::new(RefCell::new(Slot::default())),
            cancel: OtaCancel::for_retries(),
            posted: Signal::new(),
            ended: Signal::new(),
        }
    }

    /// Hands `offer` to the download half with a re-armed cancel switch; `false` when a download still runs.
    fn start(&self, offer: AdmittedOffer) -> bool {
        let started = critical_section::with(|cs| {
            let mut slot = self.slot.borrow_ref_mut(cs);
            if slot.running {
                return false;
            }
            self.cancel.rearm_for_retries();
            slot.offer = Some(offer);
            slot.running = true;
            true
        });
        if started {
            self.posted.signal(());
        }
        started
    }

    fn running(&self) -> bool {
        critical_section::with(|cs| self.slot.borrow_ref(cs).running)
    }

    fn take_offer(&self) -> Option<AdmittedOffer> {
        critical_section::with(|cs| self.slot.borrow_ref_mut(cs).offer.take())
    }

    /// The download returned: it no longer runs, and a worker waiting for it is woken.
    fn finish(&self) {
        critical_section::with(|cs| self.slot.borrow_ref_mut(cs).running = false);
        self.ended.signal(());
    }

    async fn next_offer(&self) -> AdmittedOffer {
        loop {
            if let Some(offer) = self.take_offer() {
                return offer;
            }
            self.posted.wait().await;
        }
    }

    async fn wait_ended(&self) {
        while self.running() {
            self.ended.wait().await;
        }
    }
}

/// What both halves share: everything of the runner but the worker machine.
pub(super) struct Core {
    pub(super) transport: DdiTransport,
    pub(super) settings: DdiSettings,
    pub(super) records: Records,
    pub(super) flash: &'static SharedFlash,
    pub(super) shared: &'static DdiShared,
    pub(super) restart: RestartFn,
    /// The origin of every machine time ([`Stamp::now`]): the runtime's start.
    pub(super) origin: Instant,
    pub(super) link: Link,
}

/// Experimental: API may change before 1.0.
///
/// The task of a started DDI runtime, returned by `DdiRuntime::start` next to the `DdiHandle`.
///
/// Spawn [`run`](Self::run) in its own embassy task; it never returns.
/// It is the ESP-IDF tier's `ota-worker` and `ota-download` threads as one future: the worker polls the server, admits offers, sends every feedback and answers cancels while the download of an admitted offer streams into the update slot, both on the one executor.
///
/// ```ignore
/// #[embassy_executor::task]
/// async fn ddi_task(runner: DdiRunner) -> ! {
///     runner.run().await
/// }
///
/// let (handle, runner) = runtime.start(records, stack)?;
/// spawner.spawn(ddi_task(runner).unwrap());
/// ```
pub struct DdiRunner {
    pub(super) worker: DdiWorker,
    pub(super) core: Core,
}

impl fmt::Debug for DdiRunner {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DdiRunner")
            .field("transport", &self.core.transport)
            .finish_non_exhaustive()
    }
}

impl DdiRunner {
    /// Experimental: API may change before 1.0.
    ///
    /// Runs the worker and the download until the device restarts; never returns.
    ///
    /// The restart into a new image goes through the app's restart callback; if it returns, the download half parks and the worker keeps polling.
    pub async fn run(self) -> ! {
        let DdiRunner { worker, core } = self;
        let core = &core;
        let (never, _) = join(core.worker_loop(worker), core.download_loop()).await;
        match never {}
    }
}

impl Core {
    fn flags(&self) -> OtaFlags {
        OtaFlags::new(&self.shared.flags)
    }

    /// The machine inputs read for one call: the time since the start, no wall clock (a gap of this tier), a fresh random value.
    fn stamp(&self) -> Stamp {
        Stamp {
            now: to_core(self.origin.elapsed()),
            wall_millis: None,
            random: random_u32(),
        }
    }

    /// The instant `since_origin` on the machines' clock.
    fn instant_at(&self, since_origin: Duration) -> Instant {
        self.origin
            .checked_add(to_embassy(since_origin))
            .unwrap_or(Instant::MAX)
    }

    // ── the worker half ──────────────────────────────────────────────────────────

    async fn worker_loop(&self, mut worker: DdiWorker) -> Infallible {
        let mut input = WorkerInput::Start {
            health_settled: self.flags().health_settled(),
        };
        loop {
            let at = self.stamp();
            let action =
                with_store(self.records, |store| worker.poll(store, input.clone(), at)).await;
            for event in worker.drain_events() {
                log_event(event);
            }
            input = match action {
                WorkerAction::Send(request) => self.perform(&worker, request).await,
                WorkerAction::ReadHardware(retry) => {
                    WorkerInput::Hardware(read_hardware_facts(retry))
                }
                WorkerAction::SetFlags {
                    records_suspect,
                    refuse_mark_valid,
                } => {
                    let flags = self.flags();
                    flags.set_records_suspect(records_suspect);
                    flags.set_refuse_mark_valid(refuse_mark_valid);
                    WorkerInput::FlagsSet
                }
                WorkerAction::StartDownload(offer) => {
                    WorkerInput::DownloadStarted(self.link.start(offer))
                }
                WorkerAction::CheckDownload => WorkerInput::DownloadRunning(self.link.running()),
                WorkerAction::RequestStop => WorkerInput::StopRequested(if self.link.running() {
                    self.link.cancel.cancel()
                } else {
                    CancelRequest::TooLate
                }),
                WorkerAction::AwaitDownload => {
                    self.link.wait_ended().await;
                    WorkerInput::DownloadEnded
                }
                WorkerAction::Sleep { until } => WorkerInput::Woken(self.sleep(until).await),
                WorkerAction::Ignored => {
                    log::error!("[ota] the DDI worker ignored its input; internal error");
                    Timer::after(to_embassy(MIN_POLL_INTERVAL)).await;
                    WorkerInput::Woken(Wake::default())
                }
            };
        }
    }

    /// Sleeps until `until` on the machines' clock (for ever with `None`) or until a wake-up; returns the wake-ups kept so far, at once when some were kept while the worker was busy.
    async fn sleep(&self, until: Option<Duration>) -> Wake {
        let waker = &self.shared.waker;
        waker.bell().reset();
        let kept = waker.take();
        if kept.any() {
            return kept;
        }
        match until {
            Some(until) => {
                select(waker.bell().wait(), Timer::at(self.instant_at(until))).await;
            }
            None => waker.bell().wait().await,
        }
        waker.take()
    }

    /// One request of the worker through the transport.
    async fn perform(&self, worker: &DdiWorker, request: DdiRequest) -> WorkerInput {
        let outcome = match request {
            DdiRequest::Poll => {
                let outcome = outcome(self.transport.poll().await, "poll");
                return WorkerInput::PollAnswered {
                    outcome,
                    admission_open: self.flags().admission_open(),
                };
            }
            DdiRequest::ConfigData => outcome(
                self.transport.config_data(&worker.config_data()).await,
                "configData",
            ),
            DdiRequest::Deployment(href) => {
                outcome(self.transport.deployment(&href).await, "deployment GET")
            }
            DdiRequest::CancelAction(href) => outcome(
                self.transport.cancel_action(&href).await,
                "cancelAction GET",
            ),
            DdiRequest::Feedback(post) => self.feedback(&post).await,
            DdiRequest::Verdict {
                verdict,
                timestamp_ms,
            } => {
                self.feedback(&FeedbackPost::verdict(&verdict, timestamp_ms))
                    .await
            }
        };
        WorkerInput::Answered(outcome)
    }

    /// POSTs a feedback on its path; a `proceeding` is logged as on the ESP-IDF tier.
    async fn feedback(&self, post: &FeedbackPost<'_>) -> RequestOutcome {
        let result = match post.path {
            FeedbackPath::Deployment => {
                self.transport
                    .deployment_feedback(post.action.clone(), &post.feedback())
                    .await
            }
            FeedbackPath::Cancel => {
                self.transport
                    .cancel_feedback(post.action.clone(), &post.feedback())
                    .await
            }
        };
        let outcome = outcome(result, "feedback");
        if post.execution == Execution::Proceeding {
            let action = &post.action;
            match &outcome {
                RequestOutcome::Response(r) if (200..300).contains(&r.status) => {
                    log::info!("[ota] proceeding sent for action {action}");
                }
                other => log::warn!(
                    "[ota] proceeding for action {action} answered {}",
                    Describe(other)
                ),
            }
        }
        outcome
    }

    // ── the download half ────────────────────────────────────────────────────────

    async fn download_loop(&self) -> Infallible {
        loop {
            let offer = self.link.next_offer().await;
            self.download(&offer).await;
            self.link.finish();
            self.shared.waker.notify(Wake {
                download_ended: true,
                ..Wake::default()
            });
        }
    }

    /// One admitted offer through the [`DownloadFlow`]: returns when the download failed and was settled; after a success it restarts through the app and never returns.
    async fn download(&self, offer: &AdmittedOffer) {
        let mut flow = DownloadFlow::new(offer.action.clone(), &self.settings);
        let mut input = DownloadFlowInput::Start;
        loop {
            let at = self.stamp();
            let action =
                with_store(self.records, |store| flow.poll(store, input.clone(), at)).await;
            input = match action {
                DownloadFlowAction::Attempt { remaining } => self.attempt(offer, remaining).await,
                DownloadFlowAction::Wait {
                    until,
                    error,
                    attempts,
                } => {
                    log::warn!(
                        "[ota] download of action {} failed ({}), attempt {attempts}; retrying in {} s",
                        offer.action,
                        error.code(),
                        until.saturating_sub(at.now).as_secs()
                    );
                    DownloadFlowInput::WaitEnded {
                        cancel_requested: self.wait_or_cancel(until).await,
                    }
                }
                DownloadFlowAction::Close => DownloadFlowInput::Closed {
                    cancel_first: self.link.cancel.close(),
                },
                DownloadFlowAction::ReadHardware(retry) => {
                    DownloadFlowInput::Hardware(read_hardware_facts(retry))
                }
                DownloadFlowAction::Ended(end) => {
                    log::error!(
                        "[ota] download of action {} failed: {}",
                        offer.action,
                        end.reason.code()
                    );
                    log_settle(end.settled, FailReason::Ota(end.reason));
                    return;
                }
                DownloadFlowAction::Selected {
                    recorded,
                    proceeding,
                } => {
                    if let Err(e) = recorded {
                        log::error!(
                            "[ota] the boot selection was not recorded: {e}; restarting anyway"
                        );
                    }
                    DownloadFlowInput::Answered(self.feedback(&proceeding).await)
                }
                DownloadFlowAction::RestartAt(at_time) => {
                    log::info!(
                        "[ota] action {}: boot slot selected; restarting in {} ms",
                        offer.action,
                        at_time.saturating_sub(at.now).as_millis()
                    );
                    Timer::at(self.instant_at(at_time)).await;
                    (self.restart)();
                    DownloadFlowInput::RestartReturned
                }
                DownloadFlowAction::Parked => {
                    log::error!("[ota] the restart callback returned; the download parks");
                    let never = core::future::pending::<Infallible>().await;
                    match never {}
                }
                DownloadFlowAction::Ignored => {
                    log::error!("[ota] the DDI download flow ignored its input; internal error");
                    return;
                }
            };
        }
    }

    /// One attempt on a fresh manager over the shared flash that holds `remaining` of the download deadline.
    async fn attempt(&self, offer: &AdmittedOffer, remaining: Duration) -> DownloadFlowInput {
        let mut retry_after = None;
        let config = OtaManagerConfig {
            timeout_secs: self.settings.timings.firmware_timeout_secs,
        };
        let result = match EspHalOtaManager::new_shared(config, self.flash) {
            Ok(manager) => {
                let mut manager = manager.with_deadline(to_embassy(remaining));
                self.transport
                    .download_capturing(
                        &mut manager,
                        &offer.artifact,
                        &offer.sha256,
                        offer.size,
                        &self.link.cancel,
                        &mut retry_after,
                    )
                    .await
            }
            Err(e) => Err(e),
        };
        DownloadFlowInput::AttemptEnded {
            result,
            retry_after,
        }
    }

    /// Waits until `until` in slices of [`CANCEL_POLL`]; `true` when a cancel was requested meanwhile.
    async fn wait_or_cancel(&self, until: Duration) -> bool {
        let end = self.instant_at(until);
        loop {
            if self.link.cancel.is_requested() {
                return true;
            }
            let now = Instant::now();
            if now >= end {
                return false;
            }
            Timer::after((end - now).min(CANCEL_POLL)).await;
        }
    }
}

/// The tier-neutral form of a transport result; an error raised before any request was sent reads as unreachable so every machine is answered.
fn outcome(result: Result<DdiResponse, DdiTransportError>, what: &str) -> RequestOutcome {
    match result {
        Ok(response) => RequestOutcome::Response(response),
        Err(e) => match e.clone().into_outcome() {
            Some(outcome) => outcome,
            None => {
                log::error!("[ota] DDI {what} was not sent: {e}");
                RequestOutcome::Unreachable
            }
        },
    }
}

/// What a log line may say about an outcome: the status and the failure kind, never the body (it is server-supplied).
struct Describe<'a>(&'a RequestOutcome);

impl fmt::Display for Describe<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            RequestOutcome::Response(r) => write!(f, "status {}", r.status),
            RequestOutcome::BodyFailed {
                status, failure, ..
            } => write!(f, "status {status}, body {failure:?}"),
            RequestOutcome::Timeout => f.write_str("timeout"),
            RequestOutcome::Unreachable => f.write_str("server unreachable"),
            RequestOutcome::Redirect { status } => write!(f, "redirect {status}"),
            RequestOutcome::ForeignOrigin => f.write_str("foreign origin"),
        }
    }
}

/// How a failed download was settled, in the ESP-IDF tier's words.
fn log_settle(note: SettleNote, reason: FailReason) {
    match note {
        SettleNote::Settled(FailedDownload::Cleared) => {
            if ProfileCode::for_reason(reason).is_some() {
                log::info!("[ota] the failed attempt was cleared; its verdict awaits delivery");
            } else {
                log::info!("[ota] the failed attempt was cleared");
            }
        }
        SettleNote::Settled(other) => {
            log::warn!("[ota] the failed attempt is kept for the next boot: {other:?}");
        }
        SettleNote::HardwareUnreadable(e) => {
            log::warn!("[ota] the failed attempt is kept, hardware unreadable: {e}");
        }
        SettleNote::Store(e) => {
            log::warn!("[ota] the failed attempt is kept, records unreadable: {e}");
        }
    }
}

/// Logs one worker event, in the ESP-IDF worker's words where it has them; server-supplied values never appear.
fn log_event(event: WorkerEvent) {
    match event {
        WorkerEvent::BootedForAction(action) => {
            log::info!("[ota] the new image booted for action {action}");
        }
        WorkerEvent::RunningActionUnreadable(e) => {
            log::debug!("[ota] the running DDI action is unreadable: {e}");
        }
        WorkerEvent::PollAnswered(PollVerdict::LinksRefused(case)) => {
            log::warn!("[ota] DDI poll link refused: {case}");
        }
        WorkerEvent::PollAnswered(verdict) => log::info!("[ota] DDI poll answered: {verdict:?}"),
        WorkerEvent::PollIgnored => {
            log::error!("[ota] the DDI poll machine ignored its input; ticking");
        }
        WorkerEvent::PollingStopped => {
            log::warn!("[ota] the DDI server answered 410 Gone; polling has ended until the next boot");
        }
        WorkerEvent::NudgeAfterStop => {
            log::info!("[ota] nudge not applied: polling has ended until the next boot");
        }
        WorkerEvent::Nudge(Nudge::Moved | Nudge::AlreadyDue) => {
            log::info!("[ota] nudge: polling now");
        }
        WorkerEvent::Nudge(Nudge::TooSoon) => {
            log::debug!("[ota] nudge: too soon after the last poll; kept");
        }
        WorkerEvent::Nudge(Nudge::Throttled) => {
            log::info!("[ota] nudge not applied: a Retry-After wait is running");
        }
        WorkerEvent::Nudge(Nudge::Stopped) => {
            log::info!("[ota] nudge not applied: polling has ended");
        }
        WorkerEvent::Nudge(_) => {}
        WorkerEvent::PendingVerdictUnreadable(e) | WorkerEvent::VerdictUnreadable(e) => {
            log::warn!("[ota] the pending verdict is unreadable: {e}");
        }
        WorkerEvent::ConfigData { accepted: true } => log::info!("[ota] DDI configData sent"),
        WorkerEvent::ConfigData { accepted: false } => {
            log::warn!("[ota] DDI configData not accepted");
        }
        WorkerEvent::NotRetrieved(t) => log::warn!("[ota] deployment not retrieved: {t:?}"),
        WorkerEvent::Unreportable(reason) => {
            log::warn!("[ota] deployment refused ({reason}) and no action id to report it to");
        }
        WorkerEvent::Deferred(action) => {
            log::info!("[ota] deployment {action} deferred by the server (skip)");
        }
        WorkerEvent::Refused(refusal) => match refusal.url_case {
            Some(case) => log::warn!(
                "[ota] deployment {} refused: {:?} {} ({case})",
                refusal.action,
                refusal.code,
                refusal.reason
            ),
            None => log::warn!(
                "[ota] deployment {} refused: {:?} {}",
                refusal.action,
                refusal.code,
                refusal.reason
            ),
        },
        WorkerEvent::Admitted {
            action,
            version,
            size,
        } => log::info!("[ota] deployment {action} admitted: version {version}, {size} bytes"),
        WorkerEvent::HardwareUnreadable(e) => {
            log::warn!("[ota] hardware unreadable ({e}); the offer waits for the next poll");
        }
        WorkerEvent::PendingVerify(_) => {
            log::info!("[ota] the running image is pending verification; the offer is deferred");
        }
        WorkerEvent::SlotUnusable(_) => {
            log::error!("[ota] the update slot is unusable; the offer is reported failed");
        }
        WorkerEvent::AttemptBegun { action, id } => {
            log::info!("[ota] attempt {id} begun for action {action}");
        }
        WorkerEvent::ActionClosed(action) => {
            log::warn!("[ota] action {action} is closed; it is never applied again");
        }
        WorkerEvent::OfferBlocked(_) => {
            log::info!("[ota] the offer is deferred: an earlier attempt or verdict is open");
        }
        WorkerEvent::AttemptNotBegun { error, .. } => {
            log::warn!("[ota] attempt not begun: {error}");
        }
        WorkerEvent::DownloadNotStarted { action, settled } => {
            log::error!("[ota] the download of action {action} could not be started");
            log_settle(settled, FailReason::Busy);
        }
        WorkerEvent::NoProfileCode(reason) => {
            log::info!("[ota] failure {reason} has no profile code; it is never reported");
        }
        WorkerEvent::VerdictPersisted { action, note } => match note {
            Ok(VerdictNote::Written) => log::info!("[ota] verdict for action {action} persisted"),
            Ok(VerdictNote::KeptPending) => {
                log::info!("[ota] an earlier verdict for action {action} is kept");
            }
            Ok(VerdictNote::AlreadyClosed) => {
                log::info!("[ota] action {action} is already closed");
            }
            Err(StoreError::VerdictPending) => log::warn!(
                "[ota] verdict for action {action} refused: another verdict awaits delivery; the server offers it again"
            ),
            Err(e) => log::warn!("[ota] verdict for action {action} not persisted: {e}"),
        },
        WorkerEvent::VerdictNotDue { at } => {
            log::debug!("[ota] the verdict is sent again no sooner than {at:?}");
        }
        WorkerEvent::VerdictSuspended => {
            log::debug!("[ota] verdict sending is suspended until a poll is accepted");
        }
        WorkerEvent::VerdictInFlight => {
            log::error!("[ota] a verdict send is still in flight; internal error");
        }
        WorkerEvent::VerdictDelivered { action, marked } => match marked {
            Ok(VerdictDelivery::Marked) => {
                log::info!("[ota] verdict for action {action} delivered; the action is closed");
            }
            Ok(other) => {
                log::warn!("[ota] verdict for action {action} was not marked delivered: {other:?}");
            }
            Err(e) => log::warn!("[ota] verdict for action {action} delivered but not marked: {e}"),
        },
        WorkerEvent::VerdictRetry(action) => {
            log::warn!("[ota] verdict for action {action} not delivered; next try in 30 s");
        }
        WorkerEvent::VerdictSuspendedNow(action) => log::error!(
            "[ota] three 401 answers to the verdict of action {action}; sending is suspended until a poll is accepted"
        ),
        WorkerEvent::VerdictAnswerIgnored => {
            log::error!("[ota] a verdict answer arrived with no send in flight; internal error");
        }
        WorkerEvent::RecordsUnreadableReported { action, persisted } => match persisted {
            Ok(_) => log::warn!(
                "[ota] records unreadable; action {action} is reported 1005 without being retrieved"
            ),
            Err(e) => log::warn!(
                "[ota] the records_unreadable verdict for action {action} was not persisted, the next offer is reported again: {e}"
            ),
        },
        WorkerEvent::Refreshed(refreshed) => log::info!(
            "[ota] boot reconciliation re-run after the repair: {:?}",
            refreshed.outcome.disposition
        ),
        WorkerEvent::CancelTarget { stop_id, target } => {
            log::info!("[ota] cancel of action {stop_id}: {target:?}");
        }
        WorkerEvent::CancelStopRequested(request) => {
            log::info!("[ota] cancel of the download: {request:?}");
        }
        WorkerEvent::CancelEnded(result) => log::info!("[ota] cancel ended: {result:?}"),
        WorkerEvent::CancelIgnored => {
            log::error!("[ota] the cancel machine ignored its input; internal error");
        }
        WorkerEvent::Completion(CompletionTry::Completed) => {
            log::info!("[ota] the attempt record is cleared; its success verdict awaits delivery");
        }
        WorkerEvent::Completion(CompletionTry::Gone) => {
            log::info!("[ota] no attempt record remains; the retry stops");
        }
        WorkerEvent::Completion(CompletionTry::StillFailing) => {
            log::warn!("[ota] the attempt record is still not cleared");
        }
        other => log::debug!("[ota] worker event: {other:?}"),
    }
}
