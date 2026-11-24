//! When a failed artifact download is tried again, and after how long.
//!
//! The profile makes a transient download error terminal (1001) only once the download deadline has passed, never on the first failure.
//! One deadline, [`DdiTimings::firmware_deadline`](super::DdiTimings), spans every attempt of one action.
//! Time is an input: nothing here reads a clock or sleeps, so the tier and the host harness apply the same policy.

use core::time::Duration;

use crate::ota::OtaError;

/// Experimental: API may change before 1.0.
///
/// The wait after the first failed attempt: 5 s.
pub const RETRY_INITIAL_BACKOFF: Duration = Duration::from_secs(5);
/// Experimental: API may change before 1.0.
///
/// The longest wait between two attempts: 60 s.
pub const RETRY_MAX_BACKOFF: Duration = Duration::from_secs(60);

/// Experimental: API may change before 1.0.
///
/// What to do with a failed download attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DownloadRetry {
    /// Wait `wait`, then start a new attempt with the deadline budget that is left at that time.
    Retry {
        /// The back-off before the next attempt.
        wait: Duration,
    },
    /// The failure is final: settle the attempt with the last error as its reason.
    GiveUp,
}

/// Experimental: API may change before 1.0.
///
/// Whether `error` may go away on its own: an unreachable server, a read or header timeout, a body cut short, the flash held by another download, HTTP 408, 413, 429, 5xx, or the status 0 of a rejected response shape.
///
/// Every other error is final at once: any other 4xx (including a 404 or 410 artifact), a checksum or target mismatch, a flash write or partition error, a redirect, a foreign origin and a cancel.
///
/// ```
/// use juggler::ota::ddi::is_transient_download_error;
/// use juggler::ota::OtaError;
///
/// assert!(is_transient_download_error(&OtaError::DownloadFailed { status: 503 }));
/// assert!(!is_transient_download_error(&OtaError::DownloadFailed { status: 404 }));
/// assert!(!is_transient_download_error(&OtaError::ChecksumMismatch));
/// ```
pub const fn is_transient_download_error(error: &OtaError) -> bool {
    match error {
        OtaError::ServerUnreachable
        | OtaError::DownloadTimeout
        | OtaError::SizeMismatch
        | OtaError::FlashBusy => true,
        OtaError::DownloadFailed { status } => {
            matches!(*status, 0 | 408 | 413 | 429) || (*status >= 500 && *status <= 599)
        }
        _ => false,
    }
}

/// Experimental: API may change before 1.0.
///
/// The back-off after the `attempts`-th failed attempt: [`RETRY_INITIAL_BACKOFF`] doubling per attempt, capped at [`RETRY_MAX_BACKOFF`].
///
/// `attempts` of 0 is treated as 1.
///
/// ```
/// use core::time::Duration;
/// use juggler::ota::ddi::download_retry_backoff;
///
/// assert_eq!(download_retry_backoff(1), Duration::from_secs(5));
/// assert_eq!(download_retry_backoff(2), Duration::from_secs(10));
/// assert_eq!(download_retry_backoff(5), Duration::from_secs(60));
/// assert_eq!(download_retry_backoff(1000), Duration::from_secs(60));
/// ```
pub fn download_retry_backoff(attempts: u32) -> Duration {
    let doublings = attempts.saturating_sub(1).min(16);
    RETRY_INITIAL_BACKOFF
        .saturating_mul(1 << doublings)
        .min(RETRY_MAX_BACKOFF)
}

