//! Platform-independent ESP-NOW types, traits, and validation.
//!
//! # Architecture
//!
//! - [`EspNowDriver`] — hardware-agnostic ESP-NOW driver interface
//! - [`EspNowEvent`] — received frame (fixed-size, no heap)
//! - [`PeerConfig`] — peer registration parameters
//! - [`mock::MockEspNowDriver`] — test double for host-side unit tests
//!   (requires the `mock` feature or `#[cfg(test)]`)
//!
//! # Feature flags
//!
//! | Feature | What it enables                                              |
//! |:--------|:-------------------------------------------------------------|
//! | `mock`  | `MockEspNowDriver` for downstream host-side tests            |

#![no_std]

#[cfg(any(test, feature = "mock"))]
pub mod mock;

// ─── Constants ──────────────────────────────────────────────────────────────

/// Maximum ESP-NOW payload length in bytes.
pub const MAX_DATA_LEN: usize = 250;

/// Default capacity for an ESP-NOW receive channel.
pub const DEFAULT_RX_CHANNEL_CAPACITY: usize = 32;

// ─── Types ───────────────────────────────────────────────────────────────────

/// A 6-byte IEEE 802.11 MAC address.
pub type MacAddress = [u8; 6];

/// Broadcast MAC address — addressed frames are delivered to all peers.
pub const BROADCAST_MAC: MacAddress = [0xFF; 6];

// ─── WifiInterface ──────────────────────────────────────────────────────────

/// Wi-Fi interface used for ESP-NOW peer communication.
///
/// - [`Sta`](WifiInterface::Sta) — station interface; the standard choice for
///   ESP-NOW, including devices that start Wi-Fi without connecting to an AP.
/// - [`Ap`](WifiInterface::Ap) — soft-AP interface; needed only when the device
///   runs its own access point and routes ESP-NOW frames through it.
///
/// Maps to `wifi_interface_t` on ESP-IDF; platform-independent crates use
/// this enum so they remain free of ESP-IDF types.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum WifiInterface {
    /// Station interface (default).
    #[default]
    Sta,
    /// Soft-AP interface.
    Ap,
}

// ─── Validation ─────────────────────────────────────────────────────────────

/// Returns `Ok(())` if `data` fits within the ESP-NOW payload limit, or an
/// error message suitable for wrapping with `anyhow::anyhow!`.
pub fn validate_payload(data: &[u8]) -> Result<(), &'static str> {
    if data.len() <= MAX_DATA_LEN {
        Ok(())
    } else {
        Err("ESP-NOW payload exceeds maximum length of 250 bytes")
    }
}

// ─── EspNowEvent ────────────────────────────────────────────────────────────

/// A received ESP-NOW frame.
///
/// Uses a fixed-size inline buffer to avoid heap allocation.
/// Access the actual payload with [`EspNowEvent::payload`].
#[derive(Debug, Clone)]
pub struct EspNowEvent {
    /// MAC address of the sender.
    pub mac: MacAddress,
    data: [u8; MAX_DATA_LEN],
    len: usize,
}

impl EspNowEvent {
    /// Create a new event from a sender MAC address and payload slice.
    ///
    /// Panics in debug builds if `data.len() > MAX_DATA_LEN`; truncates
    /// silently in release builds. Callers should validate with
    /// [`validate_payload`] before constructing.
    pub fn new(mac: MacAddress, data: &[u8]) -> Self {
        let len = data.len().min(MAX_DATA_LEN);
        let mut buf = [0u8; MAX_DATA_LEN];
        buf[..len].copy_from_slice(&data[..len]);
        Self {
            mac,
            data: buf,
            len,
        }
    }

    /// Returns the payload bytes that were received.
    pub fn payload(&self) -> &[u8] {
        &self.data[..self.len]
    }
}

// ─── PeerConfig ─────────────────────────────────────────────────────────────

/// Configuration for registering an ESP-NOW peer.
#[derive(Debug, Clone)]
pub struct PeerConfig {
    /// MAC address of the peer.
    pub mac: MacAddress,
    /// Wi-Fi channel on which to reach the peer (0 = current channel).
    pub channel: u8,
    /// Whether link-layer encryption is enabled for this peer.
    pub encrypt: bool,
    /// Wi-Fi interface for this peer (default: [`WifiInterface::Sta`]).
    pub interface: WifiInterface,
}

impl PeerConfig {
    /// Create a new peer configuration with default settings.
    ///
    /// Defaults: `channel = 0` (current channel), `encrypt = false`,
    /// `interface = WifiInterface::Sta`.
    pub fn new(mac: MacAddress) -> Self {
        Self {
            mac,
            channel: 0,
            encrypt: false,
            interface: WifiInterface::Sta,
        }
    }
}

// ─── EspNowDriver trait ──────────────────────────────────────────────────────

