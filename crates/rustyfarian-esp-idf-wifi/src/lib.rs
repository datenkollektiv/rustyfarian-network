//! Wi-Fi connection manager for ESP-IDF projects.
//!
//! Provides a simplified wrapper around the ESP-IDF Wi-Fi client with:
//! - Automatic connection handling with timeout
//! - Optional LED status indicator via the `StatusLed` trait from `led_effects`
//! - IP address acquisition with polling
//!
//! # Example
//!
//! ```ignore
//! use rustyfarian_esp_idf_wifi::{WiFiManager, WiFiConfig};
//!
//! let config = WiFiConfig::new("MyNetwork", "password123");
//! let wifi = WiFiManager::new_without_led(modem, sys_loop, Some(nvs), config)?;
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
//! use rustyfarian_esp_idf_wifi::{WiFiManager, WiFiConfig};
//!
//! let config = WiFiConfig::new("MyNetwork", "password123")
//!     .connect_nonblocking();
//! let wifi = WiFiManager::new_without_led(modem, sys_loop, Some(nvs), config)?;
//!
//! // new_without_led() returns immediately; association proceeds in the background.
//! if let Some(ip) = wifi.get_ip(10000)? {
//!     println!("Connected with IP: {}", ip);
//! }
//! ```
//!
//! # Using a Simple GPIO LED
//!
//! For boards with a simple on/off LED instead of an RGB LED, use `SimpleLed`:
//!
//! ```ignore
//! use rustyfarian_esp_idf_wifi::{WiFiManager, WiFiConfig, SimpleLed};
//! use esp_idf_hal::gpio::PinDriver;
//!
//! let pin = PinDriver::output(peripherals.pins.gpio8)?;
//! let mut led = SimpleLed::new(pin);
//!
//! let config = WiFiConfig::new("MyNetwork", "password123");
//! let wifi = WiFiManager::new(modem, sys_loop, Some(nvs), config, Some(&mut led))?;
//! ```

use std::net::Ipv4Addr;
use std::thread;
use std::time::Duration;

use anyhow::Context as _;
use rustyfarian_network_pure::wifi::{
    validate_password, validate_ssid, PASSWORD_MAX_LEN, SSID_MAX_LEN,
};

use esp_idf_svc::eventloop::{EspSystemEventLoop, EspSystemSubscription};
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{BlockingWifi, ClientConfiguration, Configuration, EspWifi, WifiEvent};
use led_effects::PulseEffect;
use rgb::RGB8;

// Re-export StatusLed and SimpleLed from led_effects for convenience
pub use led_effects::{SimpleLed, StatusLed};

/// Green channel brightness for the "connected" LED state.
///
/// Kept intentionally dim to avoid blinding the user in dark environments.
const CONNECTED_LED_BRIGHTNESS: u8 = 20;

/// Default connection timeout when none is specified.
const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Polling interval for connection and IP-readiness checks.
const POLL_INTERVAL_MS: u64 = 100;

/// A no-op LED implementation used by [`WiFiManager::new_without_led`].
struct NoLed;

impl StatusLed for NoLed {
    type Error = std::convert::Infallible;

    fn set_color(&mut self, _color: RGB8) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// Controls how `WiFiManager::new` handles the Wi-Fi association phase.
#[derive(Debug, Clone)]
pub enum ConnectMode {
    /// Block `WiFiManager::new` until connected or the timeout expires.
    Blocking {
        /// Maximum time to wait for association, in seconds.
        timeout_secs: u64,
    },
    /// Initiate association and return immediately.
    ///
    /// The ESP-IDF event loop drives the connection in the background.
    /// Call `get_ip()` or `is_connected()` to check readiness.
    ///
    /// **Note:** If an LED driver is provided to `WiFiManager::new`, this mode
    /// will still block until connected (or timeout) to drive the LED status.
    NonBlocking,
}

impl Default for ConnectMode {
    fn default() -> Self {
        Self::Blocking {
            timeout_secs: DEFAULT_TIMEOUT_SECS,
        }
    }
}

/// Wi-Fi connection configuration.
///
/// Construct via [`WiFiConfig::new`], then chain builder methods as needed:
///
/// ```ignore
/// let config = WiFiConfig::new("MyNetwork", "password123")
///     .with_timeout(60)      // optional: override the 30 s default
///     .connect_nonblocking(); // optional: return immediately from new()
/// ```
#[derive(Debug, Clone)]
pub struct WiFiConfig<'a> {
    ssid: &'a str,
    password: &'a str,
    connect_mode: ConnectMode,
}

impl<'a> WiFiConfig<'a> {
    /// Creates a new Wi-Fi configuration.
    ///
    /// Defaults to blocking connection with a 30-second timeout.
    pub fn new(ssid: &'a str, password: &'a str) -> Self {
        Self {
            ssid,
            password,
            connect_mode: ConnectMode::default(),
        }
    }

