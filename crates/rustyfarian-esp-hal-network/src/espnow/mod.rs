//! ESP-NOW on the bare-metal tier (ADR 020), built on `esp-radio 1.0.0-beta.1` `esp_now`.
//!
//! [`EspHalEspNow`] implements the same [`EspNowDriver`] contract as the ESP-IDF tier's
//! `EspIdfEspNow`, so frame codecs, [`juggler::espnow::PeerTracker`] and the
//! [`juggler::espnow::ScanMachine`] are shared; frames on the air are byte-identical and a
//! mixed IDF/HAL fleet interoperates.
//! Peers are addressed by their station (STA) MAC on both tiers.
//!
//! # Constructors
//!
//! | Constructor | Radio | Use |
//! |:------------|:------|:----|
//! | [`EspHalEspNow::from_controller`] | the caller's [`WifiController`](esp_radio::wifi::WifiController), station already configured and usually associated | coordinator that also talks to a router (brain) |
//! | [`EspHalEspNow::new_owned`] | built here: `WifiController::new` starts an unassociated station (default station config, no connect); no SSID, no `embassy-net` | ESP-NOW-only peers (matrix, nunchuk) |
//!
//! # Requirements
//!
//! * `esp_rtos::start` must have run (`esp-radio` panics without a preemptive scheduler).
//!   `WiFiManager::init_async` starts it; an owned-radio firmware calls it itself.
//! * A global heap (`esp_alloc::heap_allocator!`): the Wi-Fi blob and every received frame allocate.
//! * No embassy executor is needed for the sync [`EspNowDriver`] methods.
//!   The `async` helpers ([`EspHalEspNow::recv_async`], [`EspHalEspNow::send_async`],
//!   [`EspHalEspNow::send_async_timeout`], [`EspHalEspNow::scan_for_peer`]) need one, the sender
//!   runner ([`EspHalEspNow::run_sender`]) must be spawned for the send helpers, and an `embassy-time` driver (provided by
//!   `esp_rtos` with the `embassy` feature that `espnow` implies).
//! * `esp-radio` allows a single ESP-NOW instance per boot; a second constructor call returns
//!   [`EspHalEspNowError::AlreadyInitialised`] while the first handle is alive.
//!
//! # Behaviour worth knowing
//!
//! * [`EspNowDriver::send`] blocks for the TX duration (about 1-30 ms including 802.11 retries):
//!   `esp-radio`'s send waiter busy-spins until the send callback fired.
//!   It ignores the MAC-layer ACK, like the IDF `send`; [`EspHalEspNow::send_and_wait`] reports it.
//!   Prefer [`EspHalEspNow::send_async`] in embassy tasks (it needs the runner).
//! * Only one send can be in flight.
//!   The sender sits in a take-out cell; a concurrent sender gets [`EspHalEspNowError::Busy`].
//!   No critical section or interrupt-masking lock is ever held across a send.
//! * Async sends and scan probes go through [`EspHalEspNow::run_sender`], a runner task that owns every
//!   asynchronous transmission and never drops a send future; a cancelled or timed-out caller never
//!   orphans a completion, so a late send callback cannot be attributed to the next send.
//!   Without a running runner the async senders fail with [`EspHalEspNowError::NoSenderRunner`].
//!   The sync [`EspNowDriver::send`] and [`EspHalEspNow::send_and_wait`] lease the sender directly,
//!   work without the runner and return [`EspHalEspNowError::Busy`] while the runner is mid-transmission.
//!   If the radio never fires the send callback, the runner (and a sync send) waits forever, like `esp-radio`'s own send waiter.
//! * Receive: `esp-radio` keeps a static 10-frame queue that drops the oldest frame on overflow and
//!   cannot be resized (the IDF tier defaults to 32).
//!   Drain it at the frame rate of the busiest peer.
//!   Frames can reach 1470 bytes (ESP-NOW v2); frames above [`MAX_DATA_LEN`] (250) are dropped,
//!   counted ([`EspHalEspNow::oversize_dropped`]) and reported with a rate-limited warning.
//!   They are never truncated.
//! * Unicast sends report the MAC-layer ACK; broadcast sends always look successful.
//! * The broadcast peer is pre-registered on the station interface; adding it again is a no-op.
//! * Encrypted peers are rejected ([`EspHalEspNowError::EncryptionUnsupported`]); the fleet uses none.
//! * TX power is a quarter-dBm passthrough (`[8, 84]`).
//!   `esp-radio` starts at 20 (5 dBm).
//!   [`EspHalEspNow::new_owned`] applies the TX power of its config ([`EspNowConfig::with_tx_power_quarter_dbm`]), default 52 (13 dBm,
//!   the level the IDF firmware uses).
//!   [`EspHalEspNow::from_controller`] leaves the caller's TX power alone unless the config sets
//!   one: an associated station above ~8.5 dBm on a PCB-antenna board fails authentication.
//! * AMPDU RX/TX are disabled on the owned radio, matching the IDF brain's sdkconfig, because
//!   Block-Ack set-up exchanges starve ESP-NOW reception on an associated station.
//!   A caller-managed radio must be built with the same setting; see
//!   `HalWifiConfig::with_ampdu`.
//!   The `esp-radio` `coex` feature stays off: it arbitrates Wi-Fi against BLE/802.15.4 only.
//!
//! ```ignore
//! // Once, in a task that is never cancelled (needed for async sends and scans):
//! #[embassy_executor::task]
//! async fn espnow_sender(espnow: &'static EspHalEspNow) -> ! {
//!     espnow.run_sender().await
//! }
//!
//! // ESP32-C3 peer: unassociated station, ESP-NOW only.
//! esp_alloc::heap_allocator!(size: 72 * 1024);
//! let timg0 = TimerGroup::new(peripherals.TIMG0);
//! esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);
//! let espnow = EspHalEspNow::new_owned(
//!     peripherals.WIFI,
//!     EspNowConfig::default().with_channel(6),
//! )?;
//! espnow.add_peer(&PeerConfig::new(coordinator_sta_mac))?;
//! espnow.send(&coordinator_sta_mac, &frame)?;
//! while let Some(event) = espnow.try_recv() { /* ... */ }
//! ```

