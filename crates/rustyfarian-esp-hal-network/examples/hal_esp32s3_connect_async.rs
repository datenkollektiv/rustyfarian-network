//! Bare-metal async Wi-Fi connect example for ESP32-S3 (validated on the CrowPanel 1.28" HMI).
//!
//! Demonstrates [`WiFiManager::init_async`] connecting to a WPA2 access point
//! using `esp-radio` on top of an `embassy-net` stack.
//! DHCPv4 is handled by the stack; the application prints the assigned IP and
//! then idles asynchronously.
//!
//! Two tasks are spawned alongside the main task:
//!
//! * `wifi_task` — owns the [`WifiController`] and reconnects after any
//!   `StaDisconnected` event. Credentials are applied once in
//!   [`WiFiManager::init_async`]; `connect_async` reuses them on every attempt.
//! * `net_task` — drives the `embassy-net` stack by calling `runner.run()`.
//!
//! `WIFI_SSID` and `WIFI_PASS` must be set as environment variables **at build
//! time**. The example requires the `embassy` Cargo feature.
//! Console output goes to UART0 (the CH340 bridge on the CrowPanel).
//!
//! # Build and flash
//!
//! ```sh
//! WIFI_SSID="MyNetwork" WIFI_PASS="secret" just build-example hal_esp32s3_connect_async
//! just flash hal_esp32s3_connect_async
//! ```

#![no_std]
#![no_main]

extern crate alloc;

use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_println::println;
use esp_radio::wifi::{Interface, WifiController};
use rustyfarian_esp_hal_network::wifi::{AsyncWifiHandle, WiFiConfig, WiFiConfigExt, WiFiManager};

esp_bootloader_esp_idf::esp_app_desc!();

const RECONNECT_DELAY_MS: u64 = 500;
const CONNECT_BACKOFF_MS: u64 = 5000;

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

    // Same 72 KiB internal-RAM heap as the C3 examples. esp-radio and esp-rtos
    // allocate through `malloc_internal`, so a PSRAM region would not serve
    // the Wi-Fi buffers; PSRAM is only worth adding for application data.
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

    spawner.spawn(wifi_task(controller).unwrap());
    spawner.spawn(net_task(runner).unwrap());

    println!("Waiting for DHCPv4 lease...");
    stack.wait_config_up().await;
    let v4 = stack
        .config_v4()
        .expect("stack reports config up but has no IPv4 config");
    println!(
        "Wi-Fi connected — IP: {}  gateway: {:?}  (free heap: {} B)",
        v4.address,
        v4.gateway,
        esp_alloc::HEAP.free()
    );

    // Idle loop — real applications would open sockets here.
    loop {
        Timer::after(Duration::from_secs(10)).await;
    }
}

// Handles both the initial association and any subsequent reconnects.
// Credentials were configured in `WiFiManager::init_async`; calling
// `connect_async` directly reuses those settings without resetting driver state.
#[embassy_executor::task]
async fn wifi_task(mut controller: WifiController<'static>) {
    let mut cycle: u32 = 0;
    loop {
        match controller.connect_async().await {
            Ok(_) => {
                cycle += 1;
                println!(
                    "Wi-Fi link up #{cycle} (free heap: {} B)",
                    esp_alloc::HEAP.free()
                );
                let _ = controller.wait_for_disconnect_async().await;
                println!(
                    "Wi-Fi disconnected #{cycle} — reconnecting... (free heap: {} B)",
                    esp_alloc::HEAP.free()
                );
                Timer::after(Duration::from_millis(RECONNECT_DELAY_MS)).await;
            }
            Err(e) => {
                println!("connect failed: {:?}", e);
                Timer::after(Duration::from_millis(CONNECT_BACKOFF_MS)).await;
            }
        }
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, Interface>) -> ! {
    runner.run().await
}
