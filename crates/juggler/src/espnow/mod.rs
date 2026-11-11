//! Platform-independent ESP-NOW types, traits, and validation.
//!
//! # Architecture
//!
//! - [`EspNowDriver`] — hardware-agnostic ESP-NOW driver interface
//! - [`EspNowEvent`] — received frame (fixed-size, no heap)
//! - [`PeerConfig`] — peer registration parameters
//! - [`PeerTracker`] — heartbeat-based peer liveness tracking
//! - [`PeerLink`] — clock-injected supervisor that decides when to (re)scan for the coordinator
//! - [`PeerSession`] — the async helper that drives [`PeerLink`] over an [`EspNowLink`] on either
//!   tier, with its [`Clock`] port (`EmbassyClock` behind the `embassy` feature)
//! - [`EspNowLink`] — async acked send, receive and scan surface both tiers implement
//! - [`ClassifySend`] — maps a tier send error onto [`SendOutcome`]
//! - [`OversizeLog`] — counter and rate-limited log decision for dropped oversize frames
//! - [`TxCore`] — sans-IO send queue: request ids, deadlines, cancellation, scan exclusion
//! - [`ScanJobSlot`], [`InFlightMarker`] and `DeadlineSet` (`std`) — the sans-IO parts of a worker-thread driver:
//!   the scan job hand-off, the awaited send callback and the caller deadlines
//! - [`ScanMachine`] — clock-injected `scan_for_peer` state machine shared by both tiers, with its
//!   borrow-free [`ScanTeardown`]
//! - [`ScanGate`] — the scan-exclusion lifecycle on a [`TxCore`]: admission, cancellation,
//!   the teardown deferred behind a probe on the air, release
//! - `EspNowWorker` (`std`) — the thread-based transmit worker of the ESP-IDF tier behind the narrow
//!   `EspNowRadio` port, the ESP-NOW counterpart of `wifi::LinkSupervisor`
//! - [`mock::MockEspNowDriver`], [`mock::MockEspNowLink`] — test doubles for host-side unit tests
//!   (requires the `mock` feature or `#[cfg(test)]`)
//!
//! # Feature flags
//!
//! | Feature   | What it enables                                              |
//! |:----------|:-------------------------------------------------------------|
//! | `mock`    | `MockEspNowDriver` for downstream host-side tests            |
//! | `embassy` | `EmbassyClock`, the `embassy-time` clock for `PeerSession`   |

pub mod callback;
pub mod command;
#[cfg(any(test, feature = "std"))]
pub mod deadlines;
pub mod link;
pub mod link_trait;
#[cfg(any(test, feature = "mock"))]
pub mod mock;
pub mod scan;
pub mod scan_job;
pub mod session;
pub mod tracker;
pub mod tx;
#[cfg(feature = "std")]
pub mod worker;

pub use callback::InFlightMarker;
pub use command::{
    parse_frame, parse_system_command, CommandFrame, ProbeFrame, ProbeReply, SystemCommand,
    TAG_IDENTIFY, TAG_PING, TAG_PROBE, TAG_PROBE_REPLY, TAG_SELF_TEST,
};
#[cfg(any(test, feature = "std"))]
pub use deadlines::{DeadlineFull, DeadlineId, DeadlineSet, DeadlineStorage};
pub use link::{
    PeerLink, PeerLinkAction, PeerLinkConfig, PeerLinkEpoch, PeerLinkEvent, PeerLinkState,
};
pub use link_trait::{ClassifySend, EspNowLink};
pub use scan::{restore_matches, warn_on_restore_mismatch};
pub use scan::{
    ScanAction, ScanError, ScanFault, ScanInput, ScanMachine, ScanTeardown, SendOutcome,
};
pub use scan_job::{CancelOutcome, Cancelled, GateRefusal, JobEpoch, ScanGate, ScanJobSlot};
#[cfg(feature = "embassy")]
pub use session::EmbassyClock;
pub use session::{Clock, PeerSendError, PeerSession};
pub use tracker::PeerTracker;
#[allow(deprecated)]
pub use tx::DeferredRestore;
pub use tx::{
    Abandon, Completed, Completion, Next, Refusal, ScanRefusal, SendKind, TxCore, TxId,
    PROBE_RESERVED_SLOTS,
};
#[cfg(feature = "std")]
pub use worker::{
    EspNowRadio, EspNowWorker, SendEvents, TxOutcome, WorkerConfig, WorkerError, TX_SLOTS,
};

