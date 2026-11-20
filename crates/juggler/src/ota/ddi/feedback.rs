//! DDI feedback work (ADR 023 rung 3): the delivery of a persisted terminal verdict, and the feedback bodies of the deployment and cancel paths.
//!
//! [`VerdictSender`] sends the verdict [`OtaStore::pending_verdict`](crate::ota::persist::OtaStore::pending_verdict) returns as `closed` feedback on the deployment path (E3), paced and retried until the server has it; its documentation has the rules.
//!
//! # Where a send happens
//!
//! - In the work that wrote the verdict: [`PollAction::RetrieveDeployment`](super::PollAction::RetrieveDeployment) after a refusal and [`PollAction::ReportUnreadable`](super::PollAction::ReportUnreadable) persist it and send it at once.
//! - In [`PollAction::DeliverVerdict`](super::PollAction::DeliverVerdict), which ends the hand-off of every valid poll answer while a verdict is pending, so the retry follows the poll rhythm.
//! - After the health check, which persists the success verdict and nudges the poll ([`PollMachine::nudge`](super::PollMachine::nudge)).
//!
//! In each, the tier passes the hand-back of the [`SendOutcome`] to [`PollInput::Performed`](super::PollInput::Performed), so a throttling, failing or 401 answer times the next poll as a poll answer would.
//! A 404 or 410 is never handed back: it ends the action, not polling ([`feedback_hand_back`]).
//!
//! # Progress
//!
//! [`FeedbackPost::proceeding`] is the `proceeding` feedback the tier sends at three points of the deployment path: before the download, before the reboot into the new image, and once after that image boots, before its health check.
//! Progress is best-effort: it is neither persisted nor retried, and `closed` + `success` follows only once the image is marked valid.
//!
//! The feedback `timestamp` is an input, milliseconds since the Unix epoch, omitted while the wall clock is not set.

use alloc::vec::Vec;
use core::time::Duration;

use super::endpoint::DdiEndpoint;
use super::poller::{PollVerdict, WorkOutcome};
use super::response::RequestOutcome;
use super::wire::{ActionId, Execution, Feedback, Finished, ProfileCode};
use crate::ota::persist::DdiVerdict;

/// Experimental: API may change before 1.0.
///
/// Shortest time between two sends of the same terminal feedback (fleet DDI profile §4).
pub const FEEDBACK_RETRY_INTERVAL: Duration = Duration::from_secs(30);

/// Experimental: API may change before 1.0.
///
/// Consecutive 401 answers to sends after which the [`VerdictSender`] stops sending the verdict of an action until a poll answer that is not a 401.
pub const MAX_UNAUTHORIZED_SENDS: u8 = 3;

/// Most details of a [`FeedbackPost`]: a stored verdict has at most two.
const POST_DETAILS: usize = 2;

/// Experimental: API may change before 1.0.
///
/// The feedback endpoint a [`FeedbackPost`] goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackPath {
    /// `…/deploymentBase/<action>/feedback` (E3): progress, refusals and the final result.
    Deployment,
    /// `…/cancelAction/<action>/feedback` (E5): the answer to a cancel, never sent to E3.
    Cancel,
}

/// Experimental: API may change before 1.0.
///
/// One feedback POST: the endpoint, the action and the body, ready for a tier transport.
///
/// The URL is built from the configured base ([`url`](Self::url)), never taken from a link; on ESP-IDF the transport's `deployment_feedback` and `cancel_feedback` build it themselves from [`action`](Self::action) and [`feedback`](Self::feedback).
///
/// # Examples
///
/// ```
/// use juggler::ota::ddi::wire::ActionId;
/// use juggler::ota::ddi::{ControllerId, DdiEndpoint, FeedbackPath, FeedbackPost};
///
/// let endpoint = DdiEndpoint::new("http://ddi.lab/DEFAULT", &ControllerId::parse("dev-1").unwrap()).unwrap();
/// let post = FeedbackPost::proceeding(ActionId::parse("7").unwrap(), Some(1_700_000_000_123));
/// assert_eq!(post.path, FeedbackPath::Deployment);
/// assert_eq!(post.url(&endpoint), "http://ddi.lab:80/DEFAULT/controller/v1/dev-1/deploymentBase/7/feedback");
/// assert_eq!(
///     post.feedback().encode().unwrap(),
///     r#"{"status":{"execution":"proceeding","result":{"finished":"none"},"details":[]},"timestamp":1700000000123}"#
/// );
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FeedbackPost<'a> {
    /// The endpoint.
    pub path: FeedbackPath,
    /// The action in the URL.
    pub action: ActionId,
    /// `status.execution`.
    pub execution: Execution,
    /// `status.result.finished`.
    pub finished: Finished,
    /// `status.code`.
    pub code: Option<ProfileCode>,
    /// `status.details`, fixed strings only.
    pub details: heapless::Vec<&'a str, POST_DETAILS>,
    /// The feedback `timestamp`, milliseconds since the Unix epoch.
    pub timestamp: Option<u64>,
}

