//! The DDI poll loop as an action machine (ADR 023 rung 3): when to poll, what a poll answer means, and which link of it the device acts on.
//!
//! The tier loop performs each [`PollAction`] and feeds the outcome back as a [`PollInput`], with the monotonic time since boot as `now`.
//! The machine never reads a clock or draws a random number: both arrive as inputs, and a `Retry-After` date needs the wall clock as one too.
//!
//! # A poll answer
//!
//! | Answer | Next poll | Verdict |
//! |:--|:--|:--|
//! | 200, document parsed | the poll interval, `config.polling.sleep` clamped to 30 s .. 24 h (kept from the last answer when absent) | [`Ok`](PollVerdict::Ok), [`LinksRefused`](PollVerdict::LinksRefused) or [`OfferDeferred`](PollVerdict::OfferDeferred) |
//! | 404 | the poll interval | [`NotFound`](PollVerdict::NotFound) |
//! | 401 | [`backoff_delay`] from the poll interval, like a transient failure, never with another credential (fleet DDI profile §4) | [`Unauthorized`](PollVerdict::Unauthorized) |
//! | 410 | never: [`PollAction::Stopped`] for the rest of the boot | [`Gone`](PollVerdict::Gone) |
//! | 413 or 429 with a usable `Retry-After` | the `Retry-After`, clamped to 30 s .. 24 h | [`RetryAfter`](PollVerdict::RetryAfter) |
//! | anything else: 429 without it, 5xx, a timeout, no connection, a redirect, a body that failed or did not parse, an unexpected status | [`backoff_delay`] from the poll interval, up to 24 h with jitter | [`BackOff`](PollVerdict::BackOff) |
//!
//! A transient failure never ends anything on the poll path; the next poll simply waits longer.
//! A 401 counts as one failure of the same back-off, but keeps its own verdict for the consecutive-401 rule of the feedback retry.
//! Any other answer resets the failure count, except a throttling one ([`RetryAfter`](PollVerdict::RetryAfter)): it keeps the count, so a server that alternates throttling and failing still sees the back-off grow.
//!
//! # The links of a 200 answer, in order
//!
//! 1. A `deploymentBase`, `cancelAction` or `configData` link that fails the origin check ([`DdiEndpoint::check_href`], fleet DDI profile §2), or with an empty or over-long `href`, refuses the whole poll: the device requests no link of it and sends no feedback for them (a verdict of another action that awaits delivery still goes out), and keeps polling at the poll interval, which the answer's `sleep` still sets; the verdict names the case ([`PollVerdict::LinksRefused`]), so the tier logs it without the URL.
//! 2. `cancelAction` wins over `deploymentBase`: [`PollAction::RetrieveCancel`].
//! 3. A `deploymentBase` is taken only when the store allows a new offer ([`OfferGate`]): [`PollAction::RetrieveDeployment`] when open; nothing while an image waits for verification, an attempt is unresolved or a report is undelivered, so the server's silent timeout never starts; and [`PollAction::ReportUnreadable`] while the OTA records are unreadable, with the action id read from the link ([`DdiEndpoint::deployment_action`]), never retrieved; a link whose id cannot be read then refuses the poll as in step 1.
//!
//! Independent of the links, the first 200 answer of the boot whose body arrived whole yields [`PollAction::PutConfigData`] once, before any link work; the PUT goes to the URL built from the configured base, so a refused link does not stop it.
//! A valid 200 document answered while a terminal verdict awaits delivery ([`PollAnswer::verdict_pending`]) yields [`PollAction::DeliverVerdict`] last, after the link work, so a cancel that names the same action is rejected before the verdict closes it; the rest of a hand-off (link work included) is dropped when a request of it was not answered normally, so a failing or throttling server gets no further request at once and the next poll offers the work again.
//!
//! # Nudge
//!
//! [`PollMachine::nudge`] moves the next poll to now, but never closer than [`NUDGE_MIN_INTERVAL`] (10 s) after the last poll, never while a poll or its work is in flight, and never into a `Retry-After` wait (fleet DDI profile §1, §6).
//! It does cut a back-off wait: the nudge comes from the fleet backend, which is up.
//! The MQTT nudge carries no authority: its payload is ignored, and it only schedules one authenticated poll.
//!
//! # Seams
//!
//! - [`PollAction::RetrieveDeployment`] hands the link to deployment admission ([`admit_retrieved`](super::admit_retrieved)); [`PollAction::RetrieveCancel`] hands off to the [`CancelMachine`](super::CancelMachine), and [`PollAction::ReportUnreadable`] and [`PollAction::DeliverVerdict`] to the [`VerdictSender`](super::VerdictSender).
//!   The machine answers [`PollInput::Performed`] with the next step, so the next poll is timed from when that work ended.
//!   When a request of that work was not answered normally (a deployment GET that was not retrieved, say), the tier passes its outcome in [`PollInput::Performed`] as a [`WorkOutcome`]: it times the next poll as the same answer to a poll would, so a `Retry-After` is honoured, a failure or a 401 backs off and counts, and a 410 stops polling; a 2xx changes nothing.
//! - [`OfferGate`] is read from the persisted OTA records at each answer.
//! - [`PollVerdict::Unauthorized`] versus any other verdict feeds the consecutive-401 rule of the feedback retry ([`VerdictSender::poll_answered`](super::VerdictSender::poll_answered)).
//! - [`PollMachine::nudge`] is the entry of the MQTT nudge; the tier wakes its loop when it returns [`Nudge::Moved`].

use core::time::Duration;

use super::endpoint::{DdiEndpoint, UrlRefusal};
use super::response::{RequestOutcome, RetryAfter};
use super::schedule::{backoff_delay, clamp_wait, DEFAULT_POLL_INTERVAL};
use super::wire::{ActionId, DdiError, Href, PollResponse};

/// Experimental: API may change before 1.0.
///
/// Whether the persisted OTA records let the device take a new deployment, read at each poll answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfferGate {
    /// A new deployment may be retrieved.
    Open,
    /// An image waits for verification, an attempt is unresolved, a report is undelivered or an update is running: the deployment is not retrieved yet.
    Blocked,
    /// The OTA records cannot be read: the deployment is reported `closed` + `failure` + 1005 `records_unreadable` without retrieving it.
    RecordsUnreadable,
}

/// Experimental: API may change before 1.0.
///
/// The answer to a [`PollAction::Poll`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollAnswer {
    /// How the poll GET ended.
    pub outcome: RequestOutcome,
    /// The offer gate of the persisted records, read now.
    pub gate: OfferGate,
    /// Seconds since the Unix epoch, `None` while the wall clock is not set; needed only for a `Retry-After` date.
    pub wall_clock: Option<u64>,
    /// A fresh random value for the back-off jitter.
    pub random: u32,
    /// Whether a terminal verdict awaits delivery ([`OtaStore::pending_verdict`](crate::ota::persist::OtaStore::pending_verdict) is `Some`), read now; a valid 200 document then ends its hand-off with [`PollAction::DeliverVerdict`].
    pub verdict_pending: bool,
}

/// Experimental: API may change before 1.0.
///
/// The input to [`PollMachine::poll`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollInput {
    /// The tier loop woke up, after a [`PollAction::Sleep`] or for any other reason.
    Tick,
    /// The poll GET of [`PollAction::Poll`] ended.
    Answered(PollAnswer),
    /// The tier performed the last [`PollAction::PutConfigData`], [`PollAction::RetrieveCancel`], [`PollAction::RetrieveDeployment`], [`PollAction::ReportUnreadable`] or [`PollAction::DeliverVerdict`].
    ///
    /// `None` when its requests were answered normally (or it made none); otherwise the outcome of the request that was not, which times the next poll (see the [module docs](self)).
    Performed(Option<WorkOutcome>),
}

/// Experimental: API may change before 1.0.
///
/// A request of the work after a poll answer that was not answered normally, fed back with [`PollInput::Performed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkOutcome {
    /// How the request ended.
    pub outcome: RequestOutcome,
    /// Seconds since the Unix epoch, `None` while the wall clock is not set; needed only for a `Retry-After` date.
    pub wall_clock: Option<u64>,
    /// A fresh random value for the back-off jitter.
    pub random: u32,
}