    /// Sets a blocking connection with the given timeout in seconds.
    pub fn with_timeout(mut self, secs: u64) -> Self {
        self.connect_mode = ConnectMode::Blocking { timeout_secs: secs };
        self
    }

    /// Returns immediately after initiating association.
    ///
    /// The ESP-IDF event loop drives the connection in the background.
    /// Call `get_ip()` or `is_connected()` to check readiness.
    ///
    /// # LED limitation
    ///
    /// **Non-blocking mode is only effective when no LED driver is provided.**
    /// If an LED is passed to [`WiFiManager::new`], the constructor falls back to
    /// blocking behaviour (using the default 30-second timeout) so it can drive
    /// the LED status indicator.
    /// A warning is logged when this fallback occurs.
    pub fn connect_nonblocking(mut self) -> Self {
        self.connect_mode = ConnectMode::NonBlocking;
        self
    }
}

/// Maps common ESP-IDF Wi-Fi disconnect reason codes to human-readable names.
///
/// Codes follow `wifi_err_reason_t` in ESP-IDF.
/// Returns `None` for unmapped codes so callers can log the raw number instead
/// of a misleading `"unknown"` string.
fn wifi_disconnect_reason_name(reason: u16) -> Option<&'static str> {
    match reason {
        2 => Some("AUTH_EXPIRE"),
        15 => Some("4WAY_HANDSHAKE_TIMEOUT"),
        200 => Some("BEACON_TIMEOUT"),
        201 => Some("NO_AP_FOUND"),
        202 => Some("AUTH_FAIL"),
        203 => Some("ASSOC_FAIL"),
        204 => Some("HANDSHAKE_TIMEOUT"),
        _ => None,
    }
}

/// Wi-Fi connection manager with optional LED status feedback.
pub struct WiFiManager {
    wifi: BlockingWifi<EspWifi<'static>>,
    /// Kept alive to receive disconnect-reason log events in non-blocking mode.
    _disconnect_subscription: Option<EspSystemSubscription<'static>>,
}

impl WiFiManager {
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
        log::info!("Connecting to WiFi SSID (len={})", config.ssid.len());

