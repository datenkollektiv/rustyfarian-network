//! ESP-NOW scout example — AP-connected STA variant (ESP32-C3 Super Mini).
//!
//! Connects to a Wi-Fi AP (same AP as the coordinator), then initialises
//! ESP-NOW and sends a tagged message to the coordinator every second.
//!
//! Because both devices are associated to the same AP, their radios are
//! locked to the AP's channel.  No channel scanning is needed; the peer is
//! registered with `channel = 0` (use current channel) and frames go out
//! immediately.
//!
//! The onboard LED (GPIO 8, active low) shows status:
//! - **on** — booting, connecting to WiFi, and normal sending state
//! - **blink** — send failed (coordinator unreachable or wrong channel)
//!
//! # Components
//!
//! - ESP32-C3 Super Mini (onboard LED on GPIO 8)
//! - USB cable
//!
//! # Build and Flash
//!
//! `WIFI_SSID`, `WIFI_PASS`, and `COORDINATOR_MAC` must be set at build time.
//! Use the coordinator's STA MAC printed on boot ("Coordinator STA MAC: ...").
//!
//! ```sh
//! WIFI_SSID="MyNetwork" WIFI_PASS="secret" \
//!   COORDINATOR_MAC="aa:bb:cc:dd:ee:ff" \
//!   just build-example idf_c3_espnow_sta_scout
//! ```
//!
//! ```sh
//! just flash idf_c3_espnow_sta_scout
//! ```

use anyhow::Context as _;
use rustyfarian_esp_idf_network::espnow::{EspIdfEspNow, EspNowDriver, MacAddress, PeerConfig};
use rustyfarian_esp_idf_network::wifi::{WiFiConfig, WiFiConfigExt, WiFiManager};

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
const COORDINATOR_MAC_STR: &str = match option_env!("COORDINATOR_MAC") {
    Some(s) => s,
    None => "FF:FF:FF:FF:FF:FF",
};

fn parse_mac(s: &str) -> anyhow::Result<MacAddress> {
    let mut mac = [0u8; 6];
    let mut count = 0;
    for (idx, segment) in s.split(':').enumerate() {
        anyhow::ensure!(idx < 6, "MAC string has more than 6 segments: {:?}", s);
        anyhow::ensure!(
            !segment.is_empty() && segment.len() <= 2,
            "MAC segment {} has invalid length in {:?}",
            idx,
            s
        );
        mac[idx] = u8::from_str_radix(segment, 16)
            .map_err(|e| anyhow::anyhow!("MAC segment {} not valid hex in {:?}: {}", idx, s, e))?;
        count = idx + 1;
    }
    anyhow::ensure!(
        count == 6,
        "MAC string {:?} has {} segments; expected 6",
        s,
        count
    );
    Ok(mac)
}

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let coordinator_mac = parse_mac(COORDINATOR_MAC_STR)
        .with_context(|| format!("failed to parse COORDINATOR_MAC={COORDINATOR_MAC_STR:?}"))?;
    log::info!(
        "Target coordinator MAC: {:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        coordinator_mac[0],
        coordinator_mac[1],
        coordinator_mac[2],
        coordinator_mac[3],
        coordinator_mac[4],
        coordinator_mac[5],
    );

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

    let mut channel: u8 = 0;
    let mut second: esp_idf_svc::sys::wifi_second_chan_t = 0;
    // SAFETY: Wi-Fi has been started by WiFiManager::init; both out-pointers
    // are valid stack variables for the duration of the call.
    let ret = unsafe { esp_idf_svc::sys::esp_wifi_get_channel(&mut channel, &mut second) };
    if ret == esp_idf_svc::sys::ESP_OK {
        log::info!("Operating on Wi-Fi channel {}", channel);
    } else {
        log::warn!("esp_wifi_get_channel failed: error code {}", ret);
    }

    // ── ESP-NOW ─────────────────────────────────────────────────────────
    // WiFiManager has already started the radio; init() attaches ESP-NOW to it.
    let espnow = EspIdfEspNow::init()?;

    // Both devices are associated to the same AP so they share its channel.
    // channel = 0 means "use the current channel" — no explicit channel set needed.
    let peer = PeerConfig::new(coordinator_mac);
    espnow
        .add_peer(&peer)
        .context("failed to add coordinator peer")?;

    log::info!("ESP-NOW ready — sending to coordinator every 1 s");

    // ── Main loop ───────────────────────────────────────────────────────
    let mut seq: u32 = 0;
    loop {
        seq += 1;
        let msg = format!("sta-scout #{seq}");

        match espnow.send_and_wait(&coordinator_mac, msg.as_bytes(), 100) {
            Ok(()) => {
                log::info!("TX #{}: \"{}\" — ACK", seq, msg);
                led.set_low()?; // LED on
            }
            Err(e) => {
                log::warn!("TX #{} failed: {}", seq, e);
                led.set_high()?; // LED off — blink
                std::thread::sleep(std::time::Duration::from_millis(50));
                led.set_low()?;
            }
        }

        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}
