//! Limits of the manifest fetch: a size cap and a TOTAL time limit.
//!
//! The per-read timeout of the HTTP client bounds one network operation only; a server that dribbles one byte per interval would never trip it.
//! [`ManifestBody`] adds the total limit, checked after the response headers and after every read.
//! The worst case is the total limit plus one per-operation timeout, because a check only runs between operations.

use core::time::Duration;

use crate::ota::{Deadline, FailReason, ManifestError, OtaError, MAX_MANIFEST_BYTES};

/// What to do after one read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Feed {
    /// Read more.
    Continue,
    /// The body is complete (a zero-length read).
    Done,
    /// More than `MAX_MANIFEST_BYTES` arrived: stop and reject.
    TooLarge,
    /// The total time limit passed: stop and reject.
    DeadlineExceeded,
}

/// Tracks the bytes and time of one manifest body.
#[derive(Debug, Clone, Copy)]
pub struct ManifestBody {
    deadline: Deadline,
    len: usize,
}

impl ManifestBody {
    /// Starts a body with a total time limit; start the clock BEFORE the connection is created.
    pub const fn new(total: Duration) -> Self {
        Self {
            deadline: Deadline::after(total),
            len: 0,
        }
    }

    /// Bytes received so far.
    pub const fn len(&self) -> usize {
        self.len
    }

    /// No byte received yet.
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The check after connect and headers, before any body byte.
    ///
    /// # Errors
    ///
    /// [`FetchError::DeadlineExceeded`] once `elapsed` reaches the total limit.
    pub fn check(&self, elapsed: Duration) -> Result<(), FetchError> {
        self.deadline
            .check(elapsed)
            .map_err(|_| FetchError::DeadlineExceeded)
    }

    /// Accounts for one read of `n` bytes at `elapsed`.
    ///
    /// A zero-length read ends the body, and wins over the deadline: a complete body is never discarded for being late.
    /// The size is checked before the time, so an oversize body reports `TooLarge`.
    /// Once `TooLarge` is returned the caller stops reading, so a read buffer of `MAX_MANIFEST_BYTES + 1` bytes always has room for the next read.
    pub fn feed(&mut self, n: usize, elapsed: Duration) -> Feed {
        if n == 0 {
            return Feed::Done;
        }
        self.len = self.len.saturating_add(n);
        if self.len > MAX_MANIFEST_BYTES {
            return Feed::TooLarge;
        }
        match self.deadline.check(elapsed) {
            Ok(()) => Feed::Continue,
            Err(_) => Feed::DeadlineExceeded,
        }
    }
}

/// Why a manifest fetch failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchError {
    /// The connection, request or response headers failed (including a non-200 status).
    Connect(OtaError),
    /// A body read failed.
    Read(OtaError),
    /// The body is larger than `MAX_MANIFEST_BYTES`.
    TooLarge,
    /// The total time limit passed.
    DeadlineExceeded,
    /// The body is complete but is not a valid manifest.
    Parse(ManifestError),
}

impl FetchError {
    /// The status reason: the transport failures are `manifest_fetch`, an oversize body or a bad manifest is the manifest error's reason.
    pub const fn reason(&self) -> FailReason {
        match self {
            FetchError::Connect(_) | FetchError::Read(_) | FetchError::DeadlineExceeded => {
                FailReason::ManifestFetch
            }
            FetchError::TooLarge => ManifestError::TooLarge.reason(),
            FetchError::Parse(e) => e.reason(),
        }
    }
}

impl core::fmt::Display for FetchError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            FetchError::Connect(_) => f.write_str("manifest connection failed"),
            FetchError::Read(_) => f.write_str("manifest read failed"),
            FetchError::TooLarge => f.write_str("manifest body too large"),
            FetchError::DeadlineExceeded => f.write_str("manifest fetch exceeded its total limit"),
            FetchError::Parse(_) => f.write_str("manifest invalid"),
        }
    }
}

impl core::error::Error for FetchError {}

#[cfg(test)]
mod tests {
    use super::*;

    const TOTAL: Duration = Duration::from_secs(10);

    #[test]
    fn a_body_within_limits_continues_then_ends() {
        let mut b = ManifestBody::new(TOTAL);
        assert!(b.is_empty());
        assert_eq!(b.check(Duration::from_secs(1)), Ok(()));
        assert_eq!(b.feed(300, Duration::from_secs(1)), Feed::Continue);
        assert_eq!(b.feed(300, Duration::from_secs(2)), Feed::Continue);
        assert_eq!(b.len(), 600);
        assert_eq!(b.feed(0, Duration::from_secs(2)), Feed::Done);
    }

    #[test]
    fn exactly_the_size_limit_is_fine_one_more_is_too_large() {
        let mut b = ManifestBody::new(TOTAL);
        assert_eq!(b.feed(MAX_MANIFEST_BYTES, Duration::ZERO), Feed::Continue);
        assert_eq!(b.feed(1, Duration::ZERO), Feed::TooLarge);
    }

    #[test]
    fn the_total_limit_trips_on_the_first_read_at_or_after_it() {
        let mut b = ManifestBody::new(TOTAL);
        assert_eq!(b.feed(1, Duration::from_millis(9_999)), Feed::Continue);
        assert_eq!(b.feed(1, TOTAL), Feed::DeadlineExceeded);
    }

    #[test]
    fn a_dribbling_server_is_stopped_by_the_total_limit() {
        let mut b = ManifestBody::new(TOTAL);
        let mut now = Duration::ZERO;
        let mut reads = 0;
        loop {
            now += Duration::from_secs(4);
            reads += 1;
            match b.feed(1, now) {
                Feed::Continue => {}
                Feed::DeadlineExceeded => break,
                other => panic!("{other:?}"),
            }
        }
        assert_eq!(reads, 3);
    }

    #[test]
    fn a_complete_body_wins_over_a_late_deadline() {
        let mut b = ManifestBody::new(TOTAL);
        assert_eq!(b.feed(0, Duration::from_secs(60)), Feed::Done);
    }

    #[test]
    fn the_check_after_connect_uses_the_same_limit() {
        let b = ManifestBody::new(TOTAL);
        assert_eq!(b.check(Duration::from_secs(9)), Ok(()));
        assert_eq!(b.check(TOTAL), Err(FetchError::DeadlineExceeded));
    }

    #[test]
    fn size_is_reported_before_time() {
        let mut b = ManifestBody::new(TOTAL);
        assert_eq!(
            b.feed(MAX_MANIFEST_BYTES + 1, Duration::from_secs(99)),
            Feed::TooLarge
        );
    }

    #[test]
    fn reasons() {
        assert_eq!(
            FetchError::Connect(OtaError::ServerUnreachable).reason(),
            FailReason::ManifestFetch
        );
        assert_eq!(
            FetchError::Connect(OtaError::DownloadFailed { status: 404 }).reason(),
            FailReason::ManifestFetch
        );
        assert_eq!(
            FetchError::Read(OtaError::DownloadTimeout).reason(),
            FailReason::ManifestFetch
        );
        assert_eq!(
            FetchError::DeadlineExceeded.reason(),
            FailReason::ManifestFetch
        );
        assert_eq!(FetchError::TooLarge.reason(), FailReason::ManifestInvalid);
        assert_eq!(
            FetchError::Parse(ManifestError::TargetMismatch).reason(),
            FailReason::TargetMismatch
        );
        assert_eq!(
            FetchError::Parse(ManifestError::InvalidUrl).reason(),
            FailReason::ManifestInvalid
        );
    }
}
