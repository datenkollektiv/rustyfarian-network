//! Delivery of the `rolled_back` report: when to try, what an outcome means, and the retry schedule.
//!
//! The reporter thread (ESP-IDF tier) owns the loop and the effects.
//! One tier runs it on one thread, so it is flat logic (ADR 023 decision 3): the thread performs the view, publish and delivery steps in plain sequential code, and asks [`ReportPace`], [`after_publish`], [`CompletionBackoff`] and [`completion_step`] only for the decisions.
//! Time is `Duration` since the reporter started.
//!
//! The wait of the reporter is ALWAYS bounded by the retry interval, even when nothing is pending, so a report re-armed by a repair, or one that appears after boot, is picked up within one interval, and the thread notices a closed runtime.
//!
//! The `Ignored` variants of the step enums were returned only by the action machines removed in 0.8.0; no function returns them.

use core::time::Duration;

use crate::ota::persist::Delivery;
use crate::ota::runtime::config::OtaTimings;

pub use crate::ota::health::{
    completion_step, CompletionBackoff, CompletionRetryConfig, CompletionStep,
};

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

impl From<&OtaTimings> for CompletionRetryConfig {
    fn from(t: &OtaTimings) -> Self {
        Self {
            initial: t.complete_retry,
            cap: t.report_retry.max(t.complete_retry),
        }
    }
}

/// How many rejections were dropped since `last_seen`; `total` is a wrapping lifetime counter.
pub const fn rejects_delta(total: u32, last_seen: u32) -> u32 {
    total.wrapping_sub(last_seen)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ota::runtime::store_ops::CompletionTry;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
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