        // Clone before sys_loop is consumed by BlockingWifi::wrap; used for the
        // optional disconnect-event subscription in non-blocking mode.
        let sys_loop_sub = sys_loop.clone();
        let mut wifi = BlockingWifi::wrap(EspWifi::new(modem, sys_loop.clone(), nvs)?, sys_loop)?;

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
            ..Default::default()
        }))?;

        wifi.start()?;
        log::info!("WiFi started");

        // In non-blocking mode, subscribe to disconnect events so failures such as
        // WIFI_REASON_NO_AP_FOUND are visible at WARN level without enabling debug logs.
        let disconnect_subscription = if matches!(config.connect_mode, ConnectMode::NonBlocking) {
            Some(
                sys_loop_sub
                    .subscribe::<WifiEvent, _>(|event: WifiEvent<'_>| {
                        if let WifiEvent::StaDisconnected(info) = event {
                            let reason = info.reason();
                            match wifi_disconnect_reason_name(reason) {
                                Some(name) => log::warn!(
                                    "WiFi disconnected — reason {} ({}) — \
                                     check SSID/password and network availability",
                                    reason,
                                    name,
                                ),
                                None => log::warn!(
                                    "WiFi disconnected — reason {} (unmapped) — \
                                     check SSID/password and network availability",
                                    reason,
                                ),
                            }
                        }
                    })
                    .map_err(|e| anyhow::anyhow!("WiFi disconnect subscription failed: {:?}", e))?,
            )
        } else {
            None
        };

        if let Some(led_driver) = led {
            let timeout_secs = match config.connect_mode {
                ConnectMode::Blocking { timeout_secs } => timeout_secs,
                ConnectMode::NonBlocking => {
                    log::warn!(
                        "Non-blocking connection requested but LED driver is present; \
                         falling back to blocking with {}s timeout",
                        DEFAULT_TIMEOUT_SECS
                    );
                    DEFAULT_TIMEOUT_SECS
                }
            };
            Self::connect_with_led(&mut wifi, led_driver, timeout_secs)?;
        } else {
            match config.connect_mode {
                ConnectMode::Blocking { timeout_secs } => {
                    Self::wait_for_connection(&mut wifi, timeout_secs)?;
                }
                ConnectMode::NonBlocking => {
                    wifi.wifi_mut()
                        .connect()
                        .context("WiFi connect initiation failed")?;
                    log::info!("WiFi connect initiated (non-blocking)");
                }
            }
        }

        Ok(Self {
            wifi,
            _disconnect_subscription: disconnect_subscription,
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

    fn connect_with_led<L>(
        wifi: &mut BlockingWifi<EspWifi<'static>>,
        led: &mut L,
        timeout_secs: u64,
    ) -> anyhow::Result<()>
    where
        L: StatusLed,
        L::Error: std::fmt::Debug,
    {
        let mut pulse_effect = PulseEffect::new();
        let mut connect_started = false;
        let start_time = std::time::Instant::now();
        let timeout = Duration::from_secs(timeout_secs);

        log::info!("WiFi connecting...");

        loop {
            // Check for timeout
            if start_time.elapsed() >= timeout {
                log::error!("WiFi connection timeout after {} seconds", timeout_secs);

                // Brief red pulse to indicate failure before returning error
                for _ in 0..20 {
                    led.set_color(pulse_effect.update((255, 0, 0)))
                        .map_err(|e| anyhow::anyhow!("LED error: {:?}", e))?;
                    thread::sleep(Duration::from_millis(50));
                }

                return Err(anyhow::anyhow!(
                    "Wi-Fi connection timeout after {} seconds",
                    timeout_secs
                ));
            }

            // Try to start a connection if not already started
            if !connect_started {
                match wifi.wifi_mut().connect() {
                    Ok(_) => {
                        connect_started = true;
                        log::info!("Connection attempt initiated");
                    }
                    Err(e) => {
                        log::warn!("Failed to start connection: {:?}, will retry", e);
                    }
                }
            }

            // Check if connected
            if connect_started {
                match wifi.is_connected() {
                    Ok(true) => {
                        log::info!("WiFi connected");
                        break;
                    }
                    Ok(false) => {
                        // Not connected yet, keep pulsing
                    }
                    Err(e) => {
                        log::error!("WiFi connection error: {:?}", e);
                        connect_started = false;
                    }
                }
            }

            // Pulse blue light
            led.set_color(pulse_effect.update((0, 0, 255)))
                .map_err(|e| anyhow::anyhow!("LED error: {:?}", e))?;
            thread::sleep(Duration::from_millis(50));
        }

        // Wait for DHCP
        wifi.wait_netif_up()?;
        log::info!("WiFi netif up");

        led.set_color(RGB8::new(0, CONNECTED_LED_BRIGHTNESS, 0))
            .map_err(|e| anyhow::anyhow!("LED error: {:?}", e))?;

        Ok(())
    }

    /// Internal helper to wait for connection with a timeout (no LED).
    fn wait_for_connection(
        wifi: &mut BlockingWifi<EspWifi<'static>>,
        timeout_secs: u64,
    ) -> anyhow::Result<()> {
        let start_time = std::time::Instant::now();
        let timeout = Duration::from_secs(timeout_secs);
        let mut connect_started = false;

        log::info!("WiFi connecting (timeout: {}s)...", timeout_secs);

        loop {
            if start_time.elapsed() >= timeout {
                return Err(anyhow::anyhow!(
                    "Wi-Fi connection timeout after {} seconds",
                    timeout_secs
                ));
            }

            if !connect_started {
                match wifi.wifi_mut().connect() {
                    Ok(_) => {
                        connect_started = true;
                        log::info!("Connection attempt initiated");
                    }
                    Err(e) => {
                        log::warn!("Failed to start connection: {:?}, will retry", e);
                    }
                }
            }

            if connect_started && wifi.is_connected()? {
                break;
            }

            thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
        }

        wifi.wait_netif_up()?;
        log::info!("WiFi connected and netif up");

        Ok(())
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
    /// Transient ESP-IDF errors during the polling loop (common during association)
    /// are logged at `debug` level and treated as "not ready yet" rather than
    /// propagated to the caller.
    pub fn get_ip(&self, timeout_ms: u64) -> anyhow::Result<Option<Ipv4Addr>> {
        let start = std::time::Instant::now();
        let timeout = Duration::from_millis(timeout_ms);

        loop {
            match self.wifi.is_connected() {
                Ok(true) => match self.wifi.wifi().sta_netif().get_ip_info() {
                    Ok(ip_info) if !ip_info.ip.is_unspecified() => {
                        log::info!("WiFi IP: {:?}", ip_info.ip);
                        return Ok(Some(ip_info.ip));
                    }
                    Ok(_) => {}
                    Err(e) => log::debug!("get_ip_info transient error: {}", e),
                },
                Ok(false) => {}
                Err(e) => log::debug!("is_connected transient error: {}", e),
            }

            if start.elapsed() >= timeout {
                log::warn!("Timeout waiting for WiFi IP address");
                return Ok(None);
            }

            thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
        }
    }

    /// Returns whether the WiFi is currently connected.
    pub fn is_connected(&self) -> anyhow::Result<bool> {
        Ok(self.wifi.is_connected()?)
    }
}
