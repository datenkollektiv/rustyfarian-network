//! Wi-Fi connection manager for ESP-IDF projects.
//!
//! Provides a simplified wrapper around the ESP-IDF Wi-Fi client with:
//! - Automatic connection handling with timeout
//! - Optional LED status indicator via the `StatusLed` trait from `pennant`
//! - IP address acquisition with polling
//!
//! # Example
//!
//! ```ignore
//! use rustyfarian_esp_idf_network::wifi::{WiFiManager, WiFiConfig, WiFiConfigExt};
//!
//! let config = WiFiConfig::new("MyNetwork", "password123")
//!     .with_peripherals(peripherals.modem, sys_loop, Some(nvs));
//! let wifi = WiFiManager::init(config)?;
//!
//! if let Some(ip) = wifi.get_ip(10000)? {
//!     println!("Connected with IP: {}", ip);
//! }
//! ```
//!
//! # Non-Blocking Connection
//!
//! For firmware that must remain interactive during Wi-Fi association:
//!
//! ```ignore
//! use rustyfarian_esp_idf_network::wifi::{WiFiManager, WiFiConfig};
//!
//! let config = WiFiConfig::new("MyNetwork", "password123")
//!     .connect_nonblocking();
//! let wifi = WiFiManager::new_without_led(modem, sys_loop, Some(nvs), config)?;
//!
//! // init() returns immediately; the manager connects and reconnects in the background.
//! if let Some(ip) = wifi.get_ip(10000)? {
//!     println!("Connected with IP: {}", ip);
//! }
//! ```
//!
//! # Reconnect
//!
//! Both modes connect and reconnect through one worker (1 s back-off doubling to 30 s,
//! shared with the esp-hal tier through `juggler::wifi::LinkMachine`); the blocking
//! modes also retry a failed first association within their timeout, and
//! `is_connected()` is `false` for the whole outage.
//!
//! # Country
//!
//! [`WiFiManager`] and [`SoftApManager`] apply [`WiFiConfig::country`] /
//! [`ApConfig::country`] before the radio starts; the default [`Country::WORLD`]
//! allows channels 1-11.
//! Set the country of the network (`.with_country(Country::parse("DE")?)`) to join
//! an AP on channel 12 or 13 without 802.11d, or to run a SoftAP there.
//! With 802.11d on (the default) the associated station adopts its AP's country and
//! falls back to the configured one after a disconnect.
//! ESP-IDF keeps the country in flash; the library re-applies it on every boot.
//!
//! # Suspend and deep sleep
//!
//! `WiFiManager::suspend()` disconnects and stops the radio inline and pauses
//! reconnects; `resume()` starts it and connects again with a fresh back-off.
//! Both take `&self`, are idempotent and return within milliseconds. Call
//! `suspend()` right before `esp_deep_sleep_start`; an aborted sleep simply calls
//! `resume()`. The MQTT client is not told: it sends no DISCONNECT, so the broker
//! publishes its will after 1.5 × keep-alive (see the `mqtt` module docs).
//!
//! # Using a Simple GPIO LED
//!
//! For boards with a simple on/off LED instead of an RGB LED, use `SimpleLed`:
//!
//! ```ignore
//! use rustyfarian_esp_idf_network::wifi::{WiFiManager, WiFiConfig, SimpleLed};
//! use esp_idf_hal::gpio::PinDriver;
//!
//! let pin = PinDriver::output(peripherals.pins.gpio8)?;
//! let mut led = SimpleLed::new(pin);
//!
//! let config = WiFiConfig::new("MyNetwork", "password123");
//! let wifi = WiFiManager::new(modem, sys_loop, Some(nvs), config, Some(&mut led))?;
//! ```

use core::net::Ipv4Addr;
use std::thread;
use std::time::Duration;

use anyhow::Context as _;

use esp_idf_svc::eventloop::{EspSystemEventLoop, EspSystemSubscription};
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::handle::RawHandle as _;
use esp_idf_svc::netif::IpEvent;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::sys::{esp, EspError};
use esp_idf_svc::wifi::{
    BlockingWifi, ClientConfiguration, Configuration, EspWifi, ScanMethod, ScanSortMethod,
    WifiEvent,
};
use juggler::wifi::lines::{Disconnected, NetworkReady};
use juggler::wifi::{JoinedAp, LinkEvent, LinkRadio, LinkSupervisor, NetworkLink};
// Only the SoftAP path uses these; gate the import so a SoftAP-disabled build
// (where `SoftApManager` is cfg'd out) does not warn on unused imports.
#[cfg(esp_idf_esp_wifi_softap_support)]
use esp_idf_svc::wifi::{AccessPointConfiguration, AuthMethod};
use pennant::PulseEffect;
use rgb::RGB8;

// Re-export all pure types from juggler::wifi
// `WifiDriver` is deprecated (ADR 022) but stays re-exported until it is removed.
#[allow(deprecated)]
pub use juggler::wifi::{
    credentials_hint, reason_suggests_credentials, validate_ap_config, validate_password,
    validate_ssid, wifi_disconnect_reason_name, ApConfig, ApSelection, ChannelSet, ConnectMode,
    Country, CountryError, DisconnectReason, LinkAction, LinkInput, LinkMachine, LinkState,
    Regulatory, TxPowerLevel, WiFiConfig, WifiDriver, WifiPowerSave, AP_MAX_CONNECTIONS_DEFAULT,
    AP_PASSWORD_MIN_LEN, DEFAULT_TIMEOUT_SECS, PASSWORD_MAX_LEN, POLL_INTERVAL_MS,
    RECONNECT_BACKOFF_BASE_MS, RECONNECT_BACKOFF_MAX_MS, SSID_MAX_LEN, STATION_TX_POWER_DEFAULT,
};

// Re-export StatusLed and SimpleLed from pennant for convenience
pub use pennant::{SimpleLed, StatusLed};

use juggler::status_colors;

/// Stack of the link worker: it only calls `esp_wifi_connect` and formats log lines.
const LINK_WORKER_STACK_SIZE: usize = 4096;