pub(crate) mod cell;

#[cfg(all(feature = "espnow", any(feature = "esp32c3", feature = "esp32c6")))]
mod driver;

#[cfg(all(feature = "espnow", any(feature = "esp32c3", feature = "esp32c6")))]
pub use driver::EspHalEspNow;

pub use juggler::espnow::{
    EspNowDriver, EspNowEvent, MacAddress, PeerConfig, ScanConfig, ScanError, ScanResult,
    SendOutcome, WifiInterface, BROADCAST_MAC, MAX_DATA_LEN,
};
pub use juggler::wifi::TxPowerLevel;

#[cfg(all(
    feature = "espnow",
    any(feature = "esp32s3", feature = "esp32"),
    not(any(feature = "esp32c3", feature = "esp32c6"))
))]
compile_error!("the `espnow` feature is supported on `esp32c3` and `esp32c6` only");

/// Lowest Wi-Fi channel accepted by [`EspNowConfig::with_channel`] and `set_channel`.
pub const MIN_CHANNEL: u8 = 1;
/// Highest Wi-Fi channel accepted by [`EspNowConfig::with_channel`] and `set_channel`.
pub const MAX_CHANNEL: u8 = 14;
/// Lowest TX power accepted by the driver, in quarter dBm (2 dBm).
pub const MIN_TX_POWER_QUARTER_DBM: i8 = 8;
/// Highest TX power accepted by the driver, in quarter dBm (21 dBm).
pub const MAX_TX_POWER_QUARTER_DBM: i8 = 84;
/// Every n-th oversize frame is logged after the first one.
const OVERSIZE_LOG_EVERY: u32 = 32;

// ─── Config ─────────────────────────────────────────────────────────────────

/// TX power [`EspHalEspNow::new_owned`] applies when the config sets none: 13 dBm.
pub const DEFAULT_TX_POWER_QUARTER_DBM: i8 = 52;