impl FeedbackPost<'static> {
    /// `proceeding` on the deployment path, no details: sent before the download, before the reboot into the new image, and once after it boots, before its health check.
    pub fn proceeding(action: ActionId, timestamp_ms: Option<u64>) -> Self {
        Self::bare(
            FeedbackPath::Deployment,
            action,
            Execution::Proceeding,
            Finished::None,
            timestamp_ms,
        )
    }

    /// `closed` + `success` on the cancel path: the cancel is acknowledged and the action closes as canceled.
    pub fn cancel_acknowledged(action: ActionId, timestamp_ms: Option<u64>) -> Self {
        Self::bare(
            FeedbackPath::Cancel,
            action,
            Execution::Closed,
            Finished::Success,
            timestamp_ms,
        )
    }

    /// `rejected` on the cancel path: the cancel is refused and the action continues on the deployment path.
    pub fn cancel_rejected(action: ActionId, timestamp_ms: Option<u64>) -> Self {
        Self::bare(
            FeedbackPath::Cancel,
            action,
            Execution::Rejected,
            Finished::None,
            timestamp_ms,
        )
    }

    fn bare(
        path: FeedbackPath,
        action: ActionId,
        execution: Execution,
        finished: Finished,
        timestamp: Option<u64>,
    ) -> Self {
        FeedbackPost {
            path,
            action,
            execution,
            finished,
            code: None,
            details: heapless::Vec::new(),
            timestamp,
        }
    }
}

impl<'a> FeedbackPost<'a> {
    /// The `closed` feedback of a terminal verdict on the deployment path: its result, code and details.
    pub fn verdict(verdict: &'a DdiVerdict, timestamp_ms: Option<u64>) -> Self {
        FeedbackPost {
            path: FeedbackPath::Deployment,
            action: verdict.action.clone(),
            execution: Execution::Closed,
            finished: verdict.finished(),
            code: verdict.code(),
            details: verdict.detail_strs(),
            timestamp: timestamp_ms,
        }
    }

    /// The body, for [`Feedback::encode`] or a tier transport.
    ///
    /// It always encodes: at most two details of at most 32 bytes stay far inside the encoder's limits.
    pub fn feedback(&self) -> Feedback<'_> {
        Feedback {
            execution: self.execution,
            finished: self.finished,
            code: self.code,
            details: &self.details,
            timestamp: self.timestamp,
        }
    }

    /// The URL of the POST, built from the configured base.
    pub fn url(&self, endpoint: &DdiEndpoint) -> alloc::string::String {
        match self.path {
            FeedbackPath::Deployment => endpoint.deployment_feedback_url(&self.action),
            FeedbackPath::Cancel => endpoint.cancel_feedback_url(&self.action),
        }
    }
}

/// Experimental: API may change before 1.0.
///
/// What the outcome of any feedback POST hands back to [`PollInput::Performed`](super::PollInput::Performed): `None` for a 2xx, a 404 and a 410, whose meaning ends with the action, and the outcome otherwise, so a throttling, failing or 401 answer times the next poll.
///
/// A 410 handed back would stop polling for the boot, and a feedback 410 means only that the action is closed.
///
/// ```
/// use juggler::ota::ddi::{feedback_hand_back, DdiResponse, RequestOutcome, WorkOutcome};
///
/// let work = |status| WorkOutcome {
///     outcome: RequestOutcome::Response(DdiResponse { status, retry_after: None, body: Vec::new() }),
///     wall_clock: None,
///     random: 0,
/// };
/// assert_eq!(feedback_hand_back(work(410)), None);
/// assert!(feedback_hand_back(work(503)).is_some());
/// ```
pub fn feedback_hand_back(work: WorkOutcome) -> Option<WorkOutcome> {
    match status_of(&work.outcome) {
        Some(status) if (200..300).contains(&status) || matches!(status, 404 | 410) => None,
        _ => Some(work),
    }
}