/// ESP-IDF Wi-Fi configuration bundled with the hardware peripherals needed for init.
///
/// Built from a [`WiFiConfig`] via [`with_peripherals`][WiFiConfigExt::with_peripherals],
/// then passed to [`WiFiManager::init`].
pub struct IdfWifiConfig<'a> {
    ssid: &'a str,
    password: &'a str,
    connect_mode: ConnectMode,
    power_save: WifiPowerSave,
    tx_power: TxPowerLevel,
    ap_selection: ApSelection,
    country: Country,
    modem: Modem<'static>,
    sys_loop: EspSystemEventLoop,
    nvs: Option<EspDefaultNvsPartition>,
}

/// Extension trait that adds [`with_peripherals`][WiFiConfigExt::with_peripherals]
/// to [`WiFiConfig`], producing an [`IdfWifiConfig`] ready for [`WiFiManager::init`].
pub trait WiFiConfigExt<'a> {
    /// Bundles this configuration with the ESP-IDF peripherals required for Wi-Fi.
    fn with_peripherals(
        self,
        modem: Modem<'static>,
        sys_loop: EspSystemEventLoop,
        nvs: Option<EspDefaultNvsPartition>,
    ) -> IdfWifiConfig<'a>;
}

impl<'a> WiFiConfigExt<'a> for WiFiConfig<'a> {
    fn with_peripherals(
        self,
        modem: Modem<'static>,
        sys_loop: EspSystemEventLoop,
        nvs: Option<EspDefaultNvsPartition>,
    ) -> IdfWifiConfig<'a> {
        IdfWifiConfig {
            ssid: self.ssid,
            password: self.password,
            connect_mode: self.connect_mode,
            power_save: self.power_save,
            tx_power: self.tx_power,
            ap_selection: self.ap_selection,
            country: self.country,
            modem,
            sys_loop,
            nvs,
        }
    }
}

/// Maps the shared [`ApSelection`] to the station scan, so the driver default never decides.
fn map_ap_selection(selection: ApSelection) -> ScanMethod {
    match selection {
        ApSelection::Strongest => ScanMethod::CompleteScan(ScanSortMethod::Signal),
        ApSelection::FirstFound => ScanMethod::FastScan,
    }
}

/// The AP the station is associated with, for the `link up` line; `None` when not associated.
fn joined_ap() -> Option<JoinedAp> {
    // SAFETY: an all-zero `wifi_ap_record_t` is a valid value of this plain C struct.
    let mut record: esp_idf_svc::sys::wifi_ap_record_t = unsafe { core::mem::zeroed() };
    // SAFETY: `record` is a valid, writable `wifi_ap_record_t`; the call only fills it.
    let rc = unsafe { esp_idf_svc::sys::esp_wifi_sta_get_ap_info(&mut record) };
    (rc == esp_idf_svc::sys::ESP_OK).then_some(JoinedAp {
        bssid: record.bssid,
        channel: record.primary,
        rssi: record.rssi,
    })
}

/// A no-op LED implementation used by [`WiFiManager::new_without_led`].
struct NoLed;

impl StatusLed for NoLed {
    type Error = std::convert::Infallible;

    fn set_color(&mut self, _color: RGB8) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// Wi-Fi connection manager with optional LED status feedback.
///
/// # Reconnect
///
/// In every [`ConnectMode`] a [`LinkMachine`] on a small worker thread issues the
/// first `esp_wifi_connect` and every reconnect: each `StaDisconnected` event is
/// logged with its reason and answered with a 1 s back-off that doubles up to
/// 30 s and resets on `StaConnected`. The blocking modes only wait for the link
/// until their timeout, so a failed first association is retried within it.
/// [`is_connected`](Self::is_connected) follows the supervisor's
/// [`state`](Self::state), so it is `false` for the whole outage.
///
/// [`suspend`](Self::suspend) and [`resume`](Self::resume) park and restart the
/// station within the same boot, for example around deep sleep.
///
/// Dropping the manager stops and joins the worker before the driver goes away.
pub struct WiFiManager {
    // Declared before `wifi`: fields drop in order, and the worker must be joined
    // before the driver it calls into is destroyed.
    link: LinkWorker,
    wifi: BlockingWifi<EspWifi<'static>>,
    /// Tasks parked in [`NetworkLink::wait_suspended`]; woken once a suspend took effect.
    suspend_wakers: std::sync::Mutex<Vec<core::task::Waker>>,
}

impl WiFiManager {
    /// Initializes Wi-Fi from a bundled configuration — the recommended entry point.
    ///
    /// Mirrors the bare-metal crate's `WiFiManager::init_async` API
    /// so both ESP-IDF and esp-hal projects use the same builder flow:
    ///
    /// ```ignore
    /// let config = WiFiConfig::new("MyNetwork", "password123")
    ///     .with_peripherals(peripherals.modem, sys_loop, Some(nvs));
    /// let wifi = WiFiManager::init(config)?;
    /// ```
    pub fn init(config: IdfWifiConfig<'_>) -> anyhow::Result<Self> {
        let wifi_config = WiFiConfig {
            ssid: config.ssid,
            password: config.password,
            connect_mode: config.connect_mode,
            power_save: config.power_save,
            tx_power: config.tx_power,
            ap_selection: config.ap_selection,
            country: config.country,
        };
        Self::new_without_led(config.modem, config.sys_loop, config.nvs, wifi_config)
    }

