//! Sans-IO ESP-NOW peer-link supervisor.
//!
//! [`PeerLink`] decides when a node that talks to one coordinator must (re)scan for it.
//! It owns no radio, clock or thread: the application feeds [`PeerLinkEvent`] values together
//! with the current monotonic time in milliseconds and performs the returned
//! [`PeerLinkAction`].
//! Running the scan itself is the job of [`ScanMachine`](super::ScanMachine), driven by the
//! application on either tier.
//!
//! # Driving the machine
//!
//! 1. Call [`PeerLink::poll`] with [`PeerLinkEvent::Boot`]; it returns [`PeerLinkAction::Scan`].
//! 2. Start a scan and remember the [`PeerLinkEpoch`] in the action.
//!    When it ends, feed [`PeerLinkEvent::ScanFound`] or [`PeerLinkEvent::ScanFailed`] with that
//!    epoch.
//! 3. Read [`PeerLink::epoch`] whenever a send is submitted and feed the outcome back as
//!    [`PeerLinkEvent::Sent`] with the epoch that was read, so a result that belongs to an
//!    earlier link generation is recognised as stale.
//! 4. Feed [`PeerLinkEvent::PeerHeard`] for every received frame **from the configured
//!    coordinator MAC**.
//!    Frames from any other sender are unrelated to the link and are the application's to
//!    filter out; feeding them would hide a lost coordinator.
//! 5. Poll with `None` whenever time has passed (at the latest at the deadline of a
//!    [`PeerLinkAction::WaitUntil`]).
//!
//! `now_ms` is any monotonic millisecond count; only differences matter.
//!
//! # Rules
//!
//! 1. Before [`PeerLinkEvent::Boot`] nothing happens: events are answered with
//!    [`PeerLinkAction::Ignored`] and polls with [`PeerLinkAction::None`].
//!    `Boot` issues the first scan; the state is [`PeerLinkState::Searching`].
//!    A second `Boot` is [`PeerLinkAction::Ignored`].
//! 2. At most one scan is outstanding.
//!    No new scan is issued until [`PeerLinkEvent::ScanFound`] / [`PeerLinkEvent::ScanFailed`]
//!    arrives with the current id.
//!    A result with another id, or one that arrives while no scan runs, is stale: it changes
//!    nothing and is answered with [`PeerLinkAction::Ignored`].
//! 3. `ScanFound` links to the channel, clears the failure count and resets the retry backoff.
//! 4. `ScanFailed` leaves `Searching` as `Searching` and `Lost` as `Lost` and schedules a retry:
//!    [`PeerLinkAction::WaitUntil`] with `now + backoff`, after which the backoff doubles up to
//!    its cap.
//!    The first `poll` at or after the deadline returns [`PeerLinkAction::Scan`], so a boot scan
//!    is retried without any application traffic.
//!    Before the deadline `poll` keeps returning the same `WaitUntil`.
//! 5. Exception: if the coordinator was heard (`PeerHeard`) during a scan that started from
//!    `Linked` / `Lost { last_channel }` and the scan then fails, the link returns to
//!    `Linked { channel: last_channel }` without scheduling a retry.
//! 6. A `Sent` whose epoch is not the current one is stale: it changes nothing and is answered
//!    with [`PeerLinkAction::Ignored`].
//!    Sends reported while a scan runs, or before `Boot`, are answered the same way.
//! 7. `Acked` proves the link like `PeerHeard` (rule 8); `NoAck` while `Linked` increments the
//!    failure count.
//!    Reaching [`PeerLinkConfig::rescan_threshold`] consecutive `NoAck` outcomes moves the link
//!    to `Lost { last_channel }`, counts a rescan and issues a scan (the epoch increments).
//!    `Local` only increments [`PeerLink::local_failures`]: the frame never left, so it says
//!    nothing about the coordinator.
//! 8. `PeerHeard` while `Linked` clears the failure count.
//!    While `Lost` with no scan running it re-links to `Linked { last_channel }` and cancels the
//!    pending retry.
//!    While `Searching` it changes nothing, since no channel is known.
//!    During a scan see rule 5.
//! 9. Every `poll` returns the action currently required: `Scan` exactly once per issued scan
//!    (polls while it runs return `None`), `WaitUntil` while a retry is pending, else `None`.
//!    The exception is an event that was rejected (rules 1, 2 and 6): it is answered with
//!    [`PeerLinkAction::Ignored`] (ADR 023 decision 5), never a request for I/O, and the next
//!    `poll` with `None` returns the action currently required again.
//! 10. Arithmetic saturates or wraps; the machine never panics.
//!
//! [`PeerLink::rescans`] counts every scan issued while the link is `Lost`, retries included.
//!
//! The machine never loops on its own: every call to [`PeerLink::poll`] returns after a bounded
//! number of steps and never issues more than one scan.
//!
//! `PeerLink` is a perpetual supervisor, an explicit exception to ADR 023 decision 6: it has no
//! terminal action, because the link to the coordinator is watched for as long as the node
//! runs.
//! Callers bound it instead to one `poll` per event or per tick.
//!
//! # Example
//!
//! ```
//! use juggler::espnow::{
//!     PeerLink, PeerLinkAction, PeerLinkConfig, PeerLinkEvent, PeerLinkState, SendOutcome,
//! };
//!
//! let mut link = PeerLink::new(PeerLinkConfig::default().with_rescan_threshold(2));
//!
//! let PeerLinkAction::Scan { id } = link.poll(0, Some(PeerLinkEvent::Boot)) else {
//!     unreachable!()
//! };
//! link.poll(900, Some(PeerLinkEvent::ScanFound { id, channel: 6 }));
//! assert_eq!(link.state(), PeerLinkState::Linked { channel: 6 });
//!
//! let epoch = link.epoch();
//! let no_ack = PeerLinkEvent::Sent { outcome: SendOutcome::NoAck, epoch };
//! assert_eq!(link.poll(1_000, Some(no_ack)), PeerLinkAction::None);
//! let action = link.poll(1_100, Some(no_ack));
//! assert!(matches!(action, PeerLinkAction::Scan { .. }));
//! assert_eq!(link.state(), PeerLinkState::Lost { last_channel: 6 });
//! ```

