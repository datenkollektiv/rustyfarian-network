//! The firmware download order as an action machine (ADR 023 rung 3).
//!
//! Both tiers run `fetch_and_apply` through [`DownloadMachine`]: blocking threads on ESP-IDF, async on esp-hal, with transport and flash I/O outside `embedded-hal`.
//! The machine owns the order of the steps (deadline, connect and headers, the Content-Length admission, preparing the slot, the read and write stream with a deadline check after each, the SHA-256 compare, the final deadline and the activation permit); the tier loop only performs the returned [`DownloadAction`] and feeds the outcome back as a [`DownloadInput`].
//!
//! Pure and `no_std`: time enters as the elapsed time since the call started, and the machine never reads a clock.
//!
//! ```
//! use core::time::Duration;
//! use juggler::ota::download::{DownloadAction, DownloadInput, DownloadMachine};
//! use juggler::ota::Deadline;
//!
//! let mut m = DownloadMachine::new(Deadline::none(), Duration::from_secs(30), [0; 32]);
//! assert!(matches!(m.poll(DownloadInput::Begin, Duration::ZERO), DownloadAction::Connect { .. }));
//! let headers = DownloadInput::Headers { content_length: None, chunked: false, capacity: 4096 };
//! assert!(matches!(m.poll(headers, Duration::ZERO), DownloadAction::Done(Err(_))));
//! ```

use core::time::Duration;

use super::deadline::{ActivationPermit, Deadline};
use super::error::OtaError;

/// Experimental: API may change before 1.0.
///
/// The Content-Length admission rule both tiers apply before any flash is touched.
///
/// - A missing `Content-Length`, or any `Transfer-Encoding` (`chunked`), is a protocol-shape rejection: `DownloadFailed { status: 0 }`.
/// - A zero length is rejected the same way: there is no image, and `esp_ota_begin` would read a size of `0` as "erase the whole partition".
/// - A length above `capacity` (the inactive OTA partition), or above `u32::MAX`, is `InsufficientSpace`.
///
/// Returns the image length to stream.
///
/// ```
/// use juggler::ota::{check_content_length, OtaError};
///
/// assert_eq!(check_content_length(Some(1024), false, 4096), Ok(1024));
/// assert_eq!(check_content_length(None, false, 4096), Err(OtaError::DownloadFailed { status: 0 }));
/// assert_eq!(check_content_length(Some(4097), false, 4096), Err(OtaError::InsufficientSpace));
/// ```
pub fn check_content_length(
    content_length: Option<u64>,
    chunked: bool,
    capacity: u64,
) -> Result<u32, OtaError> {
    const SHAPE_REJECTED: OtaError = OtaError::DownloadFailed { status: 0 };
    if chunked {
        return Err(SHAPE_REJECTED);
    }
    match content_length {
        None | Some(0) => Err(SHAPE_REJECTED),
        Some(len) if len > capacity => Err(OtaError::InsufficientSpace),
        Some(len) => u32::try_from(len).map_err(|_| OtaError::InsufficientSpace),
    }
}

/// Experimental: API may change before 1.0.
///
/// The outcome of the previous [`DownloadAction`], fed to [`DownloadMachine::poll`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadInput {
    /// The first call.
    Begin,
    /// [`DownloadAction::Connect`] succeeded: the `200` response headers and the inactive partition's size.
    Headers {
        /// The declared `Content-Length`, `None` when absent.
        content_length: Option<u64>,
        /// The response carries a `Transfer-Encoding` header.
        chunked: bool,
        /// Size in bytes of the partition the image would be written to.
        capacity: u64,
    },
    /// [`DownloadAction::Connect`] failed: the connection, the response, or the partition lookup that sizes the image.
    ConnectFailed(OtaError),
    /// [`DownloadAction::Prepare`] succeeded: the write session is open.
    Prepared,
    /// [`DownloadAction::Prepare`] failed; no write session exists.
    PrepareFailed(OtaError),
    /// [`DownloadAction::Read`] returned `n` bytes; `0` is the end of the stream.
    Read(usize),
    /// [`DownloadAction::Read`] failed.
    ReadFailed(OtaError),
    /// [`DownloadAction::Write`] succeeded.
    Written,
    /// [`DownloadAction::Write`] failed, or the tail flush of [`DownloadAction::Finalize`] did.
    WriteFailed(OtaError),
    /// [`DownloadAction::Finalize`] computed this SHA-256 of the streamed bytes.
    Digest([u8; 32]),
    /// [`DownloadAction::Abort`] was performed.
    Aborted,
    /// [`DownloadAction::Activate`] returned.
    Activated(Result<(), OtaError>),
}

