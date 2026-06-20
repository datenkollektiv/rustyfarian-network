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

use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use esp_idf_svc::espnow::{EspNow, PeerInfo, SendStatus};
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::wifi::{AccessPointConfiguration, AuthMethod, Configuration, EspWifi};
pub use juggler::espnow::{
    EspNowDriver, EspNowEvent, MacAddress, PeerConfig, ScanConfig, ScanResult, WifiInterface,
    BROADCAST_MAC, DEFAULT_CONFIRMATION_GAP, DEFAULT_PROBE_CONFIRMATIONS, DEFAULT_PROBE_TIMEOUT,
    DEFAULT_RX_CHANNEL_CAPACITY, DEFAULT_SCAN_CHANNELS, MAX_DATA_LEN,
};

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
}

/// ESP-IDF implementation of [`EspNowDriver`].
///
/// Wraps [`EspNow<'static>`] and bridges the C receive callback into a
/// [`std::sync::mpsc::sync_channel`] for non-blocking polling.
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
    /// Explicit state for the three radio-management modes the driver supports.
    ///
    /// See [`RadioMode`] for the per-variant semantics.  All branching for
    /// `default_interface`, `scan_for_peer`, and `send_and_wait` is driven
    /// off this single field.
    mode: RadioMode,
    /// Last channel reported by a successful [`scan_for_peer`] call,
    /// reused by:
    ///
    /// - the `Err` branch of [`scan_for_peer`] itself, to restore the peer
    ///   and radio channel after a failed re-scan so the next
    ///   [`send_and_wait`] is not penalised by a stale channel + missing peer
    ///   registration; and
    /// - [`send_and_wait`] in [`RadioMode::OwnedStaPromisc`] mode, where the
    ///   STA background scanner drifts the channel and the promiscuous
    ///   bracket re-pins it before each frame.
    ///
    /// Lifecycle invariants:
    ///
    /// - Sentinel [`u8::MAX`] means "no channel ever discovered".
    /// - Written only by [`scan_for_peer`] on a successful scan.
    /// - Never cleared once set — a stale value will be overwritten on the
    ///   next successful scan; if a failed scan ever needs to invalidate it
    ///   explicitly, store [`u8::MAX`] here.
    /// - Has no effect in [`RadioMode::CallerManagedSta`] or
    ///   [`RadioMode::OwnedSoftAp`] beyond the failed-scan fallback path.
    pinned_channel: AtomicU8,
}

impl EspIdfEspNow {
    /// Initialise ESP-NOW with the default receive-queue capacity of
    /// [`DEFAULT_RX_CHANNEL_CAPACITY`] frames.
    ///
    /// The Wi-Fi radio must already be started by the caller.
    /// For ESP-NOW-only devices, use [`init_with_radio()`](Self::init_with_radio) instead.
    pub fn init() -> anyhow::Result<Self> {
        Self::init_inner(
            DEFAULT_RX_CHANNEL_CAPACITY,
            None,
            RadioMode::CallerManagedSta,
        )
    }

