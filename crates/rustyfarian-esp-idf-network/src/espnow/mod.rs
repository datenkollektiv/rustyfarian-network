//! ESP-IDF driver implementing [`EspNowDriver`] via `esp-idf-svc`.
//!
//! # Usage
//!
//! ```rust,no_run
//! use rustyfarian_esp_idf_network::espnow::EspIdfEspNow;
//! use juggler::espnow::{EspNowDriver, PeerConfig};
//!
//! let driver = EspIdfEspNow::init().unwrap();
//! let config = PeerConfig::new([0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
//! driver.add_peer(&config).unwrap();
//! driver.send(&config.mac, b"hello").unwrap();
//! ```
//!
//! # Recommended `sdkconfig` options
//!
//! ## Wi-Fi + ESP-NOW coexistence (same chip)
//!
//! ```text
//! CONFIG_ESP_WIFI_AMPDU_RX_ENABLED=n
//! CONFIG_ESP_WIFI_AMPDU_TX_ENABLED=n
//! CONFIG_ESP_COEX_SW_COEXIST_ENABLE=y
//! ```
//!
//! Disabling A-MPDU eliminates ADDBA/DELBA management frame exchanges that
//! monopolise the radio and starve ESP-NOW receives.
//! The software coexistence arbiter enables fair time-division multiplexing.
//! MQTT and other small-payload Wi-Fi traffic sees negligible throughput loss.
//!
//! `esp-idf-svc` `EspNow::take()` already calls `esp_wifi_set_ps(WIFI_PS_NONE)`
//! internally — consumers do not need to set this manually.
//!
//! ## ESP-NOW only (no Wi-Fi AP connection)
//!
//! ```text
//! CONFIG_ESP_WIFI_ESPNOW_MAX_ENCRYPT_PEER_NUM=0
//! ```
//!
//! Disables encrypted peer slots (saves RAM when encryption is unused).
//! No AMPDU or coex settings needed since there is no Wi-Fi connection
//! competing for the radio.
//!
//! ## ESP-NOW + Wi-Fi on separate chips (two-MCU architecture)
//!
//! No special `sdkconfig` needed on either chip — radio contention is
//! eliminated by design.
//!
//! # Blocking forms
//!
//! [`EspNowDriver::send`], [`EspIdfEspNow::send_and_wait`] and
//! [`EspIdfEspNow::scan_for_peer_blocking`] park the calling thread.
//! Do not run them on a thread that also polls [`EspNowLink`] futures of the same driver:
//! futures that were polled once and are then left unpolled on that thread hold queue slots, and
//! the blocking call can wait for a slot only that thread could free.
//! `send` ends with `Timeout` after [`SEND_QUEUE_DEADLINE`], `send_and_wait` at its own timeout;
//! a scan has no bound of its own.

mod worker;

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use anyhow::Context as _;
use esp_idf_svc::espnow::{EspNow, PeerInfo, SendStatus};
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::sys::{esp, EspError};
use esp_idf_svc::wifi::{
    AccessPointConfiguration, AuthMethod, ClientConfiguration, Configuration, EspWifi,
};
pub use juggler::espnow::{
    ClassifySend, Clock, EmbassyClock, EspNowConfig, EspNowConfigError, EspNowDriver, EspNowEvent,
    EspNowLink, MacAddress, PeerConfig, PeerLinkConfig, PeerLinkState, PeerSendError, PeerSession,
    ScanConfig, ScanError, ScanResult, SendOutcome, WifiInterface, BROADCAST_MAC,
    DEFAULT_CONFIRMATION_GAP, DEFAULT_PROBE_CONFIRMATIONS, DEFAULT_PROBE_TIMEOUT,
    DEFAULT_RX_CHANNEL_CAPACITY, DEFAULT_SCAN_CHANNELS, DEFAULT_TX_POWER_QUARTER_DBM, MAX_CHANNEL,
    MAX_DATA_LEN, MAX_TX_POWER_QUARTER_DBM, MIN_CHANNEL, MIN_TX_POWER_QUARTER_DBM,
};
use juggler::espnow::{OversizeLog, SendKind};
use juggler::wait_list::WaitList;
pub use juggler::wifi::{ChannelSet, Country, CountryError, Regulatory};

use worker::{block_on, Worker};

/// The channel [`EspIdfEspNow::init_with_radio`] starts its SoftAP on, which is the pin at start.
const SOFTAP_CHANNEL: u8 = 1;

/// Applies the TX power of `config`, [`DEFAULT_TX_POWER_QUARTER_DBM`] (13 dBm) when it sets none.
///
/// The ESP-IDF driver starts at 20 dBm, which corrupts the frames and the ACKs of an ESP32-C3
/// Super Mini (bug 022); the esp-hal tier applies the same default.
fn apply_tx_power(config: &EspNowConfig) -> anyhow::Result<()> {
    let quarter_dbm = config.owned_tx_power_quarter_dbm();
    // SAFETY: plain FFI call into the Wi-Fi driver, which the caller has started;
    // `validate_owned` has bounded a configured `quarter_dbm` to the driver's `[8, 84]` range,
    // and the default is inside it.
    esp!(unsafe { esp_idf_svc::sys::esp_wifi_set_max_tx_power(quarter_dbm) })
        .with_context(|| format!("failed to set ESP-NOW TX power {quarter_dbm}"))?;
    Ok(())
}

/// How long plain [`EspNowDriver::send`] waits for a queue slot and for its frame to start
/// before it fails with [`EspIdfEspNowError::Timeout`] (1 s).
///
/// A started frame is still waited out.
/// The bound keeps the call from waiting for ever when every queue slot is held, for instance
/// by futures of this driver that were polled once and left unpolled on the calling thread.
pub const SEND_QUEUE_DEADLINE: Duration = Duration::from_secs(1);