/// The status of an outcome that carries one, the body notwithstanding: a feedback answer's body means nothing.
fn status_of(outcome: &RequestOutcome) -> Option<u16> {
    match outcome {
        RequestOutcome::Response(response) => Some(response.status),
        RequestOutcome::BodyFailed { status, .. } => Some(*status),
        _ => None,
    }
}

/// How many actions' send times a [`FeedbackPacer`] remembers at once.
pub(super) const PACER_CAPACITY: usize = 8;

/// The 30 s pacing of each action's feedback, shared by [`VerdictSender`] and the cancel path.
///
/// Remembers the send time of up to [`PACER_CAPACITY`] actions, so feedback for A, then B, then A again is still paced for A.
/// An entry is dropped once its [`FEEDBACK_RETRY_INTERVAL`] has passed.
/// When the table is full of entries that all still pace, a further action is refused until the earliest of them expires: the pacer never forgets a live entry, so the 30 s per action bound cannot be bypassed.
/// A device sees one or two actions at a time, so the refusal is not reached in practice.
#[derive(Debug, Clone, Default)]
pub(super) struct FeedbackPacer {
    /// The actions still paced and when each feedback was sent.
    sent_at: Vec<(ActionId, Duration)>,
    /// The action of the last feedback.
    last: Option<ActionId>,
}

impl FeedbackPacer {
    /// The action of the last feedback.
    pub(super) fn action(&self) -> Option<&ActionId> {
        self.last.as_ref()
    }

    /// `Ok` when a feedback for `action` may go out at `now`, or `Err` with the time it may: no sooner than [`FEEDBACK_RETRY_INTERVAL`] after the last one for the same action.
    pub(super) fn due(&self, action: &ActionId, now: Duration) -> Result<(), Duration> {
        let expiry = |sent: &Duration| sent.saturating_add(FEEDBACK_RETRY_INTERVAL);
        if let Some((_, sent)) = self.sent_at.iter().find(|(known, _)| known == action) {
            let at = expiry(sent);
            return if now < at { Err(at) } else { Ok(()) };
        }
        let mut live = self
            .sent_at
            .iter()
            .map(|(_, sent)| expiry(sent))
            .filter(|at| now < *at);
        if live.clone().count() < PACER_CAPACITY {
            return Ok(());
        }
        Err(live.by_ref().min().unwrap_or(now))
    }

    /// Records a feedback for `action` sent at `now`.
    pub(super) fn sent(&mut self, action: ActionId, now: Duration) {
        self.sent_at.retain(|(known, sent)| {
            *known != action && now < sent.saturating_add(FEEDBACK_RETRY_INTERVAL)
        });
        if self.sent_at.len() >= PACER_CAPACITY {
            // Only reached when the caller skipped `due`; dropping the oldest is the last resort.
            self.sent_at.remove(0);
        }
        self.last = Some(action.clone());
        self.sent_at.push((action, now));
    }

    /// Forgets which action the last feedback was for; the send times keep pacing their actions.
    fn clear(&mut self) {
        self.last = None;
    }
}

/// Experimental: API may change before 1.0.
///
/// What [`VerdictSender::next`] decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerdictStep<'v> {
    /// POST this feedback, then answer with [`VerdictSender::answered`].
    Send(FeedbackPost<'v>),
    /// The last send of this verdict was less than [`FEEDBACK_RETRY_INTERVAL`] ago; the next one is allowed at `at`.
    NotDue {
        /// The monotonic time of the next allowed send.
        at: Duration,
    },
    /// Three consecutive 401s: no send until a poll answer that is not a 401.
    Suspended,
    /// A send is in flight: answer it with [`VerdictSender::answered`] first; nothing changed.
    InFlight,
}

/// Experimental: API may change before 1.0.
///
/// What the answer to a verdict send meant (see [`VerdictSender`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SendOutcome {
    /// A 200, 404 or 410: call [`OtaStore::mark_verdict_delivered`](crate::ota::persist::OtaStore::mark_verdict_delivered) with this action; nothing is handed back.
    Delivered(ActionId),
    /// Not delivered: the verdict stays pending; hand this to [`PollInput::Performed`](super::PollInput::Performed) (`None` for another 2xx, which re-times nothing).
    Retry(Option<WorkOutcome>),
    /// The third consecutive 401 for this action: log it (this is returned once), keep the verdict, and hand the outcome to [`PollInput::Performed`](super::PollInput::Performed).
    Suspended(WorkOutcome),
    /// No send was in flight; nothing changed (ADR 023 decision 5).
    Ignored,
}

