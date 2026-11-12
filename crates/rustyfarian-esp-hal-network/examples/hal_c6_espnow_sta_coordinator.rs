//! Bare-metal ESP-NOW coordinator for ESP32-C6: Wi-Fi station associated, ESP-NOW alongside.
//!
//! Mirrors the "brain" role of a multi-board prop: the C6 stays associated to the router (so MQTT
//! or other IP traffic could run) and uses ESP-NOW to talk to unassociated peers such as
//! `hal_c3_espnow_peer`.
//! Twin of `idf_c3_espnow_coordinator`: both run the same application body,
//! `examples-shared/espnow_echo_app.rs`, so the two tiers answer peers alike.
//! This file is setup only: Wi-Fi, ESP-NOW and the tasks around the shared app.
//!
//! * Connects the station and waits for a DHCPv4 lease **before** starting ESP-NOW; the radio
//!   channel is then the access point's and stays stable (ESP-NOW started earlier lost most
//!   frames in the field).
//! * [`EspHalEspNow::from_controller`] borrows the controller; A-MPDU is switched off in the Wi-Fi
//!   config, like the ESP-IDF tier's sdkconfig, so Block-Ack exchanges do not starve ESP-NOW RX.
//! * The shared app receives every frame, registers unknown senders as peers (by STA MAC,
//!   channel 0 = current channel), answers a scan probe (`0xF3`) with a `0xF4` reply that names
//!   the current channel (ADR 025) and echoes every other payload back to the sender.
//! * The shared app logs the counters and the channel every 10 s (`[app] rx=… reply_acked=…
//!   reply_failed=… oversize_dropped=… channel=… heap_free=…`); the station MAC is printed at
//!   boot, so the peer can be given `ESPNOW_PEER_MAC`.
//!
//! The ESP-NOW receive queue holds 10 frames (drop-oldest, not configurable); the receive loop
//! does nothing slow between receives.
//!
//! Build-time environment (`option_env!`, never logged):
//!
//! | Variable    | Meaning                                                              |
//! |:------------|:---------------------------------------------------------------------|
//! | `WIFI_SSID` | Wi-Fi SSID; unset means an empty SSID, which `init_async` rejects    |
//! | `WIFI_PASS` | Wi-Fi password; unset means an empty password                        |
//! | `WIFI_COUNTRY` | Wi-Fi country, e.g. `DE`; unset means `01` (channels 1-11, 802.11d on) |
//!
//! With 802.11d on, the station finds an access point on channel 12 or 13 by its passive
//! scan and adopts the access point's country once associated, so the default works there too.
//!
//! # Build and flash
//!
//! ```sh
//! WIFI_SSID="MyNetwork" WIFI_PASS="secret" just build-example hal_c6_espnow_sta_coordinator
//! just flash hal_c6_espnow_sta_coordinator
//! ```

#![no_std]
#![no_main]

extern crate alloc;

use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_println::println;
use esp_radio::wifi::Interface;
use rustyfarian_esp_hal_network::espnow::{EspHalEspNow, EspNowConfig};
use rustyfarian_esp_hal_network::wifi::{
    link_task, AsyncWifiHandle, Country, WiFiConfig, WiFiConfigExt, WiFiManager,
};
use static_cell::StaticCell;

#[path = "../../../examples-shared/espnow_echo_app.rs"]
mod espnow_echo_app;

esp_bootloader_esp_idf::esp_app_desc!();

static LINK: WiFiManager = WiFiManager::new();

const CONNECT_BACKOFF_MS: u64 = 5000;

/// Wi-Fi country from `WIFI_COUNTRY` (e.g. `DE`); unset or empty keeps the world-safe default
/// (`01`, channels 1-11). An unsupported code fails the build.
const COUNTRY: Option<Country> = match option_env!("WIFI_COUNTRY") {
    Some(code) if !code.is_empty() => match Country::parse(code) {
        Ok(country) => Some(country),
        Err(_) => panic!("WIFI_COUNTRY is not a supported Wi-Fi country code"),
    },
    _ => None,
};

const SSID: &str = match option_env!("WIFI_SSID") {
    Some(s) => s,
    None => "",
};
// No literal fallback: an unset secret yields the `&str` type default (`""`).
fn password() -> &'static str {
    option_env!("WIFI_PASS").unwrap_or_default()
}

fn free_heap() -> usize {
    esp_alloc::HEAP.free()
}

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    esp_println::logger::init_logger(log::LevelFilter::Info);
    log::info!("[app] start {} (esp-hal)", env!("CARGO_CRATE_NAME"));

    let peripherals = esp_hal::init(esp_hal::Config::default());

    // ESP32-C6 requires two heap regions: reclaimed IRAM for Wi-Fi DMA buffers and DRAM.
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    println!("Initializing Wi-Fi (async, A-MPDU off)...");
    let mut wifi_config = WiFiConfig::new(SSID, password());
    if let Some(country) = COUNTRY {
        wifi_config = wifi_config.with_country(country);
    }
    let config = wifi_config
        .with_peripherals(
            peripherals.TIMG0,
            peripherals.FROM_CPU_INTR0,
            peripherals.WIFI,
        )
        .with_ampdu(false);
    let handle = match WiFiManager::init_async(config) {
        Ok(h) => h,
        Err(e) => {
            println!("FATAL: Wi-Fi init failed: {}", e);
            halt().await
        }
    };
    let AsyncWifiHandle {
        mut controller,
        stack,
        runner,
    } = handle;
    spawner.spawn(net_task(runner).unwrap());

    // Associate first, then start ESP-NOW on the settled channel.
    loop {
        match controller.connect_async().await {
            Ok(_) => break,
            Err(e) => {
                println!("connect failed: {:?}", e);
                Timer::after(Duration::from_millis(CONNECT_BACKOFF_MS)).await;
            }
        }
    }
    println!("Waiting for DHCPv4 lease...");
    stack.wait_config_up().await;
    println!("Wi-Fi connected");

    // Default config: the station keeps its TX power (see the crate's TX-power policy), the
    // channel follows the access point.
    let espnow = match EspHalEspNow::from_controller(&mut controller, EspNowConfig::default()) {
        Ok(e) => e,
        Err(e) => {
            println!("FATAL: ESP-NOW init failed: {}", e);
            halt().await
        }
    };
    static ESPNOW: StaticCell<EspHalEspNow> = StaticCell::new();
    let espnow: &'static EspHalEspNow = ESPNOW.init(espnow);

    let mac = EspHalEspNow::sta_mac();
    println!(
        "ESP-NOW up on channel {:?}. STA MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} (use as ESPNOW_PEER_MAC on the peer)",
        espnow.current_channel(),
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );

    spawner.spawn(sender_task(espnow).unwrap());
    spawner.spawn(link_task(&LINK, controller, stack).unwrap());

    let cfg = espnow_echo_app::EchoAppConfig {
        oversize_dropped: &|| espnow.oversize_dropped(),
        free_heap,
    };
    espnow_echo_app::run(espnow, &cfg).await;
}

/// Owns every async ESP-NOW transmission (`send_acked` and scan probes); never cancelled.
#[embassy_executor::task]
async fn sender_task(espnow: &'static EspHalEspNow) {
    espnow.run_sender().await
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, Interface>) -> ! {
    runner.run().await
}

/// Parks the task after a fatal error (the message is already logged).
async fn halt() -> ! {
    loop {
        Timer::after(Duration::from_secs(3600)).await;
    }
}
