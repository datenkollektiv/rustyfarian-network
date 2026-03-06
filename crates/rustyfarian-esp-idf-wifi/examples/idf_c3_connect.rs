//! Blocking Wi-Fi connect example for ESP32-C3.
//!
//! Connects to a Wi-Fi network using [`WiFiManager`] in the default blocking mode,
//! then logs the assigned IP address and polls [`WiFiManager::is_connected`] every five seconds.
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
//! WIFI_SSID="MyNetwork" WIFI_PASS="<your-password>" just build-wifi-connect
//! ```
//!
//! ```sh
//! just flash rustyfarian-esp-idf-wifi idf_c3_connect
//! ```

use std::thread;
use std::time::Duration;

use rustyfarian_esp_idf_wifi::{WiFiConfig, WiFiManager};

use anyhow::anyhow;
use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::nvs::EspDefaultNvsPartition;

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let ssid = option_env!("WIFI_SSID").unwrap_or("");
    let password = option_env!("WIFI_PASS")
        .ok_or_else(|| anyhow!("WIFI_PASS environment variable must be set at build time"))?;

    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    let config = WiFiConfig::new(ssid, password);
    let wifi = WiFiManager::new_without_led(peripherals.modem, sys_loop, Some(nvs), config)?;

    match wifi.get_ip(30_000)? {
        Some(ip) => log::info!("Connected — IP address: {}", ip),
        None => log::error!("IP address not assigned within timeout"),
    }

    loop {
        match wifi.is_connected() {
            Ok(true) => log::info!("Wi-Fi status: connected"),
            Ok(false) => log::warn!("Wi-Fi status: disconnected"),
            Err(e) => log::error!("Wi-Fi status check failed: {:#}", e),
        }
        thread::sleep(Duration::from_secs(5));
    }
}
