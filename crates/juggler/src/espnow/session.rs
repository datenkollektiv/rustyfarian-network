//! [`PeerSession`] — the async helper that drives a [`PeerLink`] over an [`EspNowLink`].
//!
//! The sans-IO [`PeerLink`] decides *when* a node scans for its coordinator; this helper does
//! the I/O around it on either tier, so one application body runs unchanged on ESP-IDF and
//! esp-hal (ADR 022).
//! Time comes in through the [`Clock`] port; [`EmbassyClock`] (feature `embassy`) serves both
//! tiers, because both run `embassy-time` and the firmware selects the time driver (ADR 021).
//!
//! # Shape
//!
//! The handle owns the sends: [`PeerSession::send`] addresses the coordinator, classifies the
//! result and feeds it to the core as [`PeerLinkEvent::Sent`] with the epoch read at submit
//! time, so a result from before a rescan is recognised as stale.
//! [`PeerSession::receive`] returns every frame to the application (echoes still reach it)
//! and feeds [`PeerLinkEvent::PeerHeard`] only for frames from the coordinator MAC; it is the
//! callback-free way to hand frames on in `no_std`, so the application's receive loop awaits
//! it instead of `link.receive()`.
//! [`PeerSession::run`] never returns: it boots the core, runs every [`PeerLinkAction::Scan`]
//! through [`EspNowLink::scan_for_peer`] and feeds the result back with the scan id, and sleeps
//! until the earlier of a [`PeerLinkAction::WaitUntil`] deadline and the next accepted event
//! from `send` or `receive`.
//! The application runs the three together, for example `join3(session.run(), tx, rx)`.
//!
//! # Log lines
//!
//! The helper logs the two peer-link transitions the bench checks match, so an application
//! must not print them again: `[espnow] coordinator found on channel N` at `info` when a scan
//! finds the coordinator, and `[espnow] N sends unacknowledged - rescanning` at `warn` when the
//! rescan threshold is reached.
//! A failed scan that schedules a retry is logged at `warn` with the retry delay; a failed
//! scan during which the coordinator was heard relinks on the last channel (`PeerLink` rule 5)
//! and logs that at `info` instead.
//!
//! # Rules
//!
//! 1. A send issued while a scan is in flight is refused with [`PeerSendError::Scanning`]
//!    (classifies [`SendOutcome::Local`]) without touching the radio, and counts in
//!    [`PeerSession::local_failures`]; the tier would refuse it the same way.
//! 2. A dropped `send` future feeds nothing: the core never learns of that frame.
//! 3. `run` is started once per session; a second concurrent `run` shares one waker slot and
//!    busy-polls.
//!    A `run` dropped while its scan is in flight hands the scan back: the tier cancels the
//!    scan when the future drops, the core still counts it as outstanding, and the next `run`
//!    issues it again under the same id, so sends keep returning
//!    [`PeerSendError::Scanning`] only until that `run` is polled.
//! 4. Sends before `run` was first polled go to the radio, but their results are ignored by
//!    the core (it has not booted).

use core::cell::{Cell, RefCell};
use core::future::{poll_fn, Future};
use core::pin::pin;
use core::task::{Context, Poll};
use core::time::Duration;

use super::{
    ClassifySend, EspNowDriver, EspNowEvent, EspNowLink, MacAddress, PeerLink, PeerLinkAction,
    PeerLinkConfig, PeerLinkEpoch, PeerLinkEvent, PeerLinkState, ScanConfig, SendOutcome,
};
use crate::waker_slots::{poll_ready, ReadyPoll, WakerSlots};

/// Monotonic millisecond time and a sleep, the two things [`PeerSession::run`] needs.
///
/// `now_ms` is any monotonic count; only differences matter.
#[allow(async_fn_in_trait)] // futures deliberately carry no `Send` bound (ADR 022)
pub trait Clock {
    /// The current time in milliseconds.
    fn now_ms(&self) -> u64;

    /// Resolves once [`now_ms`](Self::now_ms) is at or past `at`; at once if it already is.
    async fn sleep_until_ms(&self, at: u64);
}

impl<C: Clock + ?Sized> Clock for &C {
    fn now_ms(&self) -> u64 {
        (**self).now_ms()
    }

    async fn sleep_until_ms(&self, at: u64) {
        (**self).sleep_until_ms(at).await
    }
}