    /// Creates a new Wi-Fi manager and connects to the network.
    ///
    /// # Arguments
    ///
    /// * `modem` - ESP-IDF modem peripheral
    /// * `sys_loop` - ESP-IDF system event loop
    /// * `nvs` - Optional NVS partition for storing WiFi credentials
    /// * `config` - Wi-Fi connection configuration
    /// * `led` - Optional status LED for visual feedback
    ///
    /// # LED Behavior
    ///
    /// If an LED is provided:
    /// - Blue pulse: Connecting
    /// - Red pulse: Connection timeout (loops forever)
    /// - Green: Connected successfully
    pub fn new<L>(
        modem: Modem<'static>,
        sys_loop: EspSystemEventLoop,
        nvs: Option<EspDefaultNvsPartition>,
        config: WiFiConfig<'_>,
        led: Option<&mut L>,
    ) -> anyhow::Result<Self>
    where
        L: StatusLed,
        L::Error: std::fmt::Debug,
    {
        log::info!("[wifi] connecting to ssid (len={})", config.ssid.len());

        // Clone before sys_loop is consumed by BlockingWifi::wrap; used for the
        // optional disconnect-event subscription in non-blocking mode.
        let sys_loop_sub = sys_loop.clone();
        let mut wifi = BlockingWifi::wrap(EspWifi::new(modem, sys_loop.clone(), nvs)?, sys_loop)?;

        // The driver is initialised but not started: the country decides the channels the
        // first scan may use.
        crate::regulatory::apply(config.country)?;

        validate_ssid(config.ssid)
            .map_err(|e| anyhow::anyhow!("WiFi SSID invalid (len={}): {}", config.ssid.len(), e))?;
        validate_password(config.password).map_err(anyhow::Error::msg)?;

        let ssid = config.ssid.try_into().map_err(|_| {
            anyhow::anyhow!(
                "internal: SSID conversion failed after validation (len={}, limit={})",
                config.ssid.len(),
                SSID_MAX_LEN
            )
        })?;
        let password = config.password.try_into().map_err(|_| {
            anyhow::anyhow!(
                "internal: password conversion failed after validation (len={}, limit={})",
                config.password.len(),
                PASSWORD_MAX_LEN
            )
        })?;

        wifi.set_configuration(&Configuration::Client(ClientConfiguration {
            ssid,
            password,
            scan_method: map_ap_selection(config.ap_selection),
            ..Default::default()
        }))?;

        wifi.start()?;

        let ps_mode = match config.power_save {
            WifiPowerSave::None => esp_idf_svc::sys::wifi_ps_type_t_WIFI_PS_NONE,
            WifiPowerSave::MinModem => esp_idf_svc::sys::wifi_ps_type_t_WIFI_PS_MIN_MODEM,
            WifiPowerSave::MaxModem => esp_idf_svc::sys::wifi_ps_type_t_WIFI_PS_MAX_MODEM,
        };
        esp_idf_svc::sys::esp!(unsafe { esp_idf_svc::sys::esp_wifi_set_ps(ps_mode) })
            .context("failed to set WiFi power save mode")?;
        log::info!("[wifi] started, power save: {:?}", config.power_save);

        let tx_power = config.tx_power.to_quarter_dbm();
        match esp_idf_svc::sys::esp!(unsafe {
            esp_idf_svc::sys::esp_wifi_set_max_tx_power(tx_power)
        }) {
            Ok(()) => log::info!(
                "[wifi] tx power set to {:?} ({} quarter-dBm)",
                config.tx_power,
                tx_power
            ),
            Err(e) => log::warn!(
                "[wifi] failed to set tx power to {:?} ({} quarter-dBm), continuing at radio default: {:?}",
                config.tx_power,
                tx_power,
                e
            ),
        }

        // Reconnect supervision: the worker owns the `LinkMachine` and issues every
        // connect, the first one included, in all modes. Declared after `wifi`, so
        // on an early return it is joined before the driver is dropped.
        let link = LinkWorker::start(&sys_loop_sub, wifi.wifi().sta_netif().handle())?;
        if !link.supervisor.start() {
            anyhow::bail!("WiFi link worker is not running");
        }

        let has_indicator = led.is_some();
        if matches!(config.connect_mode, ConnectMode::NonBlocking) && has_indicator {
            log::warn!(
                "[wifi] non-blocking connection requested but LED driver is present; \
                 falling back to blocking with {}s timeout",
                DEFAULT_TIMEOUT_SECS
            );
        }
        match (
            config.connect_mode.effective_timeout_secs(has_indicator),
            led,
        ) {
            (Some(timeout_secs), Some(led_driver)) => {
                Self::connect_with_led(&link, led_driver, timeout_secs)?;
            }
            (Some(timeout_secs), None) => Self::wait_for_connection(&link, timeout_secs)?,
            (None, _) => log::info!("[wifi] connect requested (non-blocking)"),
        }

        Ok(Self {
            link,
            wifi,
            suspend_wakers: std::sync::Mutex::new(Vec::new()),
        })
    }

    /// Creates a new Wi-Fi manager without an LED driver.
    ///
    /// Equivalent to calling [`WiFiManager::new`] with `None` for the LED,
    /// but without the need for a type annotation on the `None` argument.
    pub fn new_without_led(
        modem: Modem<'static>,
        sys_loop: EspSystemEventLoop,
        nvs: Option<EspDefaultNvsPartition>,
        config: WiFiConfig<'_>,
    ) -> anyhow::Result<Self> {
        Self::new::<NoLed>(modem, sys_loop, nvs, config, None)
    }

    fn connect_with_led<L>(link: &LinkWorker, led: &mut L, timeout_secs: u64) -> anyhow::Result<()>
    where
        L: StatusLed,
        L::Error: std::fmt::Debug,
    {
        let mut pulse_effect = PulseEffect::new();
        let start_time = std::time::Instant::now();
        let timeout = Duration::from_secs(timeout_secs);

        log::info!("[wifi] connecting...");

        // The link worker issues (and retries) the connect; this loop only waits.
        loop {
            // Check for timeout
            if start_time.elapsed() >= timeout {
                log::error!("[wifi] connection timeout after {} seconds", timeout_secs);

                // Brief red pulse to indicate failure before returning error
                for _ in 0..20 {
                    led.set_color(pulse_effect.update(status_colors::ERROR))
                        .map_err(|e| anyhow::anyhow!("LED error: {:?}", e))?;
                    thread::sleep(Duration::from_millis(50));
                }

                return Err(anyhow::anyhow!(
                    "Wi-Fi connection timeout after {} seconds",
                    timeout_secs
                ));
            }

            // Ready means associated and the station's IP reported (supervisor).
            if link.supervisor.is_ready() {
                log::info!("[wifi] connected and netif up");
                break;
            }

            // Pulse blue light
            led.set_color(pulse_effect.update(status_colors::WIFI_CONNECTING))
                .map_err(|e| anyhow::anyhow!("LED error: {:?}", e))?;
            thread::sleep(Duration::from_millis(50));
        }

        led.set_color(RGB8::from(status_colors::CONNECTED))
            .map_err(|e| anyhow::anyhow!("LED error: {:?}", e))?;

        Ok(())
    }

