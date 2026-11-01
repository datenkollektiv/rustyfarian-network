//! Bare-metal async Wi-Fi connect example for ESP32-C3 Super Mini.
//!
//! Demonstrates [`WiFiManager::init_async`] connecting to a WPA2 access point
//! using `esp-radio` on top of an `embassy-net` stack.
//! DHCPv4 is handled by the stack; the application prints the assigned IP and
//! then idles asynchronously.
//!
//! Two tasks are spawned alongside the main task:
//!
//! * `link_task` — the library's supervisor; owns the [`WifiController`] and
//!   reconnects after any `StaDisconnected` event with a 1 s back-off doubling
//!   to 30 s. Credentials are applied once in
//!   [`WiFiManager::init_async`]; `connect_async` reuses them on every attempt.
//! * `net_task` — drives the `embassy-net` stack by calling `runner.run()`.
//!
//! `WIFI_SSID` and `WIFI_PASS` must be set as environment variables **at build
//! time**. The example requires the `embassy` Cargo feature.
//!
//! # Build and flash
//!
//! ```sh
//! WIFI_SSID="MyNetwork" WIFI_PASS="secret" just build-example hal_c3_connect_async
//! just flash hal_c3_connect_async
//! ```

#![no_std]
#![no_main]

extern crate alloc;

use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_println::println;
use esp_radio::wifi::Interface;
use rustyfarian_esp_hal_network::wifi::{
    link_task, AsyncWifiHandle, WiFiConfig, WiFiConfigExt, WiFiManager, WifiLink,
};

esp_bootloader_esp_idf::esp_app_desc!();

static LINK: WifiLink = WifiLink::new();

const SSID: &str = match option_env!("WIFI_SSID") {
    Some(s) => s,
    None => "",
};
// No literal fallback: an unset `WIFI_PASS` yields the `&str` type default (`""`),
// keeping CodeQL's hard-coded-credential query source-free.
fn password() -> &'static str {
    option_env!("WIFI_PASS").unwrap_or_default()
}

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    esp_println::logger::init_logger(log::LevelFilter::Info);

    let peripherals = esp_hal::init(esp_hal::Config::default());

    // ESP32-C3 has contiguous SRAM — a single 72 KiB region is sufficient for
    // the Wi-Fi radio buffers and general-purpose allocations.
    esp_alloc::heap_allocator!(size: 72 * 1024);

    println!("Initializing Wi-Fi (async)...");

    let config = WiFiConfig::new(SSID, password()).with_peripherals(
        peripherals.TIMG0,
        peripherals.FROM_CPU_INTR0,
        peripherals.WIFI,
    );

    let handle = match WiFiManager::init_async(config) {
        Ok(h) => h,
        Err(e) => {
            println!("FATAL: Wi-Fi init failed: {}", e);
            loop {}
        }
    };

    // Destructure: `stack` is `Copy`, so we keep our own copy before moving
    // `controller` and `runner` into their tasks.
    let AsyncWifiHandle {
        controller,
        stack,
        runner,
    } = handle;

    spawner.spawn(link_task(&LINK, controller, stack).unwrap());
    spawner.spawn(net_task(runner).unwrap());

    println!("Waiting for DHCPv4 lease...");
    stack.wait_config_up().await;
    let v4 = stack
        .config_v4()
        .expect("stack reports config up but has no IPv4 config");
    println!(
        "Wi-Fi connected — IP: {}  gateway: {:?}",
        v4.address, v4.gateway
    );

    // Idle loop — real applications would open sockets here.
    loop {
        Timer::after(Duration::from_secs(10)).await;
        println!(
            "Wi-Fi link ups: {} (free heap: {} B)",
            LINK.connect_count(),
            esp_alloc::HEAP.free()
        );
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, Interface>) -> ! {
    runner.run().await
}
