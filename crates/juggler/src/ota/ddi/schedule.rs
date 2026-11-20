//! When the next DDI poll goes: the clamp of every wait, the jittered exponential back-off and the `Retry-After` parser.
//!
//! Every wait, whether `config.polling.sleep`, a `Retry-After` or a back-off, is clamped to [`MIN_POLL_INTERVAL`] .. [`MAX_POLL_INTERVAL`].
//! The 30 s floor is the fleet profile's, the 24 h ceiling is library policy; both are fixed.
//! Time and random values are inputs: nothing here reads a clock or draws a number.

use core::time::Duration;

use super::response::RetryAfter;
use super::wire::digits;

/// Experimental: API may change before 1.0.
///
/// The shortest wait before a poll: 30 s.
pub const MIN_POLL_INTERVAL: Duration = Duration::from_secs(30);
/// Experimental: API may change before 1.0.
///
/// The longest wait before a poll: 24 h.
pub const MAX_POLL_INTERVAL: Duration = Duration::from_secs(24 * 3600);
/// Experimental: API may change before 1.0.
///
/// The poll interval until a poll answer carries `config.polling.sleep`: 5 min, the hawkBit default.
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// Experimental: API may change before 1.0.
///
/// `wait` clamped to [`MIN_POLL_INTERVAL`] .. [`MAX_POLL_INTERVAL`].
///
/// ```
/// use core::time::Duration;
/// use juggler::ota::ddi::{clamp_wait, MAX_POLL_INTERVAL, MIN_POLL_INTERVAL};
///
/// assert_eq!(clamp_wait(Duration::ZERO), MIN_POLL_INTERVAL);
/// assert_eq!(clamp_wait(Duration::from_secs(300)), Duration::from_secs(300));
/// assert_eq!(clamp_wait(Duration::MAX), MAX_POLL_INTERVAL);
/// ```
pub fn clamp_wait(wait: Duration) -> Duration {
    wait.clamp(MIN_POLL_INTERVAL, MAX_POLL_INTERVAL)
}

/// Experimental: API may change before 1.0.
///
/// The wait after the `failures`-th transient poll failure in a row, backing off exponentially from `interval`.
///
/// The nominal wait is `interval * 2^failures`, capped at [`MAX_POLL_INTERVAL`]; the result is drawn from its upper half, `nominal / 2 ..= nominal`, in milliseconds by `random` modulo the span, and then clamped (see [`clamp_wait`]).
/// So the first failure waits between one and two intervals, and at the cap the devices of a fleet spread over 12 h to 24 h.
/// `failures` of 0 is treated as 1.
///
/// ```
/// use core::time::Duration;
/// use juggler::ota::ddi::backoff_delay;
///
/// let interval = Duration::from_secs(60);
/// assert_eq!(backoff_delay(interval, 1, 0), Duration::from_secs(60));
/// assert_eq!(backoff_delay(interval, 1, 60_000), Duration::from_secs(120));
/// assert_eq!(backoff_delay(interval, 2, 0), Duration::from_secs(120));
/// ```
pub fn backoff_delay(interval: Duration, failures: u32, random: u32) -> Duration {
    let cap = MAX_POLL_INTERVAL.as_millis() as u64;
    let base = clamp_wait(interval).as_millis() as u64;
    let shift = failures.clamp(1, 63);
    let nominal = base.saturating_mul(1u64 << shift).min(cap);
    let low = nominal / 2;
    let span = nominal - low;
    let jitter = u64::from(random) % (span + 1);
    clamp_wait(Duration::from_millis(low + jitter))
}

impl RetryAfter {
    /// Experimental: API may change before 1.0.
    ///
    /// The wait this `Retry-After` asks for, unclamped; `None` when the value is neither form.
    ///
    /// - `delay-seconds`: one or more ASCII digits; a value beyond `u64` saturates.
    /// - `HTTP-date` in the IMF-fixdate form (see [`parse_http_date`]): the time from `wall_clock` (seconds since the Unix epoch) to that date, zero for a date in the past, and `None` while the wall clock is unknown.
    ///
    /// Spaces around the value are ignored.
    ///
    /// ```
    /// use core::time::Duration;
    /// use juggler::ota::ddi::RetryAfter;
    ///
    /// let delta = RetryAfter::capture("120").unwrap();
    /// assert_eq!(delta.delay(None), Some(Duration::from_secs(120)));
    /// let date = RetryAfter::capture("Thu, 01 Jan 1970 00:02:00 GMT").unwrap();
    /// assert_eq!(date.delay(Some(60)), Some(Duration::from_secs(60)));
    /// assert_eq!(date.delay(None), None);
    /// ```
    pub fn delay(&self, wall_clock: Option<u64>) -> Option<Duration> {
        let text = self.as_str().trim_matches(' ');
        if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) {
            // All digits, so `digits` fails only on an overflow, which saturates.
            let secs = digits(text.as_bytes()).unwrap_or(u64::MAX);
            return Some(Duration::from_secs(secs));
        }
        let at = parse_http_date(text)?;
        Some(Duration::from_secs(at.saturating_sub(wall_clock?)))
    }
}