/// Experimental: API may change before 1.0.
///
/// What the tier loop must do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PollAction {
    /// Wait until `now` reaches `until` (or a wake-up comes), then answer with [`PollInput::Tick`].
    Sleep {
        /// The monotonic time of the next poll.
        until: Duration,
    },
    /// Send the poll GET and answer with [`PollInput::Answered`].
    Poll,
    /// Send the once-per-boot configData PUT to the URL built from the configured base, then answer with [`PollInput::Performed`]; it is never repeated in this boot, whatever its result.
    PutConfigData,
    /// Retrieve this `cancelAction` link and run the cancel, then answer with [`PollInput::Performed`].
    RetrieveCancel(Href),
    /// Retrieve this `deploymentBase` link and run admission, then answer with [`PollInput::Performed`].
    RetrieveDeployment(Href),
    /// Report this action `closed` + `failure` + 1005 with details `records_unreadable`, without retrieving it, then answer with [`PollInput::Performed`].
    ReportUnreadable(ActionId),
    /// Send the pending terminal verdict through the [`VerdictSender`](super::VerdictSender) when it is due, then answer with [`PollInput::Performed`]; answer at once when nothing is sent.
    DeliverVerdict,
    /// The server answered 410 Gone: polling has ended for this boot.
    ///
    /// Every later [`PollInput::Tick`] answers this again.
    Stopped,
    /// The input did not answer the pending action; nothing changed (ADR 023 decision 5).
    ///
    /// A tier loop never produces this on its own, so it treats it as an internal error.
    Ignored,
}

/// Experimental: API may change before 1.0.
///
/// Why a poll answer was a transient failure ([`PollVerdict::BackOff`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transient {
    /// A status the poll does not expect (5xx, 429 or 413 without a usable `Retry-After`, any other).
    Status(u16),
    /// The body of a response with this status was not read whole.
    Body(u16),
    /// A 200 body that is not a valid poll document.
    Document(DdiError),
    /// The request timed out before a status arrived.
    Timeout,
    /// The server could not be reached.
    Unreachable,
    /// The server answered with this 3xx status.
    Redirect(u16),
    /// The tier reported a foreign origin; the poll URL is built from the configured base, so this only follows a tier fault.
    ForeignOrigin,
}

/// Experimental: API may change before 1.0.
///
/// What the last poll answer meant; for logs and for the consecutive-401 rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollVerdict {
    /// 200 with a valid document; its links, if any, are acted on.
    Ok,
    /// 200 with a link that failed the origin check (its case), with an empty or over-long `href` ([`UrlRefusal::Length`]), or with a `deploymentBase` whose action id cannot be read while the records are unreadable ([`UrlRefusal::Path`]); no link of the poll is acted on.
    /// The tier logs the case as a WARN, never the link.
    LinksRefused(UrlRefusal),
    /// 200 with a `deploymentBase` the store does not let the device take now ([`OfferGate::Blocked`]).
    OfferDeferred,
    /// 404: the server does not know the device (yet).
    NotFound,
    /// 401: the token was refused; the next poll backs off like a transient failure, never with another credential.
    Unauthorized,
    /// 413 or 429 with a `Retry-After` that was honoured.
    RetryAfter,
    /// 410: polling has ended for this boot.
    Gone,
    /// A transient failure; the next poll backs off.
    BackOff {
        /// The failure.
        cause: Transient,
        /// Transient failures and 401s since the last answer that was none of these nor a throttling [`RetryAfter`](PollVerdict::RetryAfter), this one included.
        failures: u32,
    },
}

/// The link work one poll answer hands off.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LinkWork {
    Cancel(Href),
    Deployment(Href),
    Unreadable(ActionId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    /// The next poll is due at this time.
    Waiting(Duration),
    /// The poll GET is in flight.
    Polling,
    /// The answer's work is being handed off; the next poll follows `delay` after it ends.
    Handing {
        config_data: bool,
        link: Option<LinkWork>,
        deliver_verdict: bool,
        delay: Duration,
    },
    Stopped,
}

/// Experimental: API may change before 1.0.
///
/// Shortest time from the start of the last poll to a poll a nudge triggers (fleet DDI profile §6).
pub const NUDGE_MIN_INTERVAL: Duration = Duration::from_secs(10);

/// Experimental: API may change before 1.0.
///
/// What [`PollMachine::nudge`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Nudge {
    /// The next poll is due now: wake the tier loop, which answers with [`PollInput::Tick`].
    Moved,
    /// The next poll is already due; nothing changed.
    AlreadyDue,
    /// The last poll started less than [`NUDGE_MIN_INTERVAL`] ago; ignored.
    TooSoon,
    /// A poll or the work after it is in flight; ignored.
    InFlight,
    /// The next poll waits for a server `Retry-After`; ignored.
    Throttled,
    /// Polling has ended for this boot (410); ignored.
    Stopped,
}

/// Experimental: API may change before 1.0.
///
/// The DDI poll loop (see the [module docs](self)).
///
/// It is a persistent service: it runs until the server answers 410, and each step between two polls ends in a [`PollAction::Sleep`] of at least 30 s, unless a nudge ([`PollMachine::nudge`]) moves the next poll, which still comes at least [`NUDGE_MIN_INTERVAL`] after the last one.
/// The first [`PollInput::Tick`] polls at once, because a device polls at boot.
///
/// ```
/// use core::time::Duration;
/// use juggler::ota::ddi::{
///     ControllerId, DdiEndpoint, DdiResponse, OfferGate, PollAction, PollAnswer, PollInput,
///     PollMachine, RequestOutcome,
/// };
///
/// let id = ControllerId::parse("dev-1").unwrap();
/// let mut m = PollMachine::new(DdiEndpoint::new("http://ddi.lab:8080/DEFAULT", &id).unwrap());
/// assert_eq!(m.poll(PollInput::Tick, Duration::ZERO), PollAction::Poll);
///
/// let body = br#"{"config":{"polling":{"sleep":"00:01:00"}}}"#.to_vec();
/// let answer = PollAnswer {
///     outcome: RequestOutcome::Response(DdiResponse { status: 200, retry_after: None, body }),
///     gate: OfferGate::Open,
///     wall_clock: None,
///     random: 0,
///     verdict_pending: false,
/// };
/// assert_eq!(m.poll(PollInput::Answered(answer), Duration::ZERO), PollAction::PutConfigData);
/// let now = Duration::from_secs(1);
/// assert_eq!(
///     m.poll(PollInput::Performed(None), now),
///     PollAction::Sleep { until: Duration::from_secs(61) }
/// );
/// ```
#[derive(Debug, Clone)]
pub struct PollMachine {
    endpoint: DdiEndpoint,
    phase: Phase,
    interval: Duration,
    failures: u32,
    config_data_sent: bool,
    verdict: Option<PollVerdict>,
    /// When the last poll GET started.
    last_poll: Option<Duration>,
}

impl PollMachine {
    /// A machine polling `endpoint`, every [`DEFAULT_POLL_INTERVAL`] until a poll answer carries `config.polling.sleep`.
    pub fn new(endpoint: DdiEndpoint) -> Self {
        Self {
            endpoint,
            phase: Phase::Waiting(Duration::ZERO),
            interval: DEFAULT_POLL_INTERVAL,
            failures: 0,
            config_data_sent: false,
            verdict: None,
            last_poll: None,
        }
    }

    /// The current poll interval, already clamped.
    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// The verdict on the last poll answer, `None` before the first one.
    pub fn verdict(&self) -> Option<PollVerdict> {
        self.verdict
    }

    /// When the next poll is due, `None` while a poll or its work is in progress and after 410.
    pub fn next_poll_at(&self) -> Option<Duration> {
        match self.phase {
            Phase::Waiting(at) => Some(at),
            _ => None,
        }
    }

    /// The earliest monotonic time a nudge is not [`Nudge::TooSoon`]: [`NUDGE_MIN_INTERVAL`] after the start of the last poll; `None` before the first poll.
    pub fn nudge_allowed_at(&self) -> Option<Duration> {
        self.last_poll
            .map(|last| last.saturating_add(NUDGE_MIN_INTERVAL))
    }

