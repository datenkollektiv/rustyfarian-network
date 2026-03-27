//! ESP-NOW coordinator example for ESP32-C3 Super Mini.
//!
//! Connects to a Wi-Fi AP (adopting the AP's channel), then initialises
//! ESP-NOW and listens for incoming frames.  Every received frame is logged
//! with the sender MAC and payload.
//!
//! The onboard LED (GPIO 8, active low) shows status:
//! - **on** — connecting to WiFi / booting
//! - **off briefly** — WiFi connected, initialising ESP-NOW
//! - **on** — ESP-NOW ready, waiting for frames
//! - **flicker** — frame received
//!
//! The coordinator's Wi-Fi channel is dictated by the AP.  The scout device
//! discovers this channel automatically via [`scan_for_peer`].
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

use rustyfarian_esp_idf_espnow::{EspIdfEspNow, EspNowDriver};
use rustyfarian_esp_idf_wifi::{WiFiConfig, WiFiConfigExt, WiFiManager};

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::gpio::PinDriver;
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

    // ── Onboard LED (GPIO 8, active low) ────────────────────────────────
    let mut led = PinDriver::output(peripherals.pins.gpio8)?;
    led.set_low()?; // LED on — booting / connecting

    // ── Wi-Fi ───────────────────────────────────────────────────────────
    let config =
        WiFiConfig::new(SSID, PASSWORD).with_peripherals(peripherals.modem, sys_loop, Some(nvs));
    let wifi = WiFiManager::init(config)?;

    let ip = wifi.wait_connected(30_000)?;
    log::info!("Wi-Fi connected — IP: {}", ip);

    // Brief off to signal WiFi done
    led.set_high()?;
    std::thread::sleep(std::time::Duration::from_millis(200));

    let mut channel: u8 = 0;
    let mut second: esp_idf_svc::sys::wifi_second_chan_t = 0;
    unsafe { esp_idf_svc::sys::esp_wifi_get_channel(&mut channel, &mut second) };
    log::info!("Operating on Wi-Fi channel {}", channel);

    let mut mac = [0u8; 6];
    unsafe {
        esp_idf_svc::sys::esp_wifi_get_mac(
            esp_idf_svc::sys::wifi_interface_t_WIFI_IF_STA,
            mac.as_mut_ptr(),
        );
    }
    log::info!(
        "Coordinator STA MAC: {:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        mac[0],
        mac[1],
        mac[2],
        mac[3],
        mac[4],
        mac[5]
    );

    // ── ESP-NOW ─────────────────────────────────────────────────────────
    let espnow = EspIdfEspNow::init()?;
    log::info!("ESP-NOW initialised — waiting for frames ...");

    led.set_low()?; // LED on — ready

    loop {
        if let Some(event) = espnow.try_recv() {
            log::info!(
                "RX from {:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}: {:?}",
                event.mac[0],
                event.mac[1],
                event.mac[2],
                event.mac[3],
                event.mac[4],
                event.mac[5],
                core::str::from_utf8(event.payload()).unwrap_or("<binary>"),
            );
            // Flicker on receive
            led.set_high()?;
            std::thread::sleep(std::time::Duration::from_millis(50));
            led.set_low()?;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}