/// Experimental: API may change before 1.0.
///
/// What the tier loop must do next.
#[derive(Debug)]
pub enum DownloadAction {
    /// Connect, send the GET and read the response headers within `budget`, look up the inactive partition's size, and answer with [`DownloadInput::Headers`] or [`DownloadInput::ConnectFailed`].
    Connect {
        /// `min(per-operation timeout, remaining deadline)`.
        budget: Duration,
    },
    /// Open the write session for an image of exactly `len` bytes (erasing what it needs) and answer with [`DownloadInput::Prepared`] or [`DownloadInput::PrepareFailed`].
    Prepare {
        /// The admitted image length.
        len: u32,
    },
    /// Read at most `max` body bytes within `budget` and answer with [`DownloadInput::Read`] or [`DownloadInput::ReadFailed`].
    Read {
        /// Bytes still expected; never read past them.
        max: u32,
        /// `min(per-operation timeout, remaining deadline)`.
        budget: Duration,
    },
    /// Feed the `n` bytes just read to the SHA-256 and write them to the slot, then answer with [`DownloadInput::Written`] or [`DownloadInput::WriteFailed`].
    Write {
        /// Bytes of the last read.
        n: usize,
    },
    /// Flush any buffered tail to the slot, compute the SHA-256 and answer with [`DownloadInput::Digest`] (or [`DownloadInput::WriteFailed`] when the flush failed).
    Finalize,
    /// Abort the write session (the boot slot stays unchanged), then answer with [`DownloadInput::Aborted`].
    Abort(OtaError),
    /// Select the new boot slot with this permit and answer with [`DownloadInput::Activated`].
    Activate(ActivationPermit),
    /// The download is over; return this result.
    Done(Result<(), OtaError>),
    /// The input did not answer the pending action; nothing changed (ADR 023 decision 5).
    ///
    /// A tier loop never produces this on its own, so it treats it as an internal error.
    Ignored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Connecting,
    Preparing,
    Reading,
    Writing,
    Finalizing,
    Aborting(OtaError),
    Activating,
    Done,
}

/// Experimental: API may change before 1.0.
///
/// The firmware download order (see the [module docs](self)).
///
/// The rules:
/// - the deadline is checked on entry, after connect, after preparation, after every read and every write, and by the activation permit;
/// - nothing is prepared before the headers pass [`check_content_length`];
/// - a stream that ends before, or runs past, the admitted length is `DownloadFailed { status: 0 }`;
/// - every error after a successful preparation yields [`DownloadAction::Abort`] first, then [`DownloadAction::Done`] with that error;
/// - an operation's own error wins over an expired deadline at the same boundary;
/// - an input that does not answer the pending action yields [`DownloadAction::Ignored`] and changes nothing.
#[derive(Debug, Clone, Copy)]
pub struct DownloadMachine {
    deadline: Deadline,
    per_op: Duration,
    expected: [u8; 32],
    phase: Phase,
    len: u32,
    received: u32,
    steps: usize,
}

impl DownloadMachine {
    /// A machine for one download bounded by `deadline`, with `per_op` as the wait of one network operation, expecting the SHA-256 `expected`.
    pub const fn new(deadline: Deadline, per_op: Duration, expected: [u8; 32]) -> Self {
        Self {
            deadline,
            per_op,
            expected,
            phase: Phase::Idle,
            len: 0,
            received: 0,
            steps: 0,
        }
    }

    /// The most [`poll`](Self::poll) calls a download of `len` bytes can take when every successful read returns at least `min_read` bytes: `2 * ceil(len / min_read) + 8`.
    ///
    /// Transports may return short reads, so the tiers pass `min_read = 1`.
    pub const fn max_steps(len: u32, min_read: usize) -> usize {
        let min_read = if min_read == 0 { 1 } else { min_read };
        2 * (len as usize).div_ceil(min_read) + 8
    }

    /// [`poll`](Self::poll) under the step bound of ADR 023 decision 6.
    ///
    /// The bound is [`max_steps`](Self::max_steps) of the admitted length (zero before admission) with `min_read = 1`.
    ///
    /// # Errors
    ///
    /// `DownloadTimeout` once the bound is exceeded, `DownloadFailed { status: 0 }` for [`DownloadAction::Ignored`].
    /// After a successful [`DownloadAction::Prepare`] the tier must abort its write session before returning either.
    pub fn poll_bounded(
        &mut self,
        input: DownloadInput,
        elapsed: Duration,
    ) -> Result<DownloadAction, OtaError> {
        self.steps += 1;
        if self.steps > Self::max_steps(self.len, 1) {
            return Err(OtaError::DownloadTimeout);
        }
        match self.poll(input, elapsed) {
            DownloadAction::Ignored => Err(OtaError::DownloadFailed { status: 0 }),
            action => Ok(action),
        }
    }

    /// Advances the download; `elapsed` is the time since the download started.
    pub fn poll(&mut self, input: DownloadInput, elapsed: Duration) -> DownloadAction {
        use DownloadAction as A;
        use DownloadInput as I;
        match (self.phase, input) {
            (Phase::Idle, I::Begin) => match self.deadline.check(elapsed) {
                Err(e) => self.done(Err(e)),
                Ok(()) => {
                    self.phase = Phase::Connecting;
                    A::Connect {
                        budget: self.budget(elapsed),
                    }
                }
            },
            (
                Phase::Connecting,
                I::Headers {
                    content_length,
                    chunked,
                    capacity,
                },
            ) => {
                let admitted = self
                    .deadline
                    .check(elapsed)
                    .and_then(|()| check_content_length(content_length, chunked, capacity));
                match admitted {
                    Err(e) => self.done(Err(e)),
                    Ok(len) => {
                        self.len = len;
                        self.phase = Phase::Preparing;
                        A::Prepare { len }
                    }
                }
            }
            (Phase::Connecting, I::ConnectFailed(e)) | (Phase::Preparing, I::PrepareFailed(e)) => {
                self.done(Err(e))
            }
            (Phase::Preparing, I::Prepared) => match self.deadline.check(elapsed) {
                Err(e) => self.abort(e),
                Ok(()) => self.read(elapsed),
            },
            (Phase::Reading, I::Read(n)) => {
                let remaining = (self.len - self.received) as usize;
                if n == 0 || n > remaining {
                    return self.abort(OtaError::DownloadFailed { status: 0 });
                }
                self.received += n as u32;
                match self.deadline.check(elapsed) {
                    Err(e) => self.abort(e),
                    Ok(()) => {
                        self.phase = Phase::Writing;
                        A::Write { n }
                    }
                }
            }
            (Phase::Reading, I::ReadFailed(e))
            | (Phase::Writing | Phase::Finalizing, I::WriteFailed(e)) => self.abort(e),
            (Phase::Writing, I::Written) => match self.deadline.check(elapsed) {
                Err(e) => self.abort(e),
                Ok(()) if self.received == self.len => {
                    self.phase = Phase::Finalizing;
                    A::Finalize
                }
                Ok(()) => self.read(elapsed),
            },
            (Phase::Finalizing, I::Digest(digest)) => {
                if digest != self.expected {
                    return self.abort(OtaError::ChecksumMismatch);
                }
                match self.deadline.permit_activation(elapsed) {
                    Err(e) => self.abort(e),
                    Ok(permit) => {
                        self.phase = Phase::Activating;
                        A::Activate(permit)
                    }
                }
            }
            (Phase::Aborting(e), I::Aborted) => self.done(Err(e)),
            (Phase::Activating, I::Activated(result)) => self.done(result),
            _ => A::Ignored,
        }
    }

