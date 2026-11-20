//! The `ota-worker` thread: it owns the poll machine, the cancel machine, the verdict sender and every feedback, and runs the work each machine asks for.
//!
//! Every decision is made by the sans-IO machines of `juggler::ota::ddi` (host-tested); this file only performs what they ask: requests through the [`DdiTransport`], store calls, hardware reads and the spawn of the `ota-download` thread of an admitted attempt.
//! The store lock is taken only through `Records::with` closures that contain store calls and nothing else: never across a request, an `OtaSession`, a hardware read, a sleep, a join or a condition-variable wait.
//! The thread never restarts the device; the download thread does, through the app's restart callback, after the new image was selected for the next boot.
//!
//! Three things wake it out of its sleep ([`Waker`]): the MQTT nudge, the health policy (it persisted the success verdict) and an ended download thread.
//! The nudge is only ever applied here, on the worker's own thread, through `PollMachine::nudge`; the app's `on_message` closure just sets a flag.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use juggler::ota::ddi::wire::{ActionId, Chip, ConfigData, Href};
use juggler::ota::ddi::{
    admit_retrieved, AdmissionFacts, AdmittedOffer, CancelInput, CancelMachine, CancelStep,
    DdiResponse, DeploymentVerdict, FeedbackPath, FeedbackPost, Nudge, OfferGate, PollAction,
    PollAnswer, PollInput, PollMachine, PollVerdict, RequestOutcome, SendOutcome, UrlRefusal,
    VerdictSender, VerdictStep, WorkOutcome, MAX_POLL_INTERVAL,
};
use juggler::ota::health::{
    completion_step, refresh_after_repair, retry_completion, CompletionBackoff,
    CompletionRetryConfig, CompletionStep, CompletionTry,
};
use juggler::ota::persist::{DdiVerdict, VerdictDelivery, VerdictNote};
use juggler::ota::{
    BusyRetry, CancelRequest, FailReason, ImageChip, SlotState, StoreError, UpdateSlot, Version,
};

use super::download::{self, DownloadCtx};
use super::handle::DdiShared;
use super::nudge::Waker;
use super::{clock, DdiSettings, DdiTransport, DdiTransportError};
use crate::ota::flags::OtaFlags;
use crate::ota::records::{Records, RestartFn};
use crate::ota::stack::sample;
use crate::ota::store::random_u32;
use crate::ota::{read_hardware_facts, OtaCancel};

pub(super) struct WorkerCtx {
    pub(super) settings: Arc<DdiSettings>,
    pub(super) restart: RestartFn,
    pub(super) transport: Arc<DdiTransport>,
    pub(super) chip: Chip,
    pub(super) image_chip: Option<ImageChip>,
    pub(super) records: Records,
    pub(super) flags: OtaFlags,
    pub(super) shared: Arc<DdiShared>,
    pub(super) waker: Arc<Waker>,
    pub(super) origin: Instant,
    pub(super) partition_capacity: u64,
    /// The boot repair owes the server a 1005 report (`repair_report_owed`); cleared when the verdict is persisted.
    pub(super) unreported_corruption: bool,
}

/// The download thread of the live attempt.
struct RunningDownload {
    action: ActionId,
    cancel: Arc<OtaCancel>,
    join: JoinHandle<()>,
}

/// The longest pause between two background attempts to clear the attempt record: the MQTT runtime's default (`report_retry`).
const COMPLETION_RETRY_CAP: Duration = Duration::from_secs(10);

struct Worker {
    ctx: WorkerCtx,
    poller: PollMachine,
    cancel: CancelMachine,
    sender: VerdictSender,
    download: Option<RunningDownload>,
    stopped_logged: bool,
    /// A poll was asked for (nudge, health verdict, ended download) and `PollMachine::nudge` has not accepted or dropped it yet.
    nudge_pending: bool,
    /// The background retry of `complete_attempt` after `ApplyNotRecorded`; `None` while not armed.
    completion: Option<CompletionBackoff>,
}

/// ` (<case>)` after a refusal's log line when the origin check named a case, nothing otherwise.
struct CaseSuffix(Option<UrlRefusal>);

impl core::fmt::Display for CaseSuffix {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.0 {
            Some(case) => write!(f, " ({case})"),
            None => Ok(()),
        }
    }
}