// ─── Constants ──────────────────────────────────────────────────────────────

/// Maximum ESP-NOW payload length in bytes.
pub const MAX_DATA_LEN: usize = 250;

/// Default capacity for an ESP-NOW receive channel.
pub const DEFAULT_RX_CHANNEL_CAPACITY: usize = 32;

/// Default channels to scan when probing for a peer.
///
/// Covers channels 1-13 (EU/JP region).  US/CA regulatory domains restrict
/// 2.4 GHz to channels 1-11; if your peer might land on a channel that is not
/// allowed in your region the radio will silently fail to send on it.  Use
/// [`ScanConfig::with_channels`] with a region-appropriate slice when this
/// matters (e.g. `&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]` for US).
pub const DEFAULT_SCAN_CHANNELS: [u8; 13] = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13];

/// Default per-channel probe timeout when scanning for a peer.
///
/// MAC-layer ACKs typically arrive within ~5 ms; 100 ms leaves margin for
/// retries and radio contention.  Override with
/// [`ScanConfig::with_probe_timeout`].
pub const DEFAULT_PROBE_TIMEOUT: core::time::Duration = core::time::Duration::from_millis(100);

/// Default total burst timeout for a peer scan.
///
/// Caps the wall-clock time the radio spends at boosted TX power during a
/// scan, regardless of how many channels remain.  At default settings
/// (13 channels × 100 ms probe) the loop already finishes in ~1.3 s; this
/// only kicks in for long custom probe timeouts or large channel lists.
/// Override with [`ScanConfig::with_burst_timeout`].
pub const DEFAULT_BURST_TIMEOUT: core::time::Duration = core::time::Duration::from_secs(3);

/// Default number of *additional* confirmation probes required after the first
/// ACK on a channel before that channel is declared the peer's home channel.
///
/// One ACK alone is not a reliable signal: a peer whose Wi-Fi STA is in the
/// middle of an AP roam will briefly visit each channel during its own scan
/// sequence and its 802.11 hardware ACKs unicast frames addressed to its MAC
/// even while not associated.  A scan that returns the first transient ACK
/// captures a channel the peer is merely passing through, then every
/// subsequent send goes to the wrong channel.
///
/// Requiring an additional probe spaced [`DEFAULT_CONFIRMATION_GAP`] apart
/// after the first ACK forces the scan to verify the peer has actually
/// settled — its radio cannot remain on the same channel for that long
/// during a roam scan (observed active dwell is roughly 100 ms per channel
/// on common ESP-IDF / vendor stacks).
///
/// Default `1` = one extra confirmation = two total ACKs required.
/// Override with [`ScanConfig::with_probe_confirmations`].
pub const DEFAULT_PROBE_CONFIRMATIONS: u8 = 1;

/// Default gap between the first ACK and the next confirmation probe on the
/// same channel.
///
/// Sized to exceed a typical 802.11 active-scan dwell (roughly 100 ms in
/// observed deployments) so a roaming peer that briefly visits a channel
/// cannot ACK both probes.  Lower values reduce scan latency but increase
/// the risk of false-positive channel detection during roams.
/// Override with [`ScanConfig::with_confirmation_gap`].
pub const DEFAULT_CONFIRMATION_GAP: core::time::Duration = core::time::Duration::from_millis(150);