/// Hardware-agnostic ESP-NOW driver interface.
///
/// Methods take `&self` because the underlying FFI calls are thread-safe and
/// the driver state is managed through interior mutability.
///
/// # Implementors
///
/// - `rustyfarian_esp_idf_espnow::EspIdfEspNow` — ESP-IDF driver
/// - [`mock::MockEspNowDriver`] — test double (behind `mock` feature / `#[cfg(test)]`)
pub trait EspNowDriver {
    /// Driver-specific error type.
    type Error: core::fmt::Debug;

    /// Register a peer so that frames can be sent to its MAC address.
    fn add_peer(&self, config: &PeerConfig) -> Result<(), Self::Error>;

    /// Deregister a previously registered peer.
    fn remove_peer(&self, mac: &MacAddress) -> Result<(), Self::Error>;

    /// Send `data` to the peer identified by `mac`.
    ///
    /// `mac` must have been registered with [`EspNowDriver::add_peer`] first,
    /// or be the broadcast address [`BROADCAST_MAC`].
    fn send(&self, mac: &MacAddress, data: &[u8]) -> Result<(), Self::Error>;

    /// Non-blocking receive: returns the next queued [`EspNowEvent`], or
    /// `None` if the queue is empty.
    fn try_recv(&self) -> Option<EspNowEvent>;
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
extern crate alloc;

#[cfg(test)]
mod tests {
    use super::*;

    // ── Validation tests ────────────────────────────────────────────────

    #[test]
    fn validate_payload_empty_ok() {
        assert!(validate_payload(&[]).is_ok());
    }

    #[test]
    fn validate_payload_within_limit_ok() {
        let data = [0u8; MAX_DATA_LEN];
        assert!(validate_payload(&data).is_ok());
    }

    #[test]
    fn validate_payload_over_limit_rejected() {
        let data = [0u8; MAX_DATA_LEN + 1];
        assert!(validate_payload(&data).is_err());
    }

    // ── EspNowEvent tests ────────────────────────────────────────────────

    #[test]
    fn espnow_event_payload_returns_correct_slice() {
        let mac = [0x11, 0x22, 0x33, 0x44, 0x55, 0x66];
        let data = b"hello";
        let event = EspNowEvent::new(mac, data);
        assert_eq!(event.payload(), b"hello");
        assert_eq!(event.mac, mac);
    }

    // ── PeerConfig tests ─────────────────────────────────────────────────

    #[test]
    fn peer_config_defaults() {
        let mac = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06];
        let config = PeerConfig::new(mac);
        assert_eq!(config.mac, mac);
        assert_eq!(config.channel, 0);
        assert!(!config.encrypt);
        assert_eq!(config.interface, WifiInterface::Sta);
    }

    #[test]
    fn peer_config_with_ap_interface() {
        let mac = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06];
        let config = PeerConfig {
            interface: WifiInterface::Ap,
            ..PeerConfig::new(mac)
        };
        assert_eq!(config.interface, WifiInterface::Ap);
        assert_eq!(config.mac, mac);
    }

    #[test]
    fn wifi_interface_default_is_sta() {
        assert_eq!(WifiInterface::default(), WifiInterface::Sta);
    }

    // ── Constant tests ───────────────────────────────────────────────────

    #[test]
    fn broadcast_mac_is_all_ff() {
        assert_eq!(BROADCAST_MAC, [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
    }

    // ── MockEspNowDriver tests ───────────────────────────────────────────

    #[test]
    fn mock_send_records_message() {
        let driver = mock::MockEspNowDriver::new();
        let mac = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];
        driver.send(&mac, b"ping").unwrap();
        let sent = driver.sent_messages();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].0, mac);
        assert_eq!(sent[0].1, b"ping");
    }

    #[test]
    fn mock_send_failure() {
        let driver = mock::MockEspNowDriver::new();
        driver.set_fail_send(true);
        let mac = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06];
        assert!(driver.send(&mac, b"data").is_err());
    }

    #[test]
    fn mock_recv_fifo_order() {
        let driver = mock::MockEspNowDriver::new();
        let mac = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06];
        driver.queue_rx_event(EspNowEvent::new(mac, b"first"));
        driver.queue_rx_event(EspNowEvent::new(mac, b"second"));
        assert_eq!(driver.try_recv().unwrap().payload(), b"first");
        assert_eq!(driver.try_recv().unwrap().payload(), b"second");
        assert!(driver.try_recv().is_none());
    }

    #[test]
    fn mock_add_remove_peer() {
        let driver = mock::MockEspNowDriver::new();
        let mac = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06];
        let config = PeerConfig::new(mac);
        driver.add_peer(&config).unwrap();
        let peers = driver.peer_list();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0], (mac, WifiInterface::Sta));
        driver.remove_peer(&mac).unwrap();
        assert!(driver.peer_list().is_empty());
    }

    #[test]
    fn mock_add_peer_with_ap_interface() {
        let driver = mock::MockEspNowDriver::new();
        let mac = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06];
        let config = PeerConfig {
            interface: WifiInterface::Ap,
            ..PeerConfig::new(mac)
        };
        driver.add_peer(&config).unwrap();
        let peers = driver.peer_list();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0], (mac, WifiInterface::Ap));
    }
}