    /// Initialise ESP-NOW with a custom receive-queue capacity.
    ///
    /// The Wi-Fi radio must already be started by the caller.
    /// Frames received while the queue is full are dropped with a warning log.
    pub fn init_with_capacity(capacity: usize) -> anyhow::Result<Self> {
        Self::init_inner(capacity, None, RadioMode::CallerManagedSta)
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
    pub fn init_with_radio(
        modem: Modem<'static>,
        sys_loop: esp_idf_svc::eventloop::EspSystemEventLoop,
        nvs: Option<esp_idf_svc::nvs::EspDefaultNvsPartition>,
    ) -> anyhow::Result<Self> {
        let mut wifi = EspWifi::new(modem, sys_loop, nvs)
            .context("failed to create EspWifi for ESP-NOW radio")?;
        wifi.set_configuration(&Configuration::AccessPoint(AccessPointConfiguration {
            ssid_hidden: true,
            channel: 1,
            auth_method: AuthMethod::None,
            ..Default::default()
        }))
        .context("failed to configure SoftAP for ESP-NOW radio")?;
        wifi.start()
            .context("failed to start Wi-Fi SoftAP for ESP-NOW")?;
        log::info!("Wi-Fi SoftAP started for ESP-NOW (channel-stable, no AP connection)");

        Self::init_inner(
            DEFAULT_RX_CHANNEL_CAPACITY,
            Some(wifi),
            RadioMode::OwnedSoftAp,
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
    pub fn init_with_radio_sta(
        modem: Modem<'static>,
        sys_loop: esp_idf_svc::eventloop::EspSystemEventLoop,
        nvs: Option<esp_idf_svc::nvs::EspDefaultNvsPartition>,
    ) -> anyhow::Result<Self> {
        let mut wifi = EspWifi::new(modem, sys_loop, nvs)
            .context("failed to create EspWifi for ESP-NOW radio")?;
        wifi.start()
            .context("failed to start Wi-Fi radio for ESP-NOW")?;
        log::info!("Wi-Fi STA started for ESP-NOW (promiscuous-bracket channel re-pin)");

        Self::init_inner(
            DEFAULT_RX_CHANNEL_CAPACITY,
            Some(wifi),
            RadioMode::OwnedStaPromisc,
        )
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

    /// Scan Wi-Fi channels to find one where the given peer responds.
    ///
    /// Registers the peer temporarily, probes each channel in
    /// [`ScanConfig::channels`] by sending [`ScanConfig::probe_data`], and
    /// returns the first channel where the peer ACKs the frame.
    ///
    /// On success the peer is left registered with the discovered channel.
    /// On failure, if a channel was found in a previous successful scan, the
    /// peer is re-registered on that channel and the radio is restored to it,
    /// so the next [`send_and_wait`](Self::send_and_wait) can attempt delivery
    /// without an extra failure round-trip.  If no prior channel is known, the
    /// peer registration is removed.
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
    /// # Side effects
    ///
    /// - Scanning hops the radio across [`ScanConfig::channels`] via
    ///   `esp_wifi_set_channel`.
    /// - Any prior peer registration for `mac` is removed before scanning so
    ///   that a stale entry does not abort the probe loop with
    ///   `ESP_ERR_ESPNOW_EXIST`.  The peer is left registered with the
    ///   discovered channel on success and removed on failure.
    ///
    /// # Errors
    ///
    /// Returns an error if the peer does not respond on any scanned channel,
    /// or if the driver was created via [`init()`](Self::init) /
    /// [`init_with_capacity()`](Self::init_with_capacity).
    pub fn scan_for_peer(
        &self,
        mac: &MacAddress,
        config: &ScanConfig<'_>,
    ) -> anyhow::Result<ScanResult> {
        anyhow::ensure!(
            matches!(
                self.mode,
                RadioMode::OwnedSoftAp | RadioMode::OwnedStaPromisc
            ),
            "scan_for_peer requires init_with_radio or init_with_radio_sta \
             (driver must own the radio); scanning hops Wi-Fi channels and \
             would break any active AP association the caller relies on"
        );

        let temp_peer = PeerConfig {
            mac: *mac,
            channel: 0,
            encrypt: false,
            interface: self.default_interface(),
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
                self.pinned_channel
                    .store(scan_result.channel, Ordering::Relaxed);
                log::info!("Peer {:02X?} found on channel {}", mac, scan_result.channel);
            }
            Err(_) => {
                let _ = self.remove_peer(mac);
                log::warn!("Peer {:02X?} not found on any scanned channel", mac);

                // If a channel was discovered in a previous successful scan,
                // restore the peer registration and radio channel so the next
                // send_and_wait can attempt delivery without failing immediately
                // with "peer not found" or ESP_ERR_ESPNOW_CHAN.
                //
                // Without this, a failed re-scan removes the peer entry and
                // leaves the radio on the last-probed channel (e.g. ch 11),
                // causing the next send to fail before the frame is ever
                // transmitted — adding an extra failure round-trip before the
                // following re-scan can recover.
                let ch = self.pinned_channel.load(Ordering::Relaxed);
                if ch != u8::MAX {
                    let fallback = PeerConfig {
                        mac: *mac,
                        channel: ch,
                        encrypt: false,
                        interface: self.default_interface(),
                    };
                    // Restore the radio channel first so the peer registration is
                    // consistent with the physical channel.
                    // SAFETY: scan_for_peer is gated to owned-radio modes, so the
                    // radio is owned and running. ch is a valid 2.4 GHz channel
                    // written by a previous successful scan.
                    let set_ch_ret = unsafe {
                        esp_idf_svc::sys::esp_wifi_set_channel(
                            ch,
                            esp_idf_svc::sys::wifi_second_chan_t_WIFI_SECOND_CHAN_NONE,
                        )
                    };
                    if set_ch_ret != esp_idf_svc::sys::ESP_OK {
                        log::warn!(
                            "Failed to restore radio to last known channel {}: {:#x} \
                             (next send may target an inconsistent channel)",
                            ch,
                            set_ch_ret
                        );
                    }
                    if let Err(e) = self.add_peer(&fallback) {
                        log::warn!(
                            "Failed to fallback-register peer {:02X?} on channel {}: {:#} \
                             (next send_and_wait will fail until the next successful scan)",
                            mac,
                            ch,
                            e
                        );
                    } else {
                        log::debug!(
                            "Peer {:02X?} fallback-registered on last known channel {}",
                            mac,
                            ch
                        );
                    }
                }
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

    /// Scan the channels in `config` for the peer at `mac`.
    ///
    /// Note on radio side-effects: TX power is a global radio setting.
    /// While the burst is active, any concurrent Wi-Fi or ESP-NOW
    /// transmissions on this chip also go out at the boosted level.
    /// On dual-radio (Wi-Fi + ESP-NOW on the same chip) deployments,
    /// schedule scans during quiet periods if predictable per-frame
    /// power matters.
    fn scan_channels(
        &self,
        mac: &MacAddress,
        config: &ScanConfig<'_>,
    ) -> anyhow::Result<ScanResult> {
        let ack_status = AckStatus::new();

        let _cb = self.register_ack_cb(&ack_status, "scanning")?;

        // Auto-burst: boost TX power to maximum during channel scanning
        // to maximise discovery range, then restore the previous level.
        let mut saved_tx_power: i8 = 0;
        // SAFETY: esp_wifi_get_max_tx_power is an FFI call into the ESP-IDF
        // Wi-Fi subsystem, which is initialised by init_with_radio().
        // It writes the current TX power to the &mut i8 we pass in.
        let have_saved_power =
            unsafe { esp_idf_svc::sys::esp_wifi_get_max_tx_power(&mut saved_tx_power) }
                == esp_idf_svc::sys::ESP_OK;

        let burst_power = juggler::wifi::TxPowerLevel::Max.to_quarter_dbm();
        // SAFETY: esp_wifi_set_max_tx_power is an FFI call into the ESP-IDF
        // Wi-Fi subsystem, which is initialised by init_with_radio().
        // burst_power comes from TxPowerLevel::Max which is within the
        // valid [8, 84] quarter-dBm range required by the API.
        if unsafe { esp_idf_svc::sys::esp_wifi_set_max_tx_power(burst_power) }
            != esp_idf_svc::sys::ESP_OK
        {
            log::warn!("Failed to boost TX power for scanning — continuing at current level");
        }

        let scan_result = (|| -> anyhow::Result<ScanResult> {
            let burst_start = Instant::now();
            let mut probed = 0usize;

            for &channel in config.channels {
                if burst_start.elapsed() >= config.burst_timeout {
                    log::debug!(
                        "Burst timeout {:?} reached after {} channels; stopping scan",
                        config.burst_timeout,
                        probed
                    );
                    break;
                }

                log::debug!("Probing channel {} for peer {:02X?}", channel, mac);

                // SAFETY: esp_wifi_set_channel is an FFI call into the ESP-IDF
                // Wi-Fi subsystem, which is initialised by init_with_radio().
                // In SoftAP mode the channel change is a CSA with no connected
                // stations, so it takes effect immediately. channel is 1-13.
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

                probed += 1;

                match ack_status.wait(config.probe_timeout) {
                    Some(true) => {
                        // First probe ACKed.  One ACK alone may be a roaming
                        // peer transiently visiting this channel — require
                        // probe_confirmations additional ACKs spaced longer
                        // than a typical 802.11 scan dwell (observed at
                        // roughly 100 ms) to verify the peer has actually
                        // settled here.
                        let mut confirmed = true;
                        for confirmation in 0..config.probe_confirmations {
                            std::thread::sleep(config.confirmation_gap);
                            ack_status.reset();
                            if self.send(mac, config.probe_data).is_err() {
                                confirmed = false;
                                log::debug!(
                                    "Channel {} confirmation probe {} send failed",
                                    channel,
                                    confirmation + 1
                                );
                                break;
                            }
                            match ack_status.wait(config.probe_timeout) {
                                Some(true) => continue,
                                Some(false) => {
                                    log::debug!(
                                        "Channel {} unconfirmed: probe {} not ACKed \
                                         (likely roaming peer)",
                                        channel,
                                        confirmation + 1
                                    );
                                    confirmed = false;
                                    break;
                                }
                                None => {
                                    log::debug!(
                                        "Channel {} unconfirmed: probe {} timed out",
                                        channel,
                                        confirmation + 1
                                    );
                                    confirmed = false;
                                    break;
                                }
                            }
                        }
                        if confirmed {
                            return Ok(ScanResult { channel });
                        }
                        // Confirmation failed: continue to the next channel.
                    }
                    Some(false) => log::debug!("No ACK on channel {}", channel),
                    None => log::debug!("Send timeout on channel {}", channel),
                }
            }

            anyhow::bail!(
                "peer not found after probing {} of {} configured channels",
                probed,
                config.channels.len()
            )
        })();

        // Restore TX power after scanning regardless of outcome.
        // SAFETY: esp_wifi_set_max_tx_power is an FFI call into the ESP-IDF
        // Wi-Fi subsystem, which is initialised by init_with_radio().
        // saved_tx_power was previously read from esp_wifi_get_max_tx_power
        // (and only used here when have_saved_power is true), so the value
        // is guaranteed to be in the valid range the API itself produced.
        if have_saved_power
            && unsafe { esp_idf_svc::sys::esp_wifi_set_max_tx_power(saved_tx_power) }
                != esp_idf_svc::sys::ESP_OK
        {
            log::warn!("Failed to restore TX power after scanning");
        }

        scan_result
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

        match self.mode {
            RadioMode::OwnedStaPromisc => {
                let ch = self.pinned_channel.load(Ordering::Relaxed);
                if ch != u8::MAX {
                    self.send_with_promisc_repin(mac, data, ch)?;
                } else {
                    // No prior scan — no channel to re-pin to; fall through to the
                    // plain send path and let the caller's scan loop recover.
                    self.send(mac, data)?;
                }
            }
            RadioMode::OwnedSoftAp | RadioMode::CallerManagedSta => {
                // SoftAP: beacon schedule holds the channel — no re-pin needed.
                // Caller-managed: radio managed externally (typically AP-associated).
                self.send(mac, data)?;
            }
        }

        match ack_status.wait(Duration::from_millis(timeout_ms)) {
            Some(true) => Ok(()),
            Some(false) => anyhow::bail!("peer did not ACK"),
            None => anyhow::bail!("send ACK timeout after {} ms", timeout_ms),
        }
    }

    /// Re-pin the radio to `channel` and dispatch `esp_now_send` in a single
    /// `unsafe` block; used by [`send_and_wait`] in
    /// [`RadioMode::OwnedStaPromisc`] mode.
    ///
    /// `set_promiscuous(false)` and `esp_now_send` must be consecutive
    /// instructions to minimise the scheduler window in which the Wi-Fi
    /// background scan task (priority 23) can preempt the app task (priority 5)
    /// and drift the channel.  Even with the tightest possible bracket the race
    /// is not fully eliminated — see ADR 012 for the ~0–20 % `ESP_ERR_ESPNOW_CHAN`
    /// rate this path exhibits in production.
    fn send_with_promisc_repin(
        &self,
        mac: &MacAddress,
        data: &[u8],
        channel: u8,
    ) -> anyhow::Result<()> {
        juggler::espnow::validate_payload(data)
            .map_err(|e| anyhow::anyhow!(e))
            .context("payload validation failed")?;

        // SAFETY: this helper is only called from RadioMode::OwnedStaPromisc, so
        // the radio is owned and running. `channel` was written by a successful
        // scan_for_peer (the caller has already guarded against u8::MAX), so it
        // is a valid 2.4 GHz channel.  `mac` and `data` outlive this call.
        //
        // The tight bracket captures every intermediate `esp_err_t` so that a
        // channel-race or country-code failure surfaces as a specific log line
        // rather than an opaque "peer did not ACK" downstream.
        let (promisc_on_ret, set_ch_ret, promisc_off_ret, send_ret) = unsafe {
            let on = esp_idf_svc::sys::esp_wifi_set_promiscuous(true);
            let set = esp_idf_svc::sys::esp_wifi_set_channel(
                channel,
                esp_idf_svc::sys::wifi_second_chan_t_WIFI_SECOND_CHAN_NONE,
            );
            let off = esp_idf_svc::sys::esp_wifi_set_promiscuous(false);
            let send = esp_idf_svc::sys::esp_now_send(mac.as_ptr(), data.as_ptr(), data.len());
            (on, set, off, send)
        };

        if promisc_on_ret != esp_idf_svc::sys::ESP_OK {
            log::warn!(
                "esp_wifi_set_promiscuous(true) failed: {:#x} \
                 (channel re-pin skipped, send proceeded on previous channel)",
                promisc_on_ret
            );
        }
        if set_ch_ret != esp_idf_svc::sys::ESP_OK {
            log::warn!(
                "esp_wifi_set_channel({}) failed: {:#x} \
                 (frame sent on whatever channel the radio was on)",
                channel,
                set_ch_ret
            );
        }
        if promisc_off_ret != esp_idf_svc::sys::ESP_OK {
            log::warn!(
                "esp_wifi_set_promiscuous(false) failed: {:#x} \
                 (radio left in promiscuous mode)",
                promisc_off_ret
            );
        }
        if send_ret != esp_idf_svc::sys::ESP_OK {
            anyhow::bail!("esp_now_send failed: {:#x}", send_ret);
        }
        Ok(())
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

    fn init_inner(
        capacity: usize,
        wifi: Option<EspWifi<'static>>,
        mode: RadioMode,
    ) -> anyhow::Result<Self> {
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
            mode,
            pinned_channel: AtomicU8::new(u8::MAX),
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
                juggler::espnow::WifiInterface::Sta => {
                    esp_idf_svc::sys::wifi_interface_t_WIFI_IF_STA
                }
                juggler::espnow::WifiInterface::Ap => esp_idf_svc::sys::wifi_interface_t_WIFI_IF_AP,
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
        juggler::espnow::validate_payload(data)
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