/// Default wait for the peer's [`ProbeReply`] after a probe was ACKed.
///
/// A coordinator running the shared echo app answers a probe within a few
/// milliseconds; the wait only has to outlast that.
/// It starts at the ACK, so it adds to [`DEFAULT_PROBE_TIMEOUT`] on each
/// channel that ACKs and nothing on the others.
/// Override with [`ScanConfig::with_reply_timeout`].
pub const DEFAULT_REPLY_TIMEOUT: core::time::Duration = core::time::Duration::from_millis(50);

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

    /// Create an event, or return the payload length if it exceeds [`MAX_DATA_LEN`].
    ///
    /// Never truncates: the caller decides what to do with an oversize frame, normally drop it
    /// and count it with [`OversizeLog`].
    ///
    /// ```
    /// use juggler::espnow::{EspNowEvent, MAX_DATA_LEN};
    ///
    /// assert!(EspNowEvent::try_new([0; 6], b"ok").is_ok());
    /// assert_eq!(EspNowEvent::try_new([0; 6], &[0; 1470]).unwrap_err(), 1470);
    /// ```
    pub fn try_new(mac: MacAddress, data: &[u8]) -> Result<Self, usize> {
        if data.len() > MAX_DATA_LEN {
            Err(data.len())
        } else {
            Ok(Self::new(mac, data))
        }
    }

    /// Returns the payload bytes that were received.
    pub fn payload(&self) -> &[u8] {
        &self.data[..self.len]
    }
}

// ─── OversizeLog ────────────────────────────────────────────────────────────

/// Every n-th oversize frame is logged after the first one.
const OVERSIZE_LOG_EVERY: u32 = 32;

/// Counts dropped oversize frames and decides which drops are worth a log line.
///
/// Receive paths call [`record`](Self::record) for every frame [`EspNowEvent::try_new`]
/// refused; it returns `true` for the first drop and then every 32nd, so a noisy neighbour
/// cannot flood the log.
/// The counter is advisory: it uses load and store only (no read-modify-write, which
/// `riscv32imc` lacks), so concurrent recorders may lose a count.
#[derive(Debug, Default)]
pub struct OversizeLog {
    dropped: core::sync::atomic::AtomicU32,
}

impl OversizeLog {
    /// Creates a log with nothing dropped.
    pub const fn new() -> Self {
        Self {
            dropped: core::sync::atomic::AtomicU32::new(0),
        }
    }

    /// Records one dropped frame; `true` if the drop should be logged.
    pub fn record(&self) -> bool {
        use core::sync::atomic::Ordering::Relaxed;
        let dropped = self.dropped.load(Relaxed).wrapping_add(1);
        self.dropped.store(dropped, Relaxed);
        Self::should_log(dropped)
    }

    /// Number of oversize frames dropped so far (wraps at `u32::MAX`).
    pub fn dropped(&self) -> u32 {
        self.dropped.load(core::sync::atomic::Ordering::Relaxed)
    }

