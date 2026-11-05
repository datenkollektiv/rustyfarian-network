//! Delivery of the `rolled_back` report: when to try, what an outcome means, and the retry schedule.
//!
//! The reporter thread (ESP-IDF tier) owns the loop and the effects.
//! One tier runs it on one thread, so it is flat logic (ADR 023 decision 3): the thread performs the view, publish and delivery steps in plain sequential code, and asks [`ReportPace`], [`after_publish`], [`CompletionBackoff`] and [`completion_step`] only for the decisions.
//! Time is `Duration` since the reporter started.
//!
//! The wait of the reporter is ALWAYS bounded by the retry interval, even when nothing is pending, so a report re-armed by a repair, or one that appears after boot, is picked up within one interval, and the thread notices a closed runtime.
//!
//! [`ReporterMachine`] and [`CompletionRetry`] are the earlier action machines over the same decisions; they are deprecated and kept for one release.
//! The `Ignored` variants of the step enums are returned only by those machines.

#![allow(deprecated)]

use core::time::Duration;

use crate::ota::persist::Delivery;
use crate::ota::runtime::config::OtaTimings;
use crate::ota::runtime::store_ops::CompletionTry;

/// The reporter's intervals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReporterConfig {
    /// Interval between report attempts and the longest idle wait.
    pub retry: Duration,
    /// How soon to look again while the broker is disconnected.
    pub connect_poll: Duration,
    /// Immediate follow-up passes after a delivery that found a different report pending.
    pub max_other_passes: u32,
}

impl From<&OtaTimings> for ReporterConfig {
    fn from(t: &OtaTimings) -> Self {
        Self {
            retry: t.report_retry,
            connect_poll: t.reporter_connect_poll,
            max_other_passes: 2,
        }
    }
}

/// How one publish with acknowledgement ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishOutcome {
    /// The broker acknowledged the message.
    Acked,
    /// No acknowledgement in time.
    Timeout,
    /// The session dropped before the acknowledgement.
    Disconnected,
    /// The call was made from inside an MQTT callback (a bug).
    WrongThread,
    /// A local fault (topic validation, enqueue, poisoned mutex).
    Other,
}

/// What to do after a publish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishStep {
    /// Mark the report delivered now.
    MarkDelivered,
    /// Keep the report and try again at the next window.
    RetryLater {
        /// Log at error level: the outcome is a bug, not a broker condition.
        loud: bool,
    },
    /// No publish was pending (the view did not ask for one); the machine is unchanged.
    Ignored,
}

/// What the pending-report view said.
///
/// Deprecated with [`ReporterMachine`].
#[deprecated(since = "0.7.0", note = "flat logic since ADR 023: use `ReportPace`")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewStep {
    /// A report is ready: publish it.
    Publish,
    /// Nothing to publish now.
    Idle,
    /// A publish or a delivery mark is still pending; the machine is unchanged.
    Ignored,
}

/// What to do after marking a report delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryStep {
    /// Done until the next window.
    Done,
    /// Another report is pending: read the view and publish again right away.
    PublishNext,
    /// No delivery mark was pending (no acknowledged publish); the machine is unchanged.
    Ignored,
}

/// The step of the current pass the machine waits for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Awaiting {
    /// A pending-report view ([`ReporterMachine::on_view`]).
    View,
    /// A publish outcome ([`ReporterMachine::on_publish`]).
    Publish,
    /// A delivery mark ([`ReporterMachine::on_delivery`]).
    Delivery,
}

/// When the next report pass is due and how many immediate follow-up passes ran.
///
/// Plain data: every transition returns the next value, so the reporter thread keeps one `ReportPace` and replaces it after each pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReportPace {
    next_due: Duration,
    other_passes: u32,
}

impl Default for ReportPace {
    fn default() -> Self {
        Self::new()
    }
}

impl ReportPace {
    /// A pace whose first pass is due immediately.
    pub const fn new() -> Self {
        Self {
            next_due: Duration::ZERO,
            other_passes: 0,
        }
    }

