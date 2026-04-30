//! Bare-metal async Wi-Fi connect example for ESP32-C3 Super Mini.
//!
//! Demonstrates [`WiFiManager::init_async`] connecting to a WPA2 access point
//! using `esp-radio` on top of an `embassy-net` stack.
//! DHCPv4 is handled by the stack; the application prints the assigned IP and
//! then idles asynchronously.
//!
//! Two tasks are spawned alongside the main task:
//!
//! * `wifi_task` — owns the [`WifiController`] and keeps the station
//!   associated, reconnecting after any `StaDisconnected` event.
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
use esp_radio::wifi::{Interface, WifiController};
use rustyfarian_esp_hal_wifi::{AsyncWifiHandle, WiFiConfig, WiFiConfigExt, WiFiManager};

esp_bootloader_esp_idf::esp_app_desc!();

const SSID: &str = match option_env!("WIFI_SSID") {
    Some(s) => s,
    None => "",
};
const PASSWORD: &str = match option_env!("WIFI_PASS") {
    Some(s) => s,
    None => "",
};

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    esp_println::logger::init_logger(log::LevelFilter::Info);

    let peripherals = esp_hal::init(esp_hal::Config::default());

    // ESP32-C3 has contiguous SRAM — a single 72 KiB region is sufficient for
    // the Wi-Fi radio buffers and general-purpose allocations.
    esp_alloc::heap_allocator!(size: 72 * 1024);

    println!("Initializing Wi-Fi (async)...");

    let config = WiFiConfig::new(SSID, PASSWORD).with_peripherals(
        peripherals.TIMG0,
        peripherals.SW_INTERRUPT,
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

    spawner.spawn(wifi_task(controller).unwrap());
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
    }
}

// Initial association is started by `WiFiManager::init_async`; this task
// only handles reconnection after a disconnect event.
#[embassy_executor::task]
async fn wifi_task(mut controller: WifiController<'static>) {
    // `wait_for_disconnect_async` + `connect_async` replace the sync
    // `wait_for_event(StaDisconnected)` + `connect` pair removed in
    // esp-radio 0.18.
    loop {
        let _ = controller.wait_for_disconnect_async().await;
        println!("Wi-Fi disconnected — attempting to reconnect");
        Timer::after(Duration::from_millis(500)).await;
        if let Err(e) = controller.connect_async().await {
            println!("reconnect failed: {:?}", e);
        }
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, Interface<'static>>) -> ! {
    runner.run().await
}