    /// Internal helper to wait for connection with a timeout (no LED).
    fn wait_for_connection(link: &LinkWorker, timeout_secs: u64) -> anyhow::Result<()> {
        let start_time = std::time::Instant::now();
        let timeout = Duration::from_secs(timeout_secs);

        log::info!("[wifi] connecting (timeout: {}s)...", timeout_secs);

        // The link worker issues (and retries) the connect; this loop only waits.
        loop {
            if start_time.elapsed() >= timeout {
                return Err(anyhow::anyhow!(
                    "Wi-Fi connection timeout after {} seconds",
                    timeout_secs
                ));
            }

            // Ready means associated and the station's IP reported (supervisor).
            if link.supervisor.is_ready() {
                break;
            }

            thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
        }

        log::info!("[wifi] connected and netif up");

        Ok(())
    }

    /// Completes once the station is associated and has its IP address; the same
    /// call as on the esp-hal tier (ADR 022). Read the address with [`ip`](Self::ip).
    ///
    /// Has no timeout of its own; blocking code uses
    /// [`wait_connected_timeout`](Self::wait_connected_timeout).
    pub async fn wait_connected(&self) {
        self.wait_ready().await
    }

    /// Blocks until the Wi-Fi station is associated and an IP address is
    /// assigned via DHCP.
    ///
    /// Returns the assigned IPv4 address, or `Err` if the timeout elapses.
    /// Named `wait_connected(timeout_ms)` before 0.8.0; the async
    /// [`wait_connected`](Self::wait_connected) took that name for tier parity.
    pub fn wait_connected_timeout(&self, timeout_ms: u64) -> anyhow::Result<Ipv4Addr> {
        self.get_ip(timeout_ms)?
            .ok_or_else(|| anyhow::anyhow!("Wi-Fi connection timeout after {} ms", timeout_ms))
    }

    /// Waits for an IP address to be assigned.
    ///
    /// # Arguments
    ///
    /// * `timeout_ms` - Maximum time to wait in milliseconds
    ///
    /// # Returns
    ///
    /// The assigned IPv4 address, or `None` if timeout expires.
    ///
    /// Waits for the readiness the reconnect supervisor tracks (association plus
    /// the station's IP event, see [`is_ready`](Self::is_ready)), then reads the
    /// address. A transient ESP-IDF error reading it is logged at `debug` level
    /// and treated as "not ready yet" rather than propagated to the caller.
    pub fn get_ip(&self, timeout_ms: u64) -> anyhow::Result<Option<Ipv4Addr>> {
        let start = std::time::Instant::now();
        let timeout = Duration::from_millis(timeout_ms);

        loop {
            if let Some(ip) = self.ip() {
                log::debug!("[wifi] ip {}", ip);
                return Ok(Some(ip));
            }

            if start.elapsed() >= timeout {
                log::warn!("[wifi] timeout waiting for ip address");
                return Ok(None);
            }

            thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
        }
    }

    /// The station's IPv4 address while [`is_ready`](Self::is_ready), else `None`.
    ///
    /// A transient ESP-IDF error reading the address is logged at `debug` level
    /// and reported as `None`.
    pub fn ip(&self) -> Option<Ipv4Addr> {
        if !self.link.supervisor.is_ready() {
            return None;
        }
        match self.wifi.wifi().sta_netif().get_ip_info() {
            Ok(ip_info) if !ip_info.ip.is_unspecified() => Some(ip_info.ip),
            Ok(_) => None,
            Err(e) => {
                log::debug!("get_ip_info transient error: {}", e);
                None
            }
        }
    }

    /// `true` while the station is associated ([`LinkState::Connected`]); the same
    /// call as on the esp-hal tier and the `NetworkLink` default.
    pub fn is_connected(&self) -> bool {
        self.state() == LinkState::Connected
    }

    /// The reconnect supervisor's view of the link, including
    /// [`LinkState::Suspended`]; infallible, for rendering a status.
    pub fn state(&self) -> LinkState {
        self.link.supervisor.state()
    }