/// Experimental: API may change before 1.0.
///
/// Parses an HTTP date in the IMF-fixdate form (`Sun, 06 Nov 1994 08:49:37 GMT`, RFC 9110 §5.6.7) into seconds since the Unix epoch.
///
/// Exactly 29 bytes: a day name, a two-digit day that exists in that month, a month name, a four-digit year from 1970, and `HH:MM:SS` with a second of at most 60.
/// The day name is checked for spelling, not against the date.
/// The obsolete RFC 850 and asctime forms are refused; RFC 9110 lets senders generate only this one.
///
/// ```
/// use juggler::ota::ddi::parse_http_date;
///
/// assert_eq!(parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"), Some(784_111_777));
/// assert_eq!(parse_http_date("Sunday, 06-Nov-94 08:49:37 GMT"), None);
/// ```
pub fn parse_http_date(text: &str) -> Option<u64> {
    const DAYS: [&[u8]; 7] = [b"Mon", b"Tue", b"Wed", b"Thu", b"Fri", b"Sat", b"Sun"];
    const MONTHS: [&[u8]; 12] = [
        b"Jan", b"Feb", b"Mar", b"Apr", b"May", b"Jun", b"Jul", b"Aug", b"Sep", b"Oct", b"Nov",
        b"Dec",
    ];
    let b = text.as_bytes();
    if b.len() != 29
        || !DAYS.contains(&&b[0..3])
        || &b[3..5] != b", "
        || b[7] != b' '
        || b[11] != b' '
        || b[16] != b' '
        || b[19] != b':'
        || b[22] != b':'
        || &b[25..29] != b" GMT"
    {
        return None;
    }
    let day = digits(&b[5..7])?;
    let month = MONTHS.iter().position(|m| *m == &b[8..11])? as u64 + 1;
    let year = digits(&b[12..16])?;
    let hour = digits(&b[17..19])?;
    let minute = digits(&b[20..22])?;
    let second = digits(&b[23..25])?;
    if year < 1970 || day == 0 || day > days_in_month(year, month) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    let days = days_since_epoch(year, month, day);
    Some(days * 86_400 + hour * 3600 + minute * 60 + second)
}

