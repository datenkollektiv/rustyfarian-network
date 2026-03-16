//! ESP-IDF driver implementing [`EspNowDriver`] via `esp-idf-svc`.
//!
//! # Usage
//!
//! ```rust,no_run
//! use rustyfarian_esp_idf_espnow::EspIdfEspNow;
//! use espnow_pure::{EspNowDriver, PeerConfig};
//!
//! let driver = EspIdfEspNow::init().unwrap();
//! let config = PeerConfig::new([0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
//! driver.add_peer(&config).unwrap();
//! driver.send(&config.mac, b"hello").unwrap();
//! ```

use std::sync::mpsc::{sync_channel, Receiver, SyncSender};

use anyhow::Context as _;
use esp_idf_svc::espnow::{EspNow, PeerInfo};
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::wifi::EspWifi;
pub use espnow_pure::{
    EspNowDriver, EspNowEvent, MacAddress, PeerConfig, WifiInterface, BROADCAST_MAC,
    DEFAULT_RX_CHANNEL_CAPACITY, MAX_DATA_LEN,
};

/// ESP-IDF implementation of [`EspNowDriver`].
///
/// Wraps [`EspNow<'static>`] and bridges the C receive callback into a
/// [`std::sync::mpsc::sync_channel`] for non-blocking polling.
///
/// # Radio management
///
/// - [`init()`](EspIdfEspNow::init) — caller owns the Wi-Fi radio;
///   the radio must already be started before calling this.
/// - [`init_with_radio()`](EspIdfEspNow::init_with_radio) — the driver
///   starts and owns the radio internally. Use this for ESP-NOW-only devices
///   that do not connect to a Wi-Fi AP.
pub struct EspIdfEspNow {
    esp_now: EspNow<'static>,
    rx: Receiver<EspNowEvent>,
    _wifi: Option<EspWifi<'static>>,
}

impl EspIdfEspNow {
    /// Initialise ESP-NOW with the default receive-queue capacity of
    /// [`DEFAULT_RX_CHANNEL_CAPACITY`] frames.
    ///
    /// The Wi-Fi radio must already be started by the caller.
    /// For ESP-NOW-only devices, use [`init_with_radio()`](Self::init_with_radio) instead.
    pub fn init() -> anyhow::Result<Self> {
        Self::init_inner(DEFAULT_RX_CHANNEL_CAPACITY, None)
    }

    /// Initialise ESP-NOW with a custom receive-queue capacity.
    ///
    /// The Wi-Fi radio must already be started by the caller.
    /// Frames received while the queue is full are dropped with a warning log.
    pub fn init_with_capacity(capacity: usize) -> anyhow::Result<Self> {
        Self::init_inner(capacity, None)
    }

    /// Initialise ESP-NOW and start the Wi-Fi radio internally.
    ///
    /// Use this for devices that need ESP-NOW without connecting to a Wi-Fi AP.
    /// The radio is kept alive for the lifetime of the returned driver.
    ///
    /// Peers should use [`WifiInterface::Ap`] (or call
    /// [`PeerConfig::with_ap_interface()`]) since there is no STA connection.
    /// See [`default_interface()`](Self::default_interface).
    pub fn init_with_radio(
        modem: Modem<'static>,
        sys_loop: esp_idf_svc::eventloop::EspSystemEventLoop,
        nvs: Option<esp_idf_svc::nvs::EspDefaultNvsPartition>,
    ) -> anyhow::Result<Self> {
        let mut wifi = EspWifi::new(modem, sys_loop, nvs)
            .context("failed to create EspWifi for ESP-NOW radio")?;
        wifi.start()
            .context("failed to start Wi-Fi radio for ESP-NOW")?;
        log::info!("Wi-Fi radio started for ESP-NOW (no AP connection)");

        Self::init_inner(DEFAULT_RX_CHANNEL_CAPACITY, Some(wifi))
    }

    /// Returns the recommended [`WifiInterface`] for peer configuration.
    ///
    /// - [`WifiInterface::Ap`] when the driver owns the radio
    ///   (created via [`init_with_radio()`](Self::init_with_radio) — no STA connection)
    /// - [`WifiInterface::Sta`] when the caller manages Wi-Fi
    ///   (created via [`init()`](Self::init) — STA is assumed)
    pub fn default_interface(&self) -> WifiInterface {
        if self._wifi.is_some() {
            WifiInterface::Ap
        } else {
            WifiInterface::Sta
        }
    }

    fn init_inner(capacity: usize, wifi: Option<EspWifi<'static>>) -> anyhow::Result<Self> {
        let esp_now = EspNow::take().context("failed to acquire EspNow singleton")?;

        let (tx, rx): (SyncSender<EspNowEvent>, Receiver<EspNowEvent>) = sync_channel(capacity);

        esp_now
            .register_recv_cb(move |info, data| {
                let mac = *info.src_addr;
                let event = EspNowEvent::new(mac, data);
                if tx.try_send(event).is_err() {
                    log::warn!(
                        "ESP-NOW receive queue full — frame from {:02X?} dropped",
                        mac
                    );
                }
            })
            .context("failed to register ESP-NOW receive callback")?;

        Ok(Self {
            esp_now,
            rx,
            _wifi: wifi,
        })
    }
}

impl EspNowDriver for EspIdfEspNow {
    type Error = anyhow::Error;

    fn add_peer(&self, config: &PeerConfig) -> anyhow::Result<()> {
        let peer_info = PeerInfo {
            peer_addr: config.mac,
            channel: config.channel,
            encrypt: config.encrypt,
            ifidx: match config.interface {
                espnow_pure::WifiInterface::Sta => esp_idf_svc::sys::wifi_interface_t_WIFI_IF_STA,
                espnow_pure::WifiInterface::Ap => esp_idf_svc::sys::wifi_interface_t_WIFI_IF_AP,
            },
            ..Default::default()
        };
        self.esp_now
            .add_peer(peer_info)
            .context("failed to add ESP-NOW peer")
    }

    fn remove_peer(&self, mac: &MacAddress) -> anyhow::Result<()> {
        self.esp_now
            .del_peer(*mac)
            .context("failed to remove ESP-NOW peer")
    }

    fn send(&self, mac: &MacAddress, data: &[u8]) -> anyhow::Result<()> {
        espnow_pure::validate_payload(data)
            .map_err(|e| anyhow::anyhow!(e))
            .context("payload validation failed")?;
        self.esp_now
            .send(*mac, data)
            .context("failed to send ESP-NOW frame")
    }

    fn try_recv(&self) -> Option<EspNowEvent> {
        self.rx.try_recv().ok()
    }
}