use super::SendOutcome;

/// Default number of consecutive unacknowledged sends that triggers a rescan.
pub const DEFAULT_RESCAN_THRESHOLD: u16 = 20;

/// Default delay before the first retry of a failed scan.
pub const DEFAULT_BACKOFF_INITIAL_MS: u64 = 2_000;

/// Default cap of the retry delay.
pub const DEFAULT_BACKOFF_MAX_MS: u64 = 60_000;

/// Tuning of a [`PeerLink`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerLinkConfig {
    /// Consecutive `NoAck` outcomes that make the link `Lost`
    /// (default: [`DEFAULT_RESCAN_THRESHOLD`] = 20).
    pub rescan_threshold: u16,
    /// Delay before the first retry of a failed scan, in milliseconds
    /// (default: [`DEFAULT_BACKOFF_INITIAL_MS`] = 2 s).
    pub backoff_initial_ms: u64,
    /// Cap of the retry delay, which doubles per failed scan, in milliseconds
    /// (default: [`DEFAULT_BACKOFF_MAX_MS`] = 60 s).
    pub backoff_max_ms: u64,
}

impl Default for PeerLinkConfig {
    fn default() -> Self {
        Self {
            rescan_threshold: DEFAULT_RESCAN_THRESHOLD,
            backoff_initial_ms: DEFAULT_BACKOFF_INITIAL_MS,
            backoff_max_ms: DEFAULT_BACKOFF_MAX_MS,
        }
    }
}

impl PeerLinkConfig {
    /// Set the number of consecutive `NoAck` outcomes that triggers a rescan.
    #[must_use]
    pub fn with_rescan_threshold(mut self, threshold: u16) -> Self {
        self.rescan_threshold = threshold;
        self
    }

    /// Set the delay before the first retry of a failed scan.
    #[must_use]
    pub fn with_backoff_initial(mut self, ms: u64) -> Self {
        self.backoff_initial_ms = ms;
        self
    }

    /// Set the cap of the retry delay.
    #[must_use]
    pub fn with_backoff_max(mut self, ms: u64) -> Self {
        self.backoff_max_ms = ms;
        self
    }
}

/// Opaque identifier of one link generation.
///
/// It increments whenever a scan is issued.
/// The id of a [`PeerLinkAction::Scan`] goes back in the scan result; the value of
/// [`PeerLink::epoch`] read at submit time goes back in [`PeerLinkEvent::Sent`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerLinkEpoch(u32);

/// Input fed to [`PeerLink::poll`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerLinkEvent {
    /// The application is up; issue the first scan.  Ignored after the first time.
    Boot,
    /// Result of a send, tagged with the [`PeerLink::epoch`] read when it was submitted.
    Sent {
        /// What happened to the frame.
        outcome: SendOutcome,
        /// The epoch read at submit time.
        epoch: PeerLinkEpoch,
    },
    /// A frame arrived from the configured coordinator MAC (and only from it).
    PeerHeard,
    /// The scan with this id found the coordinator on `channel`.
    ScanFound {
        /// The id from [`PeerLinkAction::Scan`].
        id: PeerLinkEpoch,
        /// The coordinator's channel.
        channel: u8,
    },
    /// The scan with this id ended without finding the coordinator.
    ScanFailed {
        /// The id from [`PeerLinkAction::Scan`].
        id: PeerLinkEpoch,
    },
}

/// Action the application must perform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerLinkAction {
    /// Nothing to do.
    None,
    /// The event did not fit the current state (stale, duplicate or too early) and was
    /// dropped; the state is unchanged and nothing is to be done.
    Ignored,
    /// Start a scan and report its result with `id`.
    Scan {
        /// Identifier to return in `ScanFound` / `ScanFailed`.
        id: PeerLinkEpoch,
    },
    /// Nothing to do until the absolute time `.0` (milliseconds); poll again then.
    WaitUntil(u64),
}