    /// Rate limit: the first drop and then every 32nd.
    pub fn should_log(dropped_so_far: u32) -> bool {
        dropped_so_far
            .wrapping_sub(1)
            .is_multiple_of(OVERSIZE_LOG_EVERY)
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

    /// Sets the Wi-Fi interface to [`WifiInterface::Ap`].
    ///
    /// Only useful when the device is running in `WIFI_MODE_AP` or
    /// `WIFI_MODE_APSTA`.  Devices using `init_with_radio()` start in
    /// STA mode and should use [`WifiInterface::Sta`] (the default).
    pub fn with_ap_interface(mut self) -> Self {
        self.interface = WifiInterface::Ap;
        self
    }
}

// ─── ScanConfig ─────────────────────────────────────────────────────────────

/// Configuration for ESP-NOW channel scanning.
///
/// Controls which channels to probe, how long to wait for the MAC-layer ACK
/// on each channel, and how long to wait for the peer's [`ProbeReply`] after
/// an ACK.
/// The scan sends the [`ProbeFrame`] (`0xF3`) as its probe; the peer ACKs at
/// the MAC layer regardless of payload content, and a peer running the echo
/// app answers with its home channel (ADR 025).
#[derive(Debug, Clone, Copy)]
pub struct ScanConfig<'a> {
    /// Channels to scan, in order (default: [`DEFAULT_SCAN_CHANNELS`]).
    pub channels: &'a [u8],
    /// Ignored: the scan sends the [`ProbeFrame`] it builds itself.
    ///
    /// Kept for one release; build the config with [`ScanConfig::for_peer`].
    #[deprecated(
        since = "0.7.0",
        note = "the scan sends the 0xF3 probe frame itself; build the config with `ScanConfig::for_peer()`"
    )]
    pub probe_data: &'a [u8],
    /// Per-channel probe timeout (default: [`DEFAULT_PROBE_TIMEOUT`] = 100 ms).
    ///
    /// On both tiers it bounds queueing and the start of a probe: a probe that did not start in
    /// time is never sent and counts as a local failure.
    /// A probe that has started is waited out (fail-closed), and its real MAC-layer result is
    /// used.
    pub probe_timeout: core::time::Duration,
    /// Total burst timeout (default: [`DEFAULT_BURST_TIMEOUT`] = 3 s).
    ///
    /// The scan loop checks elapsed time before each channel and stops
    /// early once this limit is reached.  Bounds the time the radio
    /// spends at boosted TX power during peer discovery.
    pub burst_timeout: core::time::Duration,
    /// Number of additional ACK probes required to confirm a channel after
    /// the first probe ACKs (default: [`DEFAULT_PROBE_CONFIRMATIONS`] = 1).
    ///
    /// Set to `0` to declare success on the first ACK (legacy behaviour, fast
    /// but vulnerable to false positives when the peer is mid-roam).
    /// Set to `1` or higher to require confirmation probes; the channel is
    /// only declared found if all confirmations also ACK.
    pub probe_confirmations: u8,
    /// Gap between consecutive probes on the same channel during confirmation
    /// (default: [`DEFAULT_CONFIRMATION_GAP`] = 150 ms).
    ///
    /// Must exceed a typical 802.11 active-scan dwell time (observed at
    /// roughly 100 ms across common ESP-IDF / vendor stacks) for the
    /// confirmation to reliably reject a roaming peer's transient ACK.
    pub confirmation_gap: core::time::Duration,
    /// Wait for the peer's [`ProbeReply`] after a probe was ACKed
    /// (default: [`DEFAULT_REPLY_TIMEOUT`] = 50 ms).
    ///
    /// A reply that names the peer's home channel ends the scan on that
    /// channel; an ACK without a reply within this wait makes the channel a
    /// candidate that is settled after the channel list with the confirmation
    /// rule above.
    pub reply_timeout: core::time::Duration,
}

impl<'a> ScanConfig<'a> {
    /// Create a scan config with default channels (1-13) and the default
    /// timeouts and confirmation settings.
    ///
    /// The scan builds its own probe frames; there is no payload to pass.
    pub fn for_peer() -> Self {
        #[allow(deprecated)]
        Self {
            channels: &DEFAULT_SCAN_CHANNELS,
            probe_data: &[],
            probe_timeout: DEFAULT_PROBE_TIMEOUT,
            burst_timeout: DEFAULT_BURST_TIMEOUT,
            probe_confirmations: DEFAULT_PROBE_CONFIRMATIONS,
            confirmation_gap: DEFAULT_CONFIRMATION_GAP,
            reply_timeout: DEFAULT_REPLY_TIMEOUT,
        }
    }