    /// Former name of [`state`](Self::state).
    #[deprecated(
        since = "0.7.0",
        note = "use `state()`, the name both tiers share (ADR 022)"
    )]
    pub fn link_state(&self) -> LinkState {
        self.state()
    }

    /// `true` while the station is associated and has its IP address.
    pub fn is_ready(&self) -> bool {
        self.link.supervisor.is_ready()
    }

    /// Registers a sink told once whenever [`is_ready`](Self::is_ready) flips,
    /// for a worker that must gate its own work on the link — the MQTT session
    /// uses it instead of polling or a second event subscription.
    ///
    /// Returns the readiness at the moment of registration, taken under the same
    /// lock as the registration, so no transition can slip between the two:
    /// every change is either in the returned value or delivered to the sink.
    /// Transitions that happened earlier are therefore not replayed.
    /// The sink runs on the reconnect worker, or on the caller of
    /// [`suspend`](Self::suspend), while the link state is locked — it must
    /// return at once. Send to a channel; do not block or call back into this
    /// manager, and expect the sink to stay registered for good.
    pub fn subscribe_ready<F>(&self, sink: F) -> bool
    where
        F: Fn(bool) + Send + 'static,
    {
        self.link.supervisor.subscribe_ready(sink)
    }

    /// `true` once a [`suspend`](Self::suspend) has taken effect: the radio is
    /// stopped. Stays `false` after a suspend whose stop failed.
    pub fn is_suspended(&self) -> bool {
        self.link.supervisor.is_suspended()
    }

    /// Completes once a [`suspend`](Self::suspend) has stopped the radio, and stays
    /// pending while nothing is suspended (the `NetworkLink` contract), also after a
    /// suspend whose stop failed.
    /// `suspend` stops the radio before it returns, so after a successful suspend
    /// this completes at once.
    pub async fn wait_suspended(&self) {
        core::future::poll_fn(|cx| {
            if self.is_suspended() {
                return core::task::Poll::Ready(());
            }
            let mut wakers = self
                .suspend_wakers
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if !wakers.iter().any(|w| w.will_wake(cx.waker())) {
                wakers.push(cx.waker().clone());
            }
            drop(wakers);
            // A suspend that finished between the check and the registration.
            if self.is_suspended() {
                core::task::Poll::Ready(())
            } else {
                core::task::Poll::Pending
            }
        })
        .await
    }

    /// Associations since boot; a change means a new link session.
    pub fn connect_count(&self) -> u32 {
        self.link.supervisor.connect_count()
    }

    /// Completes once the station is associated and has its IP address, woken by
    /// the reconnect worker; cancelling the wait leaves nothing behind.
    ///
    /// The future behind [`wait_connected`](Self::wait_connected), returned without
    /// an `async fn` so it can be stored by name.
    pub fn wait_ready(&self) -> impl core::future::Future<Output = ()> + '_ {
        self.link.supervisor.wait_ready()
    }

    /// Disconnects and stops the radio, and pauses reconnects until
    /// [`resume`](Self::resume).
    ///
    /// Unlike the esp-hal tier, where suspend only disconnects the station,
    /// this powers the radio down.
    ///
    /// Runs the IDF calls on the caller's thread: the radio is stopped when this
    /// returns, as `esp_deep_sleep_start` needs. Idempotent, thread-safe next to
    /// the reconnect worker and other callers, and bounded to a few milliseconds.
    /// The MQTT client
    /// sends no DISCONNECT, so the broker publishes its will after
    /// 1.5 × keep-alive; the suspend also removes the station's IP, which closes
    /// the MQTT session with no attempt and no retry for the whole suspend (see
    /// [`subscribe_ready`](Self::subscribe_ready)).
    ///
    /// # Errors
    ///
    /// Returns an error if `esp_wifi_stop` fails; reconnects stay paused and a
    /// second call retries the stop.
    pub fn suspend(&self) -> anyhow::Result<()> {
        self.link
            .supervisor
            .suspend()
            .map_err(|e| anyhow::Error::from(e).context("failed to stop WiFi for suspend"))?;
        let wakers = std::mem::take(
            &mut *self
                .suspend_wakers
                .lock()
                .unwrap_or_else(|e| e.into_inner()),
        );
        for waker in wakers {
            waker.wake();
        }
        Ok(())
    }

    /// Starts the radio and connects again with a fresh back-off.
    ///
    /// A no-op unless suspended; idempotent and safe right after
    /// [`suspend`](Self::suspend) (one connect in flight, never two): driver
    /// events queued before the stop are dropped by the reconnect worker, so
    /// this never waits for them. Returns once the connect is requested;
    /// association follows asynchronously, like the initial non-blocking connect.
    ///
    /// # Errors
    ///
    /// Returns an error if the radio does not start or the connect request fails;
    /// the reconnect worker keeps retrying with its back-off either way.
    pub fn resume(&self) -> anyhow::Result<()> {
        self.link
            .supervisor
            .resume()
            .map_err(|e| anyhow::Error::from(e).context("WiFi resume failed to request a connect"))
    }
}

/// The shared [`LinkSupervisor`] plus the event subscription that feeds it.
///
/// Fields drop in order: the subscription is unregistered first, then the
/// supervisor joins its worker, so no `esp_wifi_connect` runs after the driver
/// is gone.
struct LinkWorker {
    _subscription: EspSystemSubscription<'static>,
    _ip_subscription: EspSystemSubscription<'static>,
    supervisor: LinkSupervisor<IdfRadio>,
}

impl LinkWorker {
    /// Spawns the supervisor and subscribes it to the station events; the
    /// radio must already be started. `sta_netif` identifies the station's
    /// interface, so IP events of any other interface are ignored.
    fn start(
        sys_loop: &EspSystemEventLoop,
        sta_netif: *mut esp_idf_svc::sys::esp_netif_t,
    ) -> anyhow::Result<Self> {
        let supervisor = LinkSupervisor::spawn(
            IdfRadio,
            LinkMachine::new(),
            thread::Builder::new()
                .name("wifi-link".into())
                .stack_size(LINK_WORKER_STACK_SIZE),
        )
        .context("failed to spawn the Wi-Fi link worker")?;
        let events = supervisor.events();
        let subscription = sys_loop
            .subscribe::<WifiEvent, _>(move |event: WifiEvent<'_>| match event {
                WifiEvent::StaConnected(_) => {
                    // The supervisor prints the shared `link up` line, with the AP from
                    // `IdfRadio::joined_ap`, once the machine has counted the association.
                    events.send(LinkEvent::Connected)
                }
                WifiEvent::StaStopped => events.send(LinkEvent::Stopped),
                WifiEvent::StaDisconnected(info) => {
                    // The shared line names the reason and adds the credentials
                    // hint only for reasons that point at them, so a
                    // router deauth or our own suspend no longer blames them.
                    log::warn!(
                        "{}",
                        Disconnected {
                            reason: DisconnectReason(info.reason()),
                            hint: ""
                        }
                    );
                    events.send(LinkEvent::Disconnected);
                }
                _ => {}
            })
            .map_err(|e| anyhow::anyhow!("WiFi link subscription failed: {:?}", e))?;
        // Readiness: the station's IP. Same event loop as above, so `NetworkUp`
        // reaches the worker after the `Connected` it belongs to. Only the station's
        // interface counts; the handle is kept as an address because raw pointers
        // are not `Send`.
        let events = supervisor.events();
        let sta_netif = sta_netif as usize;
        let ip_subscription = sys_loop
            .subscribe::<IpEvent, _>(move |event: IpEvent<'_>| {
                if event.handle().map(|netif| netif as usize) != Some(sta_netif) {
                    return;
                }
                match event {
                    IpEvent::DhcpIpAssigned(assignment) => {
                        log::info!(
                            "{}",
                            NetworkReady {
                                ip: Some(assignment.ip())
                            }
                        );
                        events.send(LinkEvent::NetworkUp);
                    }
                    IpEvent::DhcpIpDeassigned(_) => events.send(LinkEvent::NetworkDown),
                    _ => {}
                }
            })
            .map_err(|e| anyhow::anyhow!("WiFi IP subscription failed: {:?}", e))?;
        Ok(Self {
            _subscription: subscription,
            _ip_subscription: ip_subscription,
            supervisor,
        })
    }
}