    /// How long the loop may wait for a queued rejection: never longer than the retry interval.
    pub fn wait(&self, cfg: &ReporterConfig, now: Duration) -> Duration {
        self.next_due.saturating_sub(now).min(cfg.retry)
    }

    /// The report check is due.
    pub fn due(&self, now: Duration) -> bool {
        now >= self.next_due
    }

    /// The broker is not connected: look again soon, without reading the store.
    #[must_use]
    pub fn disconnected(self, cfg: &ReporterConfig, now: Duration) -> Self {
        Self {
            next_due: now + cfg.connect_poll,
            ..self
        }
    }

    /// The pass ended with nothing to publish, or the store could not be read or written: try again at the next window.
    #[must_use]
    pub fn pass_ended(self, cfg: &ReporterConfig, now: Duration) -> Self {
        Self {
            next_due: now + cfg.retry,
            other_passes: 0,
        }
    }

    /// The publish did not end with an acknowledgement: keep the report and try again at the next window.
    #[must_use]
    pub fn publish_failed(self, cfg: &ReporterConfig, now: Duration) -> Self {
        Self {
            next_due: now + cfg.retry,
            ..self
        }
    }

    /// The delivery mark was written (or found nothing to do).
    ///
    /// A different pending report is followed by at most `max_other_passes` immediate passes ([`DeliveryStep::PublishNext`]), then by the normal window ([`DeliveryStep::Done`]).
    #[must_use]
    pub fn delivered(
        self,
        cfg: &ReporterConfig,
        now: Duration,
        delivery: Delivery,
    ) -> (DeliveryStep, Self) {
        if let Delivery::OtherReport { .. } = delivery {
            if self.other_passes < cfg.max_other_passes {
                return (
                    DeliveryStep::PublishNext,
                    Self {
                        next_due: now,
                        other_passes: self.other_passes + 1,
                    },
                );
            }
        }
        (DeliveryStep::Done, self.pass_ended(cfg, now))
    }
}

/// What to do after a publish ended with `outcome`.
///
/// Only an acknowledgement marks the report delivered; every other outcome keeps it, and a call from inside an MQTT callback is a bug worth an error log.
pub fn after_publish(outcome: PublishOutcome) -> PublishStep {
    if outcome == PublishOutcome::Acked {
        PublishStep::MarkDelivered
    } else {
        PublishStep::RetryLater {
            loud: outcome == PublishOutcome::WrongThread,
        }
    }
}

/// The report pass as an action machine.
///
/// Deprecated: the reporter runs on one thread of one tier, so the schedule is plain data and the order is sequential code (ADR 023 decisions 2 and 3).
#[deprecated(
    since = "0.7.0",
    note = "flat logic since ADR 023: keep a `ReportPace` and call `after_publish`"
)]
#[derive(Debug, Clone, Copy)]
pub struct ReporterMachine {
    cfg: ReporterConfig,
    pace: ReportPace,
    awaiting: Awaiting,
}

impl ReporterMachine {
    /// A machine whose first attempt is due immediately.
    pub const fn new(cfg: ReporterConfig) -> Self {
        Self {
            cfg,
            pace: ReportPace::new(),
            awaiting: Awaiting::View,
        }
    }

    /// How long the loop may wait for a queued rejection: never longer than the retry interval.
    pub fn next_wait(&self, now: Duration) -> Duration {
        self.pace.wait(&self.cfg, now)
    }

    /// The report check is due.
    pub fn report_due(&self, now: Duration) -> bool {
        self.pace.due(now)
    }

    /// The broker is not connected: end the pass and look again soon, without reading the store.
    pub fn on_disconnected(&mut self, now: Duration) {
        self.awaiting = Awaiting::View;
        self.pace = self.pace.disconnected(&self.cfg, now);
    }

