//! Bare-metal ESP-NOW coordinator for ESP32-C6: Wi-Fi station associated, ESP-NOW alongside.
//!
//! Mirrors the "brain" role of a multi-board prop: the C6 stays associated to the router (so MQTT
//! or other IP traffic could run) and uses ESP-NOW to talk to unassociated peers such as
//! `hal_c3_espnow_peer`.
//!
//! * Connects the station and waits for a DHCPv4 lease **before** starting ESP-NOW; the radio
//!   channel is then the access point's and stays stable (ESP-NOW started earlier lost most
//!   frames in the field).
//! * [`EspHalEspNow::from_controller`] borrows the controller; A-MPDU is switched off in the Wi-Fi
//!   config, like the ESP-IDF tier's sdkconfig, so Block-Ack exchanges do not starve ESP-NOW RX.
//! * `rx_task` receives every frame, registers unknown senders as peers (by STA MAC, channel 0 =
//!   current channel) and echoes the payload back to the sender.
//! * The main loop logs the station MAC and counters every 10 s, so the peer can be given
//!   `ESPNOW_PEER_MAC` and the oversize-drop counter (frames above 250 B) can be watched.
//!
//! The ESP-NOW receive queue holds 10 frames (drop-oldest, not configurable); `rx_task` does
//! nothing slow between receives.
//!
//! Build-time environment (`option_env!`, never logged):
//!
//! | Variable    | Meaning                                                              |
//! |:------------|:---------------------------------------------------------------------|
//! | `WIFI_SSID` | Wi-Fi SSID; unset means an empty SSID, which `init_async` rejects    |
//! | `WIFI_PASS` | Wi-Fi password; unset means an empty password                        |
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

use core::sync::atomic::{AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_println::println;
use esp_radio::wifi::{Interface, WifiController};
use rustyfarian_esp_hal_network::espnow::{
    EspHalEspNow, EspHalEspNowError, EspNowConfig, EspNowDriver, PeerConfig,
};
use rustyfarian_esp_hal_network::wifi::{AsyncWifiHandle, WiFiConfig, WiFiConfigExt, WiFiManager};
use static_cell::StaticCell;

esp_bootloader_esp_idf::esp_app_desc!();

const RECONNECT_DELAY_MS: u64 = 500;
const CONNECT_BACKOFF_MS: u64 = 5000;
const STATS_SECS: u64 = 10;

const SSID: &str = match option_env!("WIFI_SSID") {
    Some(s) => s,
    None => "",
};
// No literal fallback: an unset secret yields the `&str` type default (`""`).
fn password() -> &'static str {
    option_env!("WIFI_PASS").unwrap_or_default()
}

static RX_FRAMES: AtomicU32 = AtomicU32::new(0);
static TX_ACKED: AtomicU32 = AtomicU32::new(0);
static TX_FAILED: AtomicU32 = AtomicU32::new(0);

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    esp_println::logger::init_logger(log::LevelFilter::Info);

    let peripherals = esp_hal::init(esp_hal::Config::default());

    // ESP32-C6 requires two heap regions: reclaimed IRAM for Wi-Fi DMA buffers and DRAM.
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    println!("Initializing Wi-Fi (async, A-MPDU off)...");
    let config = WiFiConfig::new(SSID, password())
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
        "ESP-NOW up. STA MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} (use as ESPNOW_PEER_MAC on the peer)",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );

    spawner.spawn(sender_task(espnow).unwrap());
    spawner.spawn(rx_task(espnow).unwrap());
    spawner.spawn(wifi_task(controller).unwrap());

    loop {
        Timer::after(Duration::from_secs(STATS_SECS)).await;
        println!(
            "[espnow] rx={} reply_acked={} reply_failed={} oversize_dropped={} heap_free={} B",
            RX_FRAMES.load(Ordering::Relaxed),
            TX_ACKED.load(Ordering::Relaxed),
            TX_FAILED.load(Ordering::Relaxed),
            espnow.oversize_dropped(),
            esp_alloc::HEAP.free()
        );
    }
}

/// Owns every async ESP-NOW transmission (`send_async*` and scan probes); never cancelled.
#[embassy_executor::task]
async fn sender_task(espnow: &'static EspHalEspNow) -> ! {
    espnow.run_sender().await
}

/// Receives frames and echoes each payload back to its sender.
#[embassy_executor::task]
async fn rx_task(espnow: &'static EspHalEspNow) {
    loop {
        let event = match espnow.recv_async().await {
            Ok(event) => event,
            Err(e) => {
                println!("[espnow] recv failed: {}", e);
                Timer::after(Duration::from_millis(100)).await;
                continue;
            }
        };
        RX_FRAMES.fetch_add(1, Ordering::Relaxed);

        // Unicast needs a peer entry; channel 0 follows the associated channel.
        if !espnow.peer_exists(&event.mac) {
            if let Err(e) = espnow.add_peer(&PeerConfig::new(event.mac)) {
                println!("[espnow] add_peer failed: {:?}", e);
                continue;
            }
        }
        match espnow
            .send_async_timeout(
                &event.mac,
                event.payload(),
                core::time::Duration::from_millis(100),
            )
            .await
        {
            Ok(()) => {
                TX_ACKED.fetch_add(1, Ordering::Relaxed);
            }
            Err(EspHalEspNowError::SendFailed | EspHalEspNowError::Timeout) => {
                TX_FAILED.fetch_add(1, Ordering::Relaxed);
            }
            Err(e) => println!("[espnow] reply error: {}", e),
        }
    }
}

// Reconnects after link loss; the first association happened in `main`.
#[embassy_executor::task]
async fn wifi_task(mut controller: WifiController<'static>) {
    loop {
        let _ = controller.wait_for_disconnect_async().await;
        println!("Wi-Fi disconnected - reconnecting...");
        loop {
            Timer::after(Duration::from_millis(RECONNECT_DELAY_MS)).await;
            match controller.connect_async().await {
                Ok(_) => {
                    println!("Wi-Fi link up again");
                    break;
                }
                Err(e) => {
                    println!("connect failed: {:?}", e);
                    Timer::after(Duration::from_millis(CONNECT_BACKOFF_MS)).await;
                }
            }
        }
    }
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