fn days_in_month(year: u64, month: u64) -> u64 {
    match month {
        2 if year.is_multiple_of(4) && (!year.is_multiple_of(100) || year.is_multiple_of(400)) => {
            29
        }
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

/// Days from 1970-01-01 to the given date (proleptic Gregorian, `year >= 1970`).
fn days_since_epoch(year: u64, month: u64, day: u64) -> u64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = y / 400;
    let yoe = y % 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIN: Duration = MIN_POLL_INTERVAL;
    const MAX: Duration = MAX_POLL_INTERVAL;

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    fn retry(text: &str) -> RetryAfter {
        RetryAfter::capture(text).unwrap()
    }

    #[test]
    fn clamp_bounds_are_inclusive() {
        assert_eq!(clamp_wait(secs(29)), MIN);
        assert_eq!(clamp_wait(MIN), MIN);
        assert_eq!(clamp_wait(secs(31)), secs(31));
        assert_eq!(clamp_wait(MAX), MAX);
        assert_eq!(clamp_wait(MAX + Duration::from_millis(1)), MAX);
        assert_eq!(MIN, secs(30));
        assert_eq!(MAX, secs(86_400));
    }

    #[test]
    fn backoff_doubles_from_the_interval() {
        let interval = secs(60);
        for (failures, nominal) in [(1u32, 120u64), (2, 240), (3, 480), (4, 960)] {
            let span = (nominal - nominal / 2) as u32 * 1000;
            assert_eq!(backoff_delay(interval, failures, 0), secs(nominal / 2));
            assert_eq!(backoff_delay(interval, failures, span), secs(nominal));
            assert_eq!(
                backoff_delay(interval, failures, span + 1),
                secs(nominal / 2)
            );
        }
    }

    #[test]
    fn backoff_jitter_stays_in_the_upper_half() {
        let interval = secs(300);
        for random in [0, 1, 7, 299_999, 300_000, 1 << 20, u32::MAX / 3, u32::MAX] {
            let wait = backoff_delay(interval, 1, random);
            assert!(wait >= secs(300) && wait <= secs(600), "{random}: {wait:?}");
        }
        assert_ne!(
            backoff_delay(interval, 1, 1),
            backoff_delay(interval, 1, 2),
            "the random value moves the wait"
        );
    }

    #[test]
    fn backoff_caps_at_a_day_with_jitter_kept() {
        let interval = secs(3600);
        for failures in [5, 6, 31, 63, 64, u32::MAX] {
            let low = backoff_delay(interval, failures, 0);
            let high = backoff_delay(interval, failures, u32::MAX);
            assert_eq!(low, MAX / 2, "{failures}");
            assert!(high <= MAX && high > low, "{failures}");
        }
        assert_eq!(backoff_delay(MAX, 1, 0), MAX / 2);
    }

    #[test]
    fn backoff_clamps_the_interval_first() {
        assert_eq!(backoff_delay(Duration::ZERO, 1, 0), MIN);
        assert_eq!(backoff_delay(Duration::ZERO, 0, 0), MIN);
        assert!(backoff_delay(Duration::MAX, 1, u32::MAX) <= MAX);
    }

    #[test]
    fn retry_after_delta_seconds() {
        assert_eq!(retry("120").delay(None), Some(secs(120)));
        assert_eq!(retry("0").delay(None), Some(Duration::ZERO));
        assert_eq!(retry("007").delay(Some(5)), Some(secs(7)));
        assert_eq!(retry(" 45 ").delay(None), Some(secs(45)));
        let huge = "9".repeat(32);
        assert_eq!(retry(&huge).delay(None), Some(secs(u64::MAX)));
    }

    #[test]
    fn retry_after_refuses_other_text() {
        for text in ["-1", "+5", "1.5", "1e3", "12s", "1 2", "soon", " ", "0x10"] {
            assert_eq!(retry(text).delay(Some(0)), None, "{text:?}");
        }
    }

    #[test]
    fn retry_after_date_needs_the_wall_clock() {
        let date = retry("Sun, 06 Nov 1994 08:49:37 GMT");
        assert_eq!(date.delay(None), None);
        assert_eq!(date.delay(Some(784_111_700)), Some(secs(77)));
        assert_eq!(date.delay(Some(784_111_777)), Some(Duration::ZERO));
        assert_eq!(date.delay(Some(u64::MAX)), Some(Duration::ZERO));
    }

    #[test]
    fn http_date_known_values() {
        assert_eq!(parse_http_date("Thu, 01 Jan 1970 00:00:00 GMT"), Some(0));
        assert_eq!(
            parse_http_date("Sun, 06 Nov 1994 08:49:37 GMT"),
            Some(784_111_777)
        );
        assert_eq!(
            parse_http_date("Wed, 21 Oct 2015 07:28:00 GMT"),
            Some(1_445_412_480)
        );
        assert_eq!(
            parse_http_date("Thu, 29 Feb 2024 12:00:00 GMT"),
            Some(1_709_208_000)
        );
        assert_eq!(
            parse_http_date("Tue, 29 Feb 2000 00:00:00 GMT"),
            Some(951_782_400)
        );
        assert_eq!(
            parse_http_date("Fri, 31 Dec 9999 23:59:60 GMT"),
            Some(253_402_300_800)
        );
    }

    #[test]
    fn http_date_refuses_every_other_shape() {
        for text in [
            "",
            "Sun, 06 Nov 1994 08:49:37 UTC",
            "Sun, 06 Nov 1994 08:49:37 gmt",
            "sun, 06 Nov 1994 08:49:37 GMT",
            "Sun, 06 nov 1994 08:49:37 GMT",
            "Sun, 6 Nov 1994 08:49:37 GMT",
            "Sun 06 Nov 1994 08:49:37 GMT ",
            "Sun, 06 Nov 1994 08:49:37 GMT ",
            "Sunday, 06-Nov-94 08:49:37 GMT",
            "Sun Nov  6 08:49:37 1994",
            "Sun, 00 Nov 1994 08:49:37 GMT",
            "Sun, 31 Nov 1994 08:49:37 GMT",
            "Fri, 29 Feb 2023 08:49:37 GMT",
            "Fri, 29 Feb 1900 08:49:37 GMT",
            "Sun, 06 Nov 1969 08:49:37 GMT",
            "Sun, 06 Nov 1994 24:00:00 GMT",
            "Sun, 06 Nov 1994 08:60:37 GMT",
            "Sun, 06 Nov 1994 08:49:61 GMT",
            "Sun, 06 Nov 19a4 08:49:37 GMT",
            "Sun, 06 Nov 1994 08-49-37 GMT",
            "Sun, +6 Nov 1994 08:49:37 GMT",
            "Xyz, 06 Nov 1994 08:49:37 GMT",
        ] {
            assert_eq!(parse_http_date(text), None, "{text:?}");
        }
    }

    #[test]
    fn http_date_never_panics_on_multibyte_text() {
        assert_eq!(parse_http_date("Sün, 06 Nov 1994 08:49:37 GMT"), None);
        assert_eq!(parse_http_date("Sun, 06 Növ 1994 08:49:37 GM"), None);
    }
}