    /// The store was read: `ready` says a report is waiting.
    ///
    /// Answered with [`ViewStep::Ignored`] while a publish or a delivery mark is pending.
    pub fn on_view(&mut self, now: Duration, ready: bool) -> ViewStep {
        if self.awaiting != Awaiting::View {
            return ViewStep::Ignored;
        }
        if ready {
            self.awaiting = Awaiting::Publish;
            ViewStep::Publish
        } else {
            self.pace = self.pace.pass_ended(&self.cfg, now);
            ViewStep::Idle
        }
    }

    /// The store could not be read or written, or the pass was abandoned: end it and try again at the next window.
    pub fn on_store_error(&mut self, now: Duration) {
        self.awaiting = Awaiting::View;
        self.pace = self.pace.pass_ended(&self.cfg, now);
    }

    /// The publish ended with `outcome`.
    ///
    /// Answered with [`PublishStep::Ignored`] unless the last view asked for the publish.
    pub fn on_publish(&mut self, now: Duration, outcome: PublishOutcome) -> PublishStep {
        if self.awaiting != Awaiting::Publish {
            return PublishStep::Ignored;
        }
        let step = after_publish(outcome);
        if step == PublishStep::MarkDelivered {
            self.awaiting = Awaiting::Delivery;
        } else {
            self.awaiting = Awaiting::View;
            self.pace = self.pace.publish_failed(&self.cfg, now);
        }
        step
    }

    /// The delivery mark was written (or found nothing to do).
    ///
    /// A different pending report is followed by at most `max_other_passes` immediate passes, then by the normal window.
    /// Answered with [`DeliveryStep::Ignored`] unless an acknowledged publish asked for the mark.
    pub fn on_delivery(&mut self, now: Duration, delivery: Delivery) -> DeliveryStep {
        if self.awaiting != Awaiting::Delivery {
            return DeliveryStep::Ignored;
        }
        self.awaiting = Awaiting::View;
        let (step, pace) = self.pace.delivered(&self.cfg, now, delivery);
        self.pace = pace;
        step
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

impl From<&OtaTimings> for CompletionRetryConfig {
    fn from(t: &OtaTimings) -> Self {
        Self {
            initial: t.complete_retry,
            cap: t.report_retry.max(t.complete_retry),
        }
    }
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

/// The background completion retry as an action machine.
///
/// Deprecated: the reporter thread keeps an `Option<CompletionBackoff>` instead (ADR 023 decisions 2 and 3).
#[deprecated(
    since = "0.7.0",
    note = "flat logic since ADR 023: keep an `Option<CompletionBackoff>` and call `completion_step`"
)]
#[derive(Debug, Clone, Copy)]
pub struct CompletionRetry {
    cfg: CompletionRetryConfig,
    armed: Option<CompletionBackoff>,
}

impl CompletionRetry {
    /// A machine that is not armed.
    pub const fn new(cfg: CompletionRetryConfig) -> Self {
        Self { cfg, armed: None }
    }

    /// The retry is armed.
    pub const fn is_armed(&self) -> bool {
        self.armed.is_some()
    }

    /// Arms the retry; the first attempt is due after `initial`.
    ///
    /// Arming an armed machine changes nothing, so a repeated request cannot reset the backoff.
    pub fn arm(&mut self, now: Duration) {
        if self.armed.is_none() {
            self.armed = Some(CompletionBackoff::armed(&self.cfg, now));
        }
    }

    /// How long the loop may wait before the next attempt, or `None` when not armed.
    pub fn next_wait(&self, now: Duration) -> Option<Duration> {
        self.armed.map(|b| b.wait(now))
    }

    /// An attempt is due.
    pub fn due(&self, now: Duration) -> bool {
        self.armed.is_some_and(|b| b.due(now))
    }