/// Errors of [`EspIdfEspNow`] as seen through [`EspNowDriver`] and the shared
/// [`juggler::espnow::EspNowLink`] contract.
///
/// Only [`NoAck`](Self::NoAck) means "transmitted, not acknowledged"; every other variant
/// classifies as a local failure (see [`ClassifySend`]), because the peer never had a chance
/// to answer.
/// The type implements [`std::error::Error`], so `?` converts it into [`anyhow::Error`].
#[derive(Debug)]
#[non_exhaustive]
pub enum EspIdfEspNowError {
    /// The frame was transmitted but the peer did not acknowledge it.
    NoAck,
    /// The operation did not start before its deadline.
    Timeout,
    /// Another operation holds the radio.
    Busy,
    /// A scan is running; application sends are refused until it ends.
    ScanInProgress,
    /// The operation needs a driver-owned radio and this driver does not own it.
    RadioNotOwned,
    /// The payload exceeds [`MAX_DATA_LEN`].
    PayloadTooLarge {
        /// Length of the refused payload in bytes.
        len: usize,
    },
    /// An acknowledged send to the broadcast address, which never yields a MAC ACK.
    Broadcast,
    /// The driver is shutting down.
    Shutdown,
    /// A scan failed.
    Scan(ScanError),
    /// An ESP-IDF call failed.
    Driver(anyhow::Error),
}

impl fmt::Display for EspIdfEspNowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NoAck => f.write_str("peer did not ACK"),
            Self::Timeout => f.write_str("ESP-NOW operation timed out before it started"),
            Self::Busy => f.write_str("ESP-NOW radio is busy"),
            Self::ScanInProgress => f.write_str("ESP-NOW scan in progress"),
            Self::RadioNotOwned => f.write_str(
                "operation requires a driver-owned radio (init_with_radio or init_with_radio_sta)",
            ),
            Self::PayloadTooLarge { len } => write!(
                f,
                "ESP-NOW payload of {len} bytes exceeds the maximum of {MAX_DATA_LEN} bytes"
            ),
            Self::Broadcast => f.write_str("acknowledged send to the broadcast address"),
            Self::Shutdown => f.write_str("ESP-NOW driver is shutting down"),
            Self::Scan(e) => write!(f, "ESP-NOW scan failed: {e}"),
            Self::Driver(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for EspIdfEspNowError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Scan(e) => Some(e),
            // `Display` already shows the top message of the chain; continue below it.
            Self::Driver(e) => e.chain().nth(1),
            _ => None,
        }
    }
}

impl EspIdfEspNowError {
    /// Unwraps a [`Driver`](Self::Driver) error without adding a link to its chain.
    fn into_anyhow(self) -> anyhow::Error {
        match self {
            Self::Driver(e) => e,
            other => anyhow::Error::new(other),
        }
    }
}

impl ClassifySend for EspIdfEspNowError {
    fn outcome(&self) -> SendOutcome {
        match self {
            Self::NoAck => SendOutcome::NoAck,
            _ => SendOutcome::Local,
        }
    }
}

impl From<ScanError> for EspIdfEspNowError {
    fn from(e: ScanError) -> Self {
        Self::Scan(e)
    }
}

/// The receive queue shared by the C callback, [`EspNowDriver::try_recv`] and
/// [`EspIdfEspNow::receive`]: a frame leaves it exactly once, whichever consumer pops it.
///
/// A `0xF4` probe reply that a running scan awaits never enters it: the callback offers every
/// frame to the worker's reply tap first (ADR 025).
struct RxQueue {
    frames: Mutex<VecDeque<EspNowEvent>>,
    capacity: usize,
    waiters: WaitList,
    oversize: OversizeLog,
    /// Counts frames dropped because the queue was full; reuses the oversize log's counter and
    /// its rule of logging the first drop and then every 32nd.
    overflow: OversizeLog,
}