/// The worker loop.
pub(super) fn run(ctx: WorkerCtx) {
    if ctx.image_chip.is_none() {
        log::warn!("[ota] no image-header chip check for this chip");
    }
    let mut w = Worker::new(ctx);
    w.post_boot_proceeding();
    let mut input = PollInput::Tick;
    loop {
        w.reap_download();
        let answered = matches!(input, PollInput::Answered(_));
        let now = w.now();
        let action = w.poller.poll(input, now);
        if answered {
            if let Some(verdict) = w.poller.verdict() {
                w.sender.poll_answered(verdict);
                match verdict {
                    PollVerdict::LinksRefused(case) => {
                        log::warn!("[ota] DDI poll link refused: {case}");
                    }
                    other => log::info!("[ota] DDI poll answered: {other:?}"),
                }
            }
        }
        input = match action {
            PollAction::Sleep { until } => {
                w.sleep_until(until);
                PollInput::Tick
            }
            PollAction::Poll => PollInput::Answered(w.do_poll()),
            PollAction::PutConfigData => PollInput::Performed(w.put_config_data()),
            PollAction::RetrieveCancel(href) => PollInput::Performed(w.retrieve_cancel(&href)),
            PollAction::RetrieveDeployment(href) => {
                PollInput::Performed(w.retrieve_deployment(&href))
            }
            PollAction::ReportUnreadable(action) => {
                PollInput::Performed(w.report_unreadable(&action))
            }
            PollAction::DeliverVerdict => PollInput::Performed(w.deliver_verdict()),
            PollAction::Stopped => {
                w.stopped();
                PollInput::Tick
            }
            PollAction::Ignored => {
                log::error!("[ota] the DDI poll machine ignored its input; ticking");
                PollInput::Tick
            }
        };
        sample(&w.ctx.shared.worker_free, "worker");
    }
}