impl NetworkLink for WiFiManager {
    type Error = anyhow::Error;

    fn state(&self) -> LinkState {
        WiFiManager::state(self)
    }

    fn is_suspended(&self) -> bool {
        WiFiManager::is_suspended(self)
    }

    fn connect_count(&self) -> u32 {
        WiFiManager::connect_count(self)
    }

    fn ip(&self) -> Option<Ipv4Addr> {
        WiFiManager::ip(self)
    }

    fn suspend(&self) -> anyhow::Result<()> {
        WiFiManager::suspend(self)
    }

    fn resume(&self) -> anyhow::Result<()> {
        WiFiManager::resume(self)
    }

    async fn wait_connected(&self) {
        self.wait_ready().await
    }

    async fn wait_suspended(&self) {
        WiFiManager::wait_suspended(self).await
    }
}

/// Tier parity check (ADR 022): fails to compile if `WiFiManager` stops implementing `NetworkLink`.
const _: fn() = || {
    fn implements<T: NetworkLink>() {}
    implements::<WiFiManager>();
};

/// Concrete tier parity check (ADR 022): consumer code calls `WiFiManager`
/// directly, often without importing `NetworkLink`, so these calls must compile
/// with the same names, arguments and return shapes on both tiers. The trait check
/// above cannot see an inherent method that hides a trait method with another
/// signature; this one fails on it. Keep the body identical to its twin in the
/// esp-hal tier (`rustyfarian-esp-hal-network` `wifi/link.rs`).
#[allow(dead_code)]
mod concrete_parity {
    use crate::wifi::{LinkState, WiFiManager};
    use core::net::Ipv4Addr;

    async fn calls(wifi: &WiFiManager) {
        let _: LinkState = wifi.state();
        let _: bool = wifi.is_connected();
        let _: bool = wifi.is_ready();
        let _: bool = wifi.is_suspended();
        let _: u32 = wifi.connect_count();
        let _: Option<Ipv4Addr> = wifi.ip();
        let _: Result<(), _> = wifi.suspend();
        let _: Result<(), _> = wifi.resume();
        let _: () = wifi.wait_connected().await;
        let _: () = wifi.wait_suspended().await;
    }
}

/// The station calls of the ESP-IDF driver; events arrive through the
/// system event loop in the order the driver posts them.
struct IdfRadio;

impl LinkRadio for IdfRadio {
    type Error = EspError;

    fn start(&mut self) -> Result<(), EspError> {
        esp!(unsafe { esp_idf_svc::sys::esp_wifi_start() })
    }

    fn connect(&mut self) -> Result<(), EspError> {
        esp!(unsafe { esp_idf_svc::sys::esp_wifi_connect() })
    }

    fn disconnect(&mut self) -> Result<(), EspError> {
        esp!(unsafe { esp_idf_svc::sys::esp_wifi_disconnect() })
    }

    fn stop(&mut self) -> Result<(), EspError> {
        esp!(unsafe { esp_idf_svc::sys::esp_wifi_stop() })
    }

    fn joined_ap(&self) -> Option<JoinedAp> {
        joined_ap()
    }
}

/// Applies `country` to the Wi-Fi driver and returns what the radio reports.
///
/// [`WiFiManager`], [`SoftApManager`] and the driver-owned ESP-NOW radios apply their
/// configured country themselves; call this for a radio the firmware built on its own, or to
/// change the country later.
/// The Wi-Fi driver must be initialised (`EspWifi::new`); the radio need not be started.
/// Logs the shared line, e.g. `[wifi] country DE, channels 1-13, 802.11d on`.
/// ESP-IDF stores the country in flash; the write is skipped when the radio already reports
/// the requested code and 802.11d policy.
/// With 802.11d on, an associated station adopts its AP's country, so the read-back can name
/// that one instead.
/// Same name and shape as the esp-hal tier's `wifi::set_country` (ADR 022).
///
/// # Errors
///
/// The ESP-IDF error of `esp_wifi_set_country_code` or of the read-back, e.g. when the driver
/// is not initialised.
pub fn set_country(country: Country) -> anyhow::Result<Regulatory> {
    crate::regulatory::apply(country)
}

/// The country the radio reports right now (`esp_wifi_get_country`), `None` when the Wi-Fi
/// driver is not initialised.
///
/// For diagnostics: with 802.11d on, an associated station reports its AP's country.
/// Same name and shape as the esp-hal tier's `wifi::regulatory` (ADR 022).
pub fn regulatory() -> Option<Regulatory> {
    crate::regulatory::read()
}

/// Reads the SoftAP factory MAC address from efuse.
///
/// This reads the base MAC via `esp_read_mac` with the
/// `ESP_MAC_WIFI_SOFTAP` type and works **without** initialising Wi-Fi,
/// because the value is stored in efuse rather than the Wi-Fi driver state.
/// Callers can therefore derive a MAC-suffixed SSID *before* starting the AP,
/// avoiding an SSID change after the radio is already broadcasting.
pub fn softap_mac() -> anyhow::Result<[u8; 6]> {
    let mut mac = [0u8; 6];
    esp_idf_svc::sys::esp!(unsafe {
        esp_idf_svc::sys::esp_read_mac(
            mac.as_mut_ptr(),
            esp_idf_svc::sys::esp_mac_type_t_ESP_MAC_WIFI_SOFTAP,
        )
    })
    .context("failed to read SoftAP MAC from efuse")?;
    Ok(mac)
}

