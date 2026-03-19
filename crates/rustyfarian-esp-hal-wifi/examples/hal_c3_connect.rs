//! Bare-metal Wi-Fi connect example for ESP32-C3.
//!
//! Demonstrates [`WiFiManager`] connecting to a WPA2 access point using
//! `esp-radio` on a bare-metal (no_std) target.
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
//! WIFI_SSID="MyNetwork" WIFI_PASS="secret" just build-example hal_c3_connect
//! ```
//!
//! ```sh
//! just flash hal_c3_connect
//! ```

#![no_std]
#![no_main]

extern crate alloc;

esp_bootloader_esp_idf::esp_app_desc!();

use esp_hal::clock::CpuClock;
use esp_hal::main;
use esp_println::println;
use rustyfarian_esp_hal_wifi::{WiFiConfig, WiFiConfigExt, WiFiManager, WifiError};

const SSID: &str = match option_env!("WIFI_SSID") {
    Some(s) => s,
    None => "",
};
const PASSWORD: &str = match option_env!("WIFI_PASS") {
    Some(s) => s,
    None => "",
};

#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    println!("PANIC: {}", info);
    loop {}
}

fn run() -> Result<(), WifiError> {
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));

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
    if let Err(e) = run() {
        println!("FATAL: {}", e);
    }
    loop {}
}