/// Experimental: API may change before 1.0.
///
/// Delivers persisted terminal verdicts: the 30 s pacing and the consecutive-401 rule, both per action.
///
/// It sends the verdict [`OtaStore::pending_verdict`](crate::ota::persist::OtaStore::pending_verdict) returns as `closed` feedback on the deployment path (E3) and decides what each answer means:
///
/// | Answer | Meaning |
/// |:--|:--|
/// | 200 | [`SendOutcome::Delivered`]: the server has the verdict; the tier calls [`OtaStore::mark_verdict_delivered`](crate::ota::persist::OtaStore::mark_verdict_delivered) |
/// | 404 or 410 | [`SendOutcome::Delivered`] too: the action is gone or already closed, and nothing would ever accept the verdict |
/// | 401, the third in a row to a send of the action | [`SendOutcome::Suspended`] once: no further send until a poll answer that is not a 401 ([`VerdictSender::poll_answered`]); the verdict stays persisted |
/// | anything else, another 2xx included | [`SendOutcome::Retry`]: the verdict stays pending and is sent again no sooner than [`FEEDBACK_RETRY_INTERVAL`] (30 s) after the last send |
///
/// The verdict survives reboots in the store, so the retry goes on across reboots and outages until one of the first two rows applies (fleet DDI profile §4 "Terminal feedback is retried": the server answers 200, 404 or 410, and no other status counts as delivered).
///
/// The 401 count runs over the answers to sends only: an answer that is not a 401 resets it, and a poll answer does not, because every send follows a poll the server accepted.
///
/// One send is in flight at a time: [`VerdictSender::next`] answers [`VerdictStep::InFlight`] until [`VerdictSender::answered`] took the answer.
///
/// It keeps its state in RAM; after a reboot the first send of a still-pending verdict goes out at once.
///
/// # Examples
///
/// ```
/// use core::time::Duration;
/// use juggler::ota::ddi::wire::ActionId;
/// use juggler::ota::ddi::{DdiResponse, SendOutcome, RequestOutcome, VerdictSender, VerdictStep, WorkOutcome};
/// use juggler::ota::persist::DdiVerdict;
///
/// let verdict = DdiVerdict::records_unreadable(ActionId::parse("7").unwrap());
/// let mut sender = VerdictSender::new();
/// let VerdictStep::Send(post) = sender.next(&verdict, Duration::ZERO, None) else { panic!() };
/// assert_eq!(
///     post.feedback().encode().unwrap(),
///     r#"{"status":{"execution":"closed","result":{"finished":"failure"},"code":1005,"details":["records_unreadable"]}}"#
/// );
/// let ok = WorkOutcome {
///     outcome: RequestOutcome::Response(DdiResponse { status: 200, retry_after: None, body: Vec::new() }),
///     wall_clock: None,
///     random: 0,
/// };
/// assert_eq!(sender.answered(ok), SendOutcome::Delivered(ActionId::parse("7").unwrap()));
/// ```
#[derive(Debug, Clone, Default)]
pub struct VerdictSender {
    /// The action of the last send and when it started.
    pacer: FeedbackPacer,
    in_flight: bool,
    unauthorized: u8,
    suspended: bool,
}

impl VerdictSender {
    /// A sender with no send behind it.
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether sending is suspended after [`MAX_UNAUTHORIZED_SENDS`] consecutive 401s.
    pub fn is_suspended(&self) -> bool {
        self.suspended
    }

