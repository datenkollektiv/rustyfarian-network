//! ESP-NOW coordinator example for ESP32-C3 Super Mini.
//!
//! Connects to a Wi-Fi AP (adopting the AP's channel), initialises ESP-NOW and
//! runs the shared coordinator application through `block_on`.
//! Twin of `hal_c6_espnow_sta_coordinator`: both run the same application body,
//! `examples-shared/espnow_echo_app.rs`, so the two tiers answer peers alike.
//! This file is setup only: Wi-Fi, the LED, ESP-NOW and one `block_on`.
//!
//! The shared app registers unknown senders as peers (channel 0 = current
//! channel), answers a scan probe (`0xF3`) with a `0xF4` reply that names the
//! current channel (ADR 025), echoes every other payload back to its sender and
//! logs the counters and the channel every 10 s (`[app] rx=… reply_acked=…
//! reply_failed=… oversize_dropped=… channel=… heap_free=…`).
//!
//! The onboard LED (GPIO 8, active low) shows status:
//! - **on** — connecting to WiFi / booting
//! - **off briefly** — WiFi connected, initialising ESP-NOW
//! - **on** — ESP-NOW ready, answering frames
//!
//! The per-frame flicker of earlier releases is gone: the shared app owns the
//! receive loop, and the counters line reports the traffic instead.
//!
//! The coordinator's Wi-Fi channel is dictated by the AP.  The scout device
//! discovers this channel automatically via [`scan_for_peer`]: the coordinator
//! states it in the probe reply, and the MAC-layer ACK stays the fallback.
//!
//! # Components
//!
//! - ESP32-C3 Super Mini (onboard LED on GPIO 8)
//! - USB cable
//!
//! # Build and Flash
//!
//! `WIFI_SSID` and `WIFI_PASS` must be set at build time (or via `.envrc`).
//!
//! ```sh
//! WIFI_SSID="MyNetwork" WIFI_PASS="secret" just build-example idf_c3_espnow_coordinator
//! ```
//!
//! ```sh
//! just flash idf_c3_espnow_coordinator
//! ```

use rustyfarian_esp_idf_network::espnow::EspIdfEspNow;
use rustyfarian_esp_idf_network::wifi::{WiFiConfig, WiFiConfigExt, WiFiManager};

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::gpio::PinDriver;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::hal::task::block_on;
use esp_idf_svc::nvs::EspDefaultNvsPartition;

#[path = "../../../examples-shared/espnow_echo_app.rs"]
mod espnow_echo_app;

const SSID: &str = match option_env!("WIFI_SSID") {
    Some(s) => s,
    None => "",
};
// No literal fallback: an unset `WIFI_PASS` yields the `&str` type default (`""`),
// keeping CodeQL's hard-coded-credential query source-free.
fn password() -> &'static str {
    option_env!("WIFI_PASS").unwrap_or_default()
}

fn free_heap() -> usize {
    // SAFETY: plain FFI query with no arguments or side effects.
    unsafe { esp_idf_svc::sys::esp_get_free_heap_size() as usize }
}

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    log::info!("[app] start {} (esp-idf)", env!("CARGO_CRATE_NAME"));

    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    // ── Onboard LED (GPIO 8, active low) ────────────────────────────────
    let mut led = PinDriver::output(peripherals.pins.gpio8)?;
    led.set_low()?; // LED on — booting / connecting

    // ── Wi-Fi ───────────────────────────────────────────────────────────
    let config =
        WiFiConfig::new(SSID, password()).with_peripherals(peripherals.modem, sys_loop, Some(nvs));
    let wifi = WiFiManager::init(config)?;

    let ip = wifi.wait_connected(30_000)?;
    log::info!("Wi-Fi connected — IP: {}", ip);

    // Brief off to signal WiFi done
    led.set_high()?;
    std::thread::sleep(std::time::Duration::from_millis(200));

    let mut mac = [0u8; 6];
    // SAFETY: Wi-Fi has been started by WiFiManager::init above; `mac` is a
    // 6-byte buffer and the STA interface is up.
    let ret = unsafe {
        esp_idf_svc::sys::esp_wifi_get_mac(
            esp_idf_svc::sys::wifi_interface_t_WIFI_IF_STA,
            mac.as_mut_ptr(),
        )
    };
    if ret == esp_idf_svc::sys::ESP_OK {
        log::info!(
            "Coordinator STA MAC: {:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
            mac[0],
            mac[1],
            mac[2],
            mac[3],
            mac[4],
            mac[5]
        );
    } else {
        log::warn!("esp_wifi_get_mac failed: error code {}", ret);
    }

    // ── ESP-NOW ─────────────────────────────────────────────────────────
    let espnow = EspIdfEspNow::init()?;
    match espnow.current_channel() {
        Some(channel) => log::info!("Operating on Wi-Fi channel {}", channel),
        None => log::warn!("the radio did not report its channel"),
    }
    log::info!("ESP-NOW initialised — answering frames ...");

    led.set_low()?; // LED on — ready

    let cfg = espnow_echo_app::EchoAppConfig {
        oversize_dropped: &|| espnow.oversize_dropped(),
        free_heap,
    };
    block_on(espnow_echo_app::run(&espnow, &cfg));
    Ok(())
}