    /// An early poll for the MQTT nudge; `now` is the monotonic time since boot.
    ///
    /// It moves the next poll to now, but not within [`NUDGE_MIN_INTERVAL`] (10 s) of the start of the last poll, never while a poll or its work is in flight, and never into a `Retry-After` wait (fleet DDI profile §1, §6).
    /// Only [`Nudge::Moved`] changes anything: [`next_poll_at`](Self::next_poll_at) becomes `now`.
    pub fn nudge(&mut self, now: Duration) -> Nudge {
        let at = match self.phase {
            Phase::Waiting(at) => at,
            Phase::Polling | Phase::Handing { .. } => return Nudge::InFlight,
            Phase::Stopped => return Nudge::Stopped,
        };
        if now >= at {
            return Nudge::AlreadyDue;
        }
        if self.verdict == Some(PollVerdict::RetryAfter) {
            return Nudge::Throttled;
        }
        if self
            .last_poll
            .is_some_and(|last| now < last.saturating_add(NUDGE_MIN_INTERVAL))
        {
            return Nudge::TooSoon;
        }
        self.phase = Phase::Waiting(now);
        Nudge::Moved
    }

    /// Advances the loop; `now` is the monotonic time since boot.
    pub fn poll(&mut self, input: PollInput, now: Duration) -> PollAction {
        match (&self.phase, input) {
            (Phase::Stopped, PollInput::Tick) => PollAction::Stopped,
            (Phase::Waiting(at), PollInput::Tick) => {
                if now >= *at {
                    self.phase = Phase::Polling;
                    self.last_poll = Some(now);
                    PollAction::Poll
                } else {
                    PollAction::Sleep { until: *at }
                }
            }
            (Phase::Polling, PollInput::Answered(answer)) => self.answered(answer, now),
            (Phase::Handing { .. }, PollInput::Performed(work)) => self.performed(work, now),
            _ => PollAction::Ignored,
        }
    }

    fn answered(&mut self, answer: PollAnswer, now: Duration) -> PollAction {
        let PollAnswer {
            outcome,
            gate,
            wall_clock,
            random,
            verdict_pending,
        } = answer;
        let config_data = !self.config_data_sent
            && matches!(&outcome, RequestOutcome::Response(r) if r.status == 200);
        self.config_data_sent |= config_data;
        let decided = self.judge(outcome, Some(gate), wall_clock);
        let deliver_verdict = verdict_pending
            && matches!(
                decided,
                Decided::Answer {
                    verdict: PollVerdict::Ok
                        | PollVerdict::LinksRefused(_)
                        | PollVerdict::OfferDeferred,
                    ..
                }
            );
        let Some((delay, link)) = self.settle(decided, random) else {
            return PollAction::Stopped;
        };
        self.phase = Phase::Handing {
            config_data,
            link,
            deliver_verdict,
            delay,
        };
        self.hand_off(now)
    }

    /// A hand-off ended; a [`WorkOutcome`] that is not a 2xx re-times the next poll and drops the rest of the hand-off (link work and verdict delivery), or stops polling on 410.
    fn performed(&mut self, work: Option<WorkOutcome>, now: Duration) -> PollAction {
        if let Some(WorkOutcome {
            outcome,
            wall_clock,
            random,
        }) = work
        {
            let success =
                matches!(&outcome, RequestOutcome::Response(r) if (200..300).contains(&r.status));
            if !success {
                let decided = self.judge(outcome, None, wall_clock);
                let Some((next, _)) = self.settle(decided, random) else {
                    return PollAction::Stopped;
                };
                // A failing or throttling server gets no further request of this answer: the link work and the verdict delivery wait for the next poll.
                if let Phase::Handing {
                    config_data,
                    link,
                    deliver_verdict,
                    delay,
                } = &mut self.phase
                {
                    *config_data = false;
                    *link = None;
                    *deliver_verdict = false;
                    *delay = next;
                }
            }
        }
        self.hand_off(now)
    }

    /// What a request outcome means for the next poll.
    ///
    /// `gate` is `Some` for the poll itself, whose 200 body is the poll document, and `None` for a request of the work after it.
    fn judge(
        &mut self,
        outcome: RequestOutcome,
        gate: Option<OfferGate>,
        wall_clock: Option<u64>,
    ) -> Decided {
        match outcome {
            RequestOutcome::Response(response) => match gate {
                Some(gate) if response.status == 200 => match PollResponse::parse(&response.body) {
                    Err(e) => Decided::Transient(Transient::Document(e)),
                    Ok(document) => self.document(document, gate),
                },
                _ => self.failed_status(
                    response.status,
                    response.retry_after.as_ref(),
                    true,
                    wall_clock,
                ),
            },
            RequestOutcome::BodyFailed {
                status,
                retry_after,
                ..
            } => self.failed_status(status, retry_after.as_ref(), false, wall_clock),
            RequestOutcome::Timeout => Decided::Transient(Transient::Timeout),
            RequestOutcome::Unreachable => Decided::Transient(Transient::Unreachable),
            RequestOutcome::Redirect { status } => Decided::Transient(Transient::Redirect(status)),
            RequestOutcome::ForeignOrigin => Decided::Transient(Transient::ForeignOrigin),
        }
    }

    /// Records the verdict and the failure count of `decided`, and returns the delay and the link work; `None` after 410, with polling stopped.
    fn settle(&mut self, decided: Decided, random: u32) -> Option<(Duration, Option<LinkWork>)> {
        let (verdict, delay, link) = match decided {
            Decided::Gone => {
                self.verdict = Some(PollVerdict::Gone);
                self.phase = Phase::Stopped;
                return None;
            }
            Decided::Transient(cause) => {
                let delay = self.back_off(random);
                let verdict = PollVerdict::BackOff {
                    cause,
                    failures: self.failures,
                };
                (verdict, delay, None)
            }
            Decided::Unauthorized => {
                let delay = self.back_off(random);
                (PollVerdict::Unauthorized, delay, None)
            }
            Decided::Answer {
                verdict,
                delay,
                link,
            } => {
                if verdict != PollVerdict::RetryAfter {
                    self.failures = 0;
                }
                (verdict, delay, link)
            }
        };
        self.verdict = Some(verdict);
        Some((delay, link))
    }

    /// Counts one more failure and returns its back-off from the poll interval.
    fn back_off(&mut self, random: u32) -> Duration {
        self.failures = self.failures.saturating_add(1);
        backoff_delay(self.interval, self.failures, random)
    }

    /// The meaning of any status but a 200 poll document; `whole` is `false` when the body was not read whole.
    fn failed_status(
        &self,
        status: u16,
        retry_after: Option<&RetryAfter>,
        whole: bool,
        wall_clock: Option<u64>,
    ) -> Decided {
        let answer = |verdict, delay| Decided::Answer {
            verdict,
            delay,
            link: None,
        };
        match status {
            404 => answer(PollVerdict::NotFound, self.interval),
            401 => Decided::Unauthorized,
            410 => Decided::Gone,
            413 | 429 => match retry_after.and_then(|r| r.delay(wall_clock)) {
                Some(wait) => answer(PollVerdict::RetryAfter, clamp_wait(wait)),
                None => Decided::Transient(Transient::Status(status)),
            },
            _ if !whole => Decided::Transient(Transient::Body(status)),
            _ => Decided::Transient(Transient::Status(status)),
        }
    }

    /// The links of a valid 200 document, in the order of the module docs.
    fn document(&mut self, document: PollResponse, gate: OfferGate) -> Decided {
        if let Some(sleep) = document.sleep {
            self.interval = clamp_wait(sleep);
        }
        let PollResponse {
            deployment_base,
            cancel_action,
            config_data,
            links_refused,
            ..
        } = document;
        let refused = [&deployment_base, &cancel_action, &config_data]
            .into_iter()
            .flatten()
            .find_map(|href| self.endpoint.refusal(href.as_str()));
        let (verdict, link) = if let Some(case) = refused {
            (PollVerdict::LinksRefused(case), None)
        } else if links_refused {
            (PollVerdict::LinksRefused(UrlRefusal::Length), None)
        } else if let Some(cancel) = cancel_action {
            (PollVerdict::Ok, Some(LinkWork::Cancel(cancel)))
        } else if let Some(deployment) = deployment_base {
            match gate {
                OfferGate::Open => (PollVerdict::Ok, Some(LinkWork::Deployment(deployment))),
                OfferGate::Blocked => (PollVerdict::OfferDeferred, None),
                OfferGate::RecordsUnreadable => {
                    match self.endpoint.deployment_action(&deployment) {
                        Some(action) => (PollVerdict::Ok, Some(LinkWork::Unreadable(action))),
                        None => (PollVerdict::LinksRefused(UrlRefusal::Path), None),
                    }
                }
            }
        } else {
            (PollVerdict::Ok, None)
        };
        Decided::Answer {
            verdict,
            delay: self.interval,
            link,
        }
    }