/// SoftAP (access-point) lifecycle manager.
///
/// Brings up a Wi-Fi access point from an [`ApConfig`] and exposes the AP
/// netif IP, the AP MAC, and the associated-station count — the surface the
/// captive-portal provisioning crate consumes.
///
/// # Mode
///
/// This is AP-only.
/// v1 deliberately does not run mixed AP+STA mode; the provisioning flow
/// commits credentials and the host reboots into normal STA boot
/// (reboot-based handoff), sidestepping ESP-IDF mode-switch issues.
///
/// # TX power on ESP32-C3 Super Mini boards
///
/// The ESP32-C3 Super Mini PCB-antenna lore (full TX power reflecting off the
/// antenna and corrupting frames) is a documented *bare-metal* STA finding.
/// It may also affect AP beacons, so the default here is
/// [`TxPowerLevel::Medium`].
/// [`TxPowerLevel::Low`] is suggested as an optional tweak for C3 Super Mini
/// boards if AP association proves unreliable — this is a plausible mitigation,
/// not a verified fix for the AP path.
///
/// Available only when the linked ESP-IDF is built with
/// `CONFIG_ESP_WIFI_SOFTAP_SUPPORT=y` (the default); on a SoftAP-disabled
/// sdkconfig this item is not compiled.
#[cfg(esp_idf_esp_wifi_softap_support)]
pub struct SoftApManager {
    wifi: EspWifi<'static>,
}

#[cfg(esp_idf_esp_wifi_softap_support)]
impl SoftApManager {
    /// Starts a SoftAP from the given [`ApConfig`].
    ///
    /// Validates the configuration via [`validate_ap_config`] (the channel must be
    /// one [`ApConfig::country`] allows), applies the country, configures the
    /// access point (WPA2-Personal when a password is present, open otherwise),
    /// starts the radio, then applies the requested transmit power.
    /// As with the STA path, a TX-power failure is logged at `warn` and the AP
    /// continues at the radio default rather than hard-erroring.
    /// The SSID is logged by length only, never by value.
    pub fn start(
        modem: Modem<'static>,
        sys_loop: EspSystemEventLoop,
        nvs: Option<EspDefaultNvsPartition>,
        config: ApConfig<'_>,
    ) -> anyhow::Result<Self> {
        validate_ap_config(&config).map_err(|e| {
            anyhow::anyhow!("AP config invalid (ssid len={}): {}", config.ssid.len(), e)
        })?;

        log::info!("[wifi] starting softap (ssid len={})", config.ssid.len());

        let mut wifi = EspWifi::new(modem, sys_loop, nvs)?;

        // Before the AP starts: ESP-IDF silently moves an AP on a channel outside the
        // country to channel 1, and `validate_ap_config` checked the channel against it.
        crate::regulatory::apply(config.country)?;

        let ssid = config.ssid.try_into().map_err(|_| {
            anyhow::anyhow!(
                "internal: SSID conversion failed after validation (len={}, limit={})",
                config.ssid.len(),
                SSID_MAX_LEN
            )
        })?;

        let mut ap_config = AccessPointConfiguration {
            ssid,
            channel: config.channel,
            max_connections: config.max_connections as u16,
            ..Default::default()
        };
        match config.password {
            Some(pw) => {
                ap_config.password = pw.try_into().map_err(|_| {
                    anyhow::anyhow!(
                        "internal: password conversion failed after validation (len={}, limit={})",
                        pw.len(),
                        PASSWORD_MAX_LEN
                    )
                })?;
                ap_config.auth_method = AuthMethod::WPA2Personal;
            }
            None => {
                ap_config.auth_method = AuthMethod::None;
            }
        }

        wifi.set_configuration(&Configuration::AccessPoint(ap_config))?;

        pin_ap_netif_ip(&wifi)?;

        wifi.start()?;

        let tx_power = config.tx_power.to_quarter_dbm();
        match esp_idf_svc::sys::esp!(unsafe {
            esp_idf_svc::sys::esp_wifi_set_max_tx_power(tx_power)
        }) {
            Ok(()) => log::info!(
                "[wifi] softap tx power set to {:?} ({} quarter-dBm)",
                config.tx_power,
                tx_power
            ),
            Err(e) => log::warn!(
                "[wifi] failed to set softap tx power to {:?} ({} quarter-dBm), continuing at radio default: {:?}",
                config.tx_power,
                tx_power,
                e
            ),
        }

        Ok(Self { wifi })
    }

    /// Returns the IPv4 address of the AP netif.
    ///
    /// `SoftApManager::start` pins the AP netif to the documented ESP-IDF
    /// default `192.168.4.1/24` via `pin_ap_netif_ip`, so the value returned
    /// here is stable across reboots and is not affected by stale NVS netif
    /// state. The address is still read live from the netif rather than
    /// assumed.
    pub fn ap_ip(&self) -> anyhow::Result<Ipv4Addr> {
        Ok(self.wifi.ap_netif().get_ip_info()?.ip)
    }

    /// Returns the AP interface MAC address.
    ///
    /// Wraps the unsafe `esp_wifi_get_mac(WIFI_IF_AP, ...)` IDF call; the AP
    /// interface is up after [`start`](Self::start) succeeds.
    pub fn ap_mac(&self) -> anyhow::Result<[u8; 6]> {
        let mut mac = [0u8; 6];
        esp_idf_svc::sys::esp!(unsafe {
            esp_idf_svc::sys::esp_wifi_get_mac(
                esp_idf_svc::sys::wifi_interface_t_WIFI_IF_AP,
                mac.as_mut_ptr(),
            )
        })
        .context("failed to read AP MAC")?;
        Ok(mac)
    }

