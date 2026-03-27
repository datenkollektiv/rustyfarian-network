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
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use esp_idf_svc::espnow::{EspNow, PeerInfo, SendStatus};
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::wifi::EspWifi;
pub use espnow_pure::{
    EspNowDriver, EspNowEvent, MacAddress, PeerConfig, ScanConfig, ScanResult, WifiInterface,
    BROADCAST_MAC, DEFAULT_PROBE_TIMEOUT, DEFAULT_RX_CHANNEL_CAPACITY, DEFAULT_SCAN_CHANNELS,
    MAX_DATA_LEN,
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
    /// Serialises ownership of the single global send callback that
    /// `esp_now_register_send_cb` exposes.  Both [`scan_for_peer`] and
    /// [`send_and_wait`] register their own send callback for the duration
    /// of the call; without this guard, a concurrent caller would either
    /// have its callback replaced or steal another caller's ACKs.
    send_cb_guard: Mutex<()>,
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
    /// The radio starts in STA mode — use [`WifiInterface::Sta`] for peers.
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
    /// Always returns [`WifiInterface::Sta`] because both [`init()`](Self::init)
    /// and [`init_with_radio()`](Self::init_with_radio) start the radio in
    /// STA mode.
    pub fn default_interface(&self) -> WifiInterface {
        WifiInterface::Sta
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
    /// # Side effects
    ///
    /// - Scanning hops the radio across [`ScanConfig::channels`] via
    ///   `esp_wifi_set_channel`.  Any active STA/AP connection on the device
    ///   will be disrupted while scanning is in progress, which is why this
    ///   method requires [`init_with_radio()`](Self::init_with_radio) (the
    ///   driver owns the radio and there is no concurrent AP association to
    ///   worry about).
    /// - Any prior peer registration for `mac` is removed before scanning so
    ///   that a stale entry does not abort the probe loop with
    ///   `ESP_ERR_ESPNOW_EXIST`.  The peer is left registered with the
    ///   discovered channel on success and removed on failure.
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
            "scan_for_peer requires init_with_radio (driver must own the radio); \
             scanning hops Wi-Fi channels and would break any active STA association"
        );

        // Use STA interface for scanning: init_with_radio() starts the radio
        // in STA mode, and esp_wifi_set_channel() operates on the STA interface.
        let temp_peer = PeerConfig {
            mac: *mac,
            channel: 0,
            encrypt: false,
            interface: WifiInterface::Sta,
        };

        // A previous successful scan leaves the peer registered on its
        // discovered channel.  esp_now_add_peer returns ESP_ERR_ESPNOW_EXIST
        // if the peer is already present, which would abort the scan before any
        // channel is probed.  Remove any stale registration so every scan
        // attempt starts from a clean slate.
        let _ = self.remove_peer(mac);

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
        let ack_status = AckStatus::new();

        let _cb = self.register_ack_cb(&ack_status, "scanning")?;

        for &channel in config.channels {
            log::debug!("Probing channel {} for peer {:02X?}", channel, mac);

            // SAFETY: esp_wifi_set_channel is an FFI call into the ESP-IDF
            // Wi-Fi subsystem, which is initialised by init_with_radio().
            // channel is a valid u8 (1-13).
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

            ack_status.reset();

            if self.send(mac, config.probe_data).is_err() {
                continue;
            }

            match ack_status.wait(config.probe_timeout) {
                Some(true) => return Ok(ScanResult { channel }),
                Some(false) => log::debug!("No ACK on channel {}", channel),
                None => log::debug!("Send timeout on channel {}", channel),
            }
        }

        anyhow::bail!(
            "peer not found on any of the {} scanned channels",
            config.channels.len()
        )
    }

    /// Send data and wait for the MAC-layer ACK.
    ///
    /// Unlike [`EspNowDriver::send`] which returns as soon as the frame is
    /// enqueued, this method blocks until the send callback confirms whether
    /// the peer ACKed the frame or not.
    ///
    /// Returns `Ok(())` on ACK, `Err` on NAK or timeout.
    ///
    /// This method temporarily owns the global send-completion callback;
    /// concurrent calls to `send_and_wait` or
    /// [`scan_for_peer`](Self::scan_for_peer) on the same driver are
    /// serialised internally so they cannot steal each other's ACKs.
    pub fn send_and_wait(
        &self,
        mac: &MacAddress,
        data: &[u8],
        timeout_ms: u64,
    ) -> anyhow::Result<()> {
        let ack_status = AckStatus::new();
        let _cb = self.register_ack_cb(&ack_status, "send_and_wait")?;

        ack_status.reset();
        self.send(mac, data)?;

        match ack_status.wait(Duration::from_millis(timeout_ms)) {
            Some(true) => Ok(()),
            Some(false) => anyhow::bail!("peer did not ACK"),
            None => anyhow::bail!("send ACK timeout after {} ms", timeout_ms),
        }
    }

    /// Take the send-callback guard, install an ACK-recording callback, and
    /// return a RAII scope that unregisters the callback (and releases the
    /// guard) on drop.
    fn register_ack_cb<'a>(
        &'a self,
        ack_status: &AckStatus,
        scope: &'static str,
    ) -> anyhow::Result<SendCbScope<'a>> {
        let guard = self
            .send_cb_guard
            .lock()
            .map_err(|_| anyhow::anyhow!("send_cb_guard poisoned"))?;

        let notify = ack_status.0.clone();
        self.esp_now
            .register_send_cb(move |_mac, status| {
                let (lock, cvar) = &*notify;
                let Ok(mut result) = lock.lock() else {
                    log::error!("{scope} ACK mutex poisoned — ignoring callback");
                    return;
                };
                *result = Some(matches!(status, SendStatus::SUCCESS));
                cvar.notify_one();
            })
            .with_context(|| format!("failed to register send callback for {scope}"))?;

        Ok(SendCbScope {
            esp_now: &self.esp_now,
            _guard: guard,
        })
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
            send_cb_guard: Mutex::new(()),
        })
    }
}

/// Shared state for awaiting a single MAC-layer ACK from the send callback.
struct AckStatus(Arc<(Mutex<Option<bool>>, Condvar)>);

impl AckStatus {
    fn new() -> Self {
        Self(Arc::new((Mutex::new(None), Condvar::new())))
    }

    fn reset(&self) {
        if let Ok(mut guard) = self.0 .0.lock() {
            *guard = None;
        }
    }

    /// Block until an ACK is recorded or `timeout` elapses.
    ///
    /// Loops to absorb spurious wakeups; returns `Some(true)` on ACK,
    /// `Some(false)` on NAK, `None` on timeout or poisoning.
    fn wait(&self, timeout: Duration) -> Option<bool> {
        let (lock, cvar) = &*self.0;
        let mut guard = lock.lock().ok()?;
        let deadline = Instant::now().checked_add(timeout)?;
        while guard.is_none() {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let remaining = deadline - now;
            match cvar.wait_timeout(guard, remaining) {
                Ok((g, _)) => guard = g,
                Err(_) => return None,
            }
        }
        *guard
    }
}

/// RAII scope holding the send-callback guard for the lifetime of an
/// ACK-aware operation.  Unregisters the callback on drop so subsequent
/// plain [`EspNowDriver::send`] calls are not affected.
struct SendCbScope<'a> {
    esp_now: &'a EspNow<'static>,
    _guard: std::sync::MutexGuard<'a, ()>,
}

impl Drop for SendCbScope<'_> {
    fn drop(&mut self) {
        let _ = self.esp_now.unregister_send_cb();
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
