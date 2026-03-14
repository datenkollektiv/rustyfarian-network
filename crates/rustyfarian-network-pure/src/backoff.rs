//! Exponential backoff delay iterator.
//!
//! Produces a sequence of delay values (in milliseconds) that double on each
//! step, clamped to a configured maximum. Useful for retry loops in Wi-Fi
//! reconnection and MQTT re-subscribe logic.

/// An infinite iterator that yields exponentially increasing delay values in
/// milliseconds, clamped to a configured maximum (`max_ms`, inclusive).
///
/// # Example
///
/// ```
/// use rustyfarian_network_pure::backoff::ExponentialBackoff;
///
/// let mut backoff = ExponentialBackoff::new(100, 10_000);
/// assert_eq!(backoff.next(), Some(100));
/// assert_eq!(backoff.next(), Some(200));
/// assert_eq!(backoff.next(), Some(400));
/// ```
pub struct ExponentialBackoff {
    base_ms: u64,
    max_ms: u64,
    attempt: u32,
}

impl ExponentialBackoff {
    /// Creates a new `ExponentialBackoff` starting at attempt 0.
    ///
    /// - `base_ms`: The delay for the first attempt (in milliseconds).
    /// - `max_ms`: The maximum delay to return (in milliseconds).
    pub fn new(base_ms: u64, max_ms: u64) -> Self {
        Self {
            base_ms,
            max_ms,
            attempt: 0,
        }
    }

    /// Resets the attempt counter back to 0, restarting the sequence.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

impl Iterator for ExponentialBackoff {
    type Item = u64;

    fn next(&mut self) -> Option<Self::Item> {
        let shift = 1u64.checked_shl(self.attempt).unwrap_or(u64::MAX);
        let delay = self.base_ms.saturating_mul(shift).min(self.max_ms);
        self.attempt = self.attempt.saturating_add(1);
        Some(delay)
    }
}

#[cfg(test)]
extern crate alloc;

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    #[test]
    fn progression() {
        let backoff = ExponentialBackoff::new(100, 10_000);
        let values: Vec<u64> = backoff.take(9).collect();
        assert_eq!(
            values,
            vec![100, 200, 400, 800, 1600, 3200, 6400, 10000, 10000]
        );
    }

    #[test]
    fn clamping_at_max() {
        let max_ms = 5_000;
        let backoff = ExponentialBackoff::new(100, max_ms);
        for value in backoff.take(20) {
            assert!(value <= max_ms);
        }
    }

    #[test]
    fn reset_restarts_sequence() {
        let mut backoff = ExponentialBackoff::new(100, 10_000);
        let first_three: Vec<u64> = (&mut backoff).take(3).collect();
        // Advance 2 more steps (total 5)
        backoff.next();
        backoff.next();
        backoff.reset();
        let after_reset: Vec<u64> = backoff.take(3).collect();
        assert_eq!(first_three, after_reset);
    }

    #[test]
    fn zero_base_yields_zero() {
        let backoff = ExponentialBackoff::new(0, 1_000);
        for value in backoff.take(20) {
            assert_eq!(value, 0);
        }
    }

    #[test]
    fn max_smaller_than_base() {
        let backoff = ExponentialBackoff::new(1_000, 500);
        for value in backoff.take(20) {
            assert_eq!(value, 500);
        }
    }

    #[test]
    fn large_attempt_no_overflow() {
        let max_ms = 60_000;
        let backoff = ExponentialBackoff::new(100, max_ms);
        for value in backoff.take(100) {
            assert!(value <= max_ms);
        }
    }

    #[test]
    fn single_step() {
        let backoff = ExponentialBackoff::new(42, 42);
        for value in backoff.take(10) {
            assert_eq!(value, 42);
        }
    }
}