/// Settings applied by both [`EspHalEspNow`] constructors.
///
/// The default leaves TX power and channel unset: [`EspHalEspNow::new_owned`] then uses 13 dBm
/// ([`DEFAULT_TX_POWER_QUARTER_DBM`]) and the radio's channel, [`EspHalEspNow::from_controller`]
/// keeps the caller's TX power.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EspNowConfig {
    tx_power_quarter_dbm: Option<i8>,
    channel: Option<u8>,
}

impl EspNowConfig {
    /// Sets the TX power from a named level.
    pub fn with_tx_power(mut self, level: TxPowerLevel) -> Self {
        self.tx_power_quarter_dbm = Some(level.to_quarter_dbm());
        self
    }

    /// Sets the TX power in quarter dBm (`[8, 84]`, validated by the constructor); `None` leaves
    /// it unset.
    pub fn with_tx_power_quarter_dbm(mut self, quarter_dbm: Option<i8>) -> Self {
        self.tx_power_quarter_dbm = quarter_dbm;
        self
    }

    /// Pins the primary channel after start (`1..=14`).
    ///
    /// Meaningful for the owned radio, which has no access point to follow.
    /// A caller-managed station keeps the channel of its access point: ignored there.
    pub fn with_channel(mut self, channel: u8) -> Self {
        self.channel = Some(channel);
        self
    }

    /// Configured TX power in quarter dBm, if any.
    pub fn tx_power_quarter_dbm(&self) -> Option<i8> {
        self.tx_power_quarter_dbm
    }

    /// Configured channel, if any.
    pub fn channel(&self) -> Option<u8> {
        self.channel
    }

    pub(crate) fn validate(&self) -> Result<(), EspHalEspNowError> {
        if let Some(quarter_dbm) = self.tx_power_quarter_dbm {
            validate_tx_power(quarter_dbm)?;
        }
        if let Some(channel) = self.channel {
            validate_channel(channel)?;
        }
        Ok(())
    }
}

// ─── Errors ─────────────────────────────────────────────────────────────────

/// Error reported by the `esp-now` driver of the Wi-Fi blob.
///
/// Mirrors `esp_radio::esp_now::Error` so the unstable `esp-radio` types stay out of this API.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RadioError {
    /// ESP-NOW is not initialised.
    NotInitialized,
    /// Invalid argument.
    InvalidArgument,
    /// Out of memory.
    OutOfMemory,
    /// The peer table is full.
    PeerListFull,
    /// The peer is not registered.
    NotFound,
    /// Internal driver error.
    Internal,
    /// The peer is already registered.
    PeerExists,
    /// The peer is registered on another interface.
    InterfaceMismatch,
    /// Any other driver error code.
    Other(u32),
}

/// Errors of [`EspHalEspNow`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EspHalEspNowError {
    /// The sender, receiver or controller is in use by a concurrent call.
    Busy,
    /// The frame is longer than [`MAX_DATA_LEN`].
    PayloadTooLarge {
        /// Offending length in bytes.
        len: usize,
    },
    /// Encrypted peers are not supported on this tier.
    EncryptionUnsupported,
    /// Channel outside `1..=14` (peer channel `0` means "current channel").
    InvalidChannel(u8),
    /// TX power outside `[8, 84]` quarter dBm.
    InvalidTxPower(i8),
    /// The handle was built from a caller-owned controller; change TX power on that controller.
    ControllerNotOwned,
    /// Another [`EspHalEspNow`] is alive (`esp-radio` allows one instance).
    AlreadyInitialised,
    /// The frame was transmitted but not acknowledged at the MAC layer.
    SendFailed,
    /// No send completion within the allowed time.
    Timeout,
    /// An async send needs [`EspHalEspNow::run_sender`] to be running.
    NoSenderRunner,
    /// ESP-NOW driver error.
    Radio(RadioError),
    /// [`EspHalEspNow::scan_for_peer`] did not find the peer or could not register it.
    Scan(ScanError),
    /// Wi-Fi driver error while starting or tuning the radio.
    #[cfg(all(feature = "espnow", any(feature = "esp32c3", feature = "esp32c6")))]
    Wifi(esp_radio::wifi::WifiError),
}