impl RxQueue {
    fn new(capacity: usize) -> Self {
        Self {
            frames: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
            waiters: WaitList::new(),
            oversize: OversizeLog::new(),
            overflow: OversizeLog::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, VecDeque<EspNowEvent>> {
        self.frames.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn pop(&self) -> Option<EspNowEvent> {
        self.lock().pop_front()
    }

    /// Receive-callback entry: queue a valid frame, drop and count an oversize one.
    fn push_frame(&self, mac: MacAddress, data: &[u8]) {
        let event = match EspNowEvent::try_new(mac, data) {
            Ok(event) => event,
            Err(len) => {
                if self.oversize.record() {
                    log::warn!(
                        "ESP-NOW: dropped oversize frame ({} B > {} B) from {:02X?}, {} dropped in total",
                        len,
                        MAX_DATA_LEN,
                        mac,
                        self.oversize.dropped()
                    );
                }
                return;
            }
        };
        {
            let mut frames = self.lock();
            if frames.len() >= self.capacity {
                drop(frames);
                if self.overflow.record() {
                    log::warn!(
                        "ESP-NOW receive queue full — frame from {:02X?} dropped, {} dropped in total",
                        mac,
                        self.overflow.dropped()
                    );
                }
                return;
            }
            frames.push_back(event);
        }
        self.waiters.wake_all();
    }
}

/// Radio-management mode the driver is operating in.
///
/// This is the single source of truth for branching between the three
/// initialisation paths.  Every other method (`default_interface`,
/// `scan_for_peer`, `send_and_wait`) derives its behaviour from this enum
/// rather than inspecting the presence of `_wifi` and the value of
/// `wifi_interface` independently — which previously coupled two fields
/// to encode three states.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RadioMode {
    /// Caller owns the radio (must already be started).
    ///
    /// Constructor: [`EspIdfEspNow::init`] / [`EspIdfEspNow::init_with_capacity`].
    /// Peer interface: [`WifiInterface::Sta`].
    CallerManagedSta,
    /// Driver owns the radio and started it in SoftAP mode.
    ///
    /// Constructor: [`EspIdfEspNow::init_with_radio`].
    /// Peer interface: [`WifiInterface::Ap`].
    /// AP beacon scheduling locks the channel deterministically; no per-send
    /// re-pinning is required.  This is the recommended owned-radio mode.
    OwnedSoftAp,
    /// Driver owns the radio and started it in unassociated STA mode.
    ///
    /// Constructor: [`EspIdfEspNow::init_with_radio_sta`].
    /// Peer interface: [`WifiInterface::Sta`].
    /// The Wi-Fi driver's background scanner drifts the channel; every
    /// `send_and_wait` brackets the send with promiscuous-on / set-channel /
    /// promiscuous-off using the channel last stored by `scan_for_peer`.
    /// Fundamentally racy — see ADR 012 — and offered only as a fallback
    /// when SoftAP conflicts with another radio requirement on the same chip.
    OwnedStaPromisc,
}

impl RadioMode {
    /// Wi-Fi interface peers must be registered against for this mode.
    fn wifi_interface(self) -> WifiInterface {
        match self {
            RadioMode::OwnedSoftAp => WifiInterface::Ap,
            RadioMode::CallerManagedSta | RadioMode::OwnedStaPromisc => WifiInterface::Sta,
        }
    }

    /// Whether the driver owns the radio, which a channel scan needs (ADR 012).
    fn owns_radio(self) -> bool {
        !matches!(self, RadioMode::CallerManagedSta)
    }
}

/// ESP-IDF implementation of [`EspNowDriver`].
///
/// Wraps [`EspNow<'static>`] and bridges the C receive callback into a bounded queue that
/// [`EspNowDriver::try_recv`] polls and [`EspIdfEspNow::receive`] awaits.
///
/// # Radio management
///
/// Three initialisation paths cover the three deployment patterns:
///
/// - [`init()`](EspIdfEspNow::init) — caller owns the Wi-Fi radio; the radio
///   must already be started before calling this.  Best for devices that
///   simultaneously connect to a Wi-Fi AP (both peers share the AP's channel
///   automatically — no scanning needed).
/// - [`init_with_radio()`](EspIdfEspNow::init_with_radio) — the driver starts
///   and owns the radio internally in **SoftAP mode**.  Recommended for
///   ESP-NOW-only devices: the AP beacon schedule locks the channel
///   deterministically, eliminating channel-drift races without per-send
///   workarounds.
/// - [`init_with_radio_sta()`](EspIdfEspNow::init_with_radio_sta) — the driver
///   starts and owns the radio in unassociated STA mode with a
///   promiscuous-bracket channel re-pin before every send.  Offered only as a
///   fallback when SoftAP conflicts with another radio requirement; see
///   ADR 012 for the documented ~0–20 % send failure rate.
///
/// # Send path
///
/// Every transmission runs on one `espnow-tx` thread that the driver starts in every
/// constructor and joins when it is dropped (see the `worker` module).
/// The thread owns the send callback, so an acknowledgement always belongs to the frame it was
/// reported for, and it serialises the frames: [`EspNowDriver::send`], [`send_acked`](Self::send_acked)
/// and [`send_and_wait`](Self::send_and_wait) queue behind each other, and the scan's probes go
/// through the same path.
/// The thread costs a fixed stack of about 6 KiB.
///
/// # Missing send callback (fail-closed)
///
/// The thread waits for each frame's send callback without a limit while the driver lives, so a
/// frame is never mistaken for its successor and the channel never changes under one.
/// The callback normally arrives within a few milliseconds, but nothing guarantees it: if it
/// never fires, that frame and every later transmission (acknowledged or not, scan probes
/// included) block until the driver is dropped, which ends every wait with
/// [`EspIdfEspNowError::Shutdown`].
pub struct EspIdfEspNow {
    /// The ESP-NOW singleton: dropping it deinitialises ESP-NOW and clears both callbacks.
    /// Declared first, so it drops before the radio it runs on.
    _esp_now: EspNow<'static>,
    rx: Arc<RxQueue>,
    _wifi: Option<EspWifi<'static>>,
    /// The `espnow-tx` worker: the send queue, the pin and the scan slot, and the thread that
    /// serves them; joined in [`Drop`] before `esp_now` is dropped.
    worker: Worker,
    /// Explicit state for the three radio-management modes the driver supports.
    ///
    /// The driver internally tracks which of the three init paths was taken.
    /// All branching for `default_interface` and the scan is driven off this single field.
    mode: RadioMode,
}

impl EspIdfEspNow {
    /// Initialise ESP-NOW with the default receive-queue capacity of
    /// [`DEFAULT_RX_CHANNEL_CAPACITY`] frames.
    ///
    /// The Wi-Fi radio must already be started by the caller.
    /// For ESP-NOW-only devices, use [`init_with_radio()`](Self::init_with_radio) instead.
    pub fn init() -> anyhow::Result<Self> {
        Self::init_with_capacity(DEFAULT_RX_CHANNEL_CAPACITY)
    }

    /// Initialise ESP-NOW with a custom receive-queue capacity.
    ///
    /// The Wi-Fi radio must already be started by the caller.
    /// Frames received while the queue is full are dropped with a warning log.
    ///
    /// The caller owns the radio, so its country is the caller's too (see
    /// `wifi::set_country`); the driver only reads it.
    pub fn init_with_capacity(capacity: usize) -> anyhow::Result<Self> {
        // A caller-managed radio cannot scan, so the set only matters for diagnostics.
        let allowed = crate::regulatory::read().map(|r| r.channels);
        Self::init_inner(capacity, None, RadioMode::CallerManagedSta, None, allowed)
    }

    /// Initialise ESP-NOW and start the Wi-Fi radio internally in
    /// **SoftAP mode** on channel 1.
    ///
    /// Use this for devices that need ESP-NOW without connecting to a Wi-Fi AP.
    /// The radio is kept alive for the lifetime of the returned driver.
    /// AP beacon scheduling prevents the ESP-IDF Wi-Fi driver from autonomously
    /// hopping channels in the background — the root cause of channel drift on
    /// unassociated STA.  See ADR 012 for the analysis.
    ///
    /// # Breaking semantics change vs `0.2.x`
    ///
    /// Prior to this release, `init_with_radio` started the radio in
    /// unassociated STA mode.  As of `0.2.2` it starts a **hidden SoftAP** on
    /// channel 1, and [`default_interface`](Self::default_interface) now
    /// returns [`WifiInterface::Ap`] instead of [`WifiInterface::Sta`].
    /// Downstream code that hard-codes [`WifiInterface::Sta`] for peer
    /// registration on a driver-owned radio must either call
    /// [`default_interface`](Self::default_interface) or switch to
    /// [`init_with_radio_sta`](Self::init_with_radio_sta) to keep the prior
    /// behaviour.  Devices that simultaneously run BLE or a user-facing
    /// SoftAP must use [`init_with_radio_sta`](Self::init_with_radio_sta).
    ///
    /// # Country
    ///
    /// The driver applies [`Country::WORLD`] (channels 1-11) before the radio starts, so the
    /// country ESP-IDF kept in flash never decides; scans skip the channels the country does
    /// not allow.
    /// A scout looking for a peer on a network on channel 12 or 13 starts under that network's
    /// country through [`init_with_radio_config`](Self::init_with_radio_config).
    pub fn init_with_radio(
        modem: Modem<'static>,
        sys_loop: esp_idf_svc::eventloop::EspSystemEventLoop,
        nvs: Option<esp_idf_svc::nvs::EspDefaultNvsPartition>,
    ) -> anyhow::Result<Self> {
        Self::init_with_radio_config(modem, sys_loop, nvs, EspNowConfig::default())
    }

    /// [`init_with_radio`](Self::init_with_radio) with the start settings of `config`, the same
    /// [`EspNowConfig`] the esp-hal tier's `EspHalEspNow::new_owned` takes.
    ///
    /// Applied in this order:
    ///
    /// 1. The country ([`EspNowConfig::with_country`], else [`Country::WORLD`]) once, before the
    ///    radio starts, so a boot with an unchanged country writes no flash;
    ///    [`set_country`](Self::set_country) after [`init_with_radio`](Self::init_with_radio)
    ///    writes it twice per boot (to `01` and back).
    /// 2. The hidden SoftAP starts on the channel of [`EspNowConfig::with_channel`], else
    ///    channel 1, which is the pin until a scan finds another.
    /// 3. The TX power ([`EspNowConfig::with_tx_power`]) after the start.
    ///    When the config sets none, [`DEFAULT_TX_POWER_QUARTER_DBM`] (13 dBm), as on the esp-hal
    ///    tier: the driver's own 20 dBm corrupts the frames of an ESP32-C3 Super Mini (bug 022).
    ///
    /// # Errors
    ///
    /// An [`anyhow::Error`] with the context of the failing step: a config outside the driver
    /// ranges or a channel the country does not allow ([`EspNowConfig::validate_owned`], refused
    /// before any radio work), or the ESP-IDF error of the Wi-Fi start, the country or the TX
    /// power.
    pub fn init_with_radio_config(
        modem: Modem<'static>,
        sys_loop: esp_idf_svc::eventloop::EspSystemEventLoop,
        nvs: Option<esp_idf_svc::nvs::EspDefaultNvsPartition>,
        config: EspNowConfig,
    ) -> anyhow::Result<Self> {
        config
            .validate_owned()
            .context("invalid ESP-NOW configuration")?;
        let mut wifi = EspWifi::new(modem, sys_loop, nvs)
            .context("failed to create EspWifi for ESP-NOW radio")?;
        let regulatory = crate::regulatory::apply(config.owned_country())?;
        let channel = config.channel().unwrap_or(SOFTAP_CHANNEL);
        wifi.set_configuration(&Configuration::AccessPoint(AccessPointConfiguration {
            ssid_hidden: true,
            channel,
            auth_method: AuthMethod::None,
            ..Default::default()
        }))
        .context("failed to configure SoftAP for ESP-NOW radio")?;
        wifi.start()
            .context("failed to start Wi-Fi SoftAP for ESP-NOW")?;
        log::info!("Wi-Fi SoftAP started for ESP-NOW (channel-stable, no AP connection)");
        apply_tx_power(&config)?;

        // The SoftAP started on `channel`, so that is the pin until a scan finds another.
        Self::init_inner(
            DEFAULT_RX_CHANNEL_CAPACITY,
            Some(wifi),
            RadioMode::OwnedSoftAp,
            Some(channel),
            Some(regulatory.channels),
        )
    }

    /// Initialise ESP-NOW and start the Wi-Fi radio internally in unassociated
    /// STA mode, using a promiscuous-bracket channel re-pin before every send.
    ///
    /// Use this alternative to [`init_with_radio`](Self::init_with_radio) when
    /// SoftAP mode conflicts with another radio requirement on the same device
    /// (BLE coexistence, user-facing SoftAP, etc.).
    ///
    /// # Trade-off vs SoftAP mode
    ///
    /// The ESP-IDF Wi-Fi driver's background scan task (FreeRTOS priority 23)
    /// can preempt the application task (priority 5) between the promiscuous
    /// disable and `esp_now_send`, causing `ESP_ERR_ESPNOW_CHAN` (~0–20 % of
    /// sends depending on scheduler load).  Sends that fail trigger a re-scan
    /// and retry.  Use [`init_with_radio`](Self::init_with_radio) (SoftAP) for
    /// deterministic, race-free delivery whenever possible.
    ///
    /// The radio is always put into STA mode, whatever mode the Wi-Fi driver
    /// stored in NVS (after an erased flash that is SoftAP).
    ///
    /// The country is [`Country::WORLD`] as for [`init_with_radio`](Self::init_with_radio#country);
    /// start under another one with [`init_with_radio_sta_config`](Self::init_with_radio_sta_config).
    pub fn init_with_radio_sta(
        modem: Modem<'static>,
        sys_loop: esp_idf_svc::eventloop::EspSystemEventLoop,
        nvs: Option<esp_idf_svc::nvs::EspDefaultNvsPartition>,
    ) -> anyhow::Result<Self> {
        Self::init_with_radio_sta_config(modem, sys_loop, nvs, EspNowConfig::default())
    }

    /// [`init_with_radio_sta`](Self::init_with_radio_sta) with the start settings of `config`
    /// (see [`init_with_radio_config`](Self::init_with_radio_config)).
    ///
    /// Applied in this order: the country (else [`Country::WORLD`]) before the radio starts,
    /// the TX power after the start ([`DEFAULT_TX_POWER_QUARTER_DBM`] when unset), then the channel of
    /// [`EspNowConfig::with_channel`], pinned like the esp-hal `new_owned` does.
    /// Without a configured channel the unassociated station sets none and there is no pin
    /// before a scan.
    ///
    /// # Errors
    ///
    /// As [`init_with_radio_config`](Self::init_with_radio_config); the channel is set with
    /// `esp_wifi_set_channel`.
    pub fn init_with_radio_sta_config(
        modem: Modem<'static>,
        sys_loop: esp_idf_svc::eventloop::EspSystemEventLoop,
        nvs: Option<esp_idf_svc::nvs::EspDefaultNvsPartition>,
        config: EspNowConfig,
    ) -> anyhow::Result<Self> {
        config
            .validate_owned()
            .context("invalid ESP-NOW configuration")?;
        let mut wifi = EspWifi::new(modem, sys_loop, nvs)
            .context("failed to create EspWifi for ESP-NOW radio")?;
        let regulatory = crate::regulatory::apply(config.owned_country())?;
        // `set_configuration` is the only esp-idf-svc call that sets the Wi-Fi mode; without it the
        // driver keeps the mode stored in NVS, which is SoftAP after an erased flash.
        wifi.set_configuration(&Configuration::Client(ClientConfiguration::default()))
            .context("failed to set STA mode for ESP-NOW radio")?;
        wifi.start()
            .context("failed to start Wi-Fi radio for ESP-NOW")?;
        log::info!("Wi-Fi STA started for ESP-NOW (promiscuous-bracket channel re-pin)");
        apply_tx_power(&config)?;
        if let Some(channel) = config.channel() {
            // SAFETY: plain FFI call into the Wi-Fi driver, which `EspWifi::start` has just
            // started; the station is unassociated and no frame is on the air yet.
            esp!(unsafe {
                esp_idf_svc::sys::esp_wifi_set_channel(
                    channel,
                    esp_idf_svc::sys::wifi_second_chan_t_WIFI_SECOND_CHAN_NONE,
                )
            })
            .with_context(|| format!("failed to set channel {channel} for ESP-NOW radio"))?;
        }

        // Without a configured channel none is set, so there is no pin before a scan.
        Self::init_inner(
            DEFAULT_RX_CHANNEL_CAPACITY,
            Some(wifi),
            RadioMode::OwnedStaPromisc,
            config.channel(),
            Some(regulatory.channels),
        )
    }

    /// Applies `country` to the driver-owned radio and returns what the radio reports; later
    /// scans probe only the channels it allows.
    ///
    /// [`init_with_radio`](Self::init_with_radio) and
    /// [`init_with_radio_sta`](Self::init_with_radio_sta) start under [`Country::WORLD`]
    /// (channels 1-11); a scout looking for a peer whose network runs on channel 12 or 13 needs
    /// that network's country, e.g. `Country::parse("DE")?`, best given to
    /// [`init_with_radio_config`](Self::init_with_radio_config) at bring-up.
    /// 802.11d does not help here: only an associated station adopts its AP's country.
    /// Call it before the first scan: a scan already running keeps its channel set, and the
    /// radio then refuses a forbidden channel, which the scan reports as a fault.
    /// ESP-IDF keeps the country in flash; the write is skipped when the radio already reports
    /// it, but after a constructor that applied another country each boot writes twice.
    /// Same name and shape as `EspHalEspNow::set_country` (ADR 022).
    ///
    /// # Errors
    ///
    /// * [`EspIdfEspNowError::RadioNotOwned`] for a driver created via [`init()`](Self::init) /
    ///   [`init_with_capacity()`](Self::init_with_capacity): the caller owns that radio and sets
    ///   its country through `wifi::set_country` or `WiFiConfig::with_country`.
    /// * [`EspIdfEspNowError::Driver`] with the ESP-IDF error of the write or the read-back.
    pub fn set_country(&self, country: Country) -> Result<Regulatory, EspIdfEspNowError> {
        if !self.mode.owns_radio() {
            return Err(EspIdfEspNowError::RadioNotOwned);
        }
        let regulatory = crate::regulatory::apply(country).map_err(EspIdfEspNowError::Driver)?;
        self.worker.set_allowed_channels(regulatory.channels);
        Ok(regulatory)
    }

    /// Returns the Wi-Fi interface that peers must be registered against.
    ///
    /// - [`WifiInterface::Sta`] — drivers created via [`init()`](Self::init),
    ///   [`init_with_capacity()`](Self::init_with_capacity), or
    ///   [`init_with_radio_sta()`](Self::init_with_radio_sta) (STA modes).
    /// - [`WifiInterface::Ap`] — drivers created via
    ///   [`init_with_radio()`](Self::init_with_radio) (SoftAP mode).
    pub fn default_interface(&self) -> WifiInterface {
        self.mode.wifi_interface()
    }

    /// Scan Wi-Fi channels to find one where the given peer responds, without blocking.
    ///
    /// Registers the peer temporarily, probes each channel in [`ScanConfig::channels`] with the
    /// `0xF3` probe frame, and returns the channel the peer names in its `0xF4` reply, or, for a
    /// peer that only ACKs, the channel confirmed after the sweep (ADR 025).
    /// A reply to the outstanding probe is taken from the receive callback by the scan and never
    /// reaches [`receive`](Self::receive) or [`EspNowDriver::try_recv`]; a `0xF4` that arrives
    /// between probes is queued like any frame.
    /// The scan runs on the `espnow-tx` thread; this future only waits for it.
    ///
    /// On success the peer is left registered with the discovered channel, and the driver keeps
    /// that channel as its pin ([`pinned_channel`](Self::pinned_channel)).
    /// On failure, if a channel was found in a previous successful scan, the
    /// peer is re-registered on that channel and the radio is restored to it,
    /// so the next [`send_acked`](Self::send_acked) can attempt delivery without an extra
    /// failure round-trip.  If no prior channel is known, the peer registration is removed.
    ///
    /// # Supported radio modes
    ///
    /// Both driver-owned modes support scanning:
    ///
    /// - [`init_with_radio()`](Self::init_with_radio) — SoftAP-owned radio.
    ///   `esp_wifi_set_channel` triggers an immediate CSA with no associated
    ///   stations, so the channel-hop scan works exactly as in STA mode.
    /// - [`init_with_radio_sta()`](Self::init_with_radio_sta) — owned STA
    ///   radio.  Scanning competes with the background scan task and can
    ///   produce false positives on roaming peers; the
    ///   [`ScanConfig::probe_confirmations`] gating defends against this.
    ///
    /// The caller-managed [`init()`](Self::init) path is **not** supported:
    /// channel-hop scanning would break the AP association the caller is
    /// relying on.  Devices that share an AP with the peer do not need to
    /// scan in the first place — both radios are locked to the AP's channel.
    ///
    /// # Exclusion
    ///
    /// The scan holds the radio against application sends from the moment it is admitted until
    /// channel, TX power and peer registration are restored.
    /// Application sends during that time fail with [`EspIdfEspNowError::ScanInProgress`], and
    /// those still queued when the scan begins fail the same way, unsent.
    /// A frame already on the air is waited out before the first radio action.
    ///
    /// # Cancellation
    ///
    /// Dropping the future requests cancellation: the worker aborts the scan at its next step
    /// (after the probe on the air, or at once during a confirmation gap), restores the radio
    /// through the scan's own teardown, keeps the exclusion until that is done, and discards the
    /// result.
    /// The pin is left as it was unless the scan had already found the peer.
    ///
    /// # Side effects
    ///
    /// - Scanning hops the radio across [`ScanConfig::channels`] via
    ///   `esp_wifi_set_channel`, skipping the channels the country does not allow
    ///   ([`set_country`](Self::set_country)); a not-found result counts them as `not_allowed`.
    /// - Any prior peer registration for `mac` is removed before scanning so
    ///   that a stale entry does not abort the probe loop with
    ///   `ESP_ERR_ESPNOW_EXIST`.  The peer is left registered with the
    ///   discovered channel on success and removed on failure.
    /// - [`ScanConfig::probe_timeout`] bounds a probe's queue wait only: a probe that has not
    ///   started by then is never sent and counts as a local failure, while one that has started
    ///   is waited out however long its callback takes.
    ///
    /// # Errors
    ///
    /// * [`EspIdfEspNowError::RadioNotOwned`] for a driver created via [`init()`](Self::init) /
    ///   [`init_with_capacity()`](Self::init_with_capacity).
    /// * [`EspIdfEspNowError::Busy`] while another caller's scan runs or its result is untaken.
    ///   A scan started right after a cancelled one does not fail: it waits for the cancelled
    ///   scan's restore and then runs.
    /// * [`EspIdfEspNowError::Scan`] when the peer does not respond on any scanned channel, or
    ///   another scan failure.
    /// * [`EspIdfEspNowError::Driver`] with the ESP-IDF error behind a registration failure.
    ///
    /// # Blocking
    ///
    /// The future can wait without limit if a probe's send callback never fires (fail-closed:
    /// the channel must not change under a frame on the air), because
    /// [`ScanConfig::burst_timeout`] is checked between channels only.
    /// Dropping the future cancels the scan as described above, and so does dropping the driver.
    pub async fn scan_for_peer(
        &self,
        mac: &MacAddress,
        config: &ScanConfig<'_>,
    ) -> Result<ScanResult, EspIdfEspNowError> {
        worker::scan_for_peer(&self.worker, mac, config).await
    }

    /// Blocking form of [`scan_for_peer`](Self::scan_for_peer): the calling thread parks until
    /// the scan has finished.
    ///
    /// Do not call it on a thread that also polls [`EspNowLink`] futures of this driver: futures
    /// polled once and then left unpolled on that thread keep their queue slots, and a blocking
    /// call there can wait for a slot only that thread could free.
    /// The wait has no limit of its own: a probe is waited out however long its send callback
    /// takes (fail-closed), and [`ScanConfig::burst_timeout`] is checked between channels only,
    /// so a callback that never fires stalls the scan until the driver is dropped.
    ///
    /// Renamed from `scan_for_peer`, which is now the async form.
    /// The error is an [`anyhow::Error`] wrapping [`EspIdfEspNowError`] (or the ESP-IDF error
    /// behind it), so `downcast_ref` still reaches the variant.
    ///
    /// # Errors
    ///
    /// As [`scan_for_peer`](Self::scan_for_peer).
    pub fn scan_for_peer_blocking(
        &self,
        mac: &MacAddress,
        config: &ScanConfig<'_>,
    ) -> anyhow::Result<ScanResult> {
        block_on(self.scan_for_peer(mac, config)).map_err(EspIdfEspNowError::into_anyhow)
    }

    /// The channel the driver keeps pinned, if any.
    ///
    /// On a driver from [`init_with_radio`](Self::init_with_radio) it starts as the SoftAP's
    /// channel (1); afterwards it is the channel of the last successful scan.
    /// A failed or cancelled scan leaves it as it was.
    /// It is `None` on the other constructors until the first successful scan (the unassociated
    /// station of [`init_with_radio_sta`](Self::init_with_radio_sta) sets no channel, and the
    /// caller-managed radio of [`init`](Self::init) cannot scan).
    pub fn pinned_channel(&self) -> Option<u8> {
        self.worker.pinned_channel()
    }

    /// The primary Wi-Fi channel the radio is on right now (`esp_wifi_get_channel`), or `None`
    /// when the radio is not started.
    ///
    /// Unlike [`pinned_channel`](Self::pinned_channel) this is the live radio state: on a
    /// caller-managed radio ([`init`](Self::init)) it follows the access point, and during a scan
    /// it follows the probes.
    /// A coordinator answers a scan probe with it as the reply's `home_channel` (ADR 025).
    /// Same name and shape as `EspHalEspNow::current_channel` (ADR 022).
    pub fn current_channel(&self) -> Option<u8> {
        worker::current_channel()
    }

    /// Initialise ESP-NOW, start the radio, and scan for a peer.
    ///
    /// Convenience constructor combining [`init_with_radio()`](Self::init_with_radio)
    /// and [`scan_for_peer_blocking()`](Self::scan_for_peer_blocking).
    /// On success the driver is ready to communicate with the peer on the
    /// discovered channel.
    ///
    /// The scan waits without a limit for the send callback of a probe that has started
    /// (fail-closed, see [`scan_for_peer_blocking`](Self::scan_for_peer_blocking)): a callback
    /// that never fires blocks this constructor, and every later transmission, until the driver
    /// is dropped.
    ///
    /// The scan runs under [`Country::WORLD`] (channels 1-11); a peer on channel 12 or 13 needs
    /// [`init_with_radio_scanning_config`](Self::init_with_radio_scanning_config) with a
    /// country that allows it.
    pub fn init_with_radio_scanning(
        modem: Modem<'static>,
        sys_loop: esp_idf_svc::eventloop::EspSystemEventLoop,
        nvs: Option<esp_idf_svc::nvs::EspDefaultNvsPartition>,
        peer_mac: &MacAddress,
        scan_config: &ScanConfig<'_>,
    ) -> anyhow::Result<(Self, ScanResult)> {
        Self::init_with_radio_scanning_config(
            modem,
            sys_loop,
            nvs,
            EspNowConfig::default(),
            peer_mac,
            scan_config,
        )
    }

    /// [`init_with_radio_scanning`](Self::init_with_radio_scanning) started with
    /// [`init_with_radio_config`](Self::init_with_radio_config), so the scan runs under the
    /// country of `config`.
    pub fn init_with_radio_scanning_config(
        modem: Modem<'static>,
        sys_loop: esp_idf_svc::eventloop::EspSystemEventLoop,
        nvs: Option<esp_idf_svc::nvs::EspDefaultNvsPartition>,
        config: EspNowConfig,
        peer_mac: &MacAddress,
        scan_config: &ScanConfig<'_>,
    ) -> anyhow::Result<(Self, ScanResult)> {
        let driver = Self::init_with_radio_config(modem, sys_loop, nvs, config)?;
        let result = driver.scan_for_peer_blocking(peer_mac, scan_config)?;
        Ok((driver, result))
    }

    /// Waits for the next received frame.
    ///
    /// Frames above [`MAX_DATA_LEN`] never reach the queue: they are dropped and counted
    /// ([`oversize_dropped`](Self::oversize_dropped)).
    /// Neither does a `0xF4` probe reply from the peer a running scan has probed: the scan takes
    /// it (ADR 025); every other frame is delivered unchanged.
    ///
    /// Safe to call from several tasks or threads at once: each frame goes to exactly one
    /// consumer, and no order among waiting callers is promised.
    /// [`EspNowDriver::try_recv`] draws from the same queue, so a frame taken there is not
    /// seen here.
    /// Cancel-safe: dropping the future loses no frame.
    pub async fn receive(&self) -> EspNowEvent {
        self.rx.waiters.wait(|| self.rx.pop()).await
    }

    /// Number of oversize frames dropped by the receive callback (wraps at `u32::MAX`).
    pub fn oversize_dropped(&self) -> u32 {
        self.rx.oversize.dropped()
    }

    /// Sends `data` to `mac` and resolves on the MAC-layer ACK.
    ///
    /// `timeout` bounds waiting for a queue slot, queueing and the start of the transmission; a
    /// frame that has started is waited out and its real result returned, so a late callback is
    /// never taken for the next frame's ACK.
    /// A request that did not start in time is never sent.
    /// [`Duration::MAX`] means no bound.
    /// Dropping the future is safe: a queued frame is never sent, and one on the air has its
    /// result discarded.
    ///
    /// Fail-closed: the wait for a started frame's send callback has no limit, so a callback
    /// that never fires blocks this call and every later transmission until the driver is
    /// dropped (the callback normally arrives within a few milliseconds).
    ///
    /// # Errors
    ///
    /// * [`EspIdfEspNowError::PayloadTooLarge`] for a frame above [`MAX_DATA_LEN`].
    /// * [`EspIdfEspNowError::Broadcast`] for the broadcast address, which is never
    ///   acknowledged: use [`EspNowDriver::send`].
    /// * [`EspIdfEspNowError::ScanInProgress`] while a scan runs, or when one began before this
    ///   frame started.
    /// * [`EspIdfEspNowError::Timeout`] if the frame did not start within `timeout`.
    /// * [`EspIdfEspNowError::NoAck`] when the frame was transmitted and not acknowledged.
    /// * [`EspIdfEspNowError::Driver`] if ESP-IDF rejects the frame (unknown peer, ...).
    /// * [`EspIdfEspNowError::Shutdown`] if the driver is being dropped.
    pub async fn send_acked(
        &self,
        mac: &MacAddress,
        data: &[u8],
        timeout: Duration,
    ) -> Result<(), EspIdfEspNowError> {
        let outcome =
            worker::run_frame(&self.worker, mac, data, Some(timeout), SendKind::App).await?;
        worker::require_ack(outcome)
    }

    /// Send data and block until the MAC-layer ACK arrives.
    ///
    /// Unlike [`EspNowDriver::send`], which returns once the frame has left the radio, this
    /// reports whether the peer acknowledged it.
    ///
    /// Do not call it on a thread that also polls [`EspNowLink`] futures of this driver: futures
    /// polled once and then left unpolled on that thread keep their queue slots, and this call
    /// can wait for a slot only that thread could free (it ends with `Timeout` at `timeout_ms`).
    ///
    /// `timeout_ms` bounds waiting for the queue and the start of the transmission, so `0` or a
    /// very small value fails with [`EspIdfEspNowError::Timeout`] unless the `espnow-tx` thread
    /// starts the frame at once.
    /// A frame that has started is waited out, however long its callback takes (fail-closed: a
    /// callback that never fires blocks this call and every later transmission until the driver
    /// is dropped), and a frame the peer did not acknowledge fails with
    /// [`EspIdfEspNowError::NoAck`] (inside the returned [`anyhow::Error`], reachable with
    /// `downcast_ref`).
    /// During a scan the call fails at once with [`EspIdfEspNowError::ScanInProgress`], and a
    /// send to the broadcast address fails with [`EspIdfEspNowError::Broadcast`].
    ///
    /// Concurrent calls queue behind each other and behind [`EspNowDriver::send`] on the
    /// `espnow-tx` thread, so they cannot steal each other's ACKs.
    ///
    /// # Errors
    ///
    /// As [`send_acked`](Self::send_acked).
    pub fn send_and_wait(
        &self,
        mac: &MacAddress,
        data: &[u8],
        timeout_ms: u64,
    ) -> anyhow::Result<()> {
        block_on(self.send_acked(mac, data, Duration::from_millis(timeout_ms)))
            .map_err(EspIdfEspNowError::into_anyhow)
    }

    /// Stops the driver's asynchronous machinery without dropping it; idempotent, never blocks.
    ///
    /// Queued and in-flight requests end as [`EspIdfEspNowError::Shutdown`], a running scan is
    /// aborted without a teardown, and every later send or scan is refused with the same error.
    /// A frame already on the air still finishes and its result is ignored.
    /// The `espnow-tx` thread ends on its own; dropping the driver joins it and deinitialises
    /// ESP-NOW (the drop calls this first).
    /// Same name and semantics as `EspHalEspNow::shutdown` (ADR 022).
    pub fn shutdown(&self) {
        self.worker.shutdown();
    }

    fn init_inner(
        capacity: usize,
        wifi: Option<EspWifi<'static>>,
        mode: RadioMode,
        initial_pin: Option<u8>,
        allowed: Option<ChannelSet>,
    ) -> anyhow::Result<Self> {
        let esp_now = EspNow::take().context("failed to acquire EspNow singleton")?;

        let rx = Arc::new(RxQueue::new(capacity));

        // One send callback for the driver's lifetime; the worker is the only sender, so it
        // always belongs to the frame in flight.
        let worker = worker::spawn(mode, initial_pin)?;
        if let Some(allowed) = allowed {
            worker.set_allowed_channels(allowed);
        }

        // The scan's reply tap sees every frame first; what it does not take goes to the queue.
        let callback_queue = Arc::clone(&rx);
        let reply_tap = worker.events();
        esp_now
            .register_recv_cb(move |info, data| {
                if !reply_tap.probe_reply(info.src_addr, data) {
                    callback_queue.push_frame(*info.src_addr, data);
                }
            })
            .context("failed to register ESP-NOW receive callback")?;

        let events = worker.events();
        esp_now
            .register_send_cb(move |mac, status| {
                events.send_done(mac, matches!(status, SendStatus::SUCCESS));
            })
            .context("failed to register ESP-NOW send callback")?;

        Ok(Self {
            _esp_now: esp_now,
            rx,
            _wifi: wifi,
            worker,
            mode,
        })
    }
}

impl Drop for EspIdfEspNow {
    /// Stops the `espnow-tx` thread and joins it before `EspNow` is dropped.
    ///
    /// Queued and in-flight requests end as [`EspIdfEspNowError::Shutdown`]; the join does not
    /// depend on a send callback ever firing.
    /// The send callback owns an `Arc` of the shared state, so it can never outlive it.
    ///
    /// A scan interrupted by the drop leaves the channel, the TX power and the peer registration
    /// as they were at that moment (no probe may be on the air while they change); ESP-NOW is
    /// deinitialised right after.
    fn drop(&mut self) {
        self.worker.stop();
    }
}

/// Registers `config` as an ESP-NOW peer.
fn peer_add_raw(config: &PeerConfig) -> Result<(), EspError> {
    let peer_info = PeerInfo {
        peer_addr: config.mac,
        channel: config.channel,
        encrypt: config.encrypt,
        ifidx: match config.interface {
            juggler::espnow::WifiInterface::Sta => esp_idf_svc::sys::wifi_interface_t_WIFI_IF_STA,
            juggler::espnow::WifiInterface::Ap => esp_idf_svc::sys::wifi_interface_t_WIFI_IF_AP,
        },
        ..Default::default()
    };
    // SAFETY: plain FFI call; `peer_info` is a fully initialised `esp_now_peer_info_t` that
    // lives for the duration of the call, and ESP-NOW copies it.
    esp!(unsafe { esp_idf_svc::sys::esp_now_add_peer(&peer_info) })
}

/// [`peer_add_raw`] with the driver error the public API reports.
fn peer_add(config: &PeerConfig) -> Result<(), EspIdfEspNowError> {
    peer_add_raw(config)
        .context("failed to add ESP-NOW peer")
        .map_err(EspIdfEspNowError::Driver)
}

/// Removes the ESP-NOW peer `mac`.
fn peer_del_raw(mac: &MacAddress) -> Result<(), EspError> {
    // SAFETY: plain FFI call; `mac` points to 6 bytes that live for the duration of the call.
    esp!(unsafe { esp_idf_svc::sys::esp_now_del_peer(mac.as_ptr()) })
}

/// [`peer_del_raw`] with the driver error the public API reports.
fn peer_del(mac: &MacAddress) -> Result<(), EspIdfEspNowError> {
    peer_del_raw(mac)
        .context("failed to remove ESP-NOW peer")
        .map_err(EspIdfEspNowError::Driver)
}

impl EspNowDriver for EspIdfEspNow {
    type Error = EspIdfEspNowError;

    fn add_peer(&self, config: &PeerConfig) -> Result<(), Self::Error> {
        peer_add(config)
    }

    fn remove_peer(&self, mac: &MacAddress) -> Result<(), Self::Error> {
        peer_del(mac)
    }

    /// Sends a frame and returns once it has left the radio; the MAC-layer ACK is ignored (see
    /// [`EspIdfEspNow::send_acked`]).
    ///
    /// The frame goes through the `espnow-tx` thread, so the call waits for a frame already in
    /// flight, normally a few tens of milliseconds.
    /// Waiting for a queue slot and for the frame to start is bounded by [`SEND_QUEUE_DEADLINE`]
    /// (1 s), after which the call fails with [`EspIdfEspNowError::Timeout`]; a frame that has
    /// started is waited out, without limit if its send callback never fires (fail-closed).
    /// During a scan it fails with [`EspIdfEspNowError::ScanInProgress`].
    /// Broadcast frames are allowed.
    ///
    /// Do not call it on a thread that also polls [`EspNowLink`] futures of this driver: futures
    /// polled once and then left unpolled on that thread keep their queue slots until polled or
    /// dropped, so the call can only end with `Timeout`.
    fn send(&self, mac: &MacAddress, data: &[u8]) -> Result<(), Self::Error> {
        let outcome = block_on(worker::run_frame(
            &self.worker,
            mac,
            data,
            Some(SEND_QUEUE_DEADLINE),
            SendKind::Unacked,
        ))?;
        worker::ignore_ack(outcome)
    }

    fn try_recv(&self) -> Option<EspNowEvent> {
        self.rx.pop()
    }
}

impl EspNowLink for EspIdfEspNow {
    async fn send_acked(
        &self,
        mac: &MacAddress,
        data: &[u8],
        timeout: Duration,
    ) -> Result<(), Self::Error> {
        EspIdfEspNow::send_acked(self, mac, data, timeout).await
    }

    async fn receive(&self) -> EspNowEvent {
        EspIdfEspNow::receive(self).await
    }

    async fn scan_for_peer(
        &self,
        mac: &MacAddress,
        config: &ScanConfig<'_>,
    ) -> Result<ScanResult, Self::Error> {
        EspIdfEspNow::scan_for_peer(self, mac, config).await
    }

    fn pinned_channel(&self) -> Option<u8> {
        EspIdfEspNow::pinned_channel(self)
    }

    fn current_channel(&self) -> Option<u8> {
        EspIdfEspNow::current_channel(self)
    }

    fn shutdown(&self) {
        EspIdfEspNow::shutdown(self);
    }
}

/// Tier parity check (ADR 022): fails to compile if `EspIdfEspNow` stops implementing `EspNowLink`.
const _: fn() = || {
    fn implements<T: EspNowLink>()
    where
        <T as EspNowDriver>::Error: ClassifySend,
    {
    }
    implements::<EspIdfEspNow>();
    // The inherent `shutdown` keeps the name and shape of the esp-hal tier's (ADR 022).
    let _: fn(&EspIdfEspNow) = EspIdfEspNow::shutdown;
};