    /// The next hand-off of the current answer, or the sleep until the next poll once none is left.
    fn hand_off(&mut self, now: Duration) -> PollAction {
        let Phase::Handing {
            config_data,
            link,
            deliver_verdict,
            delay,
        } = &mut self.phase
        else {
            return PollAction::Ignored;
        };
        if core::mem::take(config_data) {
            return PollAction::PutConfigData;
        }
        if let Some(link) = link.take() {
            return match link {
                LinkWork::Cancel(href) => PollAction::RetrieveCancel(href),
                LinkWork::Deployment(href) => PollAction::RetrieveDeployment(href),
                LinkWork::Unreadable(action) => PollAction::ReportUnreadable(action),
            };
        }
        if core::mem::take(deliver_verdict) {
            return PollAction::DeliverVerdict;
        }
        let until = now.saturating_add(*delay);
        self.phase = Phase::Waiting(until);
        PollAction::Sleep { until }
    }
}

/// What a poll answer decided, before the failure count and the phase are updated.
enum Decided {
    Answer {
        verdict: PollVerdict,
        delay: Duration,
        link: Option<LinkWork>,
    },
    Transient(Transient),
    /// A 401: backs off like a transient failure, with its own verdict.
    Unauthorized,
    Gone,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ota::ddi::{
        BodyFailure, ControllerId, DdiResponse, RetryAfter, MAX_POLL_INTERVAL, MIN_POLL_INTERVAL,
    };
    use alloc::format;
    use alloc::string::String;
    use alloc::vec::Vec;

    const BASE: &str = "http://ddi.lab:8080/DEFAULT";
    const CONTROLLER: &str = "http://ddi.lab:8080/DEFAULT/controller/v1/dev-1";

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    fn machine() -> PollMachine {
        let id = ControllerId::parse("dev-1").unwrap();
        PollMachine::new(DdiEndpoint::new(BASE, &id).unwrap())
    }

    fn answer(outcome: RequestOutcome) -> PollInput {
        answer_with(outcome, OfferGate::Open, None, 0)
    }

    fn answer_with(
        outcome: RequestOutcome,
        gate: OfferGate,
        wall_clock: Option<u64>,
        random: u32,
    ) -> PollInput {
        PollInput::Answered(PollAnswer {
            outcome,
            gate,
            wall_clock,
            random,
            verdict_pending: false,
        })
    }

    fn response(status: u16, retry_after: Option<&str>, body: &str) -> RequestOutcome {
        RequestOutcome::Response(DdiResponse {
            status,
            retry_after: retry_after.map(|r| RetryAfter::capture(r).unwrap()),
            body: Vec::from(body.as_bytes()),
        })
    }

    fn ok(body: &str) -> RequestOutcome {
        response(200, None, body)
    }