impl core::fmt::Display for EspHalEspNowError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Busy => write!(f, "ESP-NOW resource busy"),
            Self::PayloadTooLarge { len } => {
                write!(f, "ESP-NOW payload of {len} bytes exceeds {MAX_DATA_LEN}")
            }
            Self::EncryptionUnsupported => write!(f, "encrypted ESP-NOW peers are not supported"),
            Self::InvalidChannel(c) => write!(f, "invalid Wi-Fi channel {c}"),
            Self::InvalidTxPower(p) => write!(f, "TX power {p} outside [8, 84] quarter dBm"),
            Self::ControllerNotOwned => write!(f, "Wi-Fi controller is owned by the caller"),
            Self::AlreadyInitialised => write!(f, "an ESP-NOW instance already exists"),
            Self::SendFailed => write!(f, "ESP-NOW frame was not acknowledged"),
            Self::Timeout => write!(f, "ESP-NOW send did not complete in time"),
            Self::NoSenderRunner => write!(f, "the ESP-NOW sender runner is not running"),
            Self::Radio(e) => write!(f, "ESP-NOW driver error: {e:?}"),
            Self::Scan(e) => write!(f, "ESP-NOW scan failed: {e}"),
            #[cfg(all(feature = "espnow", any(feature = "esp32c3", feature = "esp32c6")))]
            Self::Wifi(e) => write!(f, "Wi-Fi driver error: {e:?}"),
        }
    }
}

impl core::error::Error for EspHalEspNowError {}

// ─── Pure helpers (host-tested) ─────────────────────────────────────────────

pub(crate) fn validate_channel(channel: u8) -> Result<(), EspHalEspNowError> {
    if (MIN_CHANNEL..=MAX_CHANNEL).contains(&channel) {
        Ok(())
    } else {
        Err(EspHalEspNowError::InvalidChannel(channel))
    }
}

pub(crate) fn validate_tx_power(quarter_dbm: i8) -> Result<(), EspHalEspNowError> {
    if (MIN_TX_POWER_QUARTER_DBM..=MAX_TX_POWER_QUARTER_DBM).contains(&quarter_dbm) {
        Ok(())
    } else {
        Err(EspHalEspNowError::InvalidTxPower(quarter_dbm))
    }
}

pub(crate) fn validate_frame_len(data: &[u8]) -> Result<(), EspHalEspNowError> {
    juggler::espnow::validate_payload(data)
        .map_err(|_| EspHalEspNowError::PayloadTooLarge { len: data.len() })
}

/// Maps the result of a probe send to the [`SendOutcome`] the scan machine expects.
///
/// `Ok` is a MAC-layer ACK; a transmitted but unacknowledged frame and an ACK wait that ran out
/// of time are both `NoAck`; everything else (busy sender, bad frame, driver error) means the
/// frame never went out and is `Error`.
pub(crate) fn probe_outcome(result: Result<(), EspHalEspNowError>) -> SendOutcome {
    match result {
        Ok(()) => SendOutcome::Acked,
        Err(EspHalEspNowError::SendFailed | EspHalEspNowError::Timeout) => SendOutcome::NoAck,
        Err(_) => SendOutcome::Error,
    }
}

/// True once `now` has reached `deadline`; a request without a deadline never expires.
pub(crate) fn expired<T: PartialOrd>(deadline: Option<T>, now: T) -> bool {
    deadline.is_some_and(|at| now >= at)
}

/// A [`PeerConfig`] after validation, in the shape the driver registers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PeerPlan {
    pub(crate) mac: MacAddress,
    /// `None` = current channel (`PeerConfig.channel == 0`).
    pub(crate) channel: Option<u8>,
    pub(crate) interface: WifiInterface,
}

pub(crate) fn plan_peer(config: &PeerConfig) -> Result<PeerPlan, EspHalEspNowError> {
    if config.encrypt {
        return Err(EspHalEspNowError::EncryptionUnsupported);
    }
    let channel = match config.channel {
        0 => None,
        c => {
            validate_channel(c)?;
            Some(c)
        }
    };
    Ok(PeerPlan {
        mac: config.mac,
        channel,
        interface: config.interface,
    })
}