    fn budget(&self, elapsed: Duration) -> Duration {
        self.deadline.clip(self.per_op, elapsed)
    }

    fn read(&mut self, elapsed: Duration) -> DownloadAction {
        self.phase = Phase::Reading;
        DownloadAction::Read {
            max: self.len - self.received,
            budget: self.budget(elapsed),
        }
    }

    fn abort(&mut self, e: OtaError) -> DownloadAction {
        self.phase = Phase::Aborting(e);
        DownloadAction::Abort(e)
    }

    fn done(&mut self, result: Result<(), OtaError>) -> DownloadAction {
        self.phase = Phase::Done;
        DownloadAction::Done(result)
    }
}

/// Experimental: API may change before 1.0.
///
/// The size of one read request, capped to what the transport and the caller's buffer hold.
///
/// This does NOT make a read one transport wait.
/// ESP-IDF's `esp_http_client_read` loops: while the request is not filled and the body has
/// data left, it calls `esp_transport_read` again with the full client timeout, and TCP `recv`
/// returns whatever has arrived, so a read that returns fewer bytes than asked for simply
/// waits again.
/// A request of `n` bytes can therefore take up to `n` transport waits (a server that sends one
/// byte shortly before each timeout), and the cap `min(max, buffer, transport_rx)` only limits
/// that worst case to `transport_rx` waits.
/// Use [`bounded_read_request`] to keep a call inside the time left of a deadline.
/// Never returns 0 for a positive `max`, `buffer` and `transport_rx`.
///
/// ```
/// use juggler::ota::capped_read_len;
///
/// assert_eq!(capped_read_len(1_000_000, 4096, 1024), 1024);
/// assert_eq!(capped_read_len(100, 4096, 1024), 100);
/// ```
pub const fn capped_read_len(max: u32, buffer: usize, transport_rx: usize) -> usize {
    let mut len = max as usize;
    if buffer < len {
        len = buffer;
    }
    if transport_rx < len {
        len = transport_rx;
    }
    len
}

/// Experimental: API may change before 1.0.
///
/// The wait of the follow-up read inside one [`read_bounded`] call, once the deadline is near.
///
/// The follow-up only collects bytes that have already arrived behind the first one, so it must
/// not wait for new data: a short wait lets a fast peer fill the buffer while an idle one returns
/// at once.
/// `esp_http_client_read` itself returns the bytes read so far when a later wait elapses
/// (`if (ridx) return ridx;`) and `-ESP_ERR_HTTP_EAGAIN` only when none arrived (ESP-IDF v5.3.3
/// `components/esp_http_client/esp_http_client.c`, `esp_http_client_read`, the
/// `ERR_TCP_TRANSPORT_CONNECTION_TIMEOUT` branch), so the wait costs at most this much per
/// transport read.
/// 1 ms is the finest timeout the transport takes (`esp_http_client_set_timeout_ms`), so the
/// follow-up never waits longer than needed, and a follow-up of a whole 1024-byte chunk fits
/// once a little over one second of the budget is left.
pub const FOLLOW_UP_WAIT: Duration = Duration::from_millis(1);

/// Experimental: API may change before 1.0.
///
/// One read request: how many bytes to ask for and how long the transport waits may last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadRequest {
    /// The most bytes one read action returns.
    pub len: usize,
    /// The wait of the first transport read; the tier rounds it to the transport's granularity.
    pub wait: Duration,
    /// `Some` when the deadline is near: the first read asks for ONE byte and the rest of `len`
    /// is a follow-up read with this wait (see [`read_bounded`]); `None` is a single read of `len`.
    pub follow_up_wait: Option<Duration>,
}