    /// The next step for `verdict`, the store's pending verdict; `now` is the monotonic time since boot.
    ///
    /// A verdict for another action than the last send starts with no 401 count; an action sent less than 30 s ago stays paced.
    pub fn next<'v>(
        &mut self,
        verdict: &'v DdiVerdict,
        now: Duration,
        timestamp_ms: Option<u64>,
    ) -> VerdictStep<'v> {
        if self.in_flight {
            return VerdictStep::InFlight;
        }
        if self
            .pacer
            .action()
            .is_some_and(|action| *action != verdict.action)
        {
            self.pacer.clear();
            self.unauthorized = 0;
            self.suspended = false;
        }
        if self.suspended {
            return VerdictStep::Suspended;
        }
        if let Err(at) = self.pacer.due(&verdict.action, now) {
            return VerdictStep::NotDue { at };
        }
        self.pacer.sent(verdict.action.clone(), now);
        self.in_flight = true;
        VerdictStep::Send(FeedbackPost::verdict(verdict, timestamp_ms))
    }

    /// The answer to the last [`VerdictStep::Send`].
    pub fn answered(&mut self, work: WorkOutcome) -> SendOutcome {
        let Some(action) = self.pacer.action() else {
            return SendOutcome::Ignored;
        };
        if !core::mem::take(&mut self.in_flight) {
            return SendOutcome::Ignored;
        }
        match status_of(&work.outcome) {
            Some(200 | 404 | 410) => {
                self.unauthorized = 0;
                SendOutcome::Delivered(action.clone())
            }
            Some(401) => {
                self.unauthorized = self.unauthorized.saturating_add(1);
                if self.unauthorized >= MAX_UNAUTHORIZED_SENDS {
                    self.suspended = true;
                    SendOutcome::Suspended(work)
                } else {
                    SendOutcome::Retry(Some(work))
                }
            }
            _ => {
                self.unauthorized = 0;
                SendOutcome::Retry(feedback_hand_back(work))
            }
        }
    }

    /// The verdict of a poll answer ([`PollMachine::verdict`](super::PollMachine::verdict) right after [`PollInput::Answered`](super::PollInput::Answered)): any verdict but [`PollVerdict::Unauthorized`] lifts an active suspension and starts the 401 count afresh.
    ///
    /// Before a suspension it changes nothing, so 401 answers to sends count across polls.
    pub fn poll_answered(&mut self, verdict: PollVerdict) {
        if self.suspended && verdict != PollVerdict::Unauthorized {
            self.unauthorized = 0;
            self.suspended = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ota::ddi::wire::ProfileCode;
    use crate::ota::ddi::{ControllerId, DdiResponse, RetryAfter, Transient};
    use crate::ota::wire::FailReason;
    use alloc::vec::Vec;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    fn id(text: &str) -> ActionId {
        ActionId::parse(text).unwrap()
    }

    fn work(status: u16) -> WorkOutcome {
        WorkOutcome {
            outcome: RequestOutcome::Response(DdiResponse {
                status,
                retry_after: None,
                body: Vec::new(),
            }),
            wall_clock: None,
            random: 0,
        }
    }

    fn other(outcome: RequestOutcome) -> WorkOutcome {
        WorkOutcome {
            outcome,
            wall_clock: None,
            random: 0,
        }
    }

    fn refusal(action: &str) -> DdiVerdict {
        DdiVerdict::failed(id(action), FailReason::TargetMismatch).unwrap()
    }

    fn send(sender: &mut VerdictSender, verdict: &DdiVerdict, now: Duration) {
        assert!(
            matches!(sender.next(verdict, now, None), VerdictStep::Send(_)),
            "a send at {now:?}"
        );
    }

    #[test]
    fn the_pacer_paces_an_action_across_another_one() {
        let mut pacer = FeedbackPacer::default();
        let t0 = Duration::from_secs(100);
        pacer.sent(id("1"), t0);
        pacer.sent(id("2"), t0 + Duration::from_secs(5));
        let again = t0 + Duration::from_secs(10);
        assert_eq!(
            pacer.due(&id("1"), again),
            Err(t0 + FEEDBACK_RETRY_INTERVAL)
        );
        assert_eq!(
            pacer.due(&id("2"), again),
            Err(t0 + Duration::from_secs(35))
        );
        assert_eq!(pacer.action(), Some(&id("2")));
        assert_eq!(pacer.due(&id("1"), t0 + FEEDBACK_RETRY_INTERVAL), Ok(()));
    }

    #[test]
    fn the_pacer_refuses_a_new_action_when_full_of_live_entries() {
        let mut pacer = FeedbackPacer::default();
        let t0 = Duration::from_secs(10);
        for n in 0..PACER_CAPACITY {
            pacer.sent(
                id(&alloc::format!("{n}")),
                t0 + Duration::from_secs(n as u64),
            );
        }
        let now = t0 + Duration::from_secs(20);
        assert_eq!(pacer.due(&id("99"), now), Err(t0 + FEEDBACK_RETRY_INTERVAL));
        assert!(pacer.due(&id("0"), now).is_err());
        let later = t0 + FEEDBACK_RETRY_INTERVAL;
        assert_eq!(pacer.due(&id("99"), later), Ok(()));
        pacer.sent(id("99"), later);
        assert!(pacer.due(&id("1"), later).is_err());
        assert_eq!(pacer.sent_at.len(), PACER_CAPACITY);
    }

    #[test]
    fn a_verdict_sender_paces_an_action_it_left_and_came_back_to() {
        let mut sender = VerdictSender::new();
        let (a, b) = (refusal("1"), refusal("2"));
        send(&mut sender, &a, Duration::from_secs(100));
        sender.answered(work(503));
        send(&mut sender, &b, Duration::from_secs(105));
        sender.answered(work(503));
        assert_eq!(
            sender.next(&a, Duration::from_secs(110), None),
            VerdictStep::NotDue {
                at: Duration::from_secs(130)
            }
        );
        send(&mut sender, &a, Duration::from_secs(130));
    }

    #[test]
    fn a_verdict_encodes_as_closed_feedback_with_its_code_and_details() {
        let verdict = refusal("12");
        let post = FeedbackPost::verdict(&verdict, Some(1_700_000_000_000));
        assert_eq!(post.path, FeedbackPath::Deployment);
        assert_eq!(post.action, id("12"));
        assert_eq!(
            post.feedback().encode().unwrap(),
            r#"{"status":{"execution":"closed","result":{"finished":"failure"},"code":1007,"details":["target_mismatch"]},"timestamp":1700000000000}"#
        );
    }

    #[test]
    fn a_success_verdict_has_no_code_and_keeps_its_detail() {
        let verdict = DdiVerdict {
            action: id("3"),
            result: crate::ota::persist::VerdictResult::Success,
            details: [crate::ota::persist::KvStr::try_from("downgrade").unwrap()]
                .into_iter()
                .collect(),
        };
        assert_eq!(
            FeedbackPost::verdict(&verdict, None)
                .feedback()
                .encode()
                .unwrap(),
            r#"{"status":{"execution":"closed","result":{"finished":"success"},"details":["downgrade"]}}"#
        );
    }

    #[test]
    fn a_rollback_verdict_sends_both_details() {
        let verdict = DdiVerdict {
            action: id("3"),
            result: crate::ota::persist::VerdictResult::Failure(ProfileCode::RolledBack),
            details: ["rolled_back", "health_check"]
                .into_iter()
                .map(|d| crate::ota::persist::KvStr::try_from(d).unwrap())
                .collect(),
        };
        assert_eq!(
            FeedbackPost::verdict(&verdict, None)
                .feedback()
                .encode()
                .unwrap(),
            r#"{"status":{"execution":"closed","result":{"finished":"failure"},"code":1004,"details":["rolled_back","health_check"]}}"#
        );
    }

    #[test]
    fn cancel_and_progress_bodies_match_the_profile() {
        let cases = [
            (
                FeedbackPost::cancel_acknowledged(id("4"), Some(5)),
                FeedbackPath::Cancel,
                r#"{"status":{"execution":"closed","result":{"finished":"success"},"details":[]},"timestamp":5}"#,
            ),
            (
                FeedbackPost::cancel_rejected(id("4"), None),
                FeedbackPath::Cancel,
                r#"{"status":{"execution":"rejected","result":{"finished":"none"},"details":[]}}"#,
            ),
            (
                FeedbackPost::proceeding(id("4"), None),
                FeedbackPath::Deployment,
                r#"{"status":{"execution":"proceeding","result":{"finished":"none"},"details":[]}}"#,
            ),
        ];
        for (post, path, body) in cases {
            assert_eq!(post.path, path);
            assert_eq!(post.feedback().encode().unwrap(), body);
        }
    }

    #[test]
    fn urls_are_built_from_the_configured_base() {
        let endpoint = DdiEndpoint::new(
            "http://ddi.lab:8080/DEFAULT",
            &ControllerId::parse("dev-1").unwrap(),
        )
        .unwrap();
        assert_eq!(
            FeedbackPost::cancel_rejected(id("9"), None).url(&endpoint),
            "http://ddi.lab:8080/DEFAULT/controller/v1/dev-1/cancelAction/9/feedback"
        );
        assert_eq!(
            FeedbackPost::proceeding(id("9"), None).url(&endpoint),
            "http://ddi.lab:8080/DEFAULT/controller/v1/dev-1/deploymentBase/9/feedback"
        );
    }

    #[test]
    fn the_first_send_goes_at_once_and_200_404_410_deliver() {
        for status in [200, 404, 410] {
            let verdict = refusal("12");
            let mut sender = VerdictSender::new();
            send(&mut sender, &verdict, secs(0));
            assert_eq!(
                sender.answered(work(status)),
                SendOutcome::Delivered(id("12"))
            );
        }
    }

    #[test]
    fn a_status_that_arrived_decides_even_when_the_body_failed() {
        let verdict = refusal("12");
        let mut sender = VerdictSender::new();
        send(&mut sender, &verdict, secs(0));
        let outcome = RequestOutcome::BodyFailed {
            status: 200,
            retry_after: None,
            failure: crate::ota::ddi::BodyFailure::TooLarge,
        };
        assert_eq!(
            sender.answered(other(outcome)),
            SendOutcome::Delivered(id("12"))
        );
    }

    #[test]
    fn retries_are_paced_at_30_s_from_the_last_send() {
        let verdict = refusal("12");
        let mut sender = VerdictSender::new();
        send(&mut sender, &verdict, secs(100));
        assert_eq!(
            sender.answered(work(503)),
            SendOutcome::Retry(Some(work(503)))
        );
        assert_eq!(
            sender.next(&verdict, secs(129), None),
            VerdictStep::NotDue { at: secs(130) }
        );
        send(&mut sender, &verdict, secs(130));
    }

    #[test]
    fn a_2xx_other_than_200_is_retried_with_nothing_handed_back() {
        for status in [201, 202, 204, 299] {
            let verdict = refusal("12");
            let mut sender = VerdictSender::new();
            send(&mut sender, &verdict, secs(0));
            assert_eq!(
                sender.answered(work(status)),
                SendOutcome::Retry(None),
                "{status}"
            );
            assert_eq!(
                sender.next(&verdict, secs(29), None),
                VerdictStep::NotDue { at: secs(30) },
                "{status}"
            );
        }
    }

    #[test]
    fn unauthorized_sends_count_across_accepted_polls() {
        let verdict = refusal("12");
        let mut sender = VerdictSender::new();
        for n in 0..3u64 {
            // Every send follows a poll the server accepted.
            sender.poll_answered(PollVerdict::Ok);
            send(&mut sender, &verdict, secs(30 * n));
            let outcome = sender.answered(work(401));
            if n < 2 {
                assert!(matches!(outcome, SendOutcome::Retry(Some(_))), "{n}");
            } else {
                assert!(matches!(outcome, SendOutcome::Suspended(_)), "{n}");
            }
        }
        assert!(sender.is_suspended());
        assert_eq!(
            sender.next(&verdict, secs(90), None),
            VerdictStep::Suspended
        );
        sender.poll_answered(PollVerdict::Ok);
        assert!(!sender.is_suspended());
        send(&mut sender, &verdict, secs(120));
    }

    #[test]
    fn one_send_is_in_flight_at_a_time() {
        let a = refusal("12");
        let b = refusal("13");
        let mut sender = VerdictSender::new();
        send(&mut sender, &a, secs(0));
        assert_eq!(sender.next(&b, secs(1), None), VerdictStep::InFlight);
        assert_eq!(
            sender.answered(work(200)),
            SendOutcome::Delivered(id("12")),
            "the answer stays with the action it was sent for"
        );
        send(&mut sender, &b, secs(2));
    }

    #[test]
    fn failures_without_a_status_are_handed_back() {
        for outcome in [
            RequestOutcome::Timeout,
            RequestOutcome::Unreachable,
            RequestOutcome::Redirect { status: 302 },
        ] {
            let verdict = refusal("12");
            let mut sender = VerdictSender::new();
            send(&mut sender, &verdict, secs(0));
            assert_eq!(
                sender.answered(other(outcome.clone())),
                SendOutcome::Retry(Some(other(outcome)))
            );
        }
    }

    #[test]
    fn a_throttling_answer_is_handed_back_with_its_retry_after() {
        let verdict = refusal("12");
        let mut sender = VerdictSender::new();
        send(&mut sender, &verdict, secs(0));
        let throttled = other(RequestOutcome::Response(DdiResponse {
            status: 429,
            retry_after: RetryAfter::capture("600"),
            body: Vec::new(),
        }));
        assert_eq!(
            sender.answered(throttled.clone()),
            SendOutcome::Retry(Some(throttled))
        );
    }

    #[test]
    fn three_consecutive_401s_suspend_once_until_a_poll_that_is_not_a_401() {
        let verdict = refusal("12");
        let mut sender = VerdictSender::new();
        let mut now = secs(0);
        for _ in 0..2 {
            send(&mut sender, &verdict, now);
            assert_eq!(
                sender.answered(work(401)),
                SendOutcome::Retry(Some(work(401)))
            );
            sender.poll_answered(PollVerdict::Unauthorized);
            now += FEEDBACK_RETRY_INTERVAL;
        }
        send(&mut sender, &verdict, now);
        assert_eq!(
            sender.answered(work(401)),
            SendOutcome::Suspended(work(401))
        );
        assert!(sender.is_suspended());
        now += FEEDBACK_RETRY_INTERVAL * 10;
        assert_eq!(sender.next(&verdict, now, None), VerdictStep::Suspended);
        sender.poll_answered(PollVerdict::Unauthorized);
        assert_eq!(sender.next(&verdict, now, None), VerdictStep::Suspended);

        sender.poll_answered(PollVerdict::Ok);
        assert!(!sender.is_suspended());
        send(&mut sender, &verdict, now);
        assert_eq!(
            sender.answered(work(401)),
            SendOutcome::Retry(Some(work(401))),
            "the count starts afresh"
        );
    }

    #[test]
    fn any_poll_verdict_but_401_lifts_the_suspension() {
        for verdict in [
            PollVerdict::Ok,
            PollVerdict::NotFound,
            PollVerdict::RetryAfter,
            PollVerdict::BackOff {
                cause: Transient::Timeout,
                failures: 1,
            },
        ] {
            let mut sender = VerdictSender {
                suspended: true,
                unauthorized: 3,
                ..VerdictSender::new()
            };
            sender.poll_answered(verdict);
            assert!(!sender.is_suspended(), "{verdict:?}");
        }
    }

    #[test]
    fn another_answer_between_401s_breaks_the_run() {
        let verdict = refusal("12");
        let mut sender = VerdictSender::new();
        let mut now = secs(0);
        for status in [401, 401, 503, 401, 401] {
            send(&mut sender, &verdict, now);
            assert!(matches!(
                sender.answered(work(status)),
                SendOutcome::Retry(_)
            ));
            now += FEEDBACK_RETRY_INTERVAL;
        }
        assert!(!sender.is_suspended());
    }

    #[test]
    fn a_verdict_for_another_action_starts_afresh() {
        let first = refusal("12");
        let mut sender = VerdictSender::new();
        for n in 0..3 {
            send(&mut sender, &first, FEEDBACK_RETRY_INTERVAL * n);
            sender.answered(work(401));
        }
        assert!(sender.is_suspended());
        let second = refusal("13");
        send(&mut sender, &second, FEEDBACK_RETRY_INTERVAL * 3);
        assert!(!sender.is_suspended());
    }

    #[test]
    fn an_answer_without_a_send_is_ignored() {
        let mut sender = VerdictSender::new();
        assert_eq!(sender.answered(work(200)), SendOutcome::Ignored);
        let verdict = refusal("12");
        send(&mut sender, &verdict, secs(0));
        assert_eq!(sender.answered(work(200)), SendOutcome::Delivered(id("12")));
        assert_eq!(sender.answered(work(200)), SendOutcome::Ignored);
    }

    #[test]
    fn the_hand_back_drops_only_2xx_404_and_410() {
        for status in [200, 201, 204, 404, 410] {
            assert_eq!(feedback_hand_back(work(status)), None, "{status}");
        }
        for status in [400, 401, 403, 413, 429, 500, 503, 304] {
            assert_eq!(
                feedback_hand_back(work(status)),
                Some(work(status)),
                "{status}"
            );
        }
        assert!(feedback_hand_back(other(RequestOutcome::ForeignOrigin)).is_some());
    }

    #[test]
    fn pacing_saturates_at_the_end_of_time() {
        let verdict = refusal("12");
        let mut sender = VerdictSender::new();
        send(&mut sender, &verdict, Duration::MAX - secs(1));
        sender.answered(work(503));
        assert_eq!(
            sender.next(&verdict, Duration::MAX - secs(1), None),
            VerdictStep::NotDue { at: Duration::MAX }
        );
    }
}
