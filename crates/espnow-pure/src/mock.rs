//! [`MockEspNowDriver`] — a test double for host-side unit tests.
//!
//! Enable with `features = ["mock"]` in `dev-dependencies`, or it is automatically
//! available inside `#[cfg(test)]` blocks within this crate.
//!
//! # Usage in a downstream crate
//!
//! ```toml
//! [dev-dependencies]
//! espnow-pure = { workspace = true, features = ["mock"] }
//! ```
//!
//! ```rust,ignore
//! use espnow_pure::mock::MockEspNowDriver;
//! use espnow_pure::{EspNowDriver, EspNowEvent, PeerConfig};
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

use crate::{EspNowDriver, EspNowEvent, MacAddress, PeerConfig, WifiInterface};

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
    /// This is a storage knob only — `espnow-pure` itself contains no scanning
    /// logic (channel scanning is ESP-IDF-specific).  Downstream crates can
    /// read this back via [`scan_respond_channel`](Self::scan_respond_channel)
    /// to drive their own scan-dependent test logic; the mock's
    /// [`EspNowDriver`] methods do not consult this value.
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
