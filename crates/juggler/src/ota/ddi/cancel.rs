//! The DDI cancel path (ADR 023 rung 3, fleet DDI profile §5): what the device does with the `cancelAction` it retrieved for a [`PollAction::RetrieveCancel`](super::PollAction::RetrieveCancel).
//!
//! The tier performs each [`CancelStep`] and feeds the result back as a [`CancelInput`]; at [`CancelStep::Done`] it answers the poll machine with [`PollInput::Performed`](super::PollInput::Performed) and the hand-back the step carries.
//!
//! The rules are on [`CancelMachine`].

use core::time::Duration;

use super::feedback::{feedback_hand_back, FeedbackPacer, FeedbackPost};
use super::poller::WorkOutcome;
use super::response::{BodyFailure, RequestOutcome};
use super::wire::{ActionId, CancelAction, DdiError};
use crate::ota::download::CancelRequest;

/// Experimental: API may change before 1.0.
///
/// What the device knows of the action a cancel stops, looked up by the tier for [`CancelStep::LookUp`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelTarget {
    /// A download of this action is running, or is admitted and about to start: the tier holds its cancel gate (`CancelGate`, on ESP-IDF `OtaCancel`).
    Downloading,
    /// The attempt of this action is past its activation permit: the boot slot is switched, or the new image awaits its reboot or its health check, or its verdict is written while the attempt record could not be cleared yet.
    Activated,
    /// The store's pending verdict ([`OtaStore::pending_verdict`](crate::ota::persist::OtaStore::pending_verdict)) belongs to this action.
    VerdictPending,
    /// The OTA records cannot be read.
    Unreadable,
    /// Nothing of this action runs: it was never retrieved, deferred by `skip`, is closed, is unknown, or another action's download is the one running.
    NotRunning,
}

/// Experimental: API may change before 1.0.
///
/// The input to [`CancelMachine::poll`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelInput {
    /// The GET of the [`PollAction::RetrieveCancel`](super::PollAction::RetrieveCancel) link ended; starts a cancel.
    Retrieved(WorkOutcome),
    /// The answer to [`CancelStep::LookUp`].
    Target(CancelTarget),
    /// The answer of the download's cancel gate to [`CancelStep::RequestStop`].
    StopRequested(CancelRequest),
    /// The download returned after [`CancelStep::AwaitDownload`], and the tier settled it (`settle_failed_ddi_download` with `Cancelled` writes no verdict).
    DownloadReturned,
    /// The POST of [`CancelStep::Send`] ended.
    Sent(WorkOutcome),
}

/// Experimental: API may change before 1.0.
///
/// What the tier must do next for a cancel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelStep {
    /// Look up what the device knows of this action (the cancel's `stopId`), then answer with [`CancelInput::Target`].
    LookUp(ActionId),
    /// Request the stop on the running download's cancel gate, then answer with [`CancelInput::StopRequested`].
    RequestStop,
    /// Wait until the download returns and is settled, then answer with [`CancelInput::DownloadReturned`].
    AwaitDownload,
    /// POST this cancel feedback, then answer with [`CancelInput::Sent`].
    Send(FeedbackPost<'static>),
    /// The cancel is over: answer the poll machine with [`PollInput::Performed`](super::PollInput::Performed) and this hand-back.
    Done(Option<WorkOutcome>),
    /// The input did not answer the pending step; nothing changed (ADR 023 decision 5).
    Ignored,
}