/// Experimental: API may change before 1.0.
///
/// Decides what follows a failed download attempt.
///
/// `attempts` counts the attempts made so far including the failed one (so 1 after the first), `elapsed` is the time since the first attempt began and `deadline` the one budget of all attempts, `retry_after` the wait the server's `Retry-After` asked for (`None` when there was none or its date could not be read).
/// A transient `error` is retried after the longer of [`download_retry_backoff`] and `retry_after`, that wait clamped to the time left (`deadline - elapsed`), so the last wait ends exactly at the deadline.
/// It is [`DownloadRetry::GiveUp`] only when `error` is final or `elapsed >= deadline`; a [`OtaError::DownloadTimeout`] is transient only while time is left.
/// The caller checks the clock again after the wait: if the deadline has passed it starts no attempt and gives up with the last real `error` as the reason, so a last timeout still maps to 1001.
///
/// ```
/// use core::time::Duration;
/// use juggler::ota::ddi::{decide_download_retry, DownloadRetry};
/// use juggler::ota::OtaError;
///
/// let deadline = Duration::from_secs(300);
/// assert_eq!(
///     decide_download_retry(&OtaError::ServerUnreachable, 1, Duration::from_secs(1), deadline, None),
///     DownloadRetry::Retry { wait: Duration::from_secs(5) }
/// );
/// assert_eq!(
///     decide_download_retry(&OtaError::ChecksumMismatch, 1, Duration::ZERO, deadline, None),
///     DownloadRetry::GiveUp
/// );
/// ```
pub fn decide_download_retry(
    error: &OtaError,
    attempts: u32,
    elapsed: Duration,
    deadline: Duration,
    retry_after: Option<Duration>,
) -> DownloadRetry {
    if !is_transient_download_error(error) || elapsed >= deadline {
        return DownloadRetry::GiveUp;
    }
    let wait = download_retry_backoff(attempts)
        .max(retry_after.unwrap_or(Duration::ZERO))
        .min(deadline - elapsed);
    DownloadRetry::Retry { wait }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEADLINE: Duration = Duration::from_secs(300);

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    #[test]
    fn transient_errors_are_retried() {
        let transient = [
            OtaError::ServerUnreachable,
            OtaError::DownloadTimeout,
            OtaError::SizeMismatch,
            OtaError::FlashBusy,
            OtaError::DownloadFailed { status: 0 },
            OtaError::DownloadFailed { status: 408 },
            OtaError::DownloadFailed { status: 429 },
            OtaError::DownloadFailed { status: 500 },
            OtaError::DownloadFailed { status: 503 },
            OtaError::DownloadFailed { status: 599 },
        ];
        for e in transient {
            assert_eq!(
                decide_download_retry(&e, 1, secs(1), DEADLINE, None),
                DownloadRetry::Retry { wait: secs(5) },
                "{e:?}"
            );
        }
    }

    #[test]
    fn final_errors_give_up_at_once() {
        let terminal = [
            OtaError::DownloadFailed { status: 400 },
            OtaError::DownloadFailed { status: 401 },
            OtaError::DownloadFailed { status: 403 },
            OtaError::DownloadFailed { status: 404 },
            OtaError::DownloadFailed { status: 410 },
            OtaError::DownloadFailed { status: 600 },
            OtaError::ChecksumMismatch,
            OtaError::FlashWriteFailed,
            OtaError::PartitionNotFound,
            OtaError::InsufficientSpace,
            OtaError::TargetMismatch,
            OtaError::Redirect { status: 302 },
            OtaError::ForeignOrigin,
            OtaError::Cancelled,
            OtaError::VersionInvalid,
        ];
        for e in terminal {
            assert_eq!(
                decide_download_retry(&e, 1, Duration::ZERO, DEADLINE, None),
                DownloadRetry::GiveUp,
                "{e:?}"
            );
        }
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let expected = [5, 5, 10, 20, 40, 60, 60, 60, 60];
        for (n, secs) in (0..).zip(expected) {
            assert_eq!(download_retry_backoff(n).as_secs(), secs, "attempts {n}");
        }
        assert_eq!(download_retry_backoff(u32::MAX), RETRY_MAX_BACKOFF);
    }

    #[test]
    fn the_last_wait_is_clamped_to_the_time_left() {
        let e = OtaError::DownloadFailed { status: 503 };
        assert_eq!(
            decide_download_retry(&e, 1, secs(294), DEADLINE, None),
            DownloadRetry::Retry { wait: secs(5) }
        );
        assert_eq!(
            decide_download_retry(&e, 1, secs(295), DEADLINE, None),
            DownloadRetry::Retry { wait: secs(5) }
        );
        assert_eq!(
            decide_download_retry(&e, 1, secs(298), DEADLINE, None),
            DownloadRetry::Retry { wait: secs(2) },
            "clamped to the 2 s left"
        );
        assert_eq!(
            decide_download_retry(&e, 8, secs(255), DEADLINE, None),
            DownloadRetry::Retry { wait: secs(45) },
            "a 60 s back-off from 255 s ends at the deadline"
        );
        assert_eq!(
            decide_download_retry(&e, 4, secs(200), DEADLINE, None),
            DownloadRetry::Retry { wait: secs(40) }
        );
        assert_eq!(
            decide_download_retry(&e, 4, DEADLINE, DEADLINE, None),
            DownloadRetry::GiveUp,
            "the deadline has passed"
        );
    }

    #[test]
    fn a_timeout_after_the_deadline_is_final() {
        let e = OtaError::DownloadTimeout;
        assert_eq!(
            decide_download_retry(&e, 2, DEADLINE, DEADLINE, None),
            DownloadRetry::GiveUp
        );
        assert_eq!(
            decide_download_retry(&e, 2, secs(400), DEADLINE, None),
            DownloadRetry::GiveUp
        );
        assert_eq!(
            decide_download_retry(&e, 2, secs(60), DEADLINE, None),
            DownloadRetry::Retry { wait: secs(10) }
        );
    }

    #[test]
    fn the_attempt_count_drives_the_wait() {
        let e = OtaError::ServerUnreachable;
        let wait =
            |attempts| match decide_download_retry(&e, attempts, Duration::ZERO, DEADLINE, None) {
                DownloadRetry::Retry { wait } => wait.as_secs(),
                other => panic!("{other:?}"),
            };
        assert_eq!(
            (wait(1), wait(2), wait(3), wait(6), wait(50)),
            (5, 10, 20, 60, 60)
        );
    }

    #[test]
    fn a_zero_deadline_never_retries() {
        assert_eq!(
            decide_download_retry(
                &OtaError::ServerUnreachable,
                1,
                Duration::ZERO,
                Duration::ZERO,
                None
            ),
            DownloadRetry::GiveUp
        );
    }

    #[test]
    fn retry_after_raises_the_wait_and_is_clamped_to_the_time_left() {
        let e = OtaError::DownloadFailed { status: 429 };
        let at = |attempts: u32, elapsed: u64, ra: Option<u64>| {
            decide_download_retry(&e, attempts, secs(elapsed), DEADLINE, ra.map(secs))
        };
        assert_eq!(
            at(1, 0, Some(120)),
            DownloadRetry::Retry { wait: secs(120) }
        );
        assert_eq!(
            at(5, 0, Some(10)),
            DownloadRetry::Retry { wait: secs(60) },
            "a shorter Retry-After never lowers the back-off"
        );
        assert_eq!(
            at(1, 250, Some(600)),
            DownloadRetry::Retry { wait: secs(50) },
            "clamped to the time left"
        );
        assert_eq!(at(1, 300, Some(1)), DownloadRetry::GiveUp);
    }

    #[test]
    fn a_413_is_transient() {
        assert!(is_transient_download_error(&OtaError::DownloadFailed {
            status: 413
        }));
    }
}