/// The `embassy-time` [`Clock`], shared by both tiers (feature `embassy`).
///
/// It selects no time driver: the firmware does (ADR 021).
#[cfg(feature = "embassy")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EmbassyClock;

#[cfg(feature = "embassy")]
impl Clock for EmbassyClock {
    fn now_ms(&self) -> u64 {
        embassy_time::Instant::now().as_millis()
    }

    async fn sleep_until_ms(&self, at: u64) {
        // `Instant::from_millis` multiplies unchecked; a deadline past the representable
        // range (a huge `backoff_max_ms`) is clamped instead of overflowing.
        let at = at.min(embassy_time::Instant::MAX.as_millis());
        embassy_time::Timer::at(embassy_time::Instant::from_millis(at)).await
    }
}

/// Error of [`PeerSession::send`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerSendError<E> {
    /// A scan for the coordinator is in flight; the frame was not handed to the radio.
    Scanning,
    /// The tier's [`send_acked`](EspNowLink::send_acked) failed.
    Link(E),
}

impl<E: ClassifySend> ClassifySend for PeerSendError<E> {
    fn outcome(&self) -> SendOutcome {
        match self {
            Self::Scanning => SendOutcome::Local,
            Self::Link(error) => error.outcome(),
        }
    }
}

impl<E: core::fmt::Display> core::fmt::Display for PeerSendError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Scanning => write!(f, "ESP-NOW scan for the coordinator in progress"),
            Self::Link(error) => write!(f, "{error}"),
        }
    }
}

/// Async peer-link helper over an [`EspNowLink`].  See the [module docs](self).
pub struct PeerSession<'a, L, C> {
    link: &'a L,
    coordinator: MacAddress,
    scan_config: ScanConfig<'a>,
    clock: C,
    core: RefCell<PeerLink>,
    /// A `Scan` the core issued that [`run`](Self::run) has not started yet.
    pending_scan: Cell<Option<PeerLinkEpoch>>,
    /// Sends refused by rule 1; the core never saw them.
    scan_refusals: Cell<u32>,
    notified: Cell<bool>,
    waker: RefCell<WakerSlots<1>>,
}