/// Experimental: API may change before 1.0.
///
/// Sizes one blocking read so that, once the deadline is closer than one per-read timeout, the
/// whole call fits the time left.
///
/// `budget` is the [`DownloadAction::Read`] budget, `min(per_op, remaining)`, and `per_op` the
/// configured per-read timeout.
/// While `budget >= per_op` the request is [`capped_read_len`] with a wait of `budget`: a slow
/// server may stall for a full `per_op` before the read fails, so the waits are not shortened.
/// That leaves a RESIDUAL OVERRUN: a call that makes one wait per partial segment (up to
/// `len` waits against a server that trickles bytes) can run past the deadline.
/// Once `budget < per_op` the deadline is near: the first read is ONE byte with a wait of the
/// whole `budget` (one transport wait, and an idle gap shorter than the budget cannot fail the
/// read early, because `esp_http_client_read` returns `-ESP_ERR_HTTP_EAGAIN` when a wait ends
/// with no bytes and the machine treats a failed read as final), and the other `len - 1` bytes
/// are a follow-up with [`FOLLOW_UP_WAIT`], so a fast peer still delivers chunk-sized reads.
///
/// ```
/// use core::time::Duration;
/// use juggler::ota::{bounded_read_request, FOLLOW_UP_WAIT};
///
/// let per_op = Duration::from_secs(10);
/// let near = bounded_read_request(1_000_000, 4096, 1024, Duration::from_secs(2), per_op);
/// assert_eq!(near.len, 1024);
/// assert_eq!(near.wait, Duration::from_secs(2));
/// assert_eq!(near.follow_up_wait, Some(FOLLOW_UP_WAIT));
/// ```
pub fn bounded_read_request(
    max: u32,
    buffer: usize,
    transport_rx: usize,
    budget: Duration,
    per_op: Duration,
) -> ReadRequest {
    let len = capped_read_len(max, buffer, transport_rx);
    let near = budget < per_op && len > 1;
    ReadRequest {
        len,
        wait: budget,
        follow_up_wait: near.then_some(FOLLOW_UP_WAIT),
    }
}

/// Experimental: API may change before 1.0.
///
/// How one follow-up read ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowUp {
    /// `n > 0` more bytes arrived.
    Bytes(usize),
    /// Nothing more is available right now: keep the bytes read so far.
    ///
    /// This is a timeout (`-ESP_ERR_HTTP_EAGAIN`, no bytes yet), which is not a failure of a
    /// follow-up, and a zero read (end of stream, which the next read action reports itself).
    Idle,
    /// A real read failure: it ends the download.
    Failed(OtaError),
}

/// Experimental: API may change before 1.0.
///
/// Classifies the result of a follow-up read (see [`FollowUp`]).
///
/// ```
/// use juggler::ota::{classify_follow_up, FollowUp, OtaError};
///
/// assert_eq!(classify_follow_up(Ok(7)), FollowUp::Bytes(7));
/// assert_eq!(classify_follow_up(Err(OtaError::DownloadTimeout)), FollowUp::Idle);
/// assert_eq!(
///     classify_follow_up(Err(OtaError::ServerUnreachable)),
///     FollowUp::Failed(OtaError::ServerUnreachable)
/// );
/// ```
pub fn classify_follow_up(read: Result<usize, OtaError>) -> FollowUp {
    match read {
        Ok(0) | Err(OtaError::DownloadTimeout) => FollowUp::Idle,
        Ok(n) => FollowUp::Bytes(n),
        Err(e) => FollowUp::Failed(e),
    }
}

