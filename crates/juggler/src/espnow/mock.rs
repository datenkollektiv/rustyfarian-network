//! [`MockEspNowDriver`] — a test double for host-side unit tests.
//!
//! Enable with `features = ["mock"]` in `dev-dependencies`, or it is automatically
//! available inside `#[cfg(test)]` blocks within this crate.
//!
//! # Usage in a downstream crate
//!
//! ```toml
//! [dev-dependencies]
//! juggler = { workspace = true, features = ["mock"] }
//! ```
//!
//! ```rust,ignore
//! use juggler::espnow::mock::MockEspNowDriver;
//! use juggler::espnow::{EspNowDriver, EspNowEvent, PeerConfig};
//!
//! let driver = MockEspNowDriver::new();
//! let config = PeerConfig::new([0x01, 0x02, 0x03, 0x04, 0x05, 0x06]);
//! driver.add_peer(&config).unwrap();
//! driver.send(&config.mac, b"hello").unwrap();
//! assert_eq!(driver.sent_count(), 1);
//! ```

extern crate alloc;

use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::cell::RefCell;

use core::time::Duration;

use super::{
    ClassifySend, EspNowDriver, EspNowEvent, EspNowLink, MacAddress, PeerConfig, ScanConfig,
    ScanResult, SendOutcome, WifiInterface,
};

// ─── Error ───────────────────────────────────────────────────────────────────

/// Error type for [`MockEspNowDriver`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockEspNowError {
    /// Returned when `fail_send` is set to `true`.
    SendFailed,
}

impl core::fmt::Display for MockEspNowError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "mock ESP-NOW send failed")
    }
}

// ─── Inner state ─────────────────────────────────────────────────────────────

struct MockState {
    peers: Vec<(MacAddress, WifiInterface)>,
    sent: Vec<(MacAddress, Vec<u8>)>,
    rx_queue: VecDeque<EspNowEvent>,
    fail_send: bool,
    scan_respond_on_channel: Option<u8>,
}

impl MockState {
    fn new() -> Self {
        Self {
            peers: Vec::new(),
            sent: Vec::new(),
            rx_queue: VecDeque::new(),
            fail_send: false,
            scan_respond_on_channel: None,
        }
    }
}

// ─── MockEspNowDriver ────────────────────────────────────────────────────────

/// Mock implementation of [`EspNowDriver`] for host-side unit tests.
///
/// Uses interior mutability so that `&self` methods can record state,
/// matching the interface contract of the real driver.
///
/// # Inspection helpers
///
/// All state is accessible via the inspection methods below.
/// Set `fail_send` with [`MockEspNowDriver::set_fail_send`] before calling
/// `send()` to simulate a send failure.
pub struct MockEspNowDriver {
    state: RefCell<MockState>,
}

impl MockEspNowDriver {
    /// Create a new mock driver with no peers, no sent messages, and an empty
    /// receive queue.
    pub fn new() -> Self {
        Self {
            state: RefCell::new(MockState::new()),
        }
    }

    /// When `true`, [`EspNowDriver::send`] returns `Err(MockEspNowError::SendFailed)`.
    pub fn set_fail_send(&self, fail: bool) {
        self.state.borrow_mut().fail_send = fail;
    }

    /// Enqueue a frame to be returned by the next call to [`EspNowDriver::try_recv`].
    pub fn queue_rx_event(&self, event: EspNowEvent) {
        self.state.borrow_mut().rx_queue.push_back(event);
    }

    /// Returns the number of successfully sent messages.
    pub fn sent_count(&self) -> usize {
        self.state.borrow().sent.len()
    }

    /// Copies the list of sent `(mac, data)` pairs for assertion in tests.
    pub fn sent_messages(&self) -> Vec<(MacAddress, Vec<u8>)> {
        self.state.borrow().sent.clone()
    }

    /// Copies the list of registered peers as `(mac, interface)` pairs.
    pub fn peer_list(&self) -> Vec<(MacAddress, WifiInterface)> {
        self.state.borrow().peers.clone()
    }

    /// Configure the mock to simulate a peer responding on the given channel.
    ///
    /// This is a storage knob only — `juggler`'s espnow module itself
    /// contains no scanning logic (channel scanning is ESP-IDF-specific).
    /// Downstream crates can read this back via
    /// [`scan_respond_channel`](Self::scan_respond_channel) to drive their own
    /// scan-dependent test logic; the mock's [`EspNowDriver`] methods do not
    /// consult this value.
    pub fn set_scan_respond_channel(&self, channel: u8) {
        self.state.borrow_mut().scan_respond_on_channel = Some(channel);
    }

    /// Returns the channel the mock is configured to respond on, or `None`.
    ///
    /// See [`set_scan_respond_channel`](Self::set_scan_respond_channel) — this
    /// is storage only and is not wired into any driver behaviour.
    pub fn scan_respond_channel(&self) -> Option<u8> {
        self.state.borrow().scan_respond_on_channel
    }
}

