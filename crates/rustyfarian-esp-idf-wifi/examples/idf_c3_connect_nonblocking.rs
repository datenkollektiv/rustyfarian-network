//! Non-blocking Wi-Fi connect example for ESP32-C3.
//!
//! Demonstrates [`WiFiConfig::connect_nonblocking`]: [`WiFiManager::init`] returns
//! immediately after initiating association.
//! The firmware calls [`WiFiManager::wait_connected`] to block until the link is up.
//!
//! `WIFI_SSID` and `WIFI_PASS` must be set as environment variables **at build time**.
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
//! WIFI_SSID="MyNetwork" WIFI_PASS="<your-password>" just build-example idf_c3_connect_nonblocking
//! ```
//!
//! ```sh
//! just flash idf_c3_connect_nonblocking
//! ```

use rustyfarian_esp_idf_wifi::{WiFiConfig, WiFiConfigExt, WiFiManager};

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::nvs::EspDefaultNvsPartition;

const SSID: &str = match option_env!("WIFI_SSID") {
    Some(s) => s,
    None => "",
};
const PASSWORD: &str = match option_env!("WIFI_PASS") {
    Some(s) => s,
    None => "",
};

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    let config = WiFiConfig::new(SSID, PASSWORD)
        .connect_nonblocking()
        .with_peripherals(peripherals.modem, sys_loop, Some(nvs));
    let wifi = WiFiManager::init(config)?;

    let ip = wifi.wait_connected(30_000)?;
    log::info!("Wi-Fi connected — IP: {}", ip);

    Ok(())
}
