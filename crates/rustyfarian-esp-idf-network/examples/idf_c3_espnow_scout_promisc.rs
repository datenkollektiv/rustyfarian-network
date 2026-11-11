//! ESP-NOW scout — **STA + promiscuous-bracket variant** (ESP32-C3 Super Mini).
//!
//! Starts the Wi-Fi radio in unassociated STA mode (`init_with_radio_sta`),
//! scans the channels the country allows (1-11 by default) to find the coordinator, then sends a
//! message every second.  Before each send, `send_and_wait` re-pins the radio
//! to the discovered channel using a promiscuous-mode bracket, which suppresses
//! the ESP-IDF background channel scanner for the duration of the set + send.
//!
//! # Trade-off vs SoftAP variant
//!
//! The Wi-Fi background scan task runs at FreeRTOS priority 23; the application
//! task at priority 5.  The scheduler can preempt between the promiscuous
//! disable and `esp_now_send`, causing occasional `ESP_ERR_ESPNOW_CHAN` (~0–20 %
//! of sends under load).  Failures trigger a re-scan and retry automatically.
//!
//! Use this variant when SoftAP mode conflicts with another radio requirement on
//! the same device (BLE coexistence, user-facing access point, etc.).
//! See `idf_c3_espnow_scout` for the race-free SoftAP alternative.
//!
//! The onboard LED (GPIO 8, active low) shows connection status:
//! - **off** — scanning / not connected
//! - **on** — coordinator found, sending successfully
//! - **blink** — send failed, re-scanning
//!
//! # Components
//!
//! - ESP32-C3 Super Mini (onboard LED on GPIO 8)
//! - USB cable
//!
//! # Build and Flash
//!
//! Set `WIFI_COUNTRY` (e.g. `DE`) when the coordinator's network runs on channel 12 or 13;
//! unset, the scout probes only the world-safe channels 1-11.
//!
//! ```sh
//! COORDINATOR_MAC="aa:bb:cc:dd:ee:ff" just build-example idf_c3_espnow_scout_promisc
//! ```
//!
//! ```sh
//! just flash idf_c3_espnow_scout_promisc
//! ```

use anyhow::Context as _;
use rustyfarian_esp_idf_network::espnow::{
    Country, EspIdfEspNow, EspNowConfig, MacAddress, ScanConfig,
};

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::gpio::PinDriver;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::nvs::EspDefaultNvsPartition;

/// Wi-Fi country (`WIFI_COUNTRY`, e.g. `DE`); unset or empty is the world-safe `01` (channels 1-11).
/// A code the Wi-Fi driver does not support fails the build.
const COUNTRY: Country = match option_env!("WIFI_COUNTRY") {
    Some(code) if !code.is_empty() => match Country::parse(code) {
        Ok(country) => country,
        Err(_) => panic!("WIFI_COUNTRY is not a supported country code"),
    },
    _ => Country::WORLD,
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
    log::info!("[app] start {} (esp-idf)", env!("CARGO_CRATE_NAME"));

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

    let mut led = PinDriver::output(peripherals.pins.gpio8)?;
    led.set_high()?; // LED off (active low)

    let scan_config = ScanConfig::for_peer();

    // STA mode: promiscuous bracket re-pins the channel before every send.
    let espnow = EspIdfEspNow::init_with_radio_sta_config(
        peripherals.modem,
        sys_loop,
        Some(nvs),
        EspNowConfig::default().with_country(COUNTRY),
    )?;

    // `connected` tracks whether the scout currently believes it knows the
    // coordinator's channel.  Boot in scanning state (false); the main loop
    // performs both the initial discovery and any later recovery scans so the
    // next send is only attempted once we have a fresh channel — gating
    // send_and_wait on the re-scan outcome avoids piling failures on top of a
    // stale pinned channel.
    let mut connected = false;
    let mut seq: u32 = 0;
    loop {
        if !connected {
            log::info!("Scanning for coordinator ...");
            match espnow.scan_for_peer_blocking(&coordinator_mac, &scan_config) {
                Ok(r) => {
                    log::info!("Re-scan: coordinator now on channel {}", r.channel);
                    led.set_low()?;
                    connected = true;
                }
                Err(e) => {
                    log::warn!("Re-scan failed: {} — retrying in 2s ...", e);
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    continue;
                }
            }
        }

        seq += 1;
        let msg = format!("hello #{seq}");

        match espnow.send_and_wait(&coordinator_mac, msg.as_bytes(), 100) {
            Ok(()) => {
                log::info!("TX #{}: \"{}\" — ACK", seq, msg);
                led.set_low()?;
            }
            Err(e) => {
                log::warn!("TX #{} failed: {} — dropping to scanning state", seq, e);
                led.set_high()?;
                connected = false;
                continue;
            }
        }

        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}