/// Observable state of the link.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerLinkState {
    /// Never linked; the coordinator's channel is unknown.
    Searching,
    /// The coordinator is reachable on `channel`.
    Linked {
        /// The coordinator's channel.
        channel: u8,
    },
    /// Too many sends went unacknowledged; the link is being re-established.
    Lost {
        /// The channel the link was on before it was lost.
        last_channel: u8,
    },
}

/// Pure ESP-NOW peer-link supervisor.  See the [module docs](self).
#[derive(Debug, Clone)]
pub struct PeerLink {
    config: PeerLinkConfig,
    started: bool,
    state: PeerLinkState,
    epoch: u32,
    scanning: bool,
    /// The coordinator was heard while the current scan runs.
    heard_during_scan: bool,
    retry_at: Option<u64>,
    backoff_ms: u64,
    no_ack: u16,
    rescans: u32,
    local_failures: u32,
}

impl PeerLink {
    /// Create a link that waits for [`PeerLinkEvent::Boot`].
    pub const fn new(config: PeerLinkConfig) -> Self {
        Self {
            config,
            started: false,
            state: PeerLinkState::Searching,
            epoch: 0,
            scanning: false,
            heard_during_scan: false,
            retry_at: None,
            backoff_ms: config.backoff_initial_ms,
            no_ack: 0,
            rescans: 0,
            local_failures: 0,
        }
    }
    /// Feed an event (or none) at time `now_ms` and get the action required now.
    ///
    /// Poll with `None` when time has passed, at the latest at the deadline of a
    /// [`PeerLinkAction::WaitUntil`].
    pub fn poll(&mut self, now_ms: u64, event: Option<PeerLinkEvent>) -> PeerLinkAction {
        if event.is_some_and(|event| self.rejects(event)) {
            return PeerLinkAction::Ignored;
        }
        let issued = match event {
            None => false,
            Some(PeerLinkEvent::Boot) => self.begin_scan(),
            Some(PeerLinkEvent::Sent { outcome, .. }) => self.on_sent(outcome),
            Some(PeerLinkEvent::PeerHeard) => {
                self.on_peer_heard();
                false
            }
            Some(PeerLinkEvent::ScanFound { channel, .. }) => {
                self.scanning = false;
                self.heard_during_scan = false;
                self.state = PeerLinkState::Linked { channel };
                self.reset_link_health();
                false
            }
            Some(PeerLinkEvent::ScanFailed { .. }) => {
                self.on_scan_failed(now_ms);
                false
            }
        };
        if issued {
            return self.scan_action();
        }
        if !self.started || self.scanning {
            return PeerLinkAction::None;
        }
        match self.retry_at {
            Some(at) if now_ms >= at => {
                self.retry_at = None;
                self.begin_scan();
                self.scan_action()
            }
            Some(at) => PeerLinkAction::WaitUntil(at),
            None => PeerLinkAction::None,
        }
    }

    fn scan_action(&self) -> PeerLinkAction {
        PeerLinkAction::Scan {
            id: PeerLinkEpoch(self.epoch),
        }
    }

    fn is_current_scan(&self, id: PeerLinkEpoch) -> bool {
        self.scanning && id.0 == self.epoch
    }

    /// True for an event that does not fit the current state: it changes nothing.
    fn rejects(&self, event: PeerLinkEvent) -> bool {
        match event {
            PeerLinkEvent::Boot => self.started,
            PeerLinkEvent::Sent { epoch, .. } => {
                !self.started || self.scanning || epoch.0 != self.epoch
            }
            PeerLinkEvent::PeerHeard => !self.started,
            PeerLinkEvent::ScanFound { id, .. } | PeerLinkEvent::ScanFailed { id } => {
                !self.is_current_scan(id)
            }
        }
    }

    fn reset_link_health(&mut self) {
        self.no_ack = 0;
        self.backoff_ms = self.config.backoff_initial_ms;
        self.retry_at = None;
    }

    /// Issues a scan: new epoch, no retry pending.  Always returns `true`.
    fn begin_scan(&mut self) -> bool {
        self.started = true;
        self.scanning = true;
        self.heard_during_scan = false;
        self.retry_at = None;
        self.epoch = self.epoch.wrapping_add(1);
        if matches!(self.state, PeerLinkState::Lost { .. }) {
            self.rescans = self.rescans.saturating_add(1);
        }
        true
    }

    /// A current-epoch send result while no scan runs (see [`Self::rejects`]).
    fn on_sent(&mut self, outcome: SendOutcome) -> bool {
        match outcome {
            SendOutcome::Local => {
                self.local_failures = self.local_failures.saturating_add(1);
                false
            }
            SendOutcome::Acked => {
                // An ACK proves the link exactly like a received frame; no scan runs here.
                self.on_peer_heard();
                false
            }
            SendOutcome::NoAck => {
                let PeerLinkState::Linked { channel } = self.state else {
                    return false;
                };
                self.no_ack = self.no_ack.saturating_add(1);
                if self.no_ack < self.config.rescan_threshold {
                    return false;
                }
                self.state = PeerLinkState::Lost {
                    last_channel: channel,
                };
                self.begin_scan()
            }
        }
    }

