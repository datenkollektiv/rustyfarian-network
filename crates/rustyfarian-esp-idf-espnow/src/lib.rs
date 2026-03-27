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

use std::sync::mpsc::{sync_channel, Receiver, SyncSender};

use anyhow::Context as _;
use esp_idf_svc::espnow::{EspNow, PeerInfo};
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::wifi::EspWifi;
pub use espnow_pure::{
    EspNowDriver, EspNowEvent, MacAddress, PeerConfig, ScanConfig, ScanResult, WifiInterface,
    BROADCAST_MAC, DEFAULT_RX_CHANNEL_CAPACITY, DEFAULT_SCAN_CHANNELS, MAX_DATA_LEN,
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

    /// Scan Wi-Fi channels to find one where the given peer responds.
    ///
    /// Registers the peer temporarily, probes each channel in
    /// [`ScanConfig::channels`] by sending [`ScanConfig::probe_data`], and
    /// returns the first channel where the peer ACKs the frame.
    ///
    /// On success the peer is left registered with the discovered channel.
    /// On failure any temporary peer registration is cleaned up.
    ///
    /// # Errors
    ///
    /// Returns an error if the peer does not respond on any scanned channel,
    /// or if the driver was not created via
    /// [`init_with_radio()`](Self::init_with_radio).
    pub fn scan_for_peer(
        &self,
        mac: &MacAddress,
        config: &ScanConfig<'_>,
    ) -> anyhow::Result<ScanResult> {
        anyhow::ensure!(
            self._wifi.is_some(),
            "scan_for_peer requires init_with_radio (driver must own the radio)"
        );

        let temp_peer = PeerConfig {
            mac: *mac,
            channel: 0,
            encrypt: false,
            interface: self.default_interface(),
        };
        self.add_peer(&temp_peer)
            .context("failed to register temporary peer for scanning")?;

        let result = self.scan_channels(mac, config);

        match &result {
            Ok(scan_result) => {
                let _ = self.remove_peer(mac);
                let final_peer = PeerConfig {
                    channel: scan_result.channel,
                    ..temp_peer
                };
                self.add_peer(&final_peer)
                    .context("failed to re-register peer on discovered channel")?;
                log::info!("Peer {:02X?} found on channel {}", mac, scan_result.channel);
            }
            Err(_) => {
                let _ = self.remove_peer(mac);
                log::warn!("Peer {:02X?} not found on any scanned channel", mac);
            }
        }

        result
    }

    /// Initialise ESP-NOW, start the radio, and scan for a peer.
    ///
    /// Convenience constructor combining [`init_with_radio()`](Self::init_with_radio)
    /// and [`scan_for_peer()`](Self::scan_for_peer).
    /// On success the driver is ready to communicate with the peer on the
    /// discovered channel.
    pub fn init_with_radio_scanning(
        modem: Modem<'static>,
        sys_loop: esp_idf_svc::eventloop::EspSystemEventLoop,
        nvs: Option<esp_idf_svc::nvs::EspDefaultNvsPartition>,
        peer_mac: &MacAddress,
        scan_config: &ScanConfig<'_>,
    ) -> anyhow::Result<(Self, ScanResult)> {
        let driver = Self::init_with_radio(modem, sys_loop, nvs)?;
        let result = driver.scan_for_peer(peer_mac, scan_config)?;
        Ok((driver, result))
    }

    fn scan_channels(
        &self,
        mac: &MacAddress,
        config: &ScanConfig<'_>,
    ) -> anyhow::Result<ScanResult> {
        for &channel in config.channels {
            log::debug!("Probing channel {} for peer {:02X?}", channel, mac);

            let ret = unsafe {
                esp_idf_svc::sys::esp_wifi_set_channel(
                    channel,
                    esp_idf_svc::sys::wifi_second_chan_t_WIFI_SECOND_CHAN_NONE,
                )
            };
            if ret != esp_idf_svc::sys::ESP_OK {
                log::warn!("Failed to set channel {}: error code {}", channel, ret);
                continue;
            }

            if self.send(mac, config.probe_data).is_ok() {
                return Ok(ScanResult { channel });
            }

            log::debug!("No ACK on channel {}", channel);
        }

        anyhow::bail!(
            "peer not found on any of the {} scanned channels",
            config.channels.len()
        )
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