impl Default for MockEspNowDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl EspNowDriver for MockEspNowDriver {
    type Error = MockEspNowError;

    fn add_peer(&self, config: &PeerConfig) -> Result<(), Self::Error> {
        self.state
            .borrow_mut()
            .peers
            .push((config.mac, config.interface));
        Ok(())
    }

    fn remove_peer(&self, mac: &MacAddress) -> Result<(), Self::Error> {
        self.state.borrow_mut().peers.retain(|(m, _)| m != mac);
        Ok(())
    }

    fn send(&self, mac: &MacAddress, data: &[u8]) -> Result<(), Self::Error> {
        let mut state = self.state.borrow_mut();
        if state.fail_send {
            return Err(MockEspNowError::SendFailed);
        }
        state.sent.push((*mac, data.to_vec()));
        Ok(())
    }

    fn try_recv(&self) -> Option<EspNowEvent> {
        self.state.borrow_mut().rx_queue.pop_front()
    }
}

// ─── MockEspNowLink ──────────────────────────────────────────────────────────

/// Error type for [`MockEspNowLink`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockLinkError {
    /// The frame was transmitted but not acknowledged ([`SendOutcome::NoAck`]).
    NoAck,
    /// The frame was not transmitted, or the scan failed ([`SendOutcome::Local`]).
    Local,
}

impl core::fmt::Display for MockLinkError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoAck => write!(f, "mock ESP-NOW frame not acknowledged"),
            Self::Local => write!(f, "mock ESP-NOW local failure"),
        }
    }
}

impl ClassifySend for MockLinkError {
    fn outcome(&self) -> SendOutcome {
        match self {
            Self::NoAck => SendOutcome::NoAck,
            Self::Local => SendOutcome::Local,
        }
    }
}

struct LinkState {
    send_script: VecDeque<Result<(), MockLinkError>>,
    scan_script: VecDeque<Result<ScanResult, MockLinkError>>,
    last_timeout: Option<Duration>,
    pinned: Option<u8>,
    shut_down: bool,
    hold_sends: bool,
    hold_scans: bool,
    scans_requested: usize,
}

/// Mock implementation of [`EspNowLink`] (and [`EspNowDriver`]) for host-side tests.
///
/// Peers, sent frames and the receive queue live in an inner [`MockEspNowDriver`], so the
/// inspection helpers match it.
/// [`send_acked`](EspNowLink::send_acked) and [`scan_for_peer`](EspNowLink::scan_for_peer)
/// pop the next scripted result; with an empty script a send succeeds and a scan fails
/// with [`MockLinkError::Local`].
/// A frame is recorded as sent when its scripted result is `Ok` or
/// [`MockLinkError::NoAck`] (it was transmitted), not for [`MockLinkError::Local`].
/// A successful scan pins its channel; a failed one keeps the previous pin.
/// [`receive`](EspNowLink::receive) pends while the queue is empty, so tests only await it
/// after queueing a frame.
/// While [`set_hold_sends`](Self::set_hold_sends) is on, a `send_acked` takes its scripted
/// result at the call but pends until the hold is lifted, so a test can interleave other
/// work with a frame in flight.
/// While [`set_hold_scans`](Self::set_hold_scans) is on, a `scan_for_peer` is counted at the
/// call but pends until the hold is lifted and takes its scripted result only then, so a
/// scan dropped while held consumes nothing.
pub struct MockEspNowLink {
    driver: MockEspNowDriver,
    state: RefCell<LinkState>,
}

impl MockEspNowLink {
    /// Creates a mock with empty scripts, no peers and no pin.
    pub fn new() -> Self {
        Self {
            driver: MockEspNowDriver::new(),
            state: RefCell::new(LinkState {
                send_script: VecDeque::new(),
                scan_script: VecDeque::new(),
                last_timeout: None,
                pinned: None,
                shut_down: false,
                hold_sends: false,
                hold_scans: false,
                scans_requested: 0,
            }),
        }
    }

    /// Holds every [`scan_for_peer`](EspNowLink::scan_for_peer) pending until the hold is
    /// lifted.
    ///
    /// The call is counted at once; the scripted result is taken on the first poll after
    /// `set_hold_scans(false)`.
    pub fn set_hold_scans(&self, hold: bool) {
        self.state.borrow_mut().hold_scans = hold;
    }

    /// Holds every [`send_acked`](EspNowLink::send_acked) pending until the hold is lifted.
    ///
    /// The scripted result is taken when the call starts; the frame is recorded as sent and
    /// the result returned on the first poll after `set_hold_sends(false)`.
    pub fn set_hold_sends(&self, hold: bool) {
        self.state.borrow_mut().hold_sends = hold;
    }

