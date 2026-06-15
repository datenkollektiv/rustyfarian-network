//! Bare-metal SoftAP smoke-test example for ESP32-C3.
//!
//! Demonstrates [`WiFiManager::init_softap_async`] bringing up a SoftAP
//! using `esp-radio` on top of an `embassy-net` stack with a static IP
//! (`192.168.4.1/24`).
//!
//! Three tasks run alongside the idle main loop:
//!
//! * `net_task` — drives the `embassy-net` stack by calling `runner.run().await`.
//! * `wifi_task` — owns the [`WifiController`], starts the AP, then waits for
//!   station connect/disconnect events and logs them with a running count.
//!
//! The AP comes up **open** (no password), matching the captive-portal UX of the
//! ESP-IDF `idf_c3_provision` / `idf_c3_provision_mqtt` examples — a field
//! operator joins from a phone without ever knowing a password, and the captive
//! portal (not the Wi-Fi layer) is the access control. `AP_SSID` can be set as
//! an environment variable at build time; the default is `"Rustyfarian-Smoke"`.
//!
//! # Build and flash
//!
//! ```sh
//! # default SSID
//! just build-example hal_c3_ap_smoke
//!
//! # custom SSID
//! AP_SSID="My-Portal" just build-example hal_c3_ap_smoke
//! ```

#![no_std]
#![no_main]

extern crate alloc;

use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_println::println;
use esp_radio::wifi::{AccessPointStationEventInfo, Interface, WifiController};
use rustyfarian_esp_hal_wifi::{ApConfig, ApConfigExt, SoftApHandle, WiFiManager, AP_IP};

esp_bootloader_esp_idf::esp_app_desc!();

const AP_SSID: &str = match option_env!("AP_SSID") {
    Some(s) => s,
    None => "Rustyfarian-Smoke",
};

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    esp_println::logger::init_logger(log::LevelFilter::Info);

    let peripherals = esp_hal::init(esp_hal::Config::default());

    // ESP32-C3 has contiguous SRAM — a single 72 KiB region is sufficient for
    // the Wi-Fi radio buffers and general-purpose allocations.
    esp_alloc::heap_allocator!(size: 72 * 1024);

    println!("Initializing SoftAP (bare-metal smoke test)...");

    let config = ApConfig::open(AP_SSID).with_channel(1).with_ap_peripherals(
        peripherals.TIMG0,
        peripherals.SW_INTERRUPT,
        peripherals.WIFI,
    );

    let handle = match WiFiManager::init_softap_async(config) {
        Ok(h) => h,
        Err(e) => {
            println!("FATAL: SoftAP init failed: {}", e);
            loop {}
        }
    };

    let SoftApHandle {
        controller,
        stack: _stack,
        runner,
    } = handle;

    spawner.spawn(net_task(runner).unwrap());
    spawner.spawn(wifi_task(controller).unwrap());

    // Log the AP IP and MAC so the console confirms the AP is up.
    println!("SoftAP IP: {}", AP_IP);

    // Idle loop — real applications would open TCP sockets here.
    loop {
        Timer::after(Duration::from_secs(10)).await;
        println!("SoftAP idle — AP_IP={}", AP_IP);
    }
}

/// Runs the `embassy-net` stack.
///
/// Must be polled continuously; the AP link is only available while this
/// future is being driven.
#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, Interface<'static>>) -> ! {
    runner.run().await
}

/// Waits for and logs SoftAP station connect/disconnect events.
///
/// The AP radio was already started by [`WiFiManager::init_softap_async`] via
/// `set_config` (which triggers `esp_wifi_start` internally in esp-radio 0.18).
/// There is no separate `start_async` call needed; this task goes straight to
/// the event loop.
#[embassy_executor::task]
async fn wifi_task(controller: WifiController<'static>) {
    log::info!(
        "SoftAP running — SSID=\"{}\" — waiting for station events",
        AP_SSID
    );

    let mut connected_count: u32 = 0;

    loop {
        match controller
            .wait_for_access_point_connected_event_async()
            .await
        {
            Ok(AccessPointStationEventInfo::Connected(info)) => {
                connected_count = connected_count.saturating_add(1);
                log::info!(
                    "Station connected: mac={:02x?} aid={} (total connected={})",
                    info.mac,
                    info.aid,
                    connected_count,
                );
            }
            Ok(AccessPointStationEventInfo::Disconnected(info)) => {
                connected_count = connected_count.saturating_sub(1);
                log::info!(
                    "Station disconnected: mac={:02x?} reason={:?} (total connected={})",
                    info.mac,
                    info.reason,
                    connected_count,
                );
            }
            Err(e) => {
                log::warn!("AP event error (non-fatal, continuing): {:?}", e);
            }
        }
    }
}
