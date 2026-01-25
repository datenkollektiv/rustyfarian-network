//! WiFi connection manager for ESP32 projects using ESP-IDF.
//!
//! Provides a simplified wrapper around the ESP-IDF Wi-Fi client with:
//! - Automatic connection handling with timeout
//! - Optional LED status indicator via the `StatusLed` trait from `led_effects`
//! - IP address acquisition with polling
//!
//! # Example
//!
//! ```ignore
//! use esp32_wifi_manager::{WiFiManager, WiFiConfig};
//!
//! let config = WiFiConfig::new("MyNetwork", "password123");
//! let wifi = WiFiManager::new(modem, sys_loop, Some(nvs), config, None::<&mut MyLed>)?;
//!
//! if let Some(ip) = wifi.get_ip(10000)? {
//!     println!("Connected with IP: {}", ip);
//! }
//! ```

use std::net::Ipv4Addr;
use std::thread;
use std::time::Duration;

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::modem::Modem;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use esp_idf_svc::wifi::{BlockingWifi, ClientConfiguration, Configuration, EspWifi};
use led_effects::PulseEffect;
use rgb::RGB8;

// Re-export StatusLed from led_effects for convenience
pub use led_effects::StatusLed;

/// WiFi connection configuration.
#[derive(Debug, Clone)]
pub struct WiFiConfig<'a> {
    /// WiFi network SSID
    pub ssid: &'a str,
    /// WiFi network password
    pub password: &'a str,
    /// Connection timeout in seconds (default: 10)
    pub connection_timeout_secs: Option<u64>,
}

impl<'a> WiFiConfig<'a> {
    /// Creates a new Wi-Fi configuration.
    pub fn new(ssid: &'a str, password: &'a str) -> Self {
        Self {
            ssid,
            password,
            connection_timeout_secs: None,
        }
    }

    /// Sets the connection timeout.
    pub fn with_timeout(mut self, secs: u64) -> Self {
        self.connection_timeout_secs = Some(secs);
        self
    }
}

/// Wi-Fi connection manager with optional LED status feedback.
pub struct WiFiManager {
    wifi: BlockingWifi<EspWifi<'static>>,
}

impl WiFiManager {
    /// Creates a new Wi-Fi manager and connects to the network.
    ///
    /// # Arguments
    ///
    /// * `modem` - ESP32 modem peripheral
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
        modem: Modem,
        sys_loop: EspSystemEventLoop,
        nvs: Option<EspDefaultNvsPartition>,
        config: WiFiConfig<'_>,
        led: Option<&mut L>,
    ) -> anyhow::Result<Self>
    where
        L: StatusLed,
        L::Error: std::fmt::Debug,
    {
        log::info!("Connecting to WiFi SSID: {}", config.ssid);

        let mut wifi = BlockingWifi::wrap(EspWifi::new(modem, sys_loop.clone(), nvs)?, sys_loop)?;

        let ssid = config.ssid.try_into().map_err(|_| {
            anyhow::anyhow!(
                "WiFi SSID '{}' exceeds maximum length of 32 bytes",
                config.ssid
            )
        })?;
        let password = config
            .password
            .try_into()
            .map_err(|_| anyhow::anyhow!("WiFi password exceeds maximum length of 64 bytes"))?;

        wifi.set_configuration(&Configuration::Client(ClientConfiguration {
            ssid,
            password,
            ..Default::default()
        }))?;

        wifi.start()?;
        log::info!("WiFi started");

        let timeout_secs = config.connection_timeout_secs.unwrap_or(10);

        if let Some(led_driver) = led {
            Self::connect_with_led(&mut wifi, led_driver, timeout_secs)?;
        } else {
            wifi.connect()?;
            log::info!("WiFi connected");
            wifi.wait_netif_up()?;
            log::info!("WiFi netif up");
        }

        Ok(Self { wifi })
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

        if wifi.is_connected()? {
            led.set_color(RGB8::new(0, 20, 0))
                .map_err(|e| anyhow::anyhow!("LED error: {:?}", e))?;
        }

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
    pub fn get_ip(&self, timeout_ms: u64) -> anyhow::Result<Option<Ipv4Addr>> {
        let start = std::time::Instant::now();
        let timeout = Duration::from_millis(timeout_ms);

        loop {
            if self.wifi.is_connected()? {
                let ip_info = self.wifi.wifi().sta_netif().get_ip_info()?;

                if !ip_info.ip.is_unspecified() {
                    log::info!("WiFi IP: {:?}", ip_info.ip);
                    return Ok(Some(ip_info.ip));
                }
            }

            if start.elapsed() >= timeout {
                log::warn!("Timeout waiting for WiFi IP address");
                return Ok(None);
            }

            thread::sleep(Duration::from_millis(100));
        }
    }

    /// Returns whether the WiFi is currently connected.
    pub fn is_connected(&self) -> anyhow::Result<bool> {
        Ok(self.wifi.is_connected()?)
    }
}
