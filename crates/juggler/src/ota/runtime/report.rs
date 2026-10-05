//! Delivery of the `rolled_back` report: when to try, what an outcome means, and the retry schedule.
//!
//! The reporter thread (ESP-IDF tier) owns the loop and the effects; this machine only decides.
//! Time is `Duration` since the reporter started.
//!
//! The wait of the reporter is ALWAYS bounded by the retry interval, even when nothing is pending, so a report re-armed by a repair, or one that appears after boot, is picked up within one interval, and the thread notices a closed runtime.

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
}

/// What the pending-report view said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewStep {
    /// A report is ready: publish it.
    Publish,
    /// Nothing to publish now.
    Idle,
}

/// What to do after marking a report delivered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryStep {
    /// Done until the next window.
    Done,
    /// Another report is pending: read the view and publish again right away.
    PublishNext,
}

/// The reporter's schedule and bookkeeping.
#[derive(Debug, Clone, Copy)]
pub struct ReporterMachine {
    cfg: ReporterConfig,
    next_due: Duration,
    other_passes: u32,
}

impl ReporterMachine {
    /// A machine whose first attempt is due immediately.
    pub const fn new(cfg: ReporterConfig) -> Self {
        Self {
            cfg,
            next_due: Duration::ZERO,
            other_passes: 0,
        }
    }

    /// How long the loop may wait for a queued rejection: never longer than the retry interval.
    pub fn next_wait(&self, now: Duration) -> Duration {
        self.next_due.saturating_sub(now).min(self.cfg.retry)
    }

    /// The report check is due.
    pub fn report_due(&self, now: Duration) -> bool {
        now >= self.next_due
    }

    /// The broker is not connected: look again soon, without reading the store.
    pub fn on_disconnected(&mut self, now: Duration) {
        self.next_due = now + self.cfg.connect_poll;
    }

    /// The store was read: `ready` says a report is waiting.
    pub fn on_view(&mut self, now: Duration, ready: bool) -> ViewStep {
        if ready {
            ViewStep::Publish
        } else {
            self.other_passes = 0;
            self.next_due = now + self.cfg.retry;
            ViewStep::Idle
        }
    }

    /// The store could not be read or written: try again at the next window.
    pub fn on_store_error(&mut self, now: Duration) {
        self.other_passes = 0;
        self.next_due = now + self.cfg.retry;
    }

    /// The publish ended with `outcome`.
    pub fn on_publish(&mut self, now: Duration, outcome: PublishOutcome) -> PublishStep {
        if outcome == PublishOutcome::Acked {
            return PublishStep::MarkDelivered;
        }
        self.next_due = now + self.cfg.retry;
        PublishStep::RetryLater {
            loud: outcome == PublishOutcome::WrongThread,
        }
    }

    /// The delivery mark was written (or found nothing to do).
    ///
    /// A different pending report is followed by at most `max_other_passes` immediate passes, then by the normal window.
    pub fn on_delivery(&mut self, now: Duration, delivery: Delivery) -> DeliveryStep {
        if let Delivery::OtherReport { .. } = delivery {
            if self.other_passes < self.cfg.max_other_passes {
                self.other_passes += 1;
                self.next_due = now;
                return DeliveryStep::PublishNext;
            }
        }
        self.other_passes = 0;
        self.next_due = now + self.cfg.retry;
        DeliveryStep::Done
    }
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

/// The background retry of `complete_attempt` after the health policy ended with `ApplyNotRecorded`.
///
/// The image is already valid and `applied` already published; this only keeps trying to clear the attempt record so admission reopens without a reset.
/// The reporter thread owns the loop (no sleep happens under the store lock); this machine owns the schedule.
/// Time is `Duration` since the reporter started.
#[derive(Debug, Clone, Copy)]
pub struct CompletionRetry {
    cfg: CompletionRetryConfig,
    armed: Option<Schedule>,
}

#[derive(Debug, Clone, Copy)]
struct Schedule {
    next_due: Duration,
    delay: Duration,
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
            self.armed = Some(Schedule {
                next_due: now + self.cfg.initial,
                delay: self.cfg.initial,
            });
        }
    }

    /// How long the loop may wait before the next attempt, or `None` when not armed.
    pub fn next_wait(&self, now: Duration) -> Option<Duration> {
        self.armed.map(|s| s.next_due.saturating_sub(now))
    }

    /// An attempt is due.
    pub fn due(&self, now: Duration) -> bool {
        self.armed.is_some_and(|s| now >= s.next_due)
    }

    /// An attempt ended: success or a vanished attempt disarms, a failure doubles the delay up to the cap.
    ///
    /// Returns `true` while the retry stays armed.
    pub fn on_result(&mut self, now: Duration, result: CompletionTry) -> bool {
        match result {
            CompletionTry::Completed | CompletionTry::Gone => {
                self.armed = None;
            }
            CompletionTry::StillFailing => {
                if let Some(s) = self.armed.as_mut() {
                    s.delay = s.delay.saturating_mul(2).min(self.cfg.cap);
                    s.next_due = now + s.delay;
                }
            }
        }
        self.armed.is_some()
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
        let mut m = machine();
        assert_eq!(
            m.on_publish(secs(5), PublishOutcome::Acked),
            PublishStep::MarkDelivered
        );
        for outcome in [
            PublishOutcome::Timeout,
            PublishOutcome::Disconnected,
            PublishOutcome::Other,
        ] {
            let mut m = machine();
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
        let mut m = machine();
        assert_eq!(
            m.on_publish(secs(0), PublishOutcome::WrongThread),
            PublishStep::RetryLater { loud: true }
        );
        assert!(m.report_due(secs(10)));
    }

    #[test]
    fn delivery_is_done_until_the_next_window() {
        let mut m = machine();
        assert_eq!(m.on_delivery(secs(2), Delivery::Marked), DeliveryStep::Done);
        assert!(!m.report_due(secs(11)));
        assert!(m.report_due(secs(12)));
        assert_eq!(
            m.on_delivery(secs(12), Delivery::NotPending),
            DeliveryStep::Done
        );
    }

    #[test]
    fn another_pending_report_is_followed_by_at_most_two_immediate_passes() {
        let mut m = machine();
        let other = Delivery::OtherReport { pending_id: 7 };
        assert_eq!(m.on_delivery(secs(1), other), DeliveryStep::PublishNext);
        assert!(m.report_due(secs(1)));
        assert_eq!(m.on_delivery(secs(1), other), DeliveryStep::PublishNext);
        assert_eq!(m.on_delivery(secs(1), other), DeliveryStep::Done);
        assert!(!m.report_due(secs(1)));
        assert!(m.report_due(secs(11)));
        assert_eq!(m.on_delivery(secs(11), other), DeliveryStep::PublishNext);
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
            assert!(r.on_result(now, CompletionTry::StillFailing));
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
            assert!(!r.on_result(secs(1), done));
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
            assert!(r.on_result(now, CompletionTry::StillFailing));
            now += r.next_wait(now).unwrap();
        }
        assert!(r.is_armed());
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