    /// The coordinator was heard after `Boot` (see [`Self::rejects`]).
    fn on_peer_heard(&mut self) {
        match self.state {
            PeerLinkState::Searching => {}
            PeerLinkState::Linked { .. } => self.no_ack = 0,
            PeerLinkState::Lost { last_channel } => {
                if self.scanning {
                    self.heard_during_scan = true;
                } else {
                    self.state = PeerLinkState::Linked {
                        channel: last_channel,
                    };
                    self.reset_link_health();
                }
            }
        }
    }

    fn on_scan_failed(&mut self, now_ms: u64) {
        self.scanning = false;
        let heard = core::mem::take(&mut self.heard_during_scan);
        if let (true, PeerLinkState::Lost { last_channel }) = (heard, self.state) {
            self.state = PeerLinkState::Linked {
                channel: last_channel,
            };
            self.reset_link_health();
            return;
        }
        let delay = self.backoff_ms.min(self.config.backoff_max_ms);
        self.retry_at = Some(now_ms.saturating_add(delay));
        self.backoff_ms = delay.saturating_mul(2).min(self.config.backoff_max_ms);
    }

    /// The current state.
    pub fn state(&self) -> PeerLinkState {
        self.state
    }
    /// The current epoch.
    ///
    /// Read it when a send is submitted and return it in [`PeerLinkEvent::Sent`].
    pub fn epoch(&self) -> PeerLinkEpoch {
        PeerLinkEpoch(self.epoch)
    }
    /// Whether a scan is outstanding.
    pub fn scanning(&self) -> bool {
        self.scanning
    }
    /// Scans issued after the first link was lost, retries included.
    pub fn rescans(&self) -> u32 {
        self.rescans
    }
    /// Consecutive unacknowledged sends while `Linked`.
    pub fn consecutive_no_ack(&self) -> u16 {
        self.no_ack
    }
    /// Sends that never left the node ([`SendOutcome::Local`]); they never trigger a scan.
    pub fn local_failures(&self) -> u32 {
        self.local_failures
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NO_ACK: SendOutcome = SendOutcome::NoAck;

    fn cfg(threshold: u16) -> PeerLinkConfig {
        PeerLinkConfig::default().with_rescan_threshold(threshold)
    }

    fn scan_id(action: PeerLinkAction) -> PeerLinkEpoch {
        match action {
            PeerLinkAction::Scan { id } => id,
            other => panic!("expected Scan, got {other:?}"),
        }
    }

    fn booted(config: PeerLinkConfig) -> (PeerLink, PeerLinkEpoch) {
        let mut link = PeerLink::new(config);
        let id = scan_id(link.poll(0, Some(PeerLinkEvent::Boot)));
        (link, id)
    }

    /// A link on channel 6 whose clock is at 100 ms.
    fn linked(config: PeerLinkConfig) -> PeerLink {
        let (mut link, id) = booted(config);
        link.poll(100, Some(PeerLinkEvent::ScanFound { id, channel: 6 }));
        assert_eq!(link.state(), PeerLinkState::Linked { channel: 6 });
        link
    }

    fn sent(link: &PeerLink, outcome: SendOutcome) -> PeerLinkEvent {
        PeerLinkEvent::Sent {
            outcome,
            epoch: link.epoch(),
        }
    }

    /// Drive `n` unacknowledged sends; return the action of the last one.
    fn no_acks(link: &mut PeerLink, now: u64, n: u16) -> PeerLinkAction {
        let mut last = PeerLinkAction::None;
        for _ in 0..n {
            let ev = sent(link, NO_ACK);
            last = link.poll(now, Some(ev));
        }
        last
    }

    /// A link that lost channel 6 and is scanning with id returned.
    fn lost(config: PeerLinkConfig) -> (PeerLink, PeerLinkEpoch) {
        let mut link = linked(config);
        let n = config.rescan_threshold;
        let id = scan_id(no_acks(&mut link, 200, n));
        (link, id)
    }

    #[test]
    fn nothing_happens_before_boot() {
        let mut link = PeerLink::new(cfg(3));
        assert_eq!(link.poll(0, None), PeerLinkAction::None);
        assert_eq!(link.poll(99_999, None), PeerLinkAction::None);
        let ev = sent(&link, NO_ACK);
        assert_eq!(link.poll(1, Some(ev)), PeerLinkAction::Ignored);
        assert_eq!(
            link.poll(1, Some(PeerLinkEvent::PeerHeard)),
            PeerLinkAction::Ignored
        );
        assert_eq!(link.state(), PeerLinkState::Searching);
        assert!(!link.scanning());
        assert_eq!(link.consecutive_no_ack(), 0);
        assert_eq!(link.local_failures(), 0);
    }

    #[test]
    fn boot_issues_the_first_scan() {
        let mut link = PeerLink::new(cfg(3));
        let epoch0 = link.epoch();
        let action = link.poll(5, Some(PeerLinkEvent::Boot));
        assert_eq!(action, PeerLinkAction::Scan { id: link.epoch() });
        assert_ne!(link.epoch(), epoch0);
        assert_eq!(link.state(), PeerLinkState::Searching);
        assert!(link.scanning());
        assert_eq!(link.rescans(), 0);
    }

    #[test]
    fn second_boot_is_ignored() {
        let (mut link, _) = booted(cfg(3));
        let epoch = link.epoch();
        assert_eq!(
            link.poll(1, Some(PeerLinkEvent::Boot)),
            PeerLinkAction::Ignored
        );
        assert_eq!(link.epoch(), epoch);
    }

    #[test]
    fn scan_issued_exactly_once() {
        let (mut link, _) = booted(cfg(3));
        assert_eq!(link.poll(1, None), PeerLinkAction::None);
        assert_eq!(link.poll(50_000, None), PeerLinkAction::None);
        assert!(link.scanning());
    }

    #[test]
    fn failures_below_the_threshold_do_not_scan() {
        let mut link = linked(cfg(5));
        no_acks(&mut link, 150, 2);
        assert_eq!(link.consecutive_no_ack(), 2);
        assert!(!link.scanning());
        assert_eq!(link.poll(160, None), PeerLinkAction::None);
    }

    #[test]
    fn scan_found_resets_failure_count_and_backoff() {
        let (mut link, id) = booted(cfg(3));
        link.poll(0, Some(PeerLinkEvent::ScanFailed { id }));
        let PeerLinkAction::Scan { id } = link.poll(2_000, None) else {
            panic!("retry scan expected")
        };
        link.poll(2_100, Some(PeerLinkEvent::ScanFound { id, channel: 11 }));
        assert_eq!(link.state(), PeerLinkState::Linked { channel: 11 });
        assert_eq!(link.consecutive_no_ack(), 0);
        let id = scan_id(no_acks(&mut link, 3_000, 3));
        let action = link.poll(3_000, Some(PeerLinkEvent::ScanFailed { id }));
        assert_eq!(action, PeerLinkAction::WaitUntil(5_000));
    }

    #[test]
    fn only_one_scan_outstanding_and_stale_results_are_ignored() {
        let (mut link, first) = booted(cfg(3));
        link.poll(10, Some(PeerLinkEvent::ScanFailed { id: first }));
        let second = scan_id(link.poll(2_010, None));
        assert_ne!(first, second);

        assert_eq!(
            link.poll(
                2_020,
                Some(PeerLinkEvent::ScanFound {
                    id: first,
                    channel: 3
                })
            ),
            PeerLinkAction::Ignored
        );
        assert_eq!(link.state(), PeerLinkState::Searching);
        assert!(link.scanning());
        assert_eq!(
            link.poll(2_030, Some(PeerLinkEvent::ScanFailed { id: first })),
            PeerLinkAction::Ignored
        );
        assert!(link.scanning());
        assert_eq!(link.poll(9_999_999, None), PeerLinkAction::None);

        link.poll(
            2_040,
            Some(PeerLinkEvent::ScanFound {
                id: second,
                channel: 3,
            }),
        );
        assert_eq!(link.state(), PeerLinkState::Linked { channel: 3 });
    }

    #[test]
    fn scan_result_without_a_running_scan_is_ignored() {
        let mut link = linked(cfg(3));
        let id = link.epoch();
        assert_eq!(
            link.poll(300, Some(PeerLinkEvent::ScanFound { id, channel: 9 })),
            PeerLinkAction::Ignored
        );
        assert_eq!(link.state(), PeerLinkState::Linked { channel: 6 });
        assert_eq!(
            link.poll(300, Some(PeerLinkEvent::ScanFailed { id })),
            PeerLinkAction::Ignored
        );
        assert_eq!(link.state(), PeerLinkState::Linked { channel: 6 });
    }

    #[test]
    fn boot_scan_failure_retries_without_traffic() {
        let (mut link, id) = booted(cfg(3));
        assert_eq!(
            link.poll(500, Some(PeerLinkEvent::ScanFailed { id })),
            PeerLinkAction::WaitUntil(2_500)
        );
        assert_eq!(link.state(), PeerLinkState::Searching);
        assert!(!link.scanning());
        assert_eq!(link.poll(1_000, None), PeerLinkAction::WaitUntil(2_500));
        assert_eq!(link.poll(2_499, None), PeerLinkAction::WaitUntil(2_500));
        let retry = scan_id(link.poll(2_500, None));
        assert_ne!(retry, id);
        assert!(link.scanning());
        assert_eq!(link.poll(2_501, None), PeerLinkAction::None);
        assert_eq!(link.rescans(), 0);
    }

    #[test]
    fn late_poll_after_deadline_scans() {
        let (mut link, id) = booted(cfg(3));
        link.poll(0, Some(PeerLinkEvent::ScanFailed { id }));
        assert!(matches!(
            link.poll(90_000, None),
            PeerLinkAction::Scan { .. }
        ));
    }

    #[test]
    fn backoff_doubles_and_is_capped() {
        let config = cfg(3).with_backoff_initial(1_000).with_backoff_max(5_000);
        let (mut link, mut id) = booted(config);
        let mut now = 0;
        let mut delays = [0u64; 6];
        for delay in &mut delays {
            let PeerLinkAction::WaitUntil(at) =
                link.poll(now, Some(PeerLinkEvent::ScanFailed { id }))
            else {
                panic!("WaitUntil expected")
            };
            *delay = at - now;
            now = at;
            id = scan_id(link.poll(now, None));
        }
        assert_eq!(delays, [1_000, 2_000, 4_000, 5_000, 5_000, 5_000]);
    }

    #[test]
    fn backoff_arithmetic_saturates() {
        let config = cfg(3)
            .with_backoff_initial(u64::MAX)
            .with_backoff_max(u64::MAX);
        let (mut link, id) = booted(config);
        assert_eq!(
            link.poll(u64::MAX - 1, Some(PeerLinkEvent::ScanFailed { id })),
            PeerLinkAction::WaitUntil(u64::MAX)
        );
    }

    #[test]
    fn peer_heard_while_searching_changes_nothing() {
        let (mut link, id) = booted(cfg(3));
        assert_eq!(
            link.poll(1, Some(PeerLinkEvent::PeerHeard)),
            PeerLinkAction::None
        );
        assert_eq!(link.state(), PeerLinkState::Searching);
        assert!(link.scanning());
        assert_eq!(
            link.poll(2, Some(PeerLinkEvent::ScanFailed { id })),
            PeerLinkAction::WaitUntil(2_002)
        );
        assert_eq!(link.state(), PeerLinkState::Searching);
        assert_eq!(
            link.poll(3, Some(PeerLinkEvent::PeerHeard)),
            PeerLinkAction::WaitUntil(2_002)
        );
        assert_eq!(link.state(), PeerLinkState::Searching);
    }

    #[test]
    fn acked_resets_the_failure_count() {
        let mut link = linked(cfg(5));
        no_acks(&mut link, 200, 4);
        assert_eq!(link.consecutive_no_ack(), 4);
        let ev = sent(&link, SendOutcome::Acked);
        assert_eq!(link.poll(300, Some(ev)), PeerLinkAction::None);
        assert_eq!(link.consecutive_no_ack(), 0);
        no_acks(&mut link, 400, 4);
        assert_eq!(link.state(), PeerLinkState::Linked { channel: 6 });
    }

    #[test]
    fn threshold_is_reached_exactly_at_n() {
        let mut link = linked(cfg(3));
        assert_eq!(no_acks(&mut link, 200, 2), PeerLinkAction::None);
        assert_eq!(link.state(), PeerLinkState::Linked { channel: 6 });
        assert_eq!(link.consecutive_no_ack(), 2);
        assert_eq!(link.rescans(), 0);
        let ev = sent(&link, NO_ACK);
        let action = link.poll(210, Some(ev));
        assert!(matches!(action, PeerLinkAction::Scan { .. }));
        assert_eq!(link.state(), PeerLinkState::Lost { last_channel: 6 });
        assert_eq!(link.rescans(), 1);
        assert!(link.scanning());
    }

    #[test]
    fn default_threshold_is_twenty() {
        let mut link = linked(PeerLinkConfig::default());
        assert_eq!(no_acks(&mut link, 200, 19), PeerLinkAction::None);
        assert!(matches!(
            no_acks(&mut link, 200, 1),
            PeerLinkAction::Scan { .. }
        ));
    }

    #[test]
    fn rescan_increments_the_epoch() {
        let mut link = linked(cfg(2));
        let before = link.epoch();
        let id = scan_id(no_acks(&mut link, 200, 2));
        assert_ne!(id, before);
        assert_eq!(id, link.epoch());
    }

    #[test]
    fn local_never_triggers_a_scan() {
        let mut link = linked(cfg(3));
        for i in 0..100 {
            let ev = sent(&link, SendOutcome::Local);
            assert_eq!(link.poll(200 + i, Some(ev)), PeerLinkAction::None);
        }
        assert_eq!(link.state(), PeerLinkState::Linked { channel: 6 });
        assert_eq!(link.local_failures(), 100);
        assert_eq!(link.consecutive_no_ack(), 0);
        assert!(!link.scanning());
    }

    #[test]
    fn local_does_not_reset_the_failure_count() {
        let mut link = linked(cfg(3));
        no_acks(&mut link, 200, 2);
        let ev = sent(&link, SendOutcome::Local);
        link.poll(210, Some(ev));
        assert_eq!(link.consecutive_no_ack(), 2);
        assert!(matches!(
            no_acks(&mut link, 220, 1),
            PeerLinkAction::Scan { .. }
        ));
    }

    #[test]
    fn stale_send_results_are_ignored() {
        let mut link = linked(cfg(2));
        let old = link.epoch();
        let id = scan_id(no_acks(&mut link, 200, 2));
        link.poll(300, Some(PeerLinkEvent::ScanFound { id, channel: 1 }));
        assert_eq!(link.state(), PeerLinkState::Linked { channel: 1 });
        for outcome in [NO_ACK, SendOutcome::Local, SendOutcome::Acked] {
            let ev = PeerLinkEvent::Sent {
                outcome,
                epoch: old,
            };
            assert_eq!(link.poll(310, Some(ev)), PeerLinkAction::Ignored);
        }
        assert_eq!(link.consecutive_no_ack(), 0);
        assert_eq!(link.local_failures(), 0);
        let ev = PeerLinkEvent::Sent {
            outcome: NO_ACK,
            epoch: old,
        };
        link.poll(320, Some(ev));
        link.poll(320, Some(ev));
        assert_eq!(link.state(), PeerLinkState::Linked { channel: 1 });
    }

    #[test]
    fn sends_during_a_scan_are_ignored() {
        let (mut link, id) = lost(cfg(2));
        let ev = sent(&link, NO_ACK);
        assert_eq!(link.poll(210, Some(ev)), PeerLinkAction::Ignored);
        let ev = sent(&link, SendOutcome::Local);
        assert_eq!(link.poll(211, Some(ev)), PeerLinkAction::Ignored);
        assert_eq!(link.local_failures(), 0);
        assert_eq!(link.consecutive_no_ack(), 2);
        assert_eq!(link.rescans(), 1);
        assert!(link.scanning());
        link.poll(220, Some(PeerLinkEvent::ScanFound { id, channel: 6 }));
        assert_eq!(link.state(), PeerLinkState::Linked { channel: 6 });
    }

    #[test]
    fn lost_scan_failure_stays_lost_and_retries() {
        let (mut link, id) = lost(cfg(2));
        assert_eq!(
            link.poll(1_000, Some(PeerLinkEvent::ScanFailed { id })),
            PeerLinkAction::WaitUntil(3_000)
        );
        assert_eq!(link.state(), PeerLinkState::Lost { last_channel: 6 });
        assert_eq!(link.poll(2_000, None), PeerLinkAction::WaitUntil(3_000));
        assert!(matches!(
            link.poll(3_000, None),
            PeerLinkAction::Scan { .. }
        ));
        assert_eq!(link.rescans(), 2);
    }

    #[test]
    fn peer_heard_in_lost_cancels_the_retry() {
        let (mut link, id) = lost(cfg(2));
        link.poll(1_000, Some(PeerLinkEvent::ScanFailed { id }));
        assert_eq!(
            link.poll(1_500, Some(PeerLinkEvent::PeerHeard)),
            PeerLinkAction::None
        );
        assert_eq!(link.state(), PeerLinkState::Linked { channel: 6 });
        assert_eq!(link.consecutive_no_ack(), 0);
        assert_eq!(link.poll(3_000, None), PeerLinkAction::None);
        assert_eq!(link.poll(60_000, None), PeerLinkAction::None);
        assert!(!link.scanning());
    }

    #[test]
    fn acked_in_lost_relinks_like_peer_heard() {
        let (mut link, id) = lost(cfg(2));
        link.poll(1_000, Some(PeerLinkEvent::ScanFailed { id }));
        let ev = sent(&link, SendOutcome::Acked);
        assert_eq!(link.poll(1_500, Some(ev)), PeerLinkAction::None);
        assert_eq!(link.state(), PeerLinkState::Linked { channel: 6 });
        assert_eq!(link.poll(60_000, None), PeerLinkAction::None);
        assert!(!link.scanning());
    }

    #[test]
    fn peer_heard_in_lost_resets_backoff() {
        let (mut link, id) = lost(cfg(2));
        link.poll(1_000, Some(PeerLinkEvent::ScanFailed { id }));
        let id = scan_id(link.poll(3_000, None));
        link.poll(3_000, Some(PeerLinkEvent::ScanFailed { id }));
        link.poll(3_100, Some(PeerLinkEvent::PeerHeard));
        let id = scan_id(no_acks(&mut link, 4_000, 2));
        assert_eq!(
            link.poll(4_000, Some(PeerLinkEvent::ScanFailed { id })),
            PeerLinkAction::WaitUntil(6_000)
        );
    }

    #[test]
    fn peer_heard_in_linked_resets_the_failure_count() {
        let mut link = linked(cfg(3));
        no_acks(&mut link, 200, 2);
        assert_eq!(
            link.poll(210, Some(PeerLinkEvent::PeerHeard)),
            PeerLinkAction::None
        );
        assert_eq!(link.consecutive_no_ack(), 0);
        no_acks(&mut link, 220, 2);
        assert_eq!(link.state(), PeerLinkState::Linked { channel: 6 });
    }

    #[test]
    fn heard_during_scan_then_failure_returns_to_last_channel() {
        let (mut link, id) = lost(cfg(2));
        assert_eq!(
            link.poll(250, Some(PeerLinkEvent::PeerHeard)),
            PeerLinkAction::None
        );
        assert!(link.scanning());
        assert_eq!(link.state(), PeerLinkState::Lost { last_channel: 6 });
        assert_eq!(
            link.poll(900, Some(PeerLinkEvent::ScanFailed { id })),
            PeerLinkAction::None
        );
        assert_eq!(link.state(), PeerLinkState::Linked { channel: 6 });
        assert_eq!(link.consecutive_no_ack(), 0);
        assert_eq!(link.poll(60_000, None), PeerLinkAction::None);
    }

    #[test]
    fn heard_during_scan_then_found_uses_the_new_channel() {
        let (mut link, id) = lost(cfg(2));
        link.poll(250, Some(PeerLinkEvent::PeerHeard));
        link.poll(900, Some(PeerLinkEvent::ScanFound { id, channel: 11 }));
        assert_eq!(link.state(), PeerLinkState::Linked { channel: 11 });
    }

    #[test]
    fn heard_flag_does_not_leak_into_the_next_scan() {
        let (mut link, id) = lost(cfg(2));
        link.poll(250, Some(PeerLinkEvent::PeerHeard));
        link.poll(300, Some(PeerLinkEvent::ScanFound { id, channel: 6 }));
        let id = scan_id(no_acks(&mut link, 400, 2));
        assert_eq!(
            link.poll(900, Some(PeerLinkEvent::ScanFailed { id })),
            PeerLinkAction::WaitUntil(2_900)
        );
        assert_eq!(link.state(), PeerLinkState::Lost { last_channel: 6 });
    }

    #[test]
    fn heard_before_a_boot_scan_fails_does_not_link() {
        let (mut link, id) = booted(cfg(2));
        link.poll(10, Some(PeerLinkEvent::PeerHeard));
        link.poll(20, Some(PeerLinkEvent::ScanFailed { id }));
        assert_eq!(link.state(), PeerLinkState::Searching);
    }

    #[test]
    fn config_defaults_and_builders() {
        let c = PeerLinkConfig::default();
        assert_eq!(c.rescan_threshold, 20);
        assert_eq!(c.backoff_initial_ms, 2_000);
        assert_eq!(c.backoff_max_ms, 60_000);
        let c = c
            .with_rescan_threshold(5)
            .with_backoff_initial(10)
            .with_backoff_max(20);
        assert_eq!(
            (c.rescan_threshold, c.backoff_initial_ms, c.backoff_max_ms),
            (5, 10, 20)
        );
    }

    #[test]
    fn threshold_zero_does_not_panic() {
        let mut link = linked(cfg(0));
        let ev = sent(&link, NO_ACK);
        assert!(matches!(
            link.poll(200, Some(ev)),
            PeerLinkAction::Scan { .. }
        ));
    }

    #[test]
    fn ignored_for_stale_scan_id() {
        let (mut link, first) = booted(cfg(3));
        let failed = link.poll(10, Some(PeerLinkEvent::ScanFailed { id: first }));
        assert_eq!(failed, PeerLinkAction::WaitUntil(2_010));
        let second = scan_id(link.poll(2_010, None));
        for event in [
            PeerLinkEvent::ScanFound {
                id: first,
                channel: 1,
            },
            PeerLinkEvent::ScanFailed { id: first },
        ] {
            assert_eq!(link.poll(2_020, Some(event)), PeerLinkAction::Ignored);
        }
        assert!(link.scanning());
        let found = link.poll(
            2_030,
            Some(PeerLinkEvent::ScanFound {
                id: second,
                channel: 1,
            }),
        );
        assert_eq!(found, PeerLinkAction::None);
    }

    #[test]
    fn ignored_for_stale_send_epoch() {
        let mut link = linked(cfg(2));
        let old = link.epoch();
        let id = scan_id(no_acks(&mut link, 200, 2));
        link.poll(300, Some(PeerLinkEvent::ScanFound { id, channel: 1 }));
        let stale = PeerLinkEvent::Sent {
            outcome: NO_ACK,
            epoch: old,
        };
        assert_eq!(link.poll(310, Some(stale)), PeerLinkAction::Ignored);
        let current = sent(&link, NO_ACK);
        assert_eq!(link.poll(310, Some(current)), PeerLinkAction::None);
        assert_eq!(link.consecutive_no_ack(), 1);
    }

    #[test]
    fn ignored_for_second_boot() {
        let (mut link, id) = booted(cfg(3));
        assert_eq!(
            link.poll(1, Some(PeerLinkEvent::Boot)),
            PeerLinkAction::Ignored
        );
        assert_eq!(link.epoch(), id);
        assert!(link.scanning());
    }

    #[test]
    fn ignored_before_boot() {
        let mut link = PeerLink::new(cfg(3));
        let id = link.epoch();
        for event in [
            PeerLinkEvent::PeerHeard,
            PeerLinkEvent::Sent {
                outcome: SendOutcome::Acked,
                epoch: id,
            },
            PeerLinkEvent::ScanFound { id, channel: 1 },
            PeerLinkEvent::ScanFailed { id },
        ] {
            assert_eq!(link.poll(0, Some(event)), PeerLinkAction::Ignored);
        }
        assert_eq!(link.poll(0, None), PeerLinkAction::None);
        assert!(matches!(
            link.poll(0, Some(PeerLinkEvent::Boot)),
            PeerLinkAction::Scan { .. }
        ));
    }

    #[test]
    fn idle_poll_during_scan_is_none() {
        let (mut link, _) = lost(cfg(2));
        assert!(link.scanning());
        assert_eq!(link.poll(300, None), PeerLinkAction::None);
        assert_eq!(link.poll(90_000, None), PeerLinkAction::None);
        // An accepted event during a scan is not ignored either.
        assert_eq!(
            link.poll(301, Some(PeerLinkEvent::PeerHeard)),
            PeerLinkAction::None
        );
    }
}
