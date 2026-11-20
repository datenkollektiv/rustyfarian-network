//! The wall-clock rule of the DDI runtime.
//!
//! ESP-IDF boots at 1970 until SNTP sets the clock, so a reading below [`WALL_CLOCK_FLOOR_SECS`](juggler::ota::ddi::WALL_CLOCK_FLOOR_SECS) is not a date: both values below are `None` until the clock passes the floor, and a `Retry-After` date or a feedback timestamp is then left out rather than mis-timed.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use juggler::ota::ddi::clock_is_set;

/// The time since the Unix epoch, when the clock counts as set.
fn unix_now() -> Option<Duration> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .ok()
        .filter(|d| clock_is_set(d.as_secs()))
}

/// Seconds since the Unix epoch for `PollAnswer::wall_clock` and `WorkOutcome::wall_clock`.
pub(super) fn secs() -> Option<u64> {
    unix_now().map(|d| d.as_secs())
}

/// Milliseconds since the Unix epoch for `FeedbackPost::timestamp`.
pub(super) fn millis() -> Option<u64> {
    unix_now().and_then(|d| u64::try_from(d.as_millis()).ok())
}