    fn links(deployment: Option<&str>, cancel: Option<&str>, config: Option<&str>) -> String {
        let mut parts = Vec::new();
        if let Some(d) = deployment {
            parts.push(format!(r#""deploymentBase":{{"href":"{d}"}}"#));
        }
        if let Some(c) = cancel {
            parts.push(format!(r#""cancelAction":{{"href":"{c}"}}"#));
        }
        if let Some(c) = config {
            parts.push(format!(r#""configData":{{"href":"{c}"}}"#));
        }
        format!(
            r#"{{"config":{{"polling":{{"sleep":"00:01:00"}}}},"_links":{{{}}}}}"#,
            parts.join(",")
        )
    }

    fn deployment(id: &str) -> String {
        format!("{CONTROLLER}/deploymentBase/{id}?c=-5")
    }

    fn cancel(id: &str) -> String {
        format!("{CONTROLLER}/cancelAction/{id}")
    }

    fn href_of(action: &PollAction) -> &str {
        match action {
            PollAction::RetrieveCancel(h) | PollAction::RetrieveDeployment(h) => h.as_str(),
            other => panic!("no link in {other:?}"),
        }
    }

    /// Polls once at `now` and feeds `outcome`; the configData PUT of the first 200 is consumed.
    fn poll_once(m: &mut PollMachine, input: PollInput, now: Duration) -> PollAction {
        assert_eq!(m.poll(PollInput::Tick, now), PollAction::Poll);
        match m.poll(input, now) {
            PollAction::PutConfigData => m.poll(PollInput::Performed(None), now),
            other => other,
        }
    }

    #[test]
    fn first_tick_polls_at_once_and_out_of_phase_input_is_ignored() {
        let mut m = machine();
        assert_eq!(m.next_poll_at(), Some(Duration::ZERO));
        assert_eq!(
            m.poll(PollInput::Performed(None), secs(0)),
            PollAction::Ignored
        );
        assert_eq!(m.poll(answer(ok("{}")), secs(0)), PollAction::Ignored);
        assert_eq!(m.poll(PollInput::Tick, secs(0)), PollAction::Poll);
        assert_eq!(m.next_poll_at(), None);
        assert_eq!(m.poll(PollInput::Tick, secs(1)), PollAction::Ignored);
        assert_eq!(
            m.poll(PollInput::Performed(None), secs(1)),
            PollAction::Ignored
        );
        assert_eq!(m.verdict(), None);
    }

    #[test]
    fn sleep_is_clamped_and_kept_when_absent() {
        let mut m = machine();
        assert_eq!(m.interval(), DEFAULT_POLL_INTERVAL);
        let sleep = |text: &str| {
            ok(&format!(
                r#"{{"config":{{"polling":{{"sleep":"{text}"}}}}}}"#
            ))
        };
        assert_eq!(
            poll_once(&mut m, answer(sleep("00:00:10")), secs(0)),
            PollAction::Sleep { until: secs(30) }
        );
        assert_eq!(m.interval(), MIN_POLL_INTERVAL);
        assert_eq!(
            poll_once(&mut m, answer(sleep("48:00:00")), secs(30)),
            PollAction::Sleep {
                until: secs(30) + MAX_POLL_INTERVAL
            }
        );
        let t = secs(30) + MAX_POLL_INTERVAL;
        assert_eq!(
            poll_once(&mut m, answer(sleep("00:02:00")), t),
            PollAction::Sleep {
                until: t + secs(120)
            }
        );
        assert_eq!(
            poll_once(&mut m, answer(ok("{}")), t + secs(120)),
            PollAction::Sleep {
                until: t + secs(240)
            }
        );
        assert_eq!(m.interval(), secs(120));
        assert_eq!(m.verdict(), Some(PollVerdict::Ok));
    }

    #[test]
    fn early_ticks_sleep_until_the_poll_is_due() {
        let mut m = machine();
        poll_once(&mut m, answer(ok(&links(None, None, None))), secs(0));
        assert_eq!(m.next_poll_at(), Some(secs(60)));
        assert_eq!(
            m.poll(PollInput::Tick, secs(59)),
            PollAction::Sleep { until: secs(60) }
        );
        assert_eq!(m.poll(PollInput::Tick, secs(60)), PollAction::Poll);
    }

    #[test]
    fn config_data_goes_once_at_the_first_200() {
        let mut m = machine();
        assert_eq!(m.poll(PollInput::Tick, secs(0)), PollAction::Poll);
        assert!(matches!(
            m.poll(answer(response(404, None, "")), secs(0)),
            PollAction::Sleep { .. }
        ));
        let t = secs(300);
        assert_eq!(m.poll(PollInput::Tick, t), PollAction::Poll);
        assert_eq!(m.poll(answer(ok("{}")), t), PollAction::PutConfigData);
        assert_eq!(m.poll(PollInput::Tick, t), PollAction::Ignored);
        assert_eq!(
            m.poll(PollInput::Performed(None), t + secs(2)),
            PollAction::Sleep {
                until: t + secs(2) + DEFAULT_POLL_INTERVAL
            }
        );
        let t = t + secs(2) + DEFAULT_POLL_INTERVAL;
        assert_eq!(m.poll(PollInput::Tick, t), PollAction::Poll);
        assert!(matches!(
            m.poll(answer(ok("{}")), t),
            PollAction::Sleep { .. }
        ));
    }

    #[test]
    fn config_data_waits_for_a_whole_200_body_and_precedes_link_work() {
        let mut m = machine();
        assert_eq!(m.poll(PollInput::Tick, secs(0)), PollAction::Poll);
        let failed = RequestOutcome::BodyFailed {
            status: 200,
            retry_after: None,
            failure: BodyFailure::Timeout,
        };
        assert!(matches!(
            m.poll(answer(failed), secs(0)),
            PollAction::Sleep { .. }
        ));
        let t = m.next_poll_at().unwrap();
        assert_eq!(m.poll(PollInput::Tick, t), PollAction::Poll);
        let body = links(Some(&deployment("3")), None, None);
        assert_eq!(m.poll(answer(ok(&body)), t), PollAction::PutConfigData);
        let next = m.poll(PollInput::Performed(None), t);
        assert_eq!(href_of(&next), deployment("3"));
        assert_eq!(
            m.poll(PollInput::Performed(None), t + secs(5)),
            PollAction::Sleep {
                until: t + secs(65)
            }
        );
    }

    #[test]
    fn a_malformed_200_still_sends_config_data_and_backs_off() {
        let mut m = machine();
        assert_eq!(m.poll(PollInput::Tick, secs(0)), PollAction::Poll);
        assert_eq!(
            m.poll(answer(ok("[1]")), secs(0)),
            PollAction::PutConfigData
        );
        assert!(matches!(
            m.poll(PollInput::Performed(None), secs(0)),
            PollAction::Sleep { .. }
        ));
        assert!(matches!(
            m.verdict(),
            Some(PollVerdict::BackOff {
                cause: Transient::Document(DdiError::Malformed { .. }),
                failures: 1
            })
        ));
    }

    #[test]
    fn cancel_action_wins_over_deployment_base() {
        let mut m = machine();
        let body = links(Some(&deployment("12")), Some(&cancel("11")), None);
        let action = poll_once(&mut m, answer(ok(&body)), secs(0));
        assert!(matches!(action, PollAction::RetrieveCancel(_)));
        assert_eq!(href_of(&action), cancel("11"));
        assert_eq!(
            m.poll(PollInput::Performed(None), secs(9)),
            PollAction::Sleep { until: secs(69) }
        );
    }

    #[test]
    fn cancel_action_is_retrieved_whatever_the_gate() {
        for gate in [OfferGate::Blocked, OfferGate::RecordsUnreadable] {
            let mut m = machine();
            let body = links(Some(&deployment("12")), Some(&cancel("11")), None);
            let action = poll_once(&mut m, answer_with(ok(&body), gate, None, 0), secs(0));
            assert!(matches!(action, PollAction::RetrieveCancel(_)), "{gate:?}");
        }
    }

    #[test]
    fn a_foreign_or_refused_poll_link_refuses_every_link_of_that_poll() {
        let foreign = "http://evil.lab:8080/DEFAULT/controller/v1/dev-1/deploymentBase/1";
        let other_port = "http://ddi.lab:9090/DEFAULT/controller/v1/dev-1/cancelAction/1";
        let userinfo = "http://u@ddi.lab:8080/DEFAULT/controller/v1/dev-1/cancelAction/1";
        let percent =
            "http://ddi.lab:8080/DEFAULT/controller/v1/dev-1/trap/%2e%2e/deploymentBase/1";
        let localhost = "http://localhost:8080/DEFAULT/controller/v1/dev-1/deploymentBase/1";
        let ip_literal = "http://127.0.0.1:8080/DEFAULT/controller/v1/dev-1/deploymentBase/1";
        let cases = [
            (links(Some(foreign), None, None), UrlRefusal::Origin),
            (
                links(Some(&deployment("1")), Some(other_port), None),
                UrlRefusal::Origin,
            ),
            (
                links(
                    Some(&deployment("1")),
                    None,
                    Some("https://ddi.lab:8080/cd"),
                ),
                UrlRefusal::Origin,
            ),
            (
                links(Some(&deployment("1")), Some(userinfo), None),
                UrlRefusal::ForbiddenChar,
            ),
            (links(Some(percent), None, None), UrlRefusal::ForbiddenChar),
            (links(Some(localhost), None, None), UrlRefusal::Origin),
            (links(Some(ip_literal), None, None), UrlRefusal::Origin),
            (
                links(None, Some(&cancel("1")), Some("not a url")),
                UrlRefusal::ForbiddenChar,
            ),
            (
                links(None, Some(&cancel("1")), Some("ddi.lab:8080/cd")),
                UrlRefusal::Syntax,
            ),
        ];
        for (body, case) in cases {
            let mut m = machine();
            assert_eq!(m.poll(PollInput::Tick, secs(0)), PollAction::Poll);
            assert_eq!(
                m.poll(answer(ok(&body)), secs(0)),
                PollAction::PutConfigData,
                "{body}"
            );
            assert_eq!(
                m.poll(PollInput::Performed(None), secs(0)),
                PollAction::Sleep { until: secs(60) },
                "{body}"
            );
            assert_eq!(m.verdict(), Some(PollVerdict::LinksRefused(case)), "{body}");
            let action = poll_once(&mut m, answer(ok(&body)), secs(60));
            assert_eq!(action, PollAction::Sleep { until: secs(120) });
        }
    }

    #[test]
    fn a_same_origin_config_data_link_alone_needs_no_link_work() {
        let mut m = machine();
        let body = links(None, None, Some(&format!("{CONTROLLER}/configData")));
        assert_eq!(
            poll_once(&mut m, answer(ok(&body)), secs(0)),
            PollAction::Sleep { until: secs(60) }
        );
        assert_eq!(m.verdict(), Some(PollVerdict::Ok));
    }

    #[test]
    fn deployment_follows_the_offer_gate() {
        let body = links(Some(&deployment("12")), None, None);

        let mut m = machine();
        let action = poll_once(&mut m, answer(ok(&body)), secs(0));
        assert!(matches!(action, PollAction::RetrieveDeployment(_)));
        assert_eq!(href_of(&action), deployment("12"));
        assert_eq!(m.verdict(), Some(PollVerdict::Ok));

        let mut m = machine();
        let blocked = answer_with(ok(&body), OfferGate::Blocked, None, 0);
        assert_eq!(
            poll_once(&mut m, blocked, secs(0)),
            PollAction::Sleep { until: secs(60) }
        );
        assert_eq!(m.verdict(), Some(PollVerdict::OfferDeferred));

        let mut m = machine();
        let unreadable = answer_with(ok(&body), OfferGate::RecordsUnreadable, None, 0);
        let action = poll_once(&mut m, unreadable, secs(0));
        let PollAction::ReportUnreadable(id) = action else {
            panic!("{action:?}");
        };
        assert_eq!(id.as_str(), "12");
        assert_eq!(
            m.poll(PollInput::Performed(None), secs(1)),
            PollAction::Sleep { until: secs(61) }
        );
    }

    #[test]
    fn unreadable_records_with_a_link_of_another_shape_refuse_the_poll() {
        let mut m = machine();
        let body = links(Some(&format!("{CONTROLLER}/deploymentBase/x1")), None, None);
        let input = answer_with(ok(&body), OfferGate::RecordsUnreadable, None, 0);
        assert_eq!(
            poll_once(&mut m, input, secs(0)),
            PollAction::Sleep { until: secs(60) }
        );
        assert_eq!(
            m.verdict(),
            Some(PollVerdict::LinksRefused(UrlRefusal::Path))
        );
    }

    #[test]
    fn unreadable_records_read_the_id_behind_a_proxy_prefix() {
        let mut m = machine();
        let link = "http://ddi.lab:8080/proxy/DEFAULT/controller/v1/dev-1/deploymentBase/8";
        let body = links(Some(link), None, None);
        let input = answer_with(ok(&body), OfferGate::RecordsUnreadable, None, 0);
        let action = poll_once(&mut m, input, secs(0));
        let PollAction::ReportUnreadable(id) = action else {
            panic!("{action:?}");
        };
        assert_eq!(id.as_str(), "8");
    }

    #[test]
    fn a_refused_href_refuses_the_poll_and_keeps_the_sleep() {
        let over = "h".repeat(crate::ota::ddi::wire::MAX_HREF_BYTES + 1);
        for href in [String::new(), over] {
            let mut m = machine();
            let body = links(Some(&href), Some(&cancel("4")), None);
            let action = poll_once(&mut m, answer(ok(&body)), secs(0));
            assert_eq!(action, PollAction::Sleep { until: secs(60) });
            assert_eq!(
                m.verdict(),
                Some(PollVerdict::LinksRefused(UrlRefusal::Length))
            );
            assert_eq!(m.interval(), secs(60));
        }
    }

    #[test]
    fn not_found_polls_again_at_the_interval() {
        let mut m = machine();
        assert_eq!(
            poll_once(&mut m, answer(response(404, Some("5"), "")), secs(0)),
            PollAction::Sleep {
                until: DEFAULT_POLL_INTERVAL
            }
        );
        assert_eq!(m.verdict(), Some(PollVerdict::NotFound));
        let failed = RequestOutcome::BodyFailed {
            status: 404,
            retry_after: None,
            failure: BodyFailure::TooLarge,
        };
        let t = DEFAULT_POLL_INTERVAL;
        assert_eq!(
            poll_once(&mut m, answer(failed), t),
            PollAction::Sleep {
                until: t + DEFAULT_POLL_INTERVAL
            }
        );
        assert_eq!(m.verdict(), Some(PollVerdict::NotFound));
    }

    #[test]
    fn a_401_backs_off_exponentially_and_keeps_its_verdict() {
        let mut m = machine();
        let interval = DEFAULT_POLL_INTERVAL;
        assert_eq!(
            poll_once(&mut m, answer(response(401, Some("5"), "")), secs(0)),
            PollAction::Sleep { until: interval },
            "the first 401 waits the lower bound of the first back-off, the interval"
        );
        assert_eq!(m.verdict(), Some(PollVerdict::Unauthorized));
        let failed = RequestOutcome::BodyFailed {
            status: 401,
            retry_after: None,
            failure: BodyFailure::TooLarge,
        };
        assert_eq!(
            poll_once(&mut m, answer(failed), interval),
            PollAction::Sleep {
                until: interval + interval * 2
            },
            "the second 401 doubles the wait"
        );
        assert_eq!(m.verdict(), Some(PollVerdict::Unauthorized));
        let t = interval * 3;
        assert_eq!(
            poll_once(&mut m, answer(RequestOutcome::Timeout), t),
            PollAction::Sleep {
                until: t + interval * 4
            }
        );
        assert_eq!(
            m.verdict(),
            Some(PollVerdict::BackOff {
                cause: Transient::Timeout,
                failures: 3
            }),
            "401s count with the transient failures"
        );
        let t = t + interval * 4;
        assert_eq!(
            poll_once(&mut m, answer(response(404, None, "")), t),
            PollAction::Sleep {
                until: t + interval
            }
        );
        let t = t + interval;
        assert_eq!(
            poll_once(&mut m, answer(response(401, None, "")), t),
            PollAction::Sleep {
                until: t + interval
            },
            "an answer that is no failure resets the count"
        );
        assert_eq!(m.verdict(), Some(PollVerdict::Unauthorized));
    }

    #[test]
    fn gone_stops_polling_for_the_boot() {
        let outcomes = [
            response(410, None, ""),
            RequestOutcome::BodyFailed {
                status: 410,
                retry_after: None,
                failure: BodyFailure::ServerUnreachable,
            },
        ];
        for outcome in outcomes {
            let mut m = machine();
            assert_eq!(
                poll_once(&mut m, answer(outcome), secs(0)),
                PollAction::Stopped
            );
            assert_eq!(m.verdict(), Some(PollVerdict::Gone));
            assert_eq!(m.next_poll_at(), None);
            assert_eq!(m.poll(PollInput::Tick, secs(1_000)), PollAction::Stopped);
            assert_eq!(
                m.poll(PollInput::Performed(None), secs(1_000)),
                PollAction::Ignored
            );
            assert_eq!(m.poll(answer(ok("{}")), secs(1_000)), PollAction::Ignored);
        }
    }

    #[test]
    fn retry_after_is_honoured_and_clamped_on_413_and_429() {
        for status in [413, 429] {
            for (value, wait) in [
                ("120", secs(120)),
                ("5", MIN_POLL_INTERVAL),
                ("999999999", MAX_POLL_INTERVAL),
            ] {
                let mut m = machine();
                assert_eq!(
                    poll_once(&mut m, answer(response(status, Some(value), "")), secs(10)),
                    PollAction::Sleep {
                        until: secs(10) + wait
                    },
                    "{status} {value}"
                );
                assert_eq!(m.verdict(), Some(PollVerdict::RetryAfter));
            }
        }
    }

    #[test]
    fn a_retry_after_date_needs_the_wall_clock() {
        let date = "Sun, 06 Nov 1994 08:49:37 GMT";
        let mut m = machine();
        let input = answer_with(
            response(429, Some(date), ""),
            OfferGate::Open,
            Some(784_111_477),
            0,
        );
        assert_eq!(
            poll_once(&mut m, input, secs(0)),
            PollAction::Sleep { until: secs(300) }
        );

        let mut m = machine();
        let input = answer_with(response(429, Some(date), ""), OfferGate::Open, None, 0);
        assert_eq!(
            poll_once(&mut m, input, secs(0)),
            PollAction::Sleep {
                until: DEFAULT_POLL_INTERVAL
            }
        );
        assert_eq!(
            m.verdict(),
            Some(PollVerdict::BackOff {
                cause: Transient::Status(429),
                failures: 1
            })
        );
    }

    #[test]
    fn retry_after_survives_a_failed_body() {
        let mut m = machine();
        let failed = RequestOutcome::BodyFailed {
            status: 429,
            retry_after: RetryAfter::capture("90"),
            failure: BodyFailure::LengthMismatch,
        };
        assert_eq!(
            poll_once(&mut m, answer(failed), secs(0)),
            PollAction::Sleep { until: secs(90) }
        );
    }

    #[test]
    fn retry_after_on_another_status_is_ignored() {
        let mut m = machine();
        assert_eq!(
            poll_once(&mut m, answer(response(503, Some("60"), "")), secs(0)),
            PollAction::Sleep {
                until: DEFAULT_POLL_INTERVAL
            }
        );
        assert_eq!(
            m.verdict(),
            Some(PollVerdict::BackOff {
                cause: Transient::Status(503),
                failures: 1
            })
        );
    }

    #[test]
    fn transient_failures_back_off_exponentially_and_reset_on_an_answer() {
        let mut m = machine();
        let interval = DEFAULT_POLL_INTERVAL;
        let failures = [
            (RequestOutcome::Timeout, Transient::Timeout),
            (RequestOutcome::Unreachable, Transient::Unreachable),
            (
                RequestOutcome::Redirect { status: 302 },
                Transient::Redirect(302),
            ),
            (RequestOutcome::ForeignOrigin, Transient::ForeignOrigin),
            (response(500, None, ""), Transient::Status(500)),
            (response(429, None, ""), Transient::Status(429)),
            (response(204, None, ""), Transient::Status(204)),
            (response(400, None, ""), Transient::Status(400)),
            (
                RequestOutcome::BodyFailed {
                    status: 502,
                    retry_after: None,
                    failure: BodyFailure::Timeout,
                },
                Transient::Body(502),
            ),
        ];
        let mut now = secs(0);
        for (n, (outcome, cause)) in failures.into_iter().enumerate() {
            let n = n as u32 + 1;
            let action = poll_once(&mut m, answer_with(outcome, OfferGate::Open, None, 0), now);
            let nominal = (interval * 2u32.pow(n)).min(MAX_POLL_INTERVAL);
            assert_eq!(
                action,
                PollAction::Sleep {
                    until: now + nominal / 2
                },
                "{n}"
            );
            assert_eq!(
                m.verdict(),
                Some(PollVerdict::BackOff { cause, failures: n })
            );
            now += nominal / 2;
        }
        assert_eq!(
            poll_once(&mut m, answer(response(404, None, "")), now),
            PollAction::Sleep {
                until: now + interval
            }
        );
        let action = poll_once(&mut m, answer(RequestOutcome::Timeout), now + interval);
        assert_eq!(
            action,
            PollAction::Sleep {
                until: now + interval + interval
            }
        );
    }

    #[test]
    fn a_throttling_answer_keeps_the_failure_count() {
        let mut m = machine();
        let mut now = secs(0);
        for n in 1..=3u32 {
            let action = poll_once(&mut m, answer(response(503, None, "")), now);
            let PollAction::Sleep { until } = action else {
                panic!("{action:?}");
            };
            assert_eq!(
                m.verdict(),
                Some(PollVerdict::BackOff {
                    cause: Transient::Status(503),
                    failures: 2 * n - 1
                })
            );
            now = until;
            let action = poll_once(&mut m, answer(response(429, Some("30"), "")), now);
            assert_eq!(
                action,
                PollAction::Sleep {
                    until: now + secs(30)
                }
            );
            assert_eq!(m.verdict(), Some(PollVerdict::RetryAfter));
            now += secs(30);
            let action = poll_once(&mut m, answer(response(503, None, "")), now);
            let PollAction::Sleep { until } = action else {
                panic!("{action:?}");
            };
            assert_eq!(
                m.verdict(),
                Some(PollVerdict::BackOff {
                    cause: Transient::Status(503),
                    failures: 2 * n
                })
            );
            now = until;
        }
    }

    fn work(outcome: RequestOutcome) -> PollInput {
        PollInput::Performed(Some(WorkOutcome {
            outcome,
            wall_clock: None,
            random: 0,
        }))
    }

    /// Polls a 200 with a deployment link at `now` and returns the machine waiting for the retrieval.
    fn retrieving(now: Duration) -> PollMachine {
        let mut m = machine();
        let body = links(Some(&deployment("12")), None, None);
        let action = poll_once(&mut m, answer(ok(&body)), now);
        assert!(matches!(action, PollAction::RetrieveDeployment(_)));
        m
    }

    #[test]
    fn work_that_was_answered_normally_keeps_the_poll_timing() {
        for input in [
            PollInput::Performed(None),
            work(response(200, None, "{}")),
            work(response(204, None, "")),
        ] {
            let mut m = retrieving(secs(0));
            assert_eq!(
                m.poll(input, secs(5)),
                PollAction::Sleep { until: secs(65) }
            );
            assert_eq!(m.verdict(), Some(PollVerdict::Ok));
        }
    }

    #[test]
    fn a_throttled_work_request_honours_retry_after() {
        let mut m = retrieving(secs(0));
        assert_eq!(
            m.poll(work(response(429, Some("3600"), "")), secs(5)),
            PollAction::Sleep { until: secs(3605) }
        );
        assert_eq!(m.verdict(), Some(PollVerdict::RetryAfter));
    }

    #[test]
    fn a_failed_work_request_backs_off() {
        for (outcome, cause) in [
            (response(503, None, ""), Transient::Status(503)),
            (RequestOutcome::Timeout, Transient::Timeout),
            (
                RequestOutcome::BodyFailed {
                    status: 200,
                    retry_after: None,
                    failure: BodyFailure::ServerUnreachable,
                },
                Transient::Body(200),
            ),
        ] {
            let mut m = retrieving(secs(0));
            assert_eq!(
                m.poll(work(outcome), secs(5)),
                PollAction::Sleep {
                    until: secs(5) + secs(60)
                }
            );
            assert_eq!(
                m.verdict(),
                Some(PollVerdict::BackOff { cause, failures: 1 })
            );
            let action = poll_once(&mut m, answer(RequestOutcome::Timeout), secs(65));
            assert_eq!(
                action,
                PollAction::Sleep {
                    until: secs(65) + secs(120)
                },
                "the count carries into the next poll"
            );
        }
    }

    #[test]
    fn a_401_on_work_backs_off_and_counts() {
        let mut m = retrieving(secs(0));
        let span = secs(60).as_millis() as u32;
        let unauthorized = PollInput::Performed(Some(WorkOutcome {
            outcome: response(401, None, ""),
            wall_clock: None,
            random: span,
        }));
        assert_eq!(
            m.poll(unauthorized, secs(5)),
            PollAction::Sleep { until: secs(125) },
            "the upper bound of the first back-off from the 60 s interval"
        );
        assert_eq!(m.verdict(), Some(PollVerdict::Unauthorized));
        let action = poll_once(&mut m, answer(RequestOutcome::Timeout), secs(125));
        assert_eq!(
            action,
            PollAction::Sleep {
                until: secs(125) + secs(120)
            },
            "the count carries into the next poll"
        );
        assert_eq!(
            m.verdict(),
            Some(PollVerdict::BackOff {
                cause: Transient::Timeout,
                failures: 2
            })
        );
    }

    #[test]
    fn a_410_on_work_stops_polling() {
        let mut m = retrieving(secs(0));
        assert_eq!(
            m.poll(work(response(410, None, "")), secs(5)),
            PollAction::Stopped
        );
        assert_eq!(m.verdict(), Some(PollVerdict::Gone));
        assert_eq!(m.poll(PollInput::Tick, secs(1_000)), PollAction::Stopped);
    }

    #[test]
    fn a_failed_config_data_put_defers_the_link_work_to_the_next_poll() {
        for (failure, until) in [
            (response(503, None, ""), secs(1) + secs(60)),
            (response(429, Some("600"), ""), secs(601)),
        ] {
            let mut m = machine();
            assert_eq!(m.poll(PollInput::Tick, secs(0)), PollAction::Poll);
            let body = links(Some(&deployment("3")), None, None);
            assert_eq!(
                m.poll(answer(ok(&body)), secs(0)),
                PollAction::PutConfigData
            );
            assert_eq!(
                m.poll(work(failure), secs(1)),
                PollAction::Sleep { until },
                "no further request of this answer"
            );
            let action = poll_once(&mut m, answer(ok(&body)), until);
            assert_eq!(
                href_of(&action),
                deployment("3"),
                "the next poll offers it again"
            );
        }
    }

    #[test]
    fn the_back_off_uses_the_random_input() {
        let mut low = machine();
        let mut high = machine();
        let a = poll_once(
            &mut low,
            answer_with(RequestOutcome::Timeout, OfferGate::Open, None, 0),
            secs(0),
        );
        let span = DEFAULT_POLL_INTERVAL.as_millis() as u32;
        let b = poll_once(
            &mut high,
            answer_with(RequestOutcome::Timeout, OfferGate::Open, None, span),
            secs(0),
        );
        assert_eq!(
            a,
            PollAction::Sleep {
                until: DEFAULT_POLL_INTERVAL
            }
        );
        assert_eq!(
            b,
            PollAction::Sleep {
                until: DEFAULT_POLL_INTERVAL * 2
            }
        );
    }

    #[test]
    fn the_back_off_starts_from_the_server_interval() {
        let mut m = machine();
        poll_once(&mut m, answer(ok(&links(None, None, None))), secs(0));
        assert_eq!(m.interval(), secs(60));
        assert_eq!(
            poll_once(&mut m, answer(RequestOutcome::Unreachable), secs(60)),
            PollAction::Sleep { until: secs(120) }
        );
    }

    #[test]
    fn the_failure_count_saturates() {
        let mut m = machine();
        m.failures = u32::MAX - 1;
        let mut now = secs(0);
        for _ in 0..3 {
            let PollAction::Sleep { until } =
                poll_once(&mut m, answer(RequestOutcome::Timeout), now)
            else {
                panic!("no sleep");
            };
            assert!(until - now <= MAX_POLL_INTERVAL && until - now >= MAX_POLL_INTERVAL / 2);
            now = until;
        }
        assert_eq!(
            m.verdict(),
            Some(PollVerdict::BackOff {
                cause: Transient::Timeout,
                failures: u32::MAX
            })
        );
    }

    #[test]
    fn the_next_poll_time_saturates() {
        let mut m = machine();
        let now = Duration::MAX - secs(1);
        assert_eq!(m.poll(PollInput::Tick, now), PollAction::Poll);
        assert_eq!(
            m.poll(answer(response(404, None, "")), now),
            PollAction::Sleep {
                until: Duration::MAX
            }
        );
    }

    fn pending(outcome: RequestOutcome) -> PollInput {
        PollInput::Answered(PollAnswer {
            outcome,
            gate: OfferGate::Blocked,
            wall_clock: None,
            random: 0,
            verdict_pending: true,
        })
    }

    #[test]
    fn a_pending_verdict_is_delivered_last_after_a_valid_answer() {
        let mut m = machine();
        assert_eq!(m.poll(PollInput::Tick, secs(0)), PollAction::Poll);
        let body = links(None, Some(&cancel("4")), None);
        assert_eq!(
            m.poll(pending(ok(&body)), secs(0)),
            PollAction::PutConfigData
        );
        let next = m.poll(PollInput::Performed(None), secs(1));
        assert_eq!(href_of(&next), cancel("4"));
        assert_eq!(
            m.poll(PollInput::Performed(None), secs(2)),
            PollAction::DeliverVerdict
        );
        assert_eq!(
            m.poll(PollInput::Performed(None), secs(3)),
            PollAction::Sleep { until: secs(63) }
        );
    }

    #[test]
    fn a_deferred_or_refused_answer_still_delivers_the_verdict() {
        for body in [
            links(Some(&deployment("5")), None, None),
            links(Some("http://evil.lab/x"), None, None),
            links(None, None, None),
        ] {
            let mut m = machine();
            assert_eq!(
                poll_once(&mut m, pending(ok(&body)), secs(0)),
                PollAction::DeliverVerdict,
                "{body}"
            );
        }
    }

    #[test]
    fn no_verdict_delivery_without_a_valid_answer_or_a_pending_verdict() {
        for outcome in [
            response(401, None, ""),
            response(404, None, ""),
            response(429, Some("60"), ""),
            response(503, None, ""),
            ok("[]"),
            RequestOutcome::Timeout,
        ] {
            let mut m = machine();
            assert!(
                matches!(
                    poll_once(&mut m, pending(outcome.clone()), secs(0)),
                    PollAction::Sleep { .. }
                ),
                "{outcome:?}"
            );
        }
        let mut m = machine();
        assert!(matches!(
            poll_once(&mut m, answer(ok("{}")), secs(0)),
            PollAction::Sleep { .. }
        ));
    }

    #[test]
    fn a_failed_link_request_drops_the_verdict_delivery() {
        let mut m = machine();
        let body = links(None, Some(&cancel("4")), None);
        let next = poll_once(&mut m, pending(ok(&body)), secs(0));
        assert_eq!(href_of(&next), cancel("4"));
        assert_eq!(
            m.poll(work(response(429, Some("600"), "")), secs(1)),
            PollAction::Sleep { until: secs(601) }
        );
    }

    #[test]
    fn a_failed_verdict_send_re_times_the_next_poll() {
        let mut m = machine();
        assert_eq!(
            poll_once(&mut m, pending(ok("{}")), secs(0)),
            PollAction::DeliverVerdict
        );
        assert_eq!(
            m.poll(work(response(401, None, "")), secs(1)),
            PollAction::Sleep { until: secs(301) }
        );
        assert_eq!(m.verdict(), Some(PollVerdict::Unauthorized));
    }

    /// A machine that polled at 0 and waits for the next poll at 60 s.
    fn waiting() -> PollMachine {
        let mut m = machine();
        poll_once(&mut m, answer(ok(&links(None, None, None))), secs(0));
        assert_eq!(m.next_poll_at(), Some(secs(60)));
        m
    }

    #[test]
    fn a_nudge_moves_the_next_poll_to_now() {
        let mut m = waiting();
        assert_eq!(m.nudge(secs(10)), Nudge::Moved);
        assert_eq!(m.next_poll_at(), Some(secs(10)));
        assert_eq!(m.poll(PollInput::Tick, secs(10)), PollAction::Poll);
    }

    #[test]
    fn a_nudge_polls_no_sooner_than_10_s_after_the_last_poll() {
        let mut m = waiting();
        assert_eq!(m.nudge(secs(5)), Nudge::TooSoon, "a regular poll at 0");
        assert_eq!(m.next_poll_at(), Some(secs(60)));
        assert_eq!(m.nudge(secs(10)), Nudge::Moved);
        poll_once(&mut m, answer(ok("{}")), secs(10));
        assert_eq!(m.nudge(secs(19)), Nudge::TooSoon);
        assert_eq!(m.next_poll_at(), Some(secs(70)));
        assert_eq!(m.nudge(secs(20)), Nudge::Moved);
    }

    #[test]
    fn a_nudge_is_allowed_ten_seconds_after_the_poll_started() {
        let mut m = machine();
        assert_eq!(m.nudge_allowed_at(), None, "before the first poll");
        assert_eq!(m.poll(PollInput::Tick, secs(5)), PollAction::Poll);
        assert_eq!(
            m.poll(answer(ok(&links(None, None, None))), secs(5)),
            PollAction::PutConfigData
        );
        assert!(matches!(
            m.poll(PollInput::Performed(None), secs(6)),
            PollAction::Sleep { .. }
        ));
        assert_eq!(m.nudge_allowed_at(), Some(secs(15)));
        assert_eq!(m.nudge(secs(14)), Nudge::TooSoon);
        assert_eq!(m.nudge(secs(15)), Nudge::Moved);
    }

    #[test]
    fn a_nudge_is_ignored_while_a_poll_or_its_work_is_in_flight() {
        let mut m = machine();
        assert_eq!(m.poll(PollInput::Tick, secs(0)), PollAction::Poll);
        assert_eq!(m.nudge(secs(1)), Nudge::InFlight);
        let body = links(Some(&deployment("3")), None, None);
        assert_eq!(
            m.poll(answer(ok(&body)), secs(1)),
            PollAction::PutConfigData
        );
        assert_eq!(m.nudge(secs(2)), Nudge::InFlight);
        m.poll(PollInput::Performed(None), secs(2));
        assert_eq!(m.nudge(secs(3)), Nudge::InFlight);
        m.poll(PollInput::Performed(None), secs(3));
        assert_eq!(
            m.nudge(secs(10)),
            Nudge::Moved,
            "an ignored nudge is not counted"
        );
    }

    #[test]
    fn a_nudge_never_cuts_a_retry_after_wait() {
        let mut m = machine();
        poll_once(&mut m, answer(response(429, Some("600"), "")), secs(0));
        assert_eq!(m.nudge(secs(5)), Nudge::Throttled);
        assert_eq!(m.next_poll_at(), Some(secs(600)));
    }

    #[test]
    fn a_nudge_does_cut_a_back_off_wait() {
        let mut m = machine();
        poll_once(&mut m, answer(RequestOutcome::Timeout), secs(0));
        assert_eq!(m.nudge(secs(10)), Nudge::Moved);
    }

    #[test]
    fn a_nudge_when_the_poll_is_due_or_after_410_changes_nothing() {
        let mut m = machine();
        assert_eq!(m.nudge(secs(0)), Nudge::AlreadyDue);
        let mut m = waiting();
        assert_eq!(m.nudge(secs(60)), Nudge::AlreadyDue);
        assert_eq!(m.nudge(secs(61)), Nudge::AlreadyDue);
        assert_eq!(m.poll(PollInput::Tick, secs(61)), PollAction::Poll);
        m.poll(answer(response(410, None, "")), secs(61));
        assert_eq!(m.nudge(secs(70)), Nudge::Stopped);
    }

    #[test]
    fn link_work_debug_never_shows_a_link() {
        let mut m = machine();
        let secret = format!("{CONTROLLER}/deploymentBase/1?token=secretpass");
        let body = links(Some(&secret), None, None);
        let action = poll_once(&mut m, answer(ok(&body)), secs(0));
        assert!(matches!(action, PollAction::RetrieveDeployment(_)));
        assert!(!format!("{action:?} {m:?}").contains("secretpass"));
    }
}
