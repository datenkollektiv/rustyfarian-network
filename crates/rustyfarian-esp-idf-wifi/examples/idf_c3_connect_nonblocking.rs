//! Non-blocking Wi-Fi connect example for ESP32-C3.
//!
//! Demonstrates [`WiFiConfig::connect_nonblocking`]: [`WiFiManager::new_without_led`] returns
//! immediately after initiating association.
//! The firmware must then poll [`WiFiManager::is_connected`] and [`WiFiManager::get_ip`]
//! to discover when the link is up.
//!
//! `WIFI_SSID` and `WIFI_PASS` must be set as environment variables **at build time**.
//! ESP32 firmware cannot read runtime environment variables; the values are baked into the
//! binary by the compiler.
//! With [direnv](https://direnv.net/) and a populated `.envrc`, these are set automatically.
//!
//! # Components
//!
//! - ESP32-C3 development board
//! - USB cable
//!
//! # Build and Flash
//!
//! ```sh
//! WIFI_SSID="MyNetwork" WIFI_PASS="<your-password>" just build-wifi-connect-nonblocking
//! ```
//!
//! ```sh
//! just flash rustyfarian-esp-idf-wifi idf_c3_connect_nonblocking
//! ```

use std::thread;
use std::time::Duration;

use rustyfarian_esp_idf_wifi::{WiFiConfig, WiFiManager};

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::nvs::EspDefaultNvsPartition;

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let ssid = option_env!("WIFI_SSID").unwrap_or("");
    let password = option_env!("WIFI_PASS")
        .ok_or_else(|| anyhow::Error::msg("WIFI_PASS must be set at build time"))?;

    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    let config = WiFiConfig::new(ssid, password).connect_nonblocking();
    let wifi = WiFiManager::new_without_led(peripherals.modem, sys_loop, Some(nvs), config)?;
    log::info!("Wi-Fi connect initiated, waiting for IP...");

    // Poll until an IP address is assigned.
    let ip = loop {
        thread::sleep(Duration::from_secs(1));

        match wifi.is_connected() {
            Ok(true) => {}
            Ok(false) => {
                log::info!("Wi-Fi: associating...");
                continue;
            }
            Err(e) => {
                log::warn!("Wi-Fi status check failed: {:#}", e);
                continue;
            }
        }

        match wifi.get_ip(1_000)? {
            Some(ip) => break ip,
            None => log::info!("Wi-Fi: connected but IP not yet assigned"),
        }
    };

    log::info!("Wi-Fi connected — IP address: {}", ip);

    // Monitor connection state at a relaxed cadence.
    loop {
        thread::sleep(Duration::from_secs(5));
        match wifi.is_connected() {
            Ok(true) => log::info!("Wi-Fi status: connected"),
            Ok(false) => log::warn!("Wi-Fi status: disconnected"),
            Err(e) => log::error!("Wi-Fi status check failed: {:#}", e),
        }
    }
}