/// Experimental: API may change before 1.0.
///
/// Performs one [`DownloadAction::Read`] against `read`, a blocking transport read of `buf`
/// with a wait, and returns the bytes now at the start of `buf`.
///
/// `elapsed` is the time since this call began.
/// The first read follows [`bounded_read_request`].
/// Near the deadline a first read of one byte is followed by a read of the rest with
/// [`FOLLOW_UP_WAIT`], sized so that even a peer delivering a byte per wait ends inside what is
/// left of `budget` (`esp_http_client_read` makes at most one more wait than it returns bytes);
/// an idle follow-up returns what was read ([`FollowUp::Idle`]) instead of an error, so the
/// flash write stays chunk-sized.
///
/// ```
/// use core::time::Duration;
/// use juggler::ota::read_bounded;
///
/// let mut buf = [0u8; 8];
/// let n = read_bounded(&mut buf, 8, 8, Duration::from_secs(1), Duration::from_secs(10),
///     || Duration::ZERO, |b, _| { b.fill(1); Ok(b.len()) }).unwrap();
/// assert_eq!(n, 8);
/// ```
///
/// # Errors
///
/// The error of the first read, or of a follow-up that is not [`FollowUp::Idle`].
pub fn read_bounded<R, C>(
    buf: &mut [u8],
    max: u32,
    transport_rx: usize,
    budget: Duration,
    per_op: Duration,
    mut elapsed: C,
    mut read: R,
) -> Result<usize, OtaError>
where
    R: FnMut(&mut [u8], Duration) -> Result<usize, OtaError>,
    C: FnMut() -> Duration,
{
    let request = bounded_read_request(max, buf.len(), transport_rx, budget, per_op);
    let Some(follow_up_wait) = request.follow_up_wait else {
        return read(&mut buf[..request.len], request.wait);
    };
    let first = read(&mut buf[..1], request.wait)?;
    if first == 0 {
        return Ok(0);
    }
    let remaining = budget.saturating_sub(elapsed());
    let wait = follow_up_wait.min(remaining);
    let waits = if wait.is_zero() {
        0
    } else {
        usize::try_from(remaining.as_nanos() / wait.as_nanos()).unwrap_or(usize::MAX)
    };
    let more = request
        .len
        .saturating_sub(first)
        .min(waits.saturating_sub(1));
    if more == 0 {
        return Ok(first);
    }
    match classify_follow_up(read(&mut buf[first..first + more], wait)) {
        FollowUp::Bytes(n) => Ok(first + n),
        FollowUp::Idle => Ok(first),
        FollowUp::Failed(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn one_read_never_asks_for_more_than_one_transport_buffer() {
        use super::capped_read_len;
        assert_eq!(capped_read_len(u32::MAX, 4096, 1024), 1024);
        assert_eq!(capped_read_len(4096, 4096, 512), 512);
        assert_eq!(capped_read_len(700, 4096, 1024), 700);
        assert_eq!(capped_read_len(10, 4, 1024), 4);
        assert_eq!(capped_read_len(0, 4096, 1024), 0);
    }

    use core::cell::Cell;

    /// A model of `esp_http_client_read` over a peer with `avail` bytes ready and `trickle`
    /// more arriving one per millisecond; time passes only inside the model.
    struct Peer {
        now: Cell<Duration>,
        avail: Cell<usize>,
        trickle: bool,
        fail_after_first: bool,
        calls: Cell<usize>,
        waits: Cell<Duration>,
    }

    impl Peer {
        fn new(avail: usize, trickle: bool) -> Self {
            Peer {
                now: Cell::new(Duration::ZERO),
                avail: Cell::new(avail),
                trickle,
                fail_after_first: false,
                calls: Cell::new(0),
                waits: Cell::new(Duration::ZERO),
            }
        }

        fn read(&self, buf: &mut [u8], wait: Duration) -> Result<usize, OtaError> {
            self.calls.set(self.calls.get() + 1);
            if self.fail_after_first && self.calls.get() > 1 {
                return Err(OtaError::ServerUnreachable);
            }
            let mut got = 0;
            while got < buf.len() {
                if self.avail.get() > 0 {
                    self.avail.set(self.avail.get() - 1);
                    got += 1;
                } else if self.trickle && wait >= Duration::from_millis(1) {
                    self.now.set(self.now.get() + Duration::from_millis(1));
                    got += 1;
                } else {
                    self.now.set(self.now.get() + wait);
                    self.waits.set(self.waits.get() + wait);
                    return if got > 0 {
                        Ok(got)
                    } else {
                        Err(OtaError::DownloadTimeout)
                    };
                }
            }
            Ok(got)
        }

        fn run(&self, budget: Duration) -> Result<usize, OtaError> {
            let mut buf = [0u8; 4096];
            read_bounded(
                &mut buf,
                u32::MAX,
                1024,
                budget,
                Duration::from_secs(10),
                || self.now.get(),
                |b, w| self.read(b, w),
            )
        }
    }

    #[test]
    fn a_fast_peer_near_the_deadline_still_fills_a_whole_chunk() {
        let peer = Peer::new(4096, false);
        assert_eq!(peer.run(Duration::from_secs(2)), Ok(1024));
        assert_eq!(peer.calls.get(), 2);
    }

    #[test]
    fn an_idle_follow_up_returns_the_bytes_read_so_far_not_an_error() {
        let peer = Peer::new(10, false);
        assert_eq!(peer.run(Duration::from_secs(2)), Ok(10));
        assert!(peer.now.get() <= Duration::from_secs(2));
    }

    #[test]
    fn a_silent_peer_fails_after_exactly_the_budget() {
        let peer = Peer::new(0, false);
        let budget = Duration::from_millis(1_500);
        assert_eq!(peer.run(budget), Err(OtaError::DownloadTimeout));
        assert_eq!(peer.now.get(), budget);
    }

    #[test]
    fn a_trickling_peer_cannot_push_one_call_past_the_budget() {
        for budget_ms in [3u64, 4, 5, 20, 50, 400, 2_000] {
            let peer = Peer::new(0, true);
            let budget = Duration::from_millis(budget_ms);
            assert!(peer.run(budget).is_ok(), "budget {budget_ms}");
            assert!(peer.now.get() <= budget, "budget {budget_ms}");
        }
    }

    #[test]
    fn a_read_failure_after_the_first_byte_is_not_hidden() {
        let mut peer = Peer::new(100, false);
        peer.fail_after_first = true;
        assert_eq!(
            peer.run(Duration::from_secs(2)),
            Err(OtaError::ServerUnreachable)
        );
    }

    #[test]
    fn a_full_budget_is_one_plain_read() {
        let peer = Peer::new(4096, false);
        assert_eq!(peer.run(Duration::from_secs(10)), Ok(1024));
        assert_eq!(peer.calls.get(), 1);
    }

    #[test]
    fn the_follow_up_classification() {
        assert_eq!(classify_follow_up(Ok(3)), FollowUp::Bytes(3));
        assert_eq!(classify_follow_up(Ok(0)), FollowUp::Idle);
        assert_eq!(
            classify_follow_up(Err(OtaError::DownloadTimeout)),
            FollowUp::Idle
        );
        assert_eq!(
            classify_follow_up(Err(OtaError::ServerUnreachable)),
            FollowUp::Failed(OtaError::ServerUnreachable)
        );
    }

    #[test]
    fn a_read_near_the_deadline_is_one_byte_then_a_short_follow_up() {
        let per_op = Duration::from_secs(10);
        let budget = Duration::from_millis(9_999);
        assert_eq!(
            bounded_read_request(u32::MAX, 4096, 1024, budget, per_op),
            ReadRequest {
                len: 1024,
                wait: budget,
                follow_up_wait: Some(FOLLOW_UP_WAIT)
            }
        );
        let one = bounded_read_request(1, 4096, 1024, budget, per_op);
        assert_eq!((one.len, one.follow_up_wait), (1, None));
    }

    #[test]
    fn a_read_with_a_full_per_op_budget_keeps_the_whole_wait() {
        let per_op = Duration::from_secs(10);
        assert_eq!(
            bounded_read_request(u32::MAX, 4096, 1024, per_op, per_op),
            ReadRequest {
                len: 1024,
                wait: per_op,
                follow_up_wait: None
            }
        );
        assert_eq!(bounded_read_request(10, 4096, 1024, per_op, per_op).len, 10);
    }

    use super::*;
    use DownloadInput as I;

    const SHA: [u8; 32] = [7; 32];
    const SHAPE: OtaError = OtaError::DownloadFailed { status: 0 };

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// A comparable label for an action (`ActivationPermit` is deliberately not comparable).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum K {
        Connect,
        Prepare(u32),
        Read(u32),
        Write(usize),
        Finalize,
        Abort(OtaError),
        Activate,
        Done(Result<(), OtaError>),
        Ignored,
    }

    fn kind(a: &DownloadAction) -> K {
        match a {
            DownloadAction::Connect { .. } => K::Connect,
            DownloadAction::Prepare { len } => K::Prepare(*len),
            DownloadAction::Read { max, .. } => K::Read(*max),
            DownloadAction::Write { n } => K::Write(*n),
            DownloadAction::Finalize => K::Finalize,
            DownloadAction::Abort(e) => K::Abort(*e),
            DownloadAction::Activate(_) => K::Activate,
            DownloadAction::Done(r) => K::Done(*r),
            DownloadAction::Ignored => K::Ignored,
        }
    }

    fn machine(deadline: Deadline) -> DownloadMachine {
        DownloadMachine::new(deadline, ms(1_000), SHA)
    }

    fn headers(len: u64) -> DownloadInput {
        I::Headers {
            content_length: Some(len),
            chunked: false,
            capacity: 4096,
        }
    }

    /// A machine that has opened the write session for `len` bytes and asked for the first read.
    fn prepared(deadline: Deadline, len: u64) -> DownloadMachine {
        let mut m = machine(deadline);
        assert_eq!(kind(&m.poll(I::Begin, ms(0))), K::Connect);
        assert_eq!(kind(&m.poll(headers(len), ms(0))), K::Prepare(len as u32));
        assert_eq!(kind(&m.poll(I::Prepared, ms(0))), K::Read(len as u32));
        m
    }

    #[test]
    fn content_length_cases() {
        assert_eq!(check_content_length(None, false, 4096), Err(SHAPE));
        assert_eq!(check_content_length(Some(1024), true, 4096), Err(SHAPE));
        assert_eq!(check_content_length(None, true, 4096), Err(SHAPE));
        assert_eq!(check_content_length(Some(0), false, 4096), Err(SHAPE));
        assert_eq!(check_content_length(Some(4096), false, 4096), Ok(4096));
        assert_eq!(
            check_content_length(Some(4097), false, 4096),
            Err(OtaError::InsufficientSpace)
        );
        assert_eq!(check_content_length(Some(1024), false, 4096), Ok(1024));
        assert_eq!(
            check_content_length(Some(u64::MAX), false, u64::MAX),
            Err(OtaError::InsufficientSpace)
        );
    }

    #[test]
    fn happy_path_order() {
        let mut m = prepared(Deadline::none(), 10);
        let mut seen = alloc::vec![];
        let mut input = I::Read(6);
        loop {
            let a = m.poll(input, ms(1));
            seen.push(kind(&a));
            input = match a {
                DownloadAction::Write { .. } => I::Written,
                DownloadAction::Read { max, .. } => I::Read(max as usize),
                DownloadAction::Finalize => I::Digest(SHA),
                DownloadAction::Activate(_) => I::Activated(Ok(())),
                DownloadAction::Done(_) => break,
                other => panic!("{other:?}"),
            };
        }
        assert_eq!(
            seen,
            [
                K::Write(6),
                K::Read(4),
                K::Write(4),
                K::Finalize,
                K::Activate,
                K::Done(Ok(()))
            ]
        );
    }

    #[test]
    fn no_prepare_when_headers_fail() {
        let cases = [
            (None, false, 4096, SHAPE),
            (Some(100), true, 4096, SHAPE),
            (Some(0), false, 4096, SHAPE),
            (Some(4097), false, 4096, OtaError::InsufficientSpace),
        ];
        for (content_length, chunked, capacity, err) in cases {
            let mut m = machine(Deadline::none());
            m.poll(I::Begin, ms(0));
            let a = m.poll(
                I::Headers {
                    content_length,
                    chunked,
                    capacity,
                },
                ms(0),
            );
            assert_eq!(kind(&a), K::Done(Err(err)), "{content_length:?} {chunked}");
        }
        let mut m = machine(Deadline::none());
        m.poll(I::Begin, ms(0));
        let a = m.poll(I::ConnectFailed(OtaError::ServerUnreachable), ms(0));
        assert_eq!(kind(&a), K::Done(Err(OtaError::ServerUnreachable)));
    }

    #[test]
    fn expired_deadline_stops_before_any_access() {
        let mut m = machine(Deadline::after(Duration::ZERO));
        let a = m.poll(I::Begin, ms(0));
        assert_eq!(kind(&a), K::Done(Err(OtaError::DownloadTimeout)));

        let mut m = machine(Deadline::after(ms(10)));
        m.poll(I::Begin, ms(0));
        let a = m.poll(headers(100), ms(10));
        assert_eq!(kind(&a), K::Done(Err(OtaError::DownloadTimeout)));
    }

    #[test]
    fn a_failed_prepare_has_nothing_to_abort() {
        let mut m = machine(Deadline::none());
        m.poll(I::Begin, ms(0));
        m.poll(headers(100), ms(0));
        let a = m.poll(I::PrepareFailed(OtaError::FlashWriteFailed), ms(0));
        assert_eq!(kind(&a), K::Done(Err(OtaError::FlashWriteFailed)));
    }

    #[test]
    fn abort_on_every_error_after_prepare() {
        let digest_mismatch: &[I] = &[I::Read(10), I::Written, I::Digest([0; 32])];
        let read_failed: &[I] = &[I::ReadFailed(OtaError::ServerUnreachable)];
        let write_failed: &[I] = &[I::Read(4), I::WriteFailed(OtaError::FlashWriteFailed)];
        let tail_failed: &[I] = &[
            I::Read(10),
            I::Written,
            I::WriteFailed(OtaError::FlashWriteFailed),
        ];
        let short_body: &[I] = &[I::Read(4), I::Written, I::Read(0)];
        let long_body: &[I] = &[I::Read(11)];
        let cases: [(&[I], OtaError); 6] = [
            (read_failed, OtaError::ServerUnreachable),
            (write_failed, OtaError::FlashWriteFailed),
            (tail_failed, OtaError::FlashWriteFailed),
            (digest_mismatch, OtaError::ChecksumMismatch),
            (short_body, SHAPE),
            (long_body, SHAPE),
        ];
        for (script, err) in cases {
            let mut m = prepared(Deadline::none(), 10);
            let mut last = K::Ignored;
            for input in script {
                last = kind(&m.poll(*input, ms(1)));
            }
            assert_eq!(last, K::Abort(err), "{script:?}");
            assert_eq!(kind(&m.poll(I::Aborted, ms(1))), K::Done(Err(err)));
        }
        let mut m = prepared(Deadline::after(ms(5)), 10);
        assert_eq!(
            kind(&m.poll(I::Prepared, ms(1))),
            K::Ignored,
            "already prepared"
        );
        let mut m = machine(Deadline::after(ms(5)));
        m.poll(I::Begin, ms(0));
        m.poll(headers(10), ms(0));
        assert_eq!(
            kind(&m.poll(I::Prepared, ms(5))),
            K::Abort(OtaError::DownloadTimeout),
            "expired during the erase"
        );
    }

    #[test]
    fn no_activate_after_deadline() {
        let mut m = prepared(Deadline::after(ms(50)), 10);
        m.poll(I::Read(10), ms(10));
        assert_eq!(kind(&m.poll(I::Written, ms(20))), K::Finalize);
        let a = m.poll(I::Digest(SHA), ms(50));
        assert_eq!(kind(&a), K::Abort(OtaError::DownloadTimeout));
        assert_eq!(
            kind(&m.poll(I::Aborted, ms(51))),
            K::Done(Err(OtaError::DownloadTimeout))
        );
    }

    #[test]
    fn deadline_checked_after_each_read_and_write() {
        let mut m = prepared(Deadline::after(ms(50)), 10);
        assert_eq!(
            kind(&m.poll(I::Read(4), ms(50))),
            K::Abort(OtaError::DownloadTimeout)
        );

        let mut m = prepared(Deadline::after(ms(50)), 10);
        assert_eq!(kind(&m.poll(I::Read(4), ms(49))), K::Write(4));
        assert_eq!(
            kind(&m.poll(I::Written, ms(50))),
            K::Abort(OtaError::DownloadTimeout)
        );

        let mut m = prepared(Deadline::after(ms(50)), 10);
        assert_eq!(kind(&m.poll(I::Read(10), ms(10))), K::Write(10));
        assert_eq!(
            kind(&m.poll(I::Written, ms(50))),
            K::Abort(OtaError::DownloadTimeout),
            "also after the last write"
        );
    }

    #[test]
    fn an_operation_error_wins_over_an_expired_deadline() {
        let mut m = prepared(Deadline::after(ms(50)), 10);
        let a = m.poll(I::ReadFailed(OtaError::ServerUnreachable), ms(500));
        assert_eq!(kind(&a), K::Abort(OtaError::ServerUnreachable));
        let mut m = prepared(Deadline::after(ms(50)), 10);
        m.poll(I::Read(10), ms(10));
        m.poll(I::Written, ms(10));
        let a = m.poll(I::Digest([0; 32]), ms(500));
        assert_eq!(kind(&a), K::Abort(OtaError::ChecksumMismatch));
    }

    #[test]
    fn the_budget_is_clipped_by_the_deadline() {
        let mut m = machine(Deadline::after(ms(1_500)));
        match m.poll(I::Begin, ms(800)) {
            DownloadAction::Connect { budget } => assert_eq!(budget, ms(700)),
            other => panic!("{other:?}"),
        }
        m.poll(headers(10), ms(900));
        match m.poll(I::Prepared, ms(1_000)) {
            DownloadAction::Read { max, budget } => assert_eq!((max, budget), (10, ms(500))),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn out_of_phase_input_is_ignored_and_state_unchanged() {
        let all = [
            I::Begin,
            headers(10),
            I::ConnectFailed(OtaError::ServerUnreachable),
            I::Prepared,
            I::PrepareFailed(OtaError::FlashWriteFailed),
            I::Read(1),
            I::ReadFailed(OtaError::ServerUnreachable),
            I::Written,
            I::WriteFailed(OtaError::FlashWriteFailed),
            I::Digest(SHA),
            I::Aborted,
            I::Activated(Ok(())),
        ];
        let mut m = prepared(Deadline::none(), 10);
        for input in [
            I::Begin,
            headers(10),
            I::Prepared,
            I::Written,
            I::Digest(SHA),
            I::Aborted,
            I::Activated(Ok(())),
        ] {
            assert_eq!(kind(&m.poll(input, ms(1))), K::Ignored, "{input:?}");
        }
        assert_eq!(
            kind(&m.poll(I::Read(10), ms(1))),
            K::Write(10),
            "the next correct input still advances"
        );

        let mut done = machine(Deadline::none());
        done.poll(I::Begin, ms(0));
        done.poll(I::ConnectFailed(OtaError::ServerUnreachable), ms(0));
        for input in all {
            assert_eq!(kind(&done.poll(input, ms(1))), K::Ignored, "{input:?}");
        }
    }

    #[test]
    fn poll_bounded_turns_ignored_into_an_error() {
        let mut m = machine(Deadline::none());
        assert_eq!(m.poll_bounded(I::Written, ms(0)).unwrap_err(), SHAPE);
        assert!(m.poll_bounded(I::Begin, ms(0)).is_ok());
    }

    #[test]
    fn poll_bounded_stops_a_loop_that_never_ends() {
        let mut m = machine(Deadline::none());
        let mut result = Ok(());
        for _ in 0..100 {
            if let Err(e) = m.poll_bounded(I::Begin, ms(0)) {
                result = Err(e);
                if e == OtaError::DownloadTimeout {
                    break;
                }
            }
        }
        assert_eq!(result, Err(OtaError::DownloadTimeout));
    }

    #[test]
    fn max_steps_formula() {
        assert_eq!(DownloadMachine::max_steps(0, 1), 8);
        assert_eq!(DownloadMachine::max_steps(10, 4), 2 * 3 + 8);
        assert_eq!(DownloadMachine::max_steps(10, 0), 2 * 10 + 8);
    }

    /// One failure to inject into a scripted tier.
    #[derive(Debug, Clone, Copy)]
    enum Fault {
        None,
        Connect,
        Prepare,
        Read(usize),
        Write(usize),
        ShortBody(usize),
        Digest,
        Tail,
        Activate,
        Deadline(u64),
    }

    /// Runs a simulated tier through the bounded loop and returns (result, steps, aborted).
    fn run(len: u32, chunk: usize, fault: Fault) -> (Result<(), OtaError>, usize, bool) {
        let deadline = match fault {
            Fault::Deadline(at) => Deadline::after(ms(at)),
            _ => Deadline::none(),
        };
        let mut m = machine(deadline);
        let mut input = I::Begin;
        let mut steps = 0;
        let mut reads = 0;
        let mut writes = 0;
        let mut prepared = false;
        let mut aborted = false;
        let mut t = 0;
        loop {
            t += 1;
            steps += 1;
            let action = match m.poll_bounded(input, ms(t)) {
                Ok(action) => action,
                Err(e) => panic!("bounded loop failed with {e:?} after {steps} steps"),
            };
            input = match action {
                DownloadAction::Connect { .. } => match fault {
                    Fault::Connect => I::ConnectFailed(OtaError::ServerUnreachable),
                    _ => I::Headers {
                        content_length: Some(u64::from(len)),
                        chunked: false,
                        capacity: u64::from(len),
                    },
                },
                DownloadAction::Prepare { .. } => match fault {
                    Fault::Prepare => I::PrepareFailed(OtaError::FlashWriteFailed),
                    _ => {
                        prepared = true;
                        I::Prepared
                    }
                },
                DownloadAction::Read { max, .. } => {
                    reads += 1;
                    match fault {
                        Fault::Read(at) if reads == at => {
                            I::ReadFailed(OtaError::ServerUnreachable)
                        }
                        Fault::ShortBody(at) if reads == at => I::Read(0),
                        _ => I::Read((max as usize).min(chunk)),
                    }
                }
                DownloadAction::Write { .. } => {
                    writes += 1;
                    match fault {
                        Fault::Write(at) if writes == at => {
                            I::WriteFailed(OtaError::FlashWriteFailed)
                        }
                        _ => I::Written,
                    }
                }
                DownloadAction::Finalize => match fault {
                    Fault::Tail => I::WriteFailed(OtaError::FlashWriteFailed),
                    Fault::Digest => I::Digest([0; 32]),
                    _ => I::Digest(SHA),
                },
                DownloadAction::Abort(_) => {
                    assert!(prepared, "abort without a write session");
                    aborted = true;
                    I::Aborted
                }
                DownloadAction::Activate(_) => match fault {
                    Fault::Activate => I::Activated(Err(OtaError::FlashWriteFailed)),
                    _ => I::Activated(Ok(())),
                },
                DownloadAction::Done(result) => {
                    if prepared && result.is_err() && !matches!(fault, Fault::Activate) {
                        assert!(aborted, "{fault:?}: an error after prepare must abort");
                    }
                    return (result, steps, aborted);
                }
                DownloadAction::Ignored => panic!("ignored"),
            };
        }
    }

    #[test]
    fn every_path_reaches_done_within_max_steps() {
        let len = 1_000;
        let faults = [
            Fault::None,
            Fault::Connect,
            Fault::Prepare,
            Fault::Read(1),
            Fault::Read(5),
            Fault::Write(3),
            Fault::ShortBody(4),
            Fault::Digest,
            Fault::Tail,
            Fault::Activate,
            Fault::Deadline(1),
            Fault::Deadline(4),
            Fault::Deadline(9),
            Fault::Deadline(30),
        ];
        for chunk in [1, 7, 64, 1_000, 4_096] {
            for fault in faults {
                let (result, steps, aborted) = run(len, chunk, fault);
                assert!(
                    steps <= DownloadMachine::max_steps(len, chunk),
                    "{fault:?} chunk {chunk}: {steps} steps"
                );
                let always_fails = matches!(
                    fault,
                    Fault::Connect
                        | Fault::Prepare
                        | Fault::Read(1)
                        | Fault::Digest
                        | Fault::Tail
                        | Fault::Activate
                        | Fault::Deadline(1)
                );
                if matches!(fault, Fault::None) {
                    assert_eq!(result, Ok(()), "chunk {chunk}");
                }
                if always_fails {
                    assert!(result.is_err(), "{fault:?} chunk {chunk}");
                }
                assert!(!(aborted && result.is_ok()));
            }
        }
    }
}
