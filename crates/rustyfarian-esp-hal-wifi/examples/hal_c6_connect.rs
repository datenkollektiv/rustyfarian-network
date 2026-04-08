//! Bare-metal Wi-Fi connect example for ESP32-C6.
//!
//! Demonstrates [`WiFiManager`] connecting to a WPA2 access point using
//! `esp-wifi` on a bare-metal (no_std) target.
//!
//! `WIFI_SSID` and `WIFI_PASS` must be set as environment variables **at build time**.
//! With [direnv](https://direnv.net/) and a populated `.envrc`, these are set automatically.
//!
//! # Components
//!
//! - ESP32-C6 development board
//! - USB cable
//!
//! # Build and Flash
//!
//! ```sh
//! WIFI_SSID="MyNetwork" WIFI_PASS="secret" just build-example hal_c6_connect
//! ```
//!
//! ```sh
//! just flash hal_c6_connect
//! ```

#![no_std]
#![no_main]

extern crate alloc;

use esp_backtrace as _;
use esp_hal::main;
use esp_println::println;
use rustyfarian_esp_hal_wifi::{WiFiConfig, WiFiConfigExt, WiFiManager, WifiError};

esp_bootloader_esp_idf::esp_app_desc!();

const SSID: &str = match option_env!("WIFI_SSID") {
    Some(s) => s,
    None => "",
};
const PASSWORD: &str = match option_env!("WIFI_PASS") {
    Some(s) => s,
    None => "",
};

fn run() -> Result<(), WifiError> {
    let peripherals = esp_hal::init(esp_hal::Config::default());

    // ESP32-C6 requires two heap regions: reclaimed IRAM for Wi-Fi DMA + DRAM.
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    let config = WiFiConfig::new(SSID, PASSWORD).with_peripherals(
        peripherals.TIMG0,
        peripherals.SW_INTERRUPT,
        peripherals.WIFI,
    );
    let mut wifi = WiFiManager::init(config)?;

    let ip = wifi.wait_connected(30_000)?;
    println!("Wi-Fi connected — IP: {}", ip);

    Ok(())
}

#[main]
fn main() -> ! {
    esp_println::logger::init_logger_from_env();
    if let Err(e) = run() {
        println!("FATAL: {}", e);
    }
    loop {}
}