    /// Create a scan config with default channels (1-13), the given probe
    /// payload, and the default timeouts and confirmation settings.
    ///
    /// `probe_data` is ignored: the scan sends the [`ProbeFrame`] it builds
    /// itself.
    #[deprecated(
        since = "0.7.0",
        note = "the scan sends the 0xF3 probe frame itself; use `ScanConfig::for_peer()`"
    )]
    pub fn new(probe_data: &'a [u8]) -> Self {
        #[allow(deprecated)]
        Self {
            probe_data,
            ..Self::for_peer()
        }
    }

    /// Override the channel list to scan.
    pub fn with_channels(mut self, channels: &'a [u8]) -> Self {
        self.channels = channels;
        self
    }

    /// Override the per-channel probe timeout.
    pub fn with_probe_timeout(mut self, timeout: core::time::Duration) -> Self {
        self.probe_timeout = timeout;
        self
    }

    /// Override the total burst timeout.
    ///
    /// The scan loop stops early once this elapsed time is reached, even
    /// if channels remain unprobed.  Use this to cap how long the radio
    /// runs at boosted TX power if discovery fails.
    pub fn with_burst_timeout(mut self, timeout: core::time::Duration) -> Self {
        self.burst_timeout = timeout;
        self
    }

    /// Override the number of confirmation probes required after the first
    /// ACK on a channel.  `0` disables confirmation (first-ACK-wins).
    pub fn with_probe_confirmations(mut self, count: u8) -> Self {
        self.probe_confirmations = count;
        self
    }

    /// Override the gap between confirmation probes on the same channel.
    ///
    /// Values below the typical 802.11 active-scan dwell time may reintroduce
    /// false-positive channel detection when the peer is mid-roam — see
    /// [`DEFAULT_CONFIRMATION_GAP`] for the rationale behind the default.
    /// Setting [`ScanConfig::probe_confirmations`] to a non-zero count while
    /// keeping the gap very small (or zero) defeats the confirmation gating
    /// and reduces it to a doubled-up first-ACK-wins scan.
    pub fn with_confirmation_gap(mut self, gap: core::time::Duration) -> Self {
        self.confirmation_gap = gap;
        self
    }

    /// Override the wait for the peer's [`ProbeReply`] after an ACK.
    ///
    /// The wait starts at the ACK.
    /// Too short a value turns a replying peer into an ACK-only candidate,
    /// which is settled with the confirmation rule after the channel list, so
    /// an adjacent channel may win as before the reply existed.
    pub fn with_reply_timeout(mut self, timeout: core::time::Duration) -> Self {
        self.reply_timeout = timeout;
        self
    }
}

// ─── ScanResult ─────────────────────────────────────────────────────────────

/// Result of a successful channel scan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanResult {
    /// The Wi-Fi channel on which the peer responded.
    pub channel: u8,
}

// ─── EspNowDriver trait ──────────────────────────────────────────────────────