    /// Returns the number of stations currently associated with the AP.
    ///
    /// Wraps the unsafe `esp_wifi_ap_get_sta_list` IDF call.
    pub fn station_count(&self) -> anyhow::Result<u16> {
        // SAFETY: `wifi_sta_list_t` is a plain C struct of integers and a fixed
        // array; an all-zero value is a valid empty list. `esp_wifi_ap_get_sta_list`
        // fills it in place and the AP interface is up after `start`.
        let mut sta_list: esp_idf_svc::sys::wifi_sta_list_t = unsafe { core::mem::zeroed() };
        esp_idf_svc::sys::esp!(unsafe {
            esp_idf_svc::sys::esp_wifi_ap_get_sta_list(&mut sta_list)
        })
        .context("failed to read AP station list")?;
        Ok(sta_list.num as u16)
    }

    /// Stops the SoftAP and releases the radio.
    pub fn stop(mut self) -> anyhow::Result<()> {
        self.wifi.stop()?;
        Ok(())
    }
}

/// Forces the AP netif onto the documented ESP-IDF default `192.168.4.1/24`
/// and configures the IDF DHCP server to advertise the AP as the DNS server
/// (DHCP Option 6).
///
/// `esp-idf-svc` does not expose `set_ip_info`, and `EspWifi::new` can read a
/// stale netif configuration retained in NVS that lands the SoftAP on an
/// unexpected subnet. Pinning the IP here gives `ap_ip()` callers — and any
/// hard-coded captive-portal URL — a predictable address.
///
/// Without Option 6, captive-portal clients get an IP and gateway but no DNS
/// resolver.  They cannot resolve probe domains such as
/// `connectivitycheck.gstatic.com` or `captive.apple.com`, so the OS-level
/// captive-portal detection never fires and the sign-in sheet never appears —
/// even though our DNS catch-all on port 53 is already running.  Setting the
/// AP netif's main DNS to the AP IP and enabling the DHCP-server DNS offer
/// mirrors what the esp-hal DHCP path does in Option 6 of its hand-rolled
/// OFFER/ACK messages.
///
/// Available only when the linked ESP-IDF is built with
/// `CONFIG_ESP_WIFI_SOFTAP_SUPPORT=y` (the default); on a SoftAP-disabled
/// sdkconfig this item is not compiled.
#[cfg(esp_idf_esp_wifi_softap_support)]
fn pin_ap_netif_ip(wifi: &EspWifi<'_>) -> anyhow::Result<()> {
    use core::ffi::c_void;

    use esp_idf_svc::handle::RawHandle;
    use esp_idf_svc::sys::{
        esp, esp_ip4_addr_t, esp_ip_addr_t,
        esp_netif_dhcp_option_id_t_ESP_NETIF_DOMAIN_NAME_SERVER,
        esp_netif_dhcp_option_mode_t_ESP_NETIF_OP_SET, esp_netif_dhcps_option,
        esp_netif_dhcps_start, esp_netif_dhcps_stop, esp_netif_dns_info_t,
        esp_netif_dns_type_t_ESP_NETIF_DNS_MAIN, esp_netif_ip_info_t, esp_netif_set_dns_info,
        esp_netif_set_ip_info, ESP_IPADDR_TYPE_V4,
    };

    let handle = wifi.ap_netif().handle();
    let ap_addr = u32::from_le_bytes([192, 168, 4, 1]);
    let info = esp_netif_ip_info_t {
        ip: esp_ip4_addr_t { addr: ap_addr },
        netmask: esp_ip4_addr_t {
            addr: u32::from_le_bytes([255, 255, 255, 0]),
        },
        gw: esp_ip4_addr_t { addr: ap_addr },
    };

    // Source of truth: lwIP `dhcpserver.h` defines `OFFER_DNS = 0x02`; the DHCP
    // server includes Option 6 (DNS) in its OFFER/ACK when this bit is set in the
    // `DOMAIN_NAME_SERVER` option byte. Hardware-validated (rgb-clock captive-portal
    // bring-up, 2026-06-23) — there is no host/CI test for this IDF behavior.
    const DHCPS_OFFER_DNS_BIT: u8 = 0x02;
    let mut dns_offer: u8 = DHCPS_OFFER_DNS_BIT;

    unsafe {
        // DHCPS must be stopped before set_ip_info and set_dns_info; the
        // ignored result handles the "already stopped" case
        // (ESP_ERR_ESP_NETIF_DHCP_ALREADY_STOPPED).
        let _ = esp_netif_dhcps_stop(handle);

        esp!(esp_netif_set_ip_info(handle, &info)).context("failed to set AP netif IP")?;

        // Tell the AP netif's DNS slot (MAIN) to point at the AP itself so
        // that the IDF DHCP server can advertise it via Option 6.
        // SAFETY: esp_netif_set_dns_info takes a *mut but only reads the
        // struct; the local `dns_info` outlives the call.
        let mut dns_info = esp_netif_dns_info_t {
            ip: esp_ip_addr_t {
                u_addr: esp_idf_svc::sys::_ip_addr__bindgen_ty_1 {
                    ip4: esp_ip4_addr_t { addr: ap_addr },
                },
                type_: ESP_IPADDR_TYPE_V4 as u8,
            },
        };
        esp!(esp_netif_set_dns_info(
            handle,
            esp_netif_dns_type_t_ESP_NETIF_DNS_MAIN,
            &mut dns_info,
        ))
        .context("failed to set AP netif DNS")?;

        // Enable DHCP Option 6 (DNS server) in the IDF DHCP server.
        // The opt_val is the OFFER_DNS bitmask byte from lwIP dhcpserver.h.
        // SAFETY: opt_val pointer is valid for the duration of the call.
        esp!(esp_netif_dhcps_option(
            handle,
            esp_netif_dhcp_option_mode_t_ESP_NETIF_OP_SET,
            esp_netif_dhcp_option_id_t_ESP_NETIF_DOMAIN_NAME_SERVER,
            &mut dns_offer as *mut u8 as *mut c_void,
            core::mem::size_of::<u8>() as u32,
        ))
        .context("failed to enable DHCP DNS offer")?;

        esp!(esp_netif_dhcps_start(handle)).context("failed to (re)start AP DHCP server")?;
    }
    Ok(())
}