    /// An attempt ended: success or a vanished attempt disarms, a failure doubles the delay up to the cap.
    ///
    /// Answered with [`CompletionStep::Ignored`] when the retry is not armed.
    pub fn on_result(&mut self, now: Duration, result: CompletionTry) -> CompletionStep {
        let Some(backoff) = self.armed else {
            return CompletionStep::Ignored;
        };
        let step = completion_step(result);
        self.armed = match step {
            CompletionStep::Retry => Some(backoff.failed(&self.cfg, now)),
            _ => None,
        };
        step
    }
}

/// How many rejections were dropped since `last_seen`; `total` is a wrapping lifetime counter.
pub const fn rejects_delta(total: u32, last_seen: u32) -> u32 {
    total.wrapping_sub(last_seen)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RETRY: Duration = Duration::from_secs(10);

    fn machine() -> ReporterMachine {
        ReporterMachine::new(ReporterConfig::from(&OtaTimings::default()))
    }

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// A machine whose view asked for a publish.
    fn publishing() -> ReporterMachine {
        let mut m = machine();
        assert_eq!(m.on_view(secs(0), true), ViewStep::Publish);
        m
    }

    /// A machine whose publish was acknowledged.
    fn delivering() -> ReporterMachine {
        let mut m = publishing();
        assert_eq!(
            m.on_publish(secs(0), PublishOutcome::Acked),
            PublishStep::MarkDelivered
        );
        m
    }

    fn snapshot(m: &ReporterMachine) -> (Duration, u32, Awaiting) {
        (m.pace.next_due, m.pace.other_passes, m.awaiting)
    }

    fn cfg() -> ReporterConfig {
        ReporterConfig::from(&OtaTimings::default())
    }

    fn other() -> Delivery {
        Delivery::OtherReport { pending_id: 9 }
    }

    #[test]
    fn a_new_pace_is_due_immediately_and_its_wait_is_bounded_by_the_retry() {
        let pace = ReportPace::new();
        assert!(pace.due(Duration::ZERO));
        assert_eq!(pace.wait(&cfg(), Duration::ZERO), Duration::ZERO);
        let later = pace.pass_ended(&cfg(), secs(100));
        assert!(!later.due(secs(100)));
        assert_eq!(later.wait(&cfg(), secs(100)), cfg().retry);
        let soon = pace.disconnected(&cfg(), secs(100));
        assert_eq!(
            soon.wait(&cfg(), secs(100)),
            cfg().connect_poll.min(cfg().retry)
        );
        assert!(soon.due(secs(100) + cfg().connect_poll));
    }

    #[test]
    fn only_an_acked_publish_marks_delivered() {
        assert_eq!(
            after_publish(PublishOutcome::Acked),
            PublishStep::MarkDelivered
        );
        for outcome in [
            PublishOutcome::Timeout,
            PublishOutcome::Disconnected,
            PublishOutcome::Other,
        ] {
            assert_eq!(
                after_publish(outcome),
                PublishStep::RetryLater { loud: false }
            );
        }
        assert_eq!(
            after_publish(PublishOutcome::WrongThread),
            PublishStep::RetryLater { loud: true }
        );
    }

    #[test]
    fn a_failed_publish_keeps_the_follow_up_count_a_finished_pass_resets_it() {
        let c = cfg();
        let (step, pace) = ReportPace::new().delivered(&c, secs(0), other());
        assert_eq!(step, DeliveryStep::PublishNext);
        assert!(pace.due(secs(0)), "the follow-up pass is immediate");
        let pace = pace.publish_failed(&c, secs(1));
        assert_eq!(pace.other_passes, 1);
        assert_eq!(pace.wait(&c, secs(1)), c.retry);
        assert_eq!(pace.disconnected(&c, secs(2)).other_passes, 1);
        assert_eq!(pace.pass_ended(&c, secs(3)).other_passes, 0);
    }

    #[test]
    fn another_pending_report_is_followed_by_at_most_max_other_passes() {
        let c = cfg();
        let mut pace = ReportPace::new();
        for _ in 0..c.max_other_passes {
            let (step, next) = pace.delivered(&c, secs(5), other());
            assert_eq!(step, DeliveryStep::PublishNext);
            pace = next;
        }
        let (step, pace) = pace.delivered(&c, secs(5), other());
        assert_eq!(step, DeliveryStep::Done);
        assert_eq!(pace.wait(&c, secs(5)), c.retry);
        assert_eq!(pace.other_passes, 0);
        let (step, _) = ReportPace::new().delivered(&c, secs(5), Delivery::Marked);
        assert_eq!(step, DeliveryStep::Done);
    }

    #[test]
    fn the_completion_backoff_doubles_to_the_cap_and_stops_on_success() {
        let c = CompletionRetryConfig {
            initial: secs(2),
            cap: secs(10),
        };
        let b = CompletionBackoff::armed(&c, secs(100));
        assert_eq!(b.wait(secs(100)), secs(2));
        assert!(!b.due(secs(101)));
        assert!(b.due(secs(102)));
        let b = b.failed(&c, secs(102));
        assert_eq!(b.wait(secs(102)), secs(4));
        let b = b.failed(&c, secs(106)).failed(&c, secs(114));
        assert_eq!(b.wait(secs(114)), secs(10), "capped");
        assert_eq!(
            completion_step(CompletionTry::StillFailing),
            CompletionStep::Retry
        );
        assert_eq!(
            completion_step(CompletionTry::Completed),
            CompletionStep::Stop
        );
        assert_eq!(completion_step(CompletionTry::Gone), CompletionStep::Stop);
    }

    #[test]
    fn the_first_attempt_is_immediate() {
        let m = machine();
        assert!(m.report_due(Duration::ZERO));
        assert_eq!(m.next_wait(Duration::ZERO), Duration::ZERO);
    }

    #[test]
    fn the_idle_wait_is_always_bounded_by_the_retry_interval() {
        let mut m = machine();
        assert_eq!(m.on_view(secs(0), false), ViewStep::Idle);
        for t in 0..40 {
            assert!(m.next_wait(secs(t)) <= RETRY, "t={t}");
        }
        assert_eq!(m.next_wait(secs(3)), secs(7));
        assert!(!m.report_due(secs(9)));
        assert!(m.report_due(secs(10)));
    }

    #[test]
    fn a_report_armed_later_is_picked_up_within_one_interval() {
        let mut m = machine();
        assert_eq!(m.on_view(secs(0), false), ViewStep::Idle);
        assert!(m.report_due(secs(10)));
        assert_eq!(m.on_view(secs(10), true), ViewStep::Publish);
    }

    #[test]
    fn a_disconnected_broker_defers_without_reading_the_store() {
        let mut m = machine();
        m.on_disconnected(secs(0));
        assert!(!m.report_due(Duration::from_millis(999)));
        assert!(m.report_due(secs(1)));
        assert_eq!(m.next_wait(secs(0)), secs(1));
    }

    #[test]
    fn an_ack_marks_delivered_and_a_failure_retries_at_the_next_window() {
        let mut m = publishing();
        assert_eq!(
            m.on_publish(secs(5), PublishOutcome::Acked),
            PublishStep::MarkDelivered
        );
        for outcome in [
            PublishOutcome::Timeout,
            PublishOutcome::Disconnected,
            PublishOutcome::Other,
        ] {
            let mut m = publishing();
            assert_eq!(
                m.on_publish(secs(5), outcome),
                PublishStep::RetryLater { loud: false }
            );
            assert!(!m.report_due(secs(14)));
            assert!(m.report_due(secs(15)));
        }
    }

    #[test]
    fn a_wrong_thread_publish_is_loud_and_still_retried() {
        let mut m = publishing();
        assert_eq!(
            m.on_publish(secs(0), PublishOutcome::WrongThread),
            PublishStep::RetryLater { loud: true }
        );
        assert!(m.report_due(secs(10)));
    }

    #[test]
    fn delivery_is_done_until_the_next_window() {
        let mut m = delivering();
        assert_eq!(m.on_delivery(secs(2), Delivery::Marked), DeliveryStep::Done);
        assert!(!m.report_due(secs(11)));
        assert!(m.report_due(secs(12)));
        let mut m = delivering();
        assert_eq!(
            m.on_delivery(secs(12), Delivery::NotPending),
            DeliveryStep::Done
        );
    }

    #[test]
    fn another_pending_report_is_followed_by_at_most_two_immediate_passes() {
        let mut m = machine();
        let other = Delivery::OtherReport { pending_id: 7 };
        let deliver = |m: &mut ReporterMachine, now| {
            assert_eq!(m.on_view(now, true), ViewStep::Publish);
            assert_eq!(
                m.on_publish(now, PublishOutcome::Acked),
                PublishStep::MarkDelivered
            );
            m.on_delivery(now, other)
        };
        assert_eq!(deliver(&mut m, secs(1)), DeliveryStep::PublishNext);
        assert!(m.report_due(secs(1)));
        assert_eq!(deliver(&mut m, secs(1)), DeliveryStep::PublishNext);
        assert_eq!(deliver(&mut m, secs(1)), DeliveryStep::Done);
        assert!(!m.report_due(secs(1)));
        assert!(m.report_due(secs(11)));
        assert_eq!(deliver(&mut m, secs(11)), DeliveryStep::PublishNext);
    }

    #[test]
    fn a_publish_without_a_view_is_ignored() {
        for mut m in [machine(), delivering()] {
            let before = snapshot(&m);
            assert_eq!(
                m.on_publish(secs(3), PublishOutcome::Acked),
                PublishStep::Ignored
            );
            assert_eq!(
                m.on_publish(secs(3), PublishOutcome::Timeout),
                PublishStep::Ignored
            );
            assert_eq!(snapshot(&m), before);
        }
    }

    #[test]
    fn a_delivery_without_an_acked_publish_is_ignored() {
        let mut idle_view = machine();
        assert_eq!(idle_view.on_view(secs(0), false), ViewStep::Idle);
        let mut failed = publishing();
        failed.on_publish(secs(0), PublishOutcome::Timeout);
        for mut m in [machine(), publishing(), idle_view, failed] {
            let before = snapshot(&m);
            assert_eq!(
                m.on_delivery(secs(3), Delivery::Marked),
                DeliveryStep::Ignored
            );
            assert_eq!(snapshot(&m), before);
        }
    }

    #[test]
    fn a_view_while_a_step_is_pending_is_ignored() {
        for mut m in [publishing(), delivering()] {
            let before = snapshot(&m);
            assert_eq!(m.on_view(secs(3), true), ViewStep::Ignored);
            assert_eq!(m.on_view(secs(3), false), ViewStep::Ignored);
            assert_eq!(snapshot(&m), before);
        }
    }

    #[test]
    fn a_disconnect_or_store_error_ends_the_pass_from_any_phase() {
        for mut m in [publishing(), delivering()] {
            m.on_disconnected(secs(0));
            assert_eq!(m.on_view(secs(1), true), ViewStep::Publish);
        }
        for mut m in [publishing(), delivering()] {
            m.on_store_error(secs(0));
            assert_eq!(m.on_view(secs(10), true), ViewStep::Publish);
        }
    }

    #[test]
    fn a_completed_delivery_returns_to_the_view() {
        let mut m = delivering();
        m.on_delivery(secs(0), Delivery::Marked);
        assert_eq!(m.on_view(secs(10), true), ViewStep::Publish);
    }

    #[test]
    fn a_store_error_waits_for_the_next_window() {
        let mut m = machine();
        m.on_store_error(secs(4));
        assert!(!m.report_due(secs(13)));
        assert!(m.report_due(secs(14)));
    }

    #[test]
    fn the_dropped_rejection_delta_wraps() {
        assert_eq!(rejects_delta(5, 5), 0);
        assert_eq!(rejects_delta(9, 5), 4);
        assert_eq!(rejects_delta(2, u32::MAX - 1), 4);
    }

    #[test]
    fn the_config_follows_the_timings() {
        let t = OtaTimings {
            report_retry: secs(30),
            reporter_connect_poll: Duration::from_millis(250),
            ..OtaTimings::default()
        };
        let c = ReporterConfig::from(&t);
        assert_eq!(
            (c.retry, c.connect_poll, c.max_other_passes),
            (secs(30), Duration::from_millis(250), 2)
        );
    }

    fn retry() -> CompletionRetry {
        CompletionRetry::new(CompletionRetryConfig::from(&OtaTimings::default()))
    }

    #[test]
    fn the_completion_retry_is_idle_until_armed() {
        let r = retry();
        assert!(!r.is_armed());
        assert!(!r.due(secs(1000)));
        assert_eq!(r.next_wait(secs(0)), None);
    }

    #[test]
    fn the_first_completion_attempt_is_due_after_the_initial_delay() {
        let mut r = retry();
        r.arm(secs(30));
        assert!(!r.due(secs(30)));
        assert_eq!(r.next_wait(secs(30)), Some(secs(1)));
        assert!(r.due(secs(31)));
    }

    #[test]
    fn the_completion_backoff_doubles_up_to_the_cap() {
        let mut r = retry();
        r.arm(secs(0));
        let mut now = secs(1);
        let mut waits = alloc::vec::Vec::new();
        for _ in 0..6 {
            assert!(r.due(now));
            assert_eq!(
                r.on_result(now, CompletionTry::StillFailing),
                CompletionStep::Retry
            );
            let wait = r.next_wait(now).unwrap();
            waits.push(wait.as_secs());
            now += wait;
        }
        assert_eq!(waits, [2, 4, 8, 10, 10, 10]);
    }

    #[test]
    fn rearming_does_not_reset_the_backoff() {
        let mut r = retry();
        r.arm(secs(0));
        r.on_result(secs(1), CompletionTry::StillFailing);
        r.arm(secs(1));
        assert_eq!(r.next_wait(secs(1)), Some(secs(2)));
    }

    #[test]
    fn success_or_a_vanished_attempt_stops_the_retries() {
        for done in [CompletionTry::Completed, CompletionTry::Gone] {
            let mut r = retry();
            r.arm(secs(0));
            assert_eq!(r.on_result(secs(1), done), CompletionStep::Stop);
            assert!(!r.is_armed());
            assert!(!r.due(secs(10_000)));
        }
    }

    #[test]
    fn a_persistent_failure_never_gives_up() {
        let mut r = retry();
        r.arm(secs(0));
        let mut now = secs(1);
        for _ in 0..100 {
            assert_eq!(
                r.on_result(now, CompletionTry::StillFailing),
                CompletionStep::Retry
            );
            now += r.next_wait(now).unwrap();
        }
        assert!(r.is_armed());
    }

    #[test]
    fn a_result_on_an_unarmed_retry_is_ignored() {
        for result in [
            CompletionTry::Completed,
            CompletionTry::Gone,
            CompletionTry::StillFailing,
        ] {
            let mut r = retry();
            assert_eq!(r.on_result(secs(1), result), CompletionStep::Ignored);
            assert!(!r.is_armed());
            assert_eq!(r.next_wait(secs(1)), None);
        }
        let mut r = retry();
        r.arm(secs(0));
        r.on_result(secs(1), CompletionTry::Completed);
        assert_eq!(
            r.on_result(secs(2), CompletionTry::StillFailing),
            CompletionStep::Ignored,
            "a disarmed retry stays disarmed"
        );
        assert!(!r.is_armed());
    }

    #[test]
    fn the_completion_config_follows_the_timings() {
        let t = OtaTimings {
            complete_retry: secs(3),
            report_retry: secs(2),
            ..OtaTimings::default()
        };
        let c = CompletionRetryConfig::from(&t);
        assert_eq!((c.initial, c.cap), (secs(3), secs(3)));
    }
}