/// The tier-neutral form of a transport result; an error raised before any request was sent reads as unreachable so every machine is answered.
fn request_outcome(result: Result<DdiResponse, DdiTransportError>, what: &str) -> RequestOutcome {
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
fn describe(outcome: &RequestOutcome) -> String {
    match outcome {
        RequestOutcome::Response(r) => format!("status {}", r.status),
        RequestOutcome::BodyFailed {
            status, failure, ..
        } => format!("status {status}, body {failure:?}"),
        RequestOutcome::Timeout => String::from("timeout"),
        RequestOutcome::Unreachable => String::from("server unreachable"),
        RequestOutcome::Redirect { status } => format!("redirect {status}"),
        RequestOutcome::ForeignOrigin => String::from("foreign origin"),
    }
}

fn is_2xx(outcome: &RequestOutcome) -> bool {
    matches!(outcome, RequestOutcome::Response(r) if (200..300).contains(&r.status))
}

/// Feedback sequence points 1 to 3: `proceeding`, best effort, never persisted, nothing handed back.
pub(super) fn send_proceeding(transport: &DdiTransport, action: &ActionId) {
    let post = FeedbackPost::proceeding(action.clone(), clock::millis());
    match transport.deployment_feedback(post.action.clone(), &post.feedback()) {
        Ok(r) if (200..300).contains(&r.status) => {
            log::info!("[ota] proceeding sent for action {action}");
        }
        Ok(r) => log::warn!("[ota] proceeding for action {action} answered {}", r.status),
        Err(e) => log::warn!("[ota] proceeding for action {action} failed: {e}"),
    }
}

impl Worker {
    fn new(ctx: WorkerCtx) -> Self {
        let poller = PollMachine::new(ctx.transport.endpoint().clone());
        Self {
            ctx,
            poller,
            cancel: CancelMachine::new(),
            sender: VerdictSender::new(),
            download: None,
            stopped_logged: false,
            nudge_pending: false,
            completion: None,
        }
    }

    /// The monotonic time since the runtime started, the one origin of every machine.
    fn now(&self) -> Duration {
        self.ctx.origin.elapsed()
    }

    fn running(&self) -> Version {
        self.ctx.settings.running_version
    }

    fn work(&self, outcome: RequestOutcome) -> WorkOutcome {
        WorkOutcome {
            outcome,
            wall_clock: clock::secs(),
            random: random_u32(),
        }
    }

    /// The hardware read used by admission: `1 + session_busy_retries` tries, `session_busy_delay` apart.
    fn busy_retry(&self) -> BusyRetry {
        let t = &self.ctx.settings.timings;
        BusyRetry {
            tries: u8::try_from(t.session_busy_retries.saturating_add(1)).unwrap_or(u8::MAX),
            delay: t.session_busy_delay,
        }
    }

    /// Joins the download thread when it has ended; it settled its own failure before it exited, and a successful download never returns.
    ///
    /// A failed download may have left a verdict in the store, so a join asks for an early poll to deliver it.
    fn reap_download(&mut self) {
        if !self.download.as_ref().is_some_and(|d| d.join.is_finished()) {
            return;
        }
        if let Some(d) = self.download.take() {
            if d.join.join().is_err() {
                log::error!("[ota] the download thread of action {} panicked", d.action);
            }
            self.nudge_pending = true;
        }
    }

    /// The instant `since_origin` on the monotonic clock of the machines.
    fn instant_at(&self, since_origin: Duration) -> Instant {
        self.ctx
            .origin
            .checked_add(since_origin)
            .unwrap_or_else(|| Instant::now() + MAX_POLL_INTERVAL)
    }

    fn completion_cfg(&self) -> CompletionRetryConfig {
        let initial = self.ctx.settings.timings.health.complete_retry;
        CompletionRetryConfig {
            initial,
            cap: COMPLETION_RETRY_CAP.max(initial),
        }
    }

    /// Sleeps until `until` on the monotonic clock, servicing the wake flags and the completion retry meanwhile.
    ///
    /// A nudge is applied here: `PollMachine::nudge` either moves the next poll to now (the sleep ends), keeps the request until the 10 s after the last poll have passed, or drops it.
    /// When the sleep ends because the poll is due anyway, the pending request is served by that poll and cleared.
    fn sleep_until(&mut self, until: Duration) {
        loop {
            let now = self.now();
            self.service_completion(now);
            if self.nudge_pending {
                match self.poller.nudge(now) {
                    Nudge::Moved | Nudge::AlreadyDue => {
                        self.nudge_pending = false;
                        log::info!("[ota] nudge: polling now");
                        return;
                    }
                    Nudge::TooSoon => {
                        log::debug!("[ota] nudge: too soon after the last poll; kept");
                    }
                    Nudge::InFlight => {}
                    Nudge::Throttled => {
                        self.nudge_pending = false;
                        log::info!("[ota] nudge not applied: a Retry-After wait is running");
                    }
                    Nudge::Stopped => {
                        self.nudge_pending = false;
                        log::info!("[ota] nudge not applied: polling has ended");
                    }
                }
            }
            let mut deadline = self.instant_at(until);
            if self.nudge_pending {
                if let Some(allowed) = self.poller.nudge_allowed_at() {
                    deadline = deadline.min(self.instant_at(allowed));
                }
            }
            if let Some(backoff) = self.completion {
                deadline = deadline.min(self.instant_at(now.saturating_add(backoff.wait(now))));
            }
            let wake = self.ctx.waker.wait_until(Some(deadline));
            if wake.download {
                self.reap_download();
                self.nudge_pending = true;
            }
            if wake.health || wake.nudge {
                self.nudge_pending = true;
            }
            if Instant::now() >= self.instant_at(until) {
                self.nudge_pending = false;
                return;
            }
        }
    }

    /// Polling has ended for this boot (410 Gone): park, still reaping a finished download and keeping the record upkeep alive.
    fn stopped(&mut self) {
        if !self.stopped_logged {
            self.stopped_logged = true;
            log::warn!(
                "[ota] the DDI server answered 410 Gone; polling has ended until the next boot"
            );
        }
        let now = self.now();
        self.service_completion(now);
        let deadline = self
            .completion
            .map(|backoff| self.instant_at(now.saturating_add(backoff.wait(now))));
        let wake = self.ctx.waker.wait_until(deadline);
        if wake.download {
            self.reap_download();
        }
        if wake.nudge {
            log::info!("[ota] nudge not applied: polling has ended until the next boot");
        }
        self.nudge_pending = false;
    }

    /// Arms the completion retry when the health policy asked for it and it is not armed yet.
    fn arm_completion_if_requested(&mut self, now: Duration) {
        if self.completion.is_none() && self.ctx.shared.completion_retry.load(Ordering::Acquire) {
            self.completion = Some(CompletionBackoff::armed(&self.completion_cfg(), now));
        }
    }

    /// Retries `complete_attempt` for the running version when the retry is armed and due: after `ApplyNotRecorded` the success verdict exists only once this succeeds.
    ///
    /// The store lock is held for one call only, never across the backoff.
    /// A cleared record asks for a poll so the success verdict goes out promptly.
    fn service_completion(&mut self, now: Duration) {
        self.arm_completion_if_requested(now);
        let Some(backoff) = self.completion else {
            return;
        };
        if !backoff.due(now) {
            return;
        }
        let running = self.running();
        let result = self
            .ctx
            .records
            .with(|store| retry_completion(store, running));
        match result {
            CompletionTry::Completed => {
                log::info!(
                    "[ota] the attempt record is cleared; its success verdict awaits delivery"
                );
            }
            CompletionTry::Gone => log::info!("[ota] no attempt record remains; the retry stops"),
            CompletionTry::StillFailing => {
                log::warn!("[ota] the attempt record is still not cleared");
            }
        }
        if completion_step(result) == CompletionStep::Retry {
            self.completion = Some(backoff.failed(&self.completion_cfg(), now));
        } else {
            self.completion = None;
            self.ctx
                .shared
                .completion_retry
                .store(false, Ordering::Release);
            if result == CompletionTry::Completed {
                self.nudge_pending = true;
            }
        }
    }

    /// Feedback sequence point 3: the new image booted, before its health check.
    fn post_boot_proceeding(&self) {
        if self.ctx.flags.health_settled() {
            return;
        }
        match self.ctx.records.with(|store| store.running_ddi_action()) {
            Ok(Some(action)) => {
                log::info!("[ota] the new image booted for action {action}");
                send_proceeding(&self.ctx.transport, &action);
            }
            Ok(None) => {}
            Err(e) => log::debug!("[ota] the running DDI action is unreadable: {e}"),
        }
    }

    fn do_poll(&self) -> PollAnswer {
        let outcome = request_outcome(self.ctx.transport.poll(), "poll");
        let (store_gate, pending) = self
            .ctx
            .records
            .with(|store| (store.offer_gate(), store.pending_verdict()));
        if let Err(e) = &pending {
            log::debug!("[ota] the pending verdict is unreadable: {e}");
        }
        let gate = if self.ctx.unreported_corruption {
            OfferGate::RecordsUnreadable
        } else if !self.ctx.flags.admission_open() {
            OfferGate::Blocked
        } else {
            store_gate
        };
        PollAnswer {
            outcome,
            gate,
            wall_clock: clock::secs(),
            random: random_u32(),
            verdict_pending: matches!(pending, Ok(Some(_))),
        }
    }

    fn put_config_data(&self) -> Option<WorkOutcome> {
        let data = ConfigData {
            chip: &self.ctx.chip,
            board: &self.ctx.settings.board,
            version: self.running(),
        };
        let outcome = request_outcome(self.ctx.transport.config_data(&data), "configData");
        if is_2xx(&outcome) {
            log::info!("[ota] DDI configData sent");
            None
        } else {
            log::warn!("[ota] DDI configData not accepted: {}", describe(&outcome));
            Some(self.work(outcome))
        }
    }

    fn retrieve_deployment(&mut self, href: &Href) -> Option<WorkOutcome> {
        let outcome = request_outcome(self.ctx.transport.deployment(href), "deployment GET");
        let rolled_back = self
            .ctx
            .records
            .with(|store| store.refused())
            .ok()
            .flatten();
        let facts = AdmissionFacts {
            running: self.running(),
            rolled_back,
            floor: self.ctx.settings.floor,
            partition_capacity: self.ctx.partition_capacity,
            chip: self.ctx.chip.clone(),
        };
        match admit_retrieved(href, &outcome, &facts, self.ctx.transport.endpoint()) {
            DeploymentVerdict::NotRetrieved(t) => {
                log::warn!("[ota] deployment not retrieved: {t:?}");
                Some(self.work(outcome))
            }
            DeploymentVerdict::Unreportable(reason) => {
                log::warn!("[ota] deployment refused ({reason}) and no action id to report it to");
                None
            }
            DeploymentVerdict::Deferred(action) => {
                log::info!("[ota] deployment {action} deferred by the server (skip)");
                None
            }
            DeploymentVerdict::Refused(refusal) => {
                log::warn!(
                    "[ota] deployment {} refused: {:?} {}{}",
                    refusal.action,
                    refusal.code,
                    refusal.reason,
                    CaseSuffix(refusal.url_case)
                );
                self.persist_and_deliver(&DdiVerdict::refused(&refusal))
            }
            DeploymentVerdict::Admitted(offer) => self.admit(offer),
        }
    }

    /// Persists an attempt-less verdict and sends it at once; a pending verdict of another action wins and the server offers this action again.
    fn persist_and_deliver(&mut self, verdict: &DdiVerdict) -> Option<WorkOutcome> {
        let action = &verdict.action;
        match self
            .ctx
            .records
            .with(|store| store.persist_verdict(verdict))
        {
            Ok(VerdictNote::Written) => {
                log::info!("[ota] verdict for action {action} persisted");
            }
            Ok(VerdictNote::KeptPending) => {
                log::info!("[ota] an earlier verdict for action {action} is kept");
            }
            Ok(VerdictNote::AlreadyClosed) => {
                log::info!("[ota] action {action} is already closed");
                return None;
            }
            Err(StoreError::VerdictPending) => {
                log::warn!("[ota] verdict for action {action} refused: another verdict awaits delivery; the server offers it again");
                return None;
            }
            Err(e) => {
                log::warn!("[ota] verdict for action {action} not persisted: {e}");
                return None;
            }
        }
        self.deliver_verdict()
    }

    /// Sends the store's pending verdict when it is due.
    fn deliver_verdict(&mut self) -> Option<WorkOutcome> {
        let verdict = match self.ctx.records.with(|store| store.pending_verdict()) {
            Ok(Some(verdict)) => verdict,
            Ok(None) => return None,
            Err(e) => {
                log::warn!("[ota] the pending verdict is unreadable: {e}");
                return None;
            }
        };
        let now = self.now();
        let step = self.sender.next(&verdict, now, clock::millis());
        match step {
            VerdictStep::Send(post) => {
                let result = match post.path {
                    FeedbackPath::Deployment => self
                        .ctx
                        .transport
                        .deployment_feedback(post.action.clone(), &post.feedback()),
                    FeedbackPath::Cancel => self
                        .ctx
                        .transport
                        .cancel_feedback(post.action.clone(), &post.feedback()),
                };
                let work = self.work(request_outcome(result, "verdict feedback"));
                let sent = self.sender.answered(work);
                self.verdict_answered(sent, &verdict.action)
            }
            VerdictStep::NotDue { at } => {
                log::debug!("[ota] the verdict is sent again no sooner than {at:?}");
                None
            }
            VerdictStep::Suspended => {
                log::debug!("[ota] verdict sending is suspended until a poll is accepted");
                None
            }
            VerdictStep::InFlight => {
                log::error!("[ota] a verdict send is still in flight; internal error");
                None
            }
        }
    }

    fn verdict_answered(&mut self, sent: SendOutcome, action: &ActionId) -> Option<WorkOutcome> {
        match sent {
            SendOutcome::Delivered(delivered) => {
                match self
                    .ctx
                    .records
                    .with(|store| store.mark_verdict_delivered(&delivered))
                {
                    Ok(VerdictDelivery::Marked) => log::info!(
                        "[ota] verdict for action {delivered} delivered; the action is closed"
                    ),
                    Ok(other) => log::warn!(
                        "[ota] verdict for action {delivered} was not marked delivered: {other:?}"
                    ),
                    Err(e) => log::warn!(
                        "[ota] verdict for action {delivered} delivered but not marked: {e}"
                    ),
                }
                None
            }
            SendOutcome::Retry(hand_back) => {
                log::warn!("[ota] verdict for action {action} not delivered; next try in 30 s");
                hand_back
            }
            SendOutcome::Suspended(work) => {
                log::error!("[ota] three 401 answers to the verdict of action {action}; sending is suspended until a poll is accepted");
                Some(work)
            }
            SendOutcome::Ignored => {
                log::error!(
                    "[ota] a verdict answer arrived with no send in flight; internal error"
                );
                None
            }
        }
    }

    /// Reports the next offered action `closed` + `failure` + 1005 without applying it, after the records proved unreadable at boot.
    fn report_unreadable(&mut self, action: &ActionId) -> Option<WorkOutcome> {
        log::warn!(
            "[ota] records unreadable; action {action} is reported 1005 without being retrieved"
        );
        let noted = self
            .ctx
            .records
            .with(|store| store.persist_verdict(&DdiVerdict::records_unreadable(action.clone())));
        if let Err(e) = noted {
            log::warn!(
                "[ota] the records_unreadable verdict for action {action} was not persisted, the next offer is reported again: {e}"
            );
            return None;
        }
        self.ctx.unreported_corruption = false;
        self.refresh_after_repair();
        self.deliver_verdict()
    }

    /// Re-runs the boot reconciliation after the verdict write repaired the records; the hardware is read outside the lock.
    fn refresh_after_repair(&self) {
        let hardware = read_hardware_facts(BusyRetry::default());
        let running = self.running();
        let roll_back_now = self.ctx.flags.roll_back_now_at_boot();
        let refreshed = self
            .ctx
            .records
            .with(|store| refresh_after_repair(store, hardware, Some(running), roll_back_now));
        match refreshed {
            Some(refreshed) => {
                self.ctx
                    .flags
                    .set_records_suspect(refreshed.records_suspect);
                self.ctx
                    .flags
                    .set_refuse_mark_valid(refreshed.refuse_mark_valid);
                if refreshed.outcome.roll_back_now {
                    log::warn!("[ota] after the repair a rollback would be demanded; it is left to the health policy");
                }
                log::info!(
                    "[ota] boot reconciliation re-run after the repair: {:?}",
                    refreshed.outcome.disposition
                );
            }
            None => log::warn!("[ota] the hardware was unreadable; the flags are unchanged"),
        }
    }

    /// Begins the attempt of an admitted offer and starts its download thread.
    fn admit(&mut self, offer: AdmittedOffer) -> Option<WorkOutcome> {
        log::info!(
            "[ota] deployment {} admitted: version {}, {} bytes",
            offer.action,
            offer.version,
            offer.size
        );
        let hardware = match read_hardware_facts(self.busy_retry()) {
            Ok(hardware) => hardware,
            Err(e) => {
                log::warn!("[ota] hardware unreadable ({e}); the offer waits for the next poll");
                return None;
            }
        };
        if hardware.running_state == SlotState::PendingVerify {
            log::info!("[ota] the running image is pending verification; the offer is deferred");
            return None;
        }
        let (slot, slot_was_invalid) = match hardware.update_slot {
            UpdateSlot::Present(slot, state) => (slot, state == SlotState::Invalid),
            UpdateSlot::Absent | UpdateSlot::Unreadable(_) => {
                log::error!("[ota] the update slot is unreadable; the offer is reported failed");
                return self.persist_failed(offer.action, FailReason::PartitionNotFound);
            }
        };
        let begun = self
            .ctx
            .records
            .with(|store| store.begin_ddi_attempt(&offer, slot, slot_was_invalid));
        match begun {
            Ok(id) => log::info!("[ota] attempt {id} begun for action {}", offer.action),
            Err(StoreError::ActionClosed) => {
                log::warn!(
                    "[ota] action {} is closed; it is never applied again",
                    offer.action
                );
                return None;
            }
            Err(StoreError::NotAdmitted(_) | StoreError::VerdictPending) => {
                log::info!("[ota] the offer is deferred: an earlier attempt or verdict is open");
                return None;
            }
            Err(StoreError::InvalidSlot) => {
                log::error!("[ota] the update slot is invalid; the offer is reported failed");
                return self.persist_failed(offer.action, FailReason::PartitionNotFound);
            }
            Err(e) => {
                log::warn!("[ota] attempt not begun: {e}");
                return None;
            }
        }
        send_proceeding(&self.ctx.transport, &offer.action);
        self.spawn_download(offer);
        None
    }

    fn persist_failed(&mut self, action: ActionId, reason: FailReason) -> Option<WorkOutcome> {
        match DdiVerdict::failed(action, reason) {
            Some(verdict) => self.persist_and_deliver(&verdict),
            None => {
                log::info!("[ota] failure {reason} has no profile code; it is never reported");
                None
            }
        }
    }

    fn spawn_download(&mut self, offer: AdmittedOffer) {
        let cancel = Arc::new(OtaCancel::for_retries());
        let action = offer.action.clone();
        let ctx = DownloadCtx {
            transport: Arc::clone(&self.ctx.transport),
            records: self.ctx.records.clone(),
            settings: Arc::clone(&self.ctx.settings),
            restart: Arc::clone(&self.ctx.restart),
            image_chip: self.ctx.image_chip,
            cancel: Arc::clone(&cancel),
            waker: Arc::clone(&self.ctx.waker),
            shared: Arc::clone(&self.ctx.shared),
            offer,
        };
        let spawned = thread::Builder::new()
            .name("ota-download".into())
            .stack_size(self.ctx.settings.stacks.download_bytes)
            .spawn(move || download::run(ctx));
        match spawned {
            Ok(join) => {
                self.download = Some(RunningDownload {
                    action,
                    cancel,
                    join,
                });
            }
            Err(e) => {
                log::error!("[ota] cannot spawn the download thread: {e}");
                download::settle_failed(&self.ctx.records, self.running(), FailReason::Busy);
            }
        }
    }

    /// Runs the cancel machine to its end: it decides, this performs the look-up, the stop, the join and the cancel feedback.
    fn retrieve_cancel(&mut self, href: &Href) -> Option<WorkOutcome> {
        let work = self.work(request_outcome(
            self.ctx.transport.cancel_action(href),
            "cancel GET",
        ));
        let (now, millis) = (self.now(), clock::millis());
        let mut step = self.cancel.poll(CancelInput::Retrieved(work), now, millis);
        loop {
            step = match step {
                CancelStep::LookUp(stop_id) => {
                    self.reap_download();
                    let downloading = self.download.as_ref().map(|d| &d.action);
                    let target = self
                        .ctx
                        .records
                        .with(|store| store.cancel_target(&stop_id, downloading));
                    log::info!("[ota] cancel of action {stop_id}: {target:?}");
                    let (now, millis) = (self.now(), clock::millis());
                    self.cancel.poll(CancelInput::Target(target), now, millis)
                }
                CancelStep::RequestStop => {
                    let request = self
                        .download
                        .as_ref()
                        .map_or(CancelRequest::TooLate, |d| d.cancel.cancel());
                    log::info!("[ota] cancel of the download: {request:?}");
                    let (now, millis) = (self.now(), clock::millis());
                    self.cancel
                        .poll(CancelInput::StopRequested(request), now, millis)
                }
                CancelStep::AwaitDownload => {
                    if let Some(d) = self.download.take() {
                        if d.join.join().is_err() {
                            log::error!(
                                "[ota] the download thread of action {} panicked",
                                d.action
                            );
                        }
                    }
                    let (now, millis) = (self.now(), clock::millis());
                    self.cancel.poll(CancelInput::DownloadReturned, now, millis)
                }
                CancelStep::Send(post) => {
                    let result = match post.path {
                        FeedbackPath::Cancel => self
                            .ctx
                            .transport
                            .cancel_feedback(post.action.clone(), &post.feedback()),
                        FeedbackPath::Deployment => self
                            .ctx
                            .transport
                            .deployment_feedback(post.action.clone(), &post.feedback()),
                    };
                    let work = self.work(request_outcome(result, "cancel feedback"));
                    let (now, millis) = (self.now(), clock::millis());
                    self.cancel.poll(CancelInput::Sent(work), now, millis)
                }
                CancelStep::Done(hand_back) => {
                    log::info!("[ota] cancel ended: {:?}", self.cancel.result());
                    return hand_back;
                }
                CancelStep::Ignored => {
                    log::error!("[ota] the cancel machine ignored its input; internal error");
                    return None;
                }
            };
        }
    }
}