/// Experimental: API may change before 1.0.
///
/// How the last cancel ended, for logs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelResult {
    /// `closed` + `success` was sent.
    Acknowledged,
    /// `rejected` was sent.
    Rejected,
    /// A feedback for this action went out less than 30 s ago; nothing was sent.
    Paced,
    /// The GET answered 404 or 410: the action is gone.
    Gone,
    /// The GET was not answered normally; its outcome was handed back.
    NotRetrieved,
    /// The GET answered 200 with a body that is not a cancel document.
    Invalid(DdiError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Phase {
    Idle,
    LookingUp(ActionId),
    Stopping(ActionId),
    Awaiting(ActionId),
    Sending,
}

/// Experimental: API may change before 1.0.
///
/// The cancel path (fleet DDI profile §5): what the device does with the `cancelAction` it retrieved for a [`PollAction::RetrieveCancel`](super::PollAction::RetrieveCancel).
///
/// One machine lives as long as the poll machine, so the 30 s pacing spans the cancels of successive polls.
/// A [`CancelInput::Retrieved`] always starts a cancel, also while a step of the last one is still pending, so a lost step never disables the cancel path.
///
/// # The answer, by what the device knows of the action the cancel stops (`stopId`)
///
/// | [`CancelTarget`] | Feedback on the cancel path (E5) |
/// |:--|:--|
/// | [`Downloading`](CancelTarget::Downloading), and the download's cancel gate accepts | `closed` + `success`, only after the download returned |
/// | [`Downloading`](CancelTarget::Downloading), and the gate answers too late | `rejected` |
/// | [`Activated`](CancelTarget::Activated) | `rejected`: the slot is switched, the action goes on |
/// | [`VerdictPending`](CancelTarget::VerdictPending) | `rejected`: the action returns to the deployment path, and [`PollAction::DeliverVerdict`](super::PollAction::DeliverVerdict) later in the same hand-off closes it with its real result |
/// | [`Unreadable`](CancelTarget::Unreadable) | `rejected`: the device cannot show the action never ran |
/// | [`NotRunning`](CancelTarget::NotRunning) | `closed` + `success`: nothing of the action runs, whether it was never retrieved, deferred, closed or never known |
///
/// The acknowledgement is not persisted: until it is delivered the server keeps the action canceling and offers the `cancelAction` again, and the device, which no longer runs the action, acknowledges it again.
/// At most one cancel feedback per action goes out per [`FEEDBACK_RETRY_INTERVAL`](super::FEEDBACK_RETRY_INTERVAL) (30 s); a re-offer inside it (after a nudge) sends nothing.
///
/// # The GET
///
/// A 404 or 410 means the action is gone: no feedback, and nothing is handed back, because a 410 would stop polling.
/// A 200 whose body is not a cancel document sends no feedback either; the next poll offers the link again.
/// Any other outcome (a timeout, no connection, a redirect, 401, 429, 5xx, a body that failed) is handed back, so the next poll honours it.
///
/// # Examples
///
/// ```
/// use core::time::Duration;
/// use juggler::ota::ddi::wire::ActionId;
/// use juggler::ota::ddi::{
///     CancelInput, CancelMachine, CancelStep, CancelTarget, DdiResponse, RequestOutcome, WorkOutcome,
/// };
///
/// let work = |status: u16, body: &[u8]| WorkOutcome {
///     outcome: RequestOutcome::Response(DdiResponse { status, retry_after: None, body: body.to_vec() }),
///     wall_clock: None,
///     random: 0,
/// };
/// let mut m = CancelMachine::new();
/// let now = Duration::ZERO;
/// let get = work(200, br#"{"id":"7","cancelAction":{"stopId":"7"}}"#);
/// assert_eq!(m.poll(CancelInput::Retrieved(get), now, None), CancelStep::LookUp(ActionId::parse("7").unwrap()));
/// let CancelStep::Send(post) = m.poll(CancelInput::Target(CancelTarget::NotRunning), now, None) else {
///     panic!()
/// };
/// assert_eq!(
///     post.feedback().encode().unwrap(),
///     r#"{"status":{"execution":"closed","result":{"finished":"success"},"details":[]}}"#
/// );
/// assert_eq!(m.poll(CancelInput::Sent(work(200, b"")), now, None), CancelStep::Done(None));
/// ```
#[derive(Debug, Clone)]
pub struct CancelMachine {
    phase: Phase,
    /// The cancel action of the last feedback and when it was sent.
    pacer: FeedbackPacer,
    result: Option<CancelResult>,
}

impl Default for CancelMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl CancelMachine {
    /// A machine with no cancel behind it.
    pub fn new() -> Self {
        Self {
            phase: Phase::Idle,
            pacer: FeedbackPacer::default(),
            result: None,
        }
    }

    /// How the last cancel ended, `None` before the first one.
    pub fn result(&self) -> Option<CancelResult> {
        self.result
    }

    /// Advances the cancel; `now` is the monotonic time since boot and `timestamp_ms` the feedback timestamp, milliseconds since the Unix epoch.
    pub fn poll(
        &mut self,
        input: CancelInput,
        now: Duration,
        timestamp_ms: Option<u64>,
    ) -> CancelStep {
        let phase = core::mem::replace(&mut self.phase, Phase::Idle);
        match (phase, input) {
            // A new cancel GET while a step is still pending means the tier lost that step (the poll machine hands out a cancel only after the last one was performed): start afresh rather than disable the cancel path for the boot.
            (_, CancelInput::Retrieved(work)) => self.retrieved(work),
            (Phase::LookingUp(action), CancelInput::Target(target)) => match target {
                CancelTarget::Downloading => {
                    self.phase = Phase::Stopping(action);
                    CancelStep::RequestStop
                }
                CancelTarget::NotRunning => self.answer(action, true, now, timestamp_ms),
                CancelTarget::Activated
                | CancelTarget::VerdictPending
                | CancelTarget::Unreadable => self.answer(action, false, now, timestamp_ms),
            },
            (Phase::Stopping(action), CancelInput::StopRequested(request)) => match request {
                CancelRequest::Accepted => {
                    self.phase = Phase::Awaiting(action);
                    CancelStep::AwaitDownload
                }
                CancelRequest::TooLate => self.answer(action, false, now, timestamp_ms),
            },
            (Phase::Awaiting(action), CancelInput::DownloadReturned) => {
                self.answer(action, true, now, timestamp_ms)
            }
            (Phase::Sending, CancelInput::Sent(work)) => CancelStep::Done(feedback_hand_back(work)),
            (phase, _) => {
                self.phase = phase;
                CancelStep::Ignored
            }
        }
    }

    /// Judges the GET (see "The GET" on [`CancelMachine`]).
    fn retrieved(&mut self, work: WorkOutcome) -> CancelStep {
        let (result, hand_back) = match &work.outcome {
            RequestOutcome::Response(response) if response.status == 200 => {
                match CancelAction::parse(&response.body) {
                    Ok(cancel) => {
                        self.phase = Phase::LookingUp(cancel.id);
                        return CancelStep::LookUp(cancel.stop_id);
                    }
                    Err(e) => (CancelResult::Invalid(e), None),
                }
            }
            RequestOutcome::BodyFailed {
                status: 200,
                failure: BodyFailure::TooLarge,
                ..
            } => (CancelResult::Invalid(DdiError::TooLarge), None),
            RequestOutcome::Response(response) if matches!(response.status, 404 | 410) => {
                (CancelResult::Gone, None)
            }
            RequestOutcome::BodyFailed {
                status: 404 | 410, ..
            } => (CancelResult::Gone, None),
            _ => (CancelResult::NotRetrieved, Some(work)),
        };
        self.result = Some(result);
        CancelStep::Done(hand_back)
    }

    /// Sends the acknowledgement or the rejection for the cancel action `action`, unless one went out less than 30 s ago.
    fn answer(
        &mut self,
        action: ActionId,
        acknowledge: bool,
        now: Duration,
        timestamp_ms: Option<u64>,
    ) -> CancelStep {
        if self.pacer.due(&action, now).is_err() {
            self.result = Some(CancelResult::Paced);
            return CancelStep::Done(None);
        }
        self.pacer.sent(action.clone(), now);
        let (post, result) = if acknowledge {
            (
                FeedbackPost::cancel_acknowledged(action, timestamp_ms),
                CancelResult::Acknowledged,
            )
        } else {
            (
                FeedbackPost::cancel_rejected(action, timestamp_ms),
                CancelResult::Rejected,
            )
        };
        self.result = Some(result);
        self.phase = Phase::Sending;
        CancelStep::Send(post)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ota::ddi::wire::{Execution, Finished};
    use crate::ota::ddi::{DdiResponse, FeedbackPath, RetryAfter};
    use alloc::format;
    use alloc::vec::Vec;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    fn id(text: &str) -> ActionId {
        ActionId::parse(text).unwrap()
    }

    fn other(outcome: RequestOutcome) -> WorkOutcome {
        WorkOutcome {
            outcome,
            wall_clock: None,
            random: 0,
        }
    }

    fn work(status: u16, body: &str) -> WorkOutcome {
        other(RequestOutcome::Response(DdiResponse {
            status,
            retry_after: None,
            body: Vec::from(body.as_bytes()),
        }))
    }

    fn cancel_body(id: &str, stop: &str) -> WorkOutcome {
        work(
            200,
            &format!(r#"{{"id":"{id}","cancelAction":{{"stopId":"{stop}"}}}}"#),
        )
    }

    /// Retrieves a cancel of action 7 and answers the look-up with `target`.
    fn decide(m: &mut CancelMachine, target: CancelTarget, now: Duration) -> CancelStep {
        assert_eq!(
            m.poll(CancelInput::Retrieved(cancel_body("7", "7")), now, None),
            CancelStep::LookUp(id("7"))
        );
        m.poll(CancelInput::Target(target), now, Some(42))
    }

    fn sent(step: CancelStep) -> FeedbackPost<'static> {
        match step {
            CancelStep::Send(post) => post,
            other => panic!("no send in {other:?}"),
        }
    }

    fn is_ack(post: &FeedbackPost<'_>) -> bool {
        post.execution == Execution::Closed && post.finished == Finished::Success
    }

    fn is_reject(post: &FeedbackPost<'_>) -> bool {
        post.execution == Execution::Rejected && post.finished == Finished::None
    }

    #[test]
    fn a_cancel_of_an_action_that_does_not_run_is_acknowledged() {
        let mut m = CancelMachine::new();
        let post = sent(decide(&mut m, CancelTarget::NotRunning, secs(0)));
        assert!(is_ack(&post));
        assert_eq!(
            (post.path, post.action.as_str(), post.timestamp),
            (FeedbackPath::Cancel, "7", Some(42))
        );
        assert!(post.code.is_none() && post.details.is_empty());
        assert_eq!(m.result(), Some(CancelResult::Acknowledged));
        assert_eq!(
            m.poll(CancelInput::Sent(work(200, "")), secs(1), None),
            CancelStep::Done(None)
        );
    }

    #[test]
    fn a_running_download_is_stopped_and_acknowledged_only_after_it_returned() {
        let mut m = CancelMachine::new();
        assert_eq!(
            decide(&mut m, CancelTarget::Downloading, secs(0)),
            CancelStep::RequestStop
        );
        assert_eq!(
            m.poll(
                CancelInput::StopRequested(CancelRequest::Accepted),
                secs(0),
                None
            ),
            CancelStep::AwaitDownload
        );
        assert_eq!(
            m.poll(CancelInput::Sent(work(200, "")), secs(1), None),
            CancelStep::Ignored,
            "no feedback before the download returned"
        );
        let post = sent(m.poll(CancelInput::DownloadReturned, secs(2), Some(9)));
        assert!(is_ack(&post));
        assert_eq!(post.timestamp, Some(9));
    }

    #[test]
    fn a_cancel_too_late_for_the_download_is_rejected() {
        let mut m = CancelMachine::new();
        decide(&mut m, CancelTarget::Downloading, secs(0));
        let post = sent(m.poll(
            CancelInput::StopRequested(CancelRequest::TooLate),
            secs(0),
            None,
        ));
        assert!(is_reject(&post));
        assert_eq!(post.path, FeedbackPath::Cancel);
        assert_eq!(m.result(), Some(CancelResult::Rejected));
    }

    #[test]
    fn activated_pending_and_unreadable_targets_are_rejected() {
        for target in [
            CancelTarget::Activated,
            CancelTarget::VerdictPending,
            CancelTarget::Unreadable,
        ] {
            let mut m = CancelMachine::new();
            let post = sent(decide(&mut m, target, secs(0)));
            assert!(is_reject(&post), "{target:?}");
        }
    }

    #[test]
    fn the_feedback_goes_to_the_cancel_action_and_the_look_up_to_its_stop_id() {
        let mut m = CancelMachine::new();
        assert_eq!(
            m.poll(
                CancelInput::Retrieved(cancel_body("11", "10")),
                secs(0),
                None
            ),
            CancelStep::LookUp(id("10"))
        );
        let post = sent(m.poll(CancelInput::Target(CancelTarget::NotRunning), secs(0), None));
        assert_eq!(post.action, id("11"));
    }

    #[test]
    fn a_re_offer_within_30_s_sends_nothing_and_after_it_answers_again() {
        let mut m = CancelMachine::new();
        sent(decide(&mut m, CancelTarget::NotRunning, secs(100)));
        m.poll(CancelInput::Sent(work(503, "")), secs(100), None);
        assert_eq!(
            decide(&mut m, CancelTarget::NotRunning, secs(129)),
            CancelStep::Done(None)
        );
        assert_eq!(m.result(), Some(CancelResult::Paced));
        assert!(is_ack(&sent(decide(
            &mut m,
            CancelTarget::NotRunning,
            secs(130)
        ))));
    }

    #[test]
    fn the_pacing_survives_an_answer_for_another_action_in_between() {
        let mut m = CancelMachine::new();
        assert!(matches!(
            m.answer(id("1"), true, secs(100), None),
            CancelStep::Send(_)
        ));
        assert!(matches!(
            m.answer(id("2"), true, secs(105), None),
            CancelStep::Send(_)
        ));
        assert_eq!(
            m.answer(id("1"), true, secs(110), None),
            CancelStep::Done(None)
        );
        assert_eq!(m.result(), Some(CancelResult::Paced));
        assert!(matches!(
            m.answer(id("1"), true, secs(130), None),
            CancelStep::Send(_)
        ));
    }

    #[test]
    fn the_pacing_is_per_action() {
        let mut m = CancelMachine::new();
        sent(decide(&mut m, CancelTarget::NotRunning, secs(0)));
        m.poll(CancelInput::Sent(work(200, "")), secs(0), None);
        m.poll(CancelInput::Retrieved(cancel_body("8", "8")), secs(1), None);
        assert!(is_ack(&sent(m.poll(
            CancelInput::Target(CancelTarget::NotRunning),
            secs(1),
            None
        ))));
    }

    #[test]
    fn the_feedback_answer_is_handed_back_unless_it_ends_the_action() {
        for (status, handed) in [
            (200, false),
            (404, false),
            (410, false),
            (401, true),
            (503, true),
        ] {
            let mut m = CancelMachine::new();
            sent(decide(&mut m, CancelTarget::NotRunning, secs(0)));
            let answer = work(status, "");
            let expected = handed.then(|| answer.clone());
            assert_eq!(
                m.poll(CancelInput::Sent(answer), secs(0), None),
                CancelStep::Done(expected),
                "{status}"
            );
        }
    }

    #[test]
    fn a_gone_action_sends_nothing_and_hands_nothing_back() {
        for outcome in [
            work(404, "").outcome,
            work(410, "").outcome,
            RequestOutcome::BodyFailed {
                status: 410,
                retry_after: None,
                failure: BodyFailure::ServerUnreachable,
            },
        ] {
            let mut m = CancelMachine::new();
            assert_eq!(
                m.poll(CancelInput::Retrieved(other(outcome)), secs(0), None),
                CancelStep::Done(None)
            );
            assert_eq!(m.result(), Some(CancelResult::Gone));
        }
    }

    #[test]
    fn an_invalid_document_sends_nothing() {
        for (outcome, error) in [
            (
                work(200, "[]").outcome,
                DdiError::Malformed { line: 1, column: 1 },
            ),
            (
                work(200, r#"{"id":"x","cancelAction":{"stopId":"1"}}"#).outcome,
                DdiError::InvalidActionId,
            ),
            (
                RequestOutcome::BodyFailed {
                    status: 200,
                    retry_after: None,
                    failure: BodyFailure::TooLarge,
                },
                DdiError::TooLarge,
            ),
        ] {
            let mut m = CancelMachine::new();
            assert_eq!(
                m.poll(CancelInput::Retrieved(other(outcome)), secs(0), None),
                CancelStep::Done(None)
            );
            assert_eq!(m.result(), Some(CancelResult::Invalid(error)));
        }
    }

    #[test]
    fn a_get_that_was_not_answered_normally_is_handed_back() {
        for outcome in [
            RequestOutcome::Timeout,
            RequestOutcome::Unreachable,
            RequestOutcome::Redirect { status: 302 },
            RequestOutcome::ForeignOrigin,
            work(401, "").outcome,
            work(503, "").outcome,
            RequestOutcome::Response(DdiResponse {
                status: 429,
                retry_after: RetryAfter::capture("60"),
                body: Vec::new(),
            }),
            RequestOutcome::BodyFailed {
                status: 200,
                retry_after: None,
                failure: BodyFailure::ServerUnreachable,
            },
        ] {
            let mut m = CancelMachine::new();
            let get = other(outcome);
            assert_eq!(
                m.poll(CancelInput::Retrieved(get.clone()), secs(0), None),
                CancelStep::Done(Some(get))
            );
            assert_eq!(m.result(), Some(CancelResult::NotRetrieved));
        }
    }

    #[test]
    fn out_of_phase_inputs_are_ignored_and_change_nothing() {
        let mut m = CancelMachine::new();
        for input in [
            CancelInput::Target(CancelTarget::NotRunning),
            CancelInput::StopRequested(CancelRequest::Accepted),
            CancelInput::DownloadReturned,
            CancelInput::Sent(work(200, "")),
        ] {
            assert_eq!(m.poll(input, secs(0), None), CancelStep::Ignored);
        }
        assert_eq!(
            m.poll(CancelInput::Retrieved(cancel_body("7", "7")), secs(0), None),
            CancelStep::LookUp(id("7"))
        );
        assert_eq!(
            m.poll(CancelInput::DownloadReturned, secs(0), None),
            CancelStep::Ignored
        );
        assert!(is_ack(&sent(m.poll(
            CancelInput::Target(CancelTarget::NotRunning),
            secs(0),
            None
        ))));
    }

    #[test]
    fn a_new_cancel_restarts_a_machine_that_lost_a_step() {
        let mut m = CancelMachine::new();
        assert_eq!(
            m.poll(CancelInput::Retrieved(cancel_body("7", "7")), secs(0), None),
            CancelStep::LookUp(id("7"))
        );
        assert_eq!(
            m.poll(
                CancelInput::Target(CancelTarget::Downloading),
                secs(0),
                None
            ),
            CancelStep::RequestStop
        );
        assert_eq!(
            m.poll(
                CancelInput::StopRequested(CancelRequest::Accepted),
                secs(0),
                None
            ),
            CancelStep::AwaitDownload
        );
        // `DownloadReturned` never arrives; the next poll offers a cancel again.
        assert_eq!(
            m.poll(
                CancelInput::Retrieved(cancel_body("8", "8")),
                secs(60),
                None
            ),
            CancelStep::LookUp(id("8"))
        );
        assert!(is_ack(&sent(m.poll(
            CancelInput::Target(CancelTarget::NotRunning),
            secs(60),
            None
        ))));
    }

    #[test]
    fn a_finished_cancel_accepts_the_next_one() {
        let mut m = CancelMachine::new();
        sent(decide(&mut m, CancelTarget::Activated, secs(0)));
        m.poll(CancelInput::Sent(work(200, "")), secs(0), None);
        assert_eq!(
            m.poll(
                CancelInput::Retrieved(cancel_body("9", "9")),
                secs(60),
                None
            ),
            CancelStep::LookUp(id("9"))
        );
    }
}