/// Hardware-agnostic ESP-NOW driver interface.
///
/// Methods take `&self` because the underlying FFI calls are thread-safe and
/// the driver state is managed through interior mutability.
///
/// # Implementors
///
/// - `rustyfarian_esp_idf_network::espnow::EspIdfEspNow` — ESP-IDF driver
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

    #[test]
    fn try_new_keeps_payload_up_to_the_limit() {
        let data = [0xA5u8; MAX_DATA_LEN];
        let event = EspNowEvent::try_new([1; 6], &data).unwrap();
        assert_eq!(event.mac, [1; 6]);
        assert_eq!(event.payload(), &data[..]);
        assert_eq!(EspNowEvent::try_new([1; 6], &[]).unwrap().payload(), b"");
    }

    #[test]
    fn try_new_reports_the_oversize_length_without_truncating() {
        assert_eq!(
            EspNowEvent::try_new([1; 6], &[0; MAX_DATA_LEN + 1]).unwrap_err(),
            MAX_DATA_LEN + 1
        );
        assert_eq!(EspNowEvent::try_new([1; 6], &[0; 1470]).unwrap_err(), 1470);
    }

    #[test]
    fn oversize_log_rate_limits_to_first_then_every_32nd() {
        assert!(OversizeLog::should_log(1));
        assert!(!OversizeLog::should_log(2));
        assert!(!OversizeLog::should_log(32));
        assert!(OversizeLog::should_log(33));
        assert!(OversizeLog::should_log(65));
        let logged = (1..=1000u32)
            .filter(|n| OversizeLog::should_log(*n))
            .count();
        assert_eq!(logged, 32);
    }

    #[test]
    fn oversize_log_counts_every_drop_and_logs_selectively() {
        let log = OversizeLog::new();
        assert_eq!(log.dropped(), 0);
        let decisions: alloc::vec::Vec<bool> = (0..34).map(|_| log.record()).collect();
        assert_eq!(log.dropped(), 34);
        assert!(decisions[0]);
        assert!(decisions[1..32].iter().all(|d| !d));
        assert!(decisions[32]);
        assert!(!decisions[33]);
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
        let config = PeerConfig::new(mac).with_ap_interface();
        assert_eq!(config.interface, WifiInterface::Ap);
        assert_eq!(config.mac, mac);
        assert_eq!(config.channel, 0);
        assert!(!config.encrypt);
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

    // ── ScanConfig / ScanResult tests ─────────────────────────────────────

    #[test]
    fn scan_config_default_channels() {
        let config = ScanConfig::for_peer();
        assert_eq!(config.channels, &DEFAULT_SCAN_CHANNELS);
        assert_eq!(config.probe_timeout, DEFAULT_PROBE_TIMEOUT);
        assert_eq!(config.burst_timeout, DEFAULT_BURST_TIMEOUT);
        assert_eq!(config.probe_confirmations, DEFAULT_PROBE_CONFIRMATIONS);
        assert_eq!(config.confirmation_gap, DEFAULT_CONFIRMATION_GAP);
        assert_eq!(config.reply_timeout, DEFAULT_REPLY_TIMEOUT);
    }

    #[test]
    fn scan_config_custom_reply_timeout() {
        let timeout = core::time::Duration::from_millis(20);
        let config = ScanConfig::for_peer().with_reply_timeout(timeout);
        assert_eq!(config.reply_timeout, timeout);
    }

    #[test]
    #[allow(deprecated)]
    fn deprecated_scan_config_new_still_builds_the_default_config() {
        let config = ScanConfig::new(b"probe");
        assert_eq!(config.probe_data, b"probe");
        assert_eq!(config.channels, &DEFAULT_SCAN_CHANNELS);
        assert_eq!(config.reply_timeout, DEFAULT_REPLY_TIMEOUT);
    }

    #[test]
    fn scan_config_custom_channels() {
        let channels = [1, 6, 11];
        let config = ScanConfig::for_peer().with_channels(&channels);
        assert_eq!(config.channels, &[1, 6, 11]);
    }

    #[test]
    fn scan_config_custom_probe_confirmations() {
        let config = ScanConfig::for_peer().with_probe_confirmations(3);
        assert_eq!(config.probe_confirmations, 3);
    }

    #[test]
    fn scan_config_zero_probe_confirmations_disables_confirmation() {
        let config = ScanConfig::for_peer().with_probe_confirmations(0);
        assert_eq!(config.probe_confirmations, 0);
    }

    #[test]
    fn scan_config_custom_confirmation_gap() {
        let gap = core::time::Duration::from_millis(200);
        let config = ScanConfig::for_peer().with_confirmation_gap(gap);
        assert_eq!(config.confirmation_gap, gap);
    }

    #[test]
    fn default_probe_confirmations_is_one() {
        assert_eq!(DEFAULT_PROBE_CONFIRMATIONS, 1);
    }

    #[test]
    fn default_confirmation_gap_exceeds_typical_scan_dwell() {
        // 802.11 active-scan dwell is ~120 ms; gap must be larger so a roaming
        // peer cannot ACK both the first probe and the confirmation probe.
        assert!(DEFAULT_CONFIRMATION_GAP >= core::time::Duration::from_millis(130));
    }

    #[test]
    fn scan_config_custom_probe_timeout() {
        let timeout = core::time::Duration::from_millis(250);
        let config = ScanConfig::for_peer().with_probe_timeout(timeout);
        assert_eq!(config.probe_timeout, timeout);
    }

    #[test]
    fn scan_config_custom_burst_timeout() {
        let timeout = core::time::Duration::from_secs(5);
        let config = ScanConfig::for_peer().with_burst_timeout(timeout);
        assert_eq!(config.burst_timeout, timeout);
    }

    #[test]
    fn scan_result_equality() {
        assert_eq!(ScanResult { channel: 6 }, ScanResult { channel: 6 });
        assert_ne!(ScanResult { channel: 6 }, ScanResult { channel: 11 });
    }

    // ── Mock scan tests ──────────────────────────────────────────────────

    #[test]
    fn mock_scan_respond_channel() {
        let driver = mock::MockEspNowDriver::new();
        assert!(driver.scan_respond_channel().is_none());
        driver.set_scan_respond_channel(6);
        assert_eq!(driver.scan_respond_channel(), Some(6));
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