/// True when "peer already exists" is success: the broadcast peer on the station interface is
/// registered by `esp-radio` itself.
pub(crate) fn existing_peer_is_idempotent(plan: &PeerPlan) -> bool {
    plan.mac == BROADCAST_MAC && plan.interface == WifiInterface::Sta
}

/// Builds the event for a received frame, or returns the oversize length.
///
/// `EspNowEvent` holds 250 bytes; longer frames must never reach it.
pub(crate) fn frame_event(src: MacAddress, data: &[u8]) -> Result<EspNowEvent, usize> {
    if data.len() > MAX_DATA_LEN {
        Err(data.len())
    } else {
        Ok(EspNowEvent::new(src, data))
    }
}

/// Rate limit for the oversize warning: the first drop and then every 32nd.
pub(crate) fn should_log_oversize(dropped_so_far: u32) -> bool {
    dropped_so_far
        .wrapping_sub(1)
        .is_multiple_of(OVERSIZE_LOG_EVERY)
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::time::Duration;

    const MAC: MacAddress = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];

    #[test]
    fn default_config_leaves_tx_power_unset_and_is_valid() {
        let c = EspNowConfig::default();
        assert_eq!(c.tx_power_quarter_dbm(), None);
        assert_eq!(DEFAULT_TX_POWER_QUARTER_DBM, 52);
        assert_eq!(c.channel(), None);
        assert!(c.validate().is_ok());
    }

    #[test]
    fn config_tx_power_bounds() {
        let ok_lo = EspNowConfig::default().with_tx_power_quarter_dbm(Some(8));
        let ok_hi = EspNowConfig::default().with_tx_power_quarter_dbm(Some(84));
        assert!(ok_lo.validate().is_ok());
        assert!(ok_hi.validate().is_ok());
        assert_eq!(
            EspNowConfig::default()
                .with_tx_power_quarter_dbm(Some(7))
                .validate(),
            Err(EspHalEspNowError::InvalidTxPower(7))
        );
        assert_eq!(
            EspNowConfig::default()
                .with_tx_power_quarter_dbm(Some(85))
                .validate(),
            Err(EspHalEspNowError::InvalidTxPower(85))
        );
        assert_eq!(
            EspNowConfig::default()
                .with_tx_power_quarter_dbm(Some(-1))
                .validate(),
            Err(EspHalEspNowError::InvalidTxPower(-1))
        );
    }

    #[test]
    fn config_levels_map_to_quarter_dbm() {
        let max = EspNowConfig::default().with_tx_power(TxPowerLevel::Max);
        assert_eq!(max.tx_power_quarter_dbm(), Some(78));
        assert!(max.validate().is_ok());
    }

    #[test]
    fn config_channel_bounds() {
        assert!(EspNowConfig::default().with_channel(1).validate().is_ok());
        assert!(EspNowConfig::default().with_channel(14).validate().is_ok());
        assert_eq!(
            EspNowConfig::default().with_channel(0).validate(),
            Err(EspHalEspNowError::InvalidChannel(0))
        );
        assert_eq!(
            EspNowConfig::default().with_channel(15).validate(),
            Err(EspHalEspNowError::InvalidChannel(15))
        );
    }

    #[test]
    fn frame_length_limit_is_250() {
        assert!(validate_frame_len(&[0u8; 250]).is_ok());
        assert_eq!(
            validate_frame_len(&[0u8; 251]),
            Err(EspHalEspNowError::PayloadTooLarge { len: 251 })
        );
        assert!(validate_frame_len(&[]).is_ok());
    }

    #[test]
    fn peer_channel_zero_means_current() {
        let plan = plan_peer(&PeerConfig::new(MAC)).expect("plan");
        assert_eq!(plan.channel, None);
        assert_eq!(plan.interface, WifiInterface::Sta);
        assert_eq!(plan.mac, MAC);
    }

    #[test]
    fn peer_channel_is_validated() {
        let mut cfg = PeerConfig::new(MAC);
        cfg.channel = 6;
        assert_eq!(plan_peer(&cfg).expect("plan").channel, Some(6));
        cfg.channel = 15;
        assert_eq!(plan_peer(&cfg), Err(EspHalEspNowError::InvalidChannel(15)));
    }

    #[test]
    fn peer_encryption_is_rejected() {
        let mut cfg = PeerConfig::new(MAC);
        cfg.encrypt = true;
        assert_eq!(
            plan_peer(&cfg),
            Err(EspHalEspNowError::EncryptionUnsupported)
        );
    }

    #[test]
    fn peer_interface_is_carried() {
        let plan = plan_peer(&PeerConfig::new(MAC).with_ap_interface()).expect("plan");
        assert_eq!(plan.interface, WifiInterface::Ap);
    }

    #[test]
    fn only_broadcast_on_sta_is_idempotent() {
        let bc = plan_peer(&PeerConfig::new(BROADCAST_MAC)).expect("plan");
        assert!(existing_peer_is_idempotent(&bc));
        let bc_ap = plan_peer(&PeerConfig::new(BROADCAST_MAC).with_ap_interface()).expect("plan");
        assert!(!existing_peer_is_idempotent(&bc_ap));
        let uni = plan_peer(&PeerConfig::new(MAC)).expect("plan");
        assert!(!existing_peer_is_idempotent(&uni));
    }

    #[test]
    fn frame_event_keeps_payload_up_to_limit() {
        let data = [0xA5u8; MAX_DATA_LEN];
        let ev = frame_event(MAC, &data).expect("event");
        assert_eq!(ev.mac, MAC);
        assert_eq!(ev.payload(), &data[..]);
        let short = frame_event(MAC, b"hi").expect("event");
        assert_eq!(short.payload(), b"hi");
    }

    #[test]
    fn frame_event_rejects_oversize_without_truncating() {
        let data = [0u8; MAX_DATA_LEN + 1];
        assert_eq!(frame_event(MAC, &data).unwrap_err(), MAX_DATA_LEN + 1);
        let v2 = [0u8; 1470];
        assert_eq!(frame_event(MAC, &v2).unwrap_err(), 1470);
    }

    #[test]
    fn oversize_log_is_rate_limited() {
        assert!(should_log_oversize(1));
        assert!(!should_log_oversize(2));
        assert!(!should_log_oversize(32));
        assert!(should_log_oversize(33));
        assert!(!should_log_oversize(34));
        assert!(should_log_oversize(65));
        let logged = (1..=1000u32).filter(|n| should_log_oversize(*n)).count();
        assert_eq!(logged, 32);
    }

    #[test]
    fn expiry_is_reached_at_the_deadline_and_never_without_one() {
        let at = Duration::from_millis(100);
        assert!(!expired(Some(at), Duration::from_millis(99)));
        assert!(expired(Some(at), at));
        assert!(expired(Some(at), Duration::from_millis(101)));
        assert!(!expired(None, Duration::MAX));
    }

    #[test]
    fn probe_outcome_maps_send_results() {
        assert_eq!(probe_outcome(Ok(())), SendOutcome::Acked);
        assert_eq!(
            probe_outcome(Err(EspHalEspNowError::SendFailed)),
            SendOutcome::NoAck
        );
        assert_eq!(
            probe_outcome(Err(EspHalEspNowError::Timeout)),
            SendOutcome::NoAck
        );
        for e in [
            EspHalEspNowError::Busy,
            EspHalEspNowError::PayloadTooLarge { len: 251 },
            EspHalEspNowError::Radio(RadioError::Internal),
            EspHalEspNowError::InvalidChannel(0),
        ] {
            assert_eq!(probe_outcome(Err(e)), SendOutcome::Error);
        }
    }

    #[test]
    fn scan_error_displays() {
        use core::fmt::Write;
        let mut s = heapless::String::<96>::new();
        write!(
            s,
            "{}",
            EspHalEspNowError::Scan(ScanError::NotFound {
                probed: 3,
                configured: 13
            })
        )
        .expect("fits");
        assert!(s.contains("13"));
    }

    #[test]
    fn errors_display_without_payload() {
        use core::fmt::Write;
        let mut s = heapless::String::<64>::new();
        write!(s, "{}", EspHalEspNowError::PayloadTooLarge { len: 300 }).expect("fits");
        assert!(s.contains("300"));
    }
}