    /// Number of [`scan_for_peer`](EspNowLink::scan_for_peer) calls so far.
    pub fn scans_requested(&self) -> usize {
        self.state.borrow().scans_requested
    }

    /// Queues the result of a future [`send_acked`](EspNowLink::send_acked) call.
    pub fn script_send(&self, result: Result<(), MockLinkError>) {
        self.state.borrow_mut().send_script.push_back(result);
    }

    /// Queues the result of a future [`scan_for_peer`](EspNowLink::scan_for_peer) call.
    pub fn script_scan(&self, result: Result<ScanResult, MockLinkError>) {
        self.state.borrow_mut().scan_script.push_back(result);
    }

    /// Enqueues a frame for [`receive`](EspNowLink::receive) and `try_recv`.
    pub fn queue_rx_event(&self, event: EspNowEvent) {
        self.driver.queue_rx_event(event);
    }

    /// Sets the pinned channel directly.
    pub fn set_pinned_channel(&self, channel: Option<u8>) {
        self.state.borrow_mut().pinned = channel;
    }

    /// Copies the transmitted `(mac, data)` pairs.
    pub fn sent_messages(&self) -> Vec<(MacAddress, Vec<u8>)> {
        self.driver.sent_messages()
    }

    /// Copies the registered peers as `(mac, interface)` pairs.
    pub fn peer_list(&self) -> Vec<(MacAddress, WifiInterface)> {
        self.driver.peer_list()
    }

    /// True once [`shutdown`](EspNowLink::shutdown) was called.
    pub fn is_shut_down(&self) -> bool {
        self.state.borrow().shut_down
    }

    /// The timeout passed to the latest [`send_acked`](EspNowLink::send_acked), if any.
    pub fn last_send_timeout(&self) -> Option<Duration> {
        self.state.borrow().last_timeout
    }
}

impl Default for MockEspNowLink {
    fn default() -> Self {
        Self::new()
    }
}

impl EspNowDriver for MockEspNowLink {
    type Error = MockLinkError;

    fn add_peer(&self, config: &PeerConfig) -> Result<(), Self::Error> {
        self.driver
            .add_peer(config)
            .map_err(|_| MockLinkError::Local)
    }

    fn remove_peer(&self, mac: &MacAddress) -> Result<(), Self::Error> {
        self.driver
            .remove_peer(mac)
            .map_err(|_| MockLinkError::Local)
    }

    fn send(&self, mac: &MacAddress, data: &[u8]) -> Result<(), Self::Error> {
        self.driver
            .send(mac, data)
            .map_err(|_| MockLinkError::Local)
    }

    fn try_recv(&self) -> Option<EspNowEvent> {
        self.driver.try_recv()
    }
}

impl EspNowLink for MockEspNowLink {
    async fn send_acked(
        &self,
        mac: &MacAddress,
        data: &[u8],
        timeout: Duration,
    ) -> Result<(), Self::Error> {
        let scripted = {
            let mut state = self.state.borrow_mut();
            state.last_timeout = Some(timeout);
            state.send_script.pop_front().unwrap_or(Ok(()))
        };
        core::future::poll_fn(|cx| {
            if self.state.borrow().hold_sends {
                cx.waker().wake_by_ref();
                core::task::Poll::Pending
            } else {
                core::task::Poll::Ready(())
            }
        })
        .await;
        if !matches!(scripted, Err(MockLinkError::Local)) {
            self.driver
                .send(mac, data)
                .map_err(|_| MockLinkError::Local)?;
        }
        scripted
    }

    async fn receive(&self) -> EspNowEvent {
        core::future::poll_fn(|cx| match self.driver.try_recv() {
            Some(event) => core::task::Poll::Ready(event),
            None => {
                cx.waker().wake_by_ref();
                core::task::Poll::Pending
            }
        })
        .await
    }

    async fn scan_for_peer(
        &self,
        _mac: &MacAddress,
        _config: &ScanConfig<'_>,
    ) -> Result<ScanResult, Self::Error> {
        self.state.borrow_mut().scans_requested += 1;
        core::future::poll_fn(|cx| {
            if self.state.borrow().hold_scans {
                cx.waker().wake_by_ref();
                core::task::Poll::Pending
            } else {
                core::task::Poll::Ready(())
            }
        })
        .await;
        let mut state = self.state.borrow_mut();
        let result = state
            .scan_script
            .pop_front()
            .unwrap_or(Err(MockLinkError::Local));
        if let Ok(found) = result {
            state.pinned = Some(found.channel);
        }
        result
    }

    fn pinned_channel(&self) -> Option<u8> {
        self.state.borrow().pinned
    }

    /// The mock has no radio: its live channel is its pin (see
    /// [`set_pinned_channel`](Self::set_pinned_channel)).
    fn current_channel(&self) -> Option<u8> {
        self.state.borrow().pinned
    }

    fn shutdown(&self) {
        self.state.borrow_mut().shut_down = true;
    }
}