impl<'a, L, C> PeerSession<'a, L, C>
where
    L: EspNowLink,
    <L as EspNowDriver>::Error: ClassifySend,
    C: Clock,
{
    /// A session for the coordinator at `coordinator`, scanning with [`ScanConfig::for_peer`].
    ///
    /// The first successful scan registers the coordinator as a peer on both tiers; register
    /// it beforehand only if you send before [`run`](Self::run) has found it, because such a
    /// send fails on the tier.
    pub fn new(link: &'a L, coordinator: MacAddress, config: PeerLinkConfig, clock: C) -> Self {
        Self {
            link,
            coordinator,
            scan_config: ScanConfig::for_peer(),
            clock,
            core: RefCell::new(PeerLink::new(config)),
            pending_scan: Cell::new(None),
            scan_refusals: Cell::new(0),
            notified: Cell::new(false),
            waker: RefCell::new(WakerSlots::new()),
        }
    }

    /// Replaces the scan configuration used by [`run`](Self::run).
    #[must_use]
    pub fn with_scan_config(mut self, scan_config: ScanConfig<'a>) -> Self {
        self.scan_config = scan_config;
        self
    }

    /// The configured coordinator MAC.
    pub fn coordinator(&self) -> &MacAddress {
        &self.coordinator
    }

    /// The current link state.
    pub fn state(&self) -> PeerLinkState {
        self.core.borrow().state()
    }

    /// Whether a scan is outstanding (issued, in flight, or waiting for its result).
    pub fn scanning(&self) -> bool {
        self.core.borrow().scanning()
    }

    /// Scans issued after the first link was lost, retries included.
    pub fn rescans(&self) -> u32 {
        self.core.borrow().rescans()
    }

    /// Sends that never left the node: those the core classified [`SendOutcome::Local`] plus
    /// those this handle refused during a scan (rule 1).
    pub fn local_failures(&self) -> u32 {
        self.core
            .borrow()
            .local_failures()
            .saturating_add(self.scan_refusals.get())
    }

    /// Consecutive unacknowledged sends while linked.
    pub fn consecutive_no_ack(&self) -> u16 {
        self.core.borrow().consecutive_no_ack()
    }

    /// Sends `data` to the coordinator and feeds the outcome to the link core.
    ///
    /// `timeout` is passed to [`EspNowLink::send_acked`] unchanged.
    /// During a scan the call returns [`PeerSendError::Scanning`] at once without radio I/O
    /// (rule 1).
    /// Classify the result with [`SendOutcome::of`] for counters.
    pub async fn send(
        &self,
        data: &[u8],
        timeout: Duration,
    ) -> Result<(), PeerSendError<<L as EspNowDriver>::Error>> {
        // The epoch is read here, before the radio I/O, and goes back with the outcome: a
        // frame submitted before a rescan must not count against the channel found after it.
        // Mutation check: feeding `self.core.borrow().epoch()` after the await instead makes
        // `stale_send_outcomes_are_ignored` fail (the old `NoAck` counts, `consecutive_no_ack`
        // becomes 1).
        let epoch = {
            let core = self.core.borrow();
            if core.scanning() {
                drop(core);
                self.scan_refusals
                    .set(self.scan_refusals.get().saturating_add(1));
                return Err(PeerSendError::Scanning);
            }
            core.epoch()
        };
        let result = self.link.send_acked(&self.coordinator, data, timeout).await;
        let outcome = SendOutcome::of(&result);
        if let PeerLinkAction::Scan { .. } = self.feed(PeerLinkEvent::Sent { outcome, epoch }) {
            log::warn!(
                "[espnow] {} sends unacknowledged - rescanning",
                self.core.borrow().consecutive_no_ack()
            );
        }
        result.map_err(PeerSendError::Link)
    }

    /// Waits for the next frame and returns it; a frame from the coordinator also proves the
    /// link ([`PeerLinkEvent::PeerHeard`]).
    ///
    /// Call this instead of [`EspNowLink::receive`] on the link, or the core never hears the
    /// coordinator.
    pub async fn receive(&self) -> EspNowEvent {
        let event = self.link.receive().await;
        // Mutation check: dropping the MAC comparison (feeding `PeerHeard` for every frame)
        // makes `unrelated_frames_give_no_peer_heard` fail (a stranger's frame clears the
        // failure count, so the link never becomes `Lost`).
        if event.mac == self.coordinator {
            self.feed(PeerLinkEvent::PeerHeard);
        }
        event
    }

    /// Drives the link core forever: boot scan, rescans, retry backoff.
    ///
    /// Start it once, alongside the send and receive loops.
    pub async fn run(&self) -> ! {
        let now = self.clock.now_ms();
        self.note(self.core.borrow_mut().poll(now, Some(PeerLinkEvent::Boot)));
        loop {
            if let Some(id) = self.pending_scan.take() {
                self.scan(id).await;
                continue;
            }
            let now = self.clock.now_ms();
            let action = self.core.borrow_mut().poll(now, None);
            match action {
                PeerLinkAction::Scan { id } => self.pending_scan.set(Some(id)),
                PeerLinkAction::WaitUntil(at) => self.wait_notified_or_until(at).await,
                PeerLinkAction::None | PeerLinkAction::Ignored => self.wait_notified().await,
            }
        }
    }

    async fn scan(&self, id: PeerLinkEpoch) {
        // Rule 3: until the result is fed, a dropped `run` hands the scan back to
        // `pending_scan`, so the core's outstanding scan is issued again by the next `run`.
        // Mutation check: dropping the guard (or never arming it) makes
        // `dropped_run_mid_scan_is_resumed_by_the_next_run` fail (no second scan, the session
        // stays `Searching` with `scanning()` true).
        let mut guard = PendingScanGuard {
            pending: &self.pending_scan,
            id: Some(id),
        };
        let result = self
            .link
            .scan_for_peer(&self.coordinator, &self.scan_config)
            .await;
        guard.id = None;
        let event = match &result {
            Ok(found) => PeerLinkEvent::ScanFound {
                id,
                channel: found.channel,
            },
            Err(_) => PeerLinkEvent::ScanFailed { id },
        };
        let now = self.clock.now_ms();
        let action = self.core.borrow_mut().poll(now, Some(event));
        match result {
            Ok(found) => log::info!("[espnow] coordinator found on channel {}", found.channel),
            Err(error) => match (action, self.core.borrow().state()) {
                (PeerLinkAction::WaitUntil(at), _) => log::warn!(
                    "[espnow] scan failed: {error:?} - retrying in {} ms",
                    at.saturating_sub(now)
                ),
                (_, PeerLinkState::Linked { channel }) => log::info!(
                    "[espnow] coordinator heard during the scan - relinked on channel {channel}"
                ),
                _ => log::warn!("[espnow] scan failed: {error:?}"),
            },
        }
        self.note(action);
    }

    /// Feeds an event from `send` or `receive` and wakes `run` if it was accepted.
    fn feed(&self, event: PeerLinkEvent) -> PeerLinkAction {
        let now = self.clock.now_ms();
        let action = self.core.borrow_mut().poll(now, Some(event));
        if action != PeerLinkAction::Ignored {
            self.note(action);
            self.notify();
        }
        action
    }

    /// Remembers a `Scan` the core issued so `run` starts it.
    fn note(&self, action: PeerLinkAction) {
        if let PeerLinkAction::Scan { id } = action {
            self.pending_scan.set(Some(id));
        }
    }

    fn notify(&self) {
        self.notified.set(true);
        self.waker.borrow_mut().wake_all();
    }

    fn poll_notified(&self, cx: &mut Context<'_>) -> Poll<()> {
        let registered = self.waker.borrow_mut().register(cx.waker());
        match poll_ready(registered, self.notified.replace(false)) {
            ReadyPoll::Ready => Poll::Ready(()),
            ReadyPoll::Waiting => Poll::Pending,
            ReadyPoll::Full => {
                // A second `run` (rule 3): no slot, so re-poll instead of parking forever.
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }

    async fn wait_notified(&self) {
        poll_fn(|cx| self.poll_notified(cx)).await
    }

    async fn wait_notified_or_until(&self, at: u64) {
        let mut sleep = pin!(self.clock.sleep_until_ms(at));
        poll_fn(|cx| {
            if self.poll_notified(cx).is_ready() {
                return Poll::Ready(());
            }
            sleep.as_mut().poll(cx)
        })
        .await
    }
}

/// Hands a scan back to `pending_scan` if [`PeerSession::scan`] is dropped before its result
/// was fed (rule 3).
struct PendingScanGuard<'a> {
    pending: &'a Cell<Option<PeerLinkEpoch>>,
    id: Option<PeerLinkEpoch>,
}

impl Drop for PendingScanGuard<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            self.pending.set(Some(id));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::espnow::mock::{MockEspNowLink, MockLinkError};
    use crate::espnow::{PeerConfig, ScanResult};
    use crate::test_support::block_on;
    use alloc::sync::Arc;
    use alloc::task::Wake;
    use core::pin::Pin;
    use core::sync::atomic::{AtomicU32, Ordering};
    use core::task::Waker;

    const COORDINATOR: MacAddress = [0x02, 0, 0, 0, 0, 0x01];
    const STRANGER: MacAddress = [0x02, 0, 0, 0, 0, 0x02];
    const T: Duration = Duration::from_millis(50);

    struct FakeClock {
        now: Cell<u64>,
    }

    impl FakeClock {
        fn new() -> Self {
            Self { now: Cell::new(0) }
        }

        fn set(&self, ms: u64) {
            self.now.set(ms);
        }
    }

    impl Clock for FakeClock {
        fn now_ms(&self) -> u64 {
            self.now.get()
        }

        async fn sleep_until_ms(&self, at: u64) {
            poll_fn(|cx| {
                if self.now.get() >= at {
                    Poll::Ready(())
                } else {
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            })
            .await
        }
    }

    struct Counter(AtomicU32);

    impl Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn counting() -> (Arc<Counter>, Waker) {
        let counter = Arc::new(Counter(AtomicU32::new(0)));
        (Arc::clone(&counter), Waker::from(counter))
    }

    /// One poll with `waker`; the mocks wake themselves while pending, so one poll of `run`
    /// advances until it parks on a wait.
    fn poll_with<F: Future>(future: Pin<&mut F>, waker: &Waker) -> Poll<F::Output> {
        future.poll(&mut Context::from_waker(waker))
    }

    fn poll_once<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
        poll_with(future, Waker::noop())
    }

    fn cfg(threshold: u16) -> PeerLinkConfig {
        PeerLinkConfig::default().with_rescan_threshold(threshold)
    }

    fn link() -> MockEspNowLink {
        let link = MockEspNowLink::new();
        link.add_peer(&PeerConfig::new(COORDINATOR)).unwrap();
        link
    }

    fn session<'a>(
        link: &'a MockEspNowLink,
        clock: &'a FakeClock,
        threshold: u16,
    ) -> PeerSession<'a, MockEspNowLink, &'a FakeClock> {
        PeerSession::new(link, COORDINATOR, cfg(threshold), clock)
    }

    fn no_ack(link: &MockEspNowLink, session: &PeerSession<'_, MockEspNowLink, &FakeClock>) {
        link.script_send(Err(MockLinkError::NoAck));
        let result = block_on(session.send(b"tick", T));
        assert_eq!(SendOutcome::of(&result), SendOutcome::NoAck);
    }

    #[test]
    fn boot_scan_retries_without_traffic() {
        let link = link();
        let clock = FakeClock::new();
        let session = session(&link, &clock, 3);
        link.script_scan(Err(MockLinkError::Local));
        let mut run = pin!(session.run());

        assert!(poll_once(run.as_mut()).is_pending());
        assert_eq!(link.scans_requested(), 1);
        assert_eq!(session.state(), PeerLinkState::Searching);
        assert!(!session.scanning());

        clock.set(1_999);
        assert!(poll_once(run.as_mut()).is_pending());
        assert_eq!(link.scans_requested(), 1, "before the backoff deadline");

        clock.set(2_000);
        link.script_scan(Ok(ScanResult { channel: 6 }));
        assert!(poll_once(run.as_mut()).is_pending());
        assert_eq!(
            link.scans_requested(),
            2,
            "retried without any send or frame"
        );
        assert_eq!(session.state(), PeerLinkState::Linked { channel: 6 });
        assert_eq!(link.pinned_channel(), Some(6));
        assert_eq!(session.rescans(), 0, "a boot retry is not a rescan");
        assert!(link.sent_messages().is_empty());
    }

    /// A session whose boot scan will find channel 6; `boot` runs it.
    fn linked<'a>(
        link: &'a MockEspNowLink,
        clock: &'a FakeClock,
        threshold: u16,
    ) -> PeerSession<'a, MockEspNowLink, &'a FakeClock> {
        let session = session(link, clock, threshold);
        link.script_scan(Ok(ScanResult { channel: 6 }));
        session
    }

    /// Polls `run` once with `waker`: the boot scan links on channel 6 and the loop parks.
    ///
    /// Each test owns one `run` future, because a dropped `run` leaves its waker slot taken
    /// until the next wake (rule 3).
    fn boot<F: Future>(
        run: Pin<&mut F>,
        session: &PeerSession<'_, MockEspNowLink, &FakeClock>,
        waker: &Waker,
    ) {
        assert!(poll_with(run, waker).is_pending());
        assert_eq!(session.state(), PeerLinkState::Linked { channel: 6 });
    }

    #[test]
    fn unrelated_frames_give_no_peer_heard() {
        let link = link();
        let clock = FakeClock::new();
        let session = linked(&link, &clock, 3);
        let mut run = pin!(session.run());
        boot(run.as_mut(), &session, Waker::noop());
        no_ack(&link, &session);
        no_ack(&link, &session);
        assert_eq!(session.consecutive_no_ack(), 2);

        link.queue_rx_event(EspNowEvent::new(STRANGER, b"echo"));
        let event = block_on(session.receive());
        assert_eq!(event.mac, STRANGER, "every frame reaches the application");
        assert_eq!(event.payload(), b"echo");
        assert_eq!(session.consecutive_no_ack(), 2, "a stranger proves nothing");

        link.queue_rx_event(EspNowEvent::new(COORDINATOR, b"pong"));
        let event = block_on(session.receive());
        assert_eq!(event.mac, COORDINATOR);
        assert_eq!(
            session.consecutive_no_ack(),
            0,
            "the coordinator proves the link"
        );

        no_ack(&link, &session);
        no_ack(&link, &session);
        assert_eq!(session.state(), PeerLinkState::Linked { channel: 6 });
        link.queue_rx_event(EspNowEvent::new(STRANGER, b"echo"));
        block_on(session.receive());
        no_ack(&link, &session);
        assert_eq!(session.state(), PeerLinkState::Lost { last_channel: 6 });
    }

    #[test]
    fn local_never_triggers_a_scan() {
        let link = link();
        let clock = FakeClock::new();
        let session = linked(&link, &clock, 3);
        let mut run = pin!(session.run());
        boot(run.as_mut(), &session, Waker::noop());
        for _ in 0..50 {
            link.script_send(Err(MockLinkError::Local));
            let result = block_on(session.send(b"tick", T));
            assert_eq!(SendOutcome::of(&result), SendOutcome::Local);
            assert!(matches!(
                result,
                Err(PeerSendError::Link(MockLinkError::Local))
            ));
        }
        clock.set(60_000);
        assert!(poll_once(run.as_mut()).is_pending());
        assert_eq!(link.scans_requested(), 1, "only the boot scan");
        assert_eq!(session.state(), PeerLinkState::Linked { channel: 6 });
        assert_eq!(session.local_failures(), 50);
        assert_eq!(session.consecutive_no_ack(), 0);
        assert_eq!(session.rescans(), 0);
        assert!(link.sent_messages().is_empty(), "local failures never left");
    }

    #[test]
    fn send_during_a_scan_returns_local_without_radio_io() {
        let link = link();
        let clock = FakeClock::new();
        let session = linked(&link, &clock, 2);
        let mut run = pin!(session.run());
        boot(run.as_mut(), &session, Waker::noop());
        no_ack(&link, &session);
        no_ack(&link, &session);
        assert!(session.scanning(), "the threshold issued a scan");
        assert_eq!(link.sent_messages().len(), 2);

        link.script_send(Ok(()));
        let result = block_on(session.send(b"tick", T));
        assert!(matches!(result, Err(PeerSendError::Scanning)));
        assert_eq!(SendOutcome::of(&result), SendOutcome::Local);
        assert_eq!(
            link.sent_messages().len(),
            2,
            "no radio I/O during the scan"
        );
        assert_eq!(session.local_failures(), 1);

        link.script_scan(Ok(ScanResult { channel: 11 }));
        assert!(poll_once(run.as_mut()).is_pending());
        assert_eq!(session.state(), PeerLinkState::Linked { channel: 11 });
        assert_eq!(session.rescans(), 1);
        let result = block_on(session.send(b"tick", T));
        assert!(result.is_ok(), "the scripted Ok was kept for the next send");
        assert_eq!(link.sent_messages().len(), 3);
    }

    #[test]
    fn stale_send_outcomes_are_ignored() {
        let link = link();
        let clock = FakeClock::new();
        let session = linked(&link, &clock, 2);
        let mut run = pin!(session.run());
        boot(run.as_mut(), &session, Waker::noop());

        link.set_hold_sends(true);
        link.script_send(Err(MockLinkError::NoAck));
        let mut stale = pin!(session.send(b"old", T));
        assert!(poll_once(stale.as_mut()).is_pending(), "in flight");
        link.set_hold_sends(false);

        no_ack(&link, &session);
        no_ack(&link, &session);
        link.script_scan(Ok(ScanResult { channel: 11 }));
        assert!(poll_once(run.as_mut()).is_pending());
        assert_eq!(session.state(), PeerLinkState::Linked { channel: 11 });
        assert_eq!(session.consecutive_no_ack(), 0);

        let result = poll_once(stale.as_mut());
        let Poll::Ready(result) = result else {
            panic!("the held send completes once released")
        };
        assert_eq!(SendOutcome::of(&result), SendOutcome::NoAck);
        assert_eq!(
            session.consecutive_no_ack(),
            0,
            "a NoAck from before the rescan does not count against the new channel"
        );
        assert_eq!(session.state(), PeerLinkState::Linked { channel: 11 });
        assert_eq!(link.scans_requested(), 2);
    }

    #[test]
    fn run_loop_wakes_on_a_threshold_no_ack() {
        let link = link();
        let clock = FakeClock::new();
        let session = linked(&link, &clock, 2);
        let (wakes, waker) = counting();
        let mut run = pin!(session.run());
        boot(run.as_mut(), &session, &waker);
        assert_eq!(wakes.0.load(Ordering::SeqCst), 0, "parked");

        no_ack(&link, &session);
        assert_eq!(wakes.0.load(Ordering::SeqCst), 1, "an accepted event wakes");
        assert!(!session.scanning());

        link.script_scan(Ok(ScanResult { channel: 3 }));
        no_ack(&link, &session);
        assert!(session.scanning());
        assert_eq!(session.state(), PeerLinkState::Lost { last_channel: 6 });
        assert_eq!(link.scans_requested(), 1, "the loop has not run yet");

        assert!(poll_with(run.as_mut(), &waker).is_pending());
        assert_eq!(link.scans_requested(), 2);
        assert_eq!(session.state(), PeerLinkState::Linked { channel: 3 });
        assert_eq!(session.rescans(), 1);
        assert!(!session.scanning());
    }

    #[test]
    fn peer_heard_during_backoff_cancels_the_retry() {
        let link = link();
        let clock = FakeClock::new();
        let session = linked(&link, &clock, 2);
        let mut run = pin!(session.run());
        boot(run.as_mut(), &session, Waker::noop());
        link.script_scan(Err(MockLinkError::Local));
        no_ack(&link, &session);
        no_ack(&link, &session);
        clock.set(1_000);
        assert!(poll_once(run.as_mut()).is_pending());
        assert_eq!(session.state(), PeerLinkState::Lost { last_channel: 6 });
        assert_eq!(link.scans_requested(), 2);

        link.queue_rx_event(EspNowEvent::new(COORDINATOR, b"pong"));
        block_on(session.receive());
        assert_eq!(session.state(), PeerLinkState::Linked { channel: 6 });

        clock.set(90_000);
        assert!(poll_once(run.as_mut()).is_pending());
        assert_eq!(link.scans_requested(), 2, "the retry was cancelled");
    }

    #[test]
    fn dropped_run_mid_scan_is_resumed_by_the_next_run() {
        let link = link();
        let clock = FakeClock::new();
        let session = session(&link, &clock, 3);
        link.set_hold_scans(true);
        link.script_scan(Ok(ScanResult { channel: 6 }));
        {
            let mut run = pin!(session.run());
            assert!(poll_once(run.as_mut()).is_pending());
            assert_eq!(link.scans_requested(), 1);
            assert!(session.scanning(), "the boot scan is in flight");
            let result = block_on(session.send(b"tick", T));
            assert!(matches!(result, Err(PeerSendError::Scanning)));
        }
        assert!(
            session.scanning(),
            "the core still counts the scan as outstanding"
        );
        link.set_hold_scans(false);

        let mut run = pin!(session.run());
        assert!(poll_once(run.as_mut()).is_pending());
        assert_eq!(
            link.scans_requested(),
            2,
            "the next run issues the scan again"
        );
        assert_eq!(session.state(), PeerLinkState::Linked { channel: 6 });
        assert!(!session.scanning());
        assert!(block_on(session.send(b"tick", T)).is_ok());
        assert_eq!(link.sent_messages().len(), 1);
    }

    #[test]
    fn failed_scan_after_peer_heard_relinks_without_retry() {
        let link = link();
        let clock = FakeClock::new();
        let session = linked(&link, &clock, 2);
        let mut run = pin!(session.run());
        boot(run.as_mut(), &session, Waker::noop());
        link.set_hold_scans(true);
        link.script_scan(Err(MockLinkError::Local));
        no_ack(&link, &session);
        no_ack(&link, &session);
        assert!(poll_once(run.as_mut()).is_pending());
        assert!(session.scanning(), "the rescan is in flight");

        link.queue_rx_event(EspNowEvent::new(COORDINATOR, b"pong"));
        block_on(session.receive());
        link.set_hold_scans(false);
        assert!(poll_once(run.as_mut()).is_pending());
        assert_eq!(session.state(), PeerLinkState::Linked { channel: 6 });
        assert_eq!(link.scans_requested(), 2);

        clock.set(90_000);
        assert!(poll_once(run.as_mut()).is_pending());
        assert_eq!(link.scans_requested(), 2, "no retry was scheduled");
    }

    #[test]
    fn send_error_classifies_and_displays() {
        assert_eq!(
            PeerSendError::<MockLinkError>::Scanning.outcome(),
            SendOutcome::Local
        );
        assert_eq!(
            PeerSendError::Link(MockLinkError::NoAck).outcome(),
            SendOutcome::NoAck
        );
        let text = alloc::format!("{}", PeerSendError::<MockLinkError>::Scanning);
        assert!(text.contains("scan"));
        let text = alloc::format!("{}", PeerSendError::Link(MockLinkError::NoAck));
        assert!(text.contains("acknowledged"));
    }

    #[test]
    fn clock_is_implemented_for_references() {
        fn now<C: Clock>(clock: C) -> u64 {
            clock.now_ms()
        }
        let clock = FakeClock::new();
        clock.set(42);
        assert_eq!(now(&clock), 42);
    }
}
