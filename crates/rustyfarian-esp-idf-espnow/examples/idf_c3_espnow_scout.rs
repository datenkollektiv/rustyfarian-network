//! ESP-NOW scout example for ESP32-C3 Super Mini.
//!
//! Starts the Wi-Fi radio without connecting to an AP, then scans channels
//! 1-13 to find the coordinator by MAC-layer ACK.  Once the channel is
//! discovered, the scout sends a message every second.
//!
//! The onboard LED (GPIO 8, active low) shows connection status:
//! - **off** — scanning / not connected
//! - **on** — coordinator found, sending successfully
//! - **blink** — send failed, re-scanning
//!
//! If sending fails (coordinator unreachable), the scout re-scans.
//!
//! Set `COORDINATOR_MAC` at build time to the coordinator's STA MAC address.
//! The coordinator prints its MAC on boot — look for the
//! "Coordinator STA MAC: ..." log line.
//!
//! # Components
//!
//! - ESP32-C3 Super Mini (onboard LED on GPIO 8)
//! - USB cable
//!
//! # Build and Flash
//!
//! ```sh
//! COORDINATOR_MAC="aa:bb:cc:dd:ee:ff" just build-example idf_c3_espnow_scout
//! ```
//!
//! ```sh
//! just flash idf_c3_espnow_scout
//! ```

use anyhow::Context as _;
use rustyfarian_esp_idf_espnow::{EspIdfEspNow, MacAddress, ScanConfig};

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::gpio::PinDriver;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::nvs::EspDefaultNvsPartition;

const COORDINATOR_MAC_STR: &str = match option_env!("COORDINATOR_MAC") {
    Some(s) => s,
    None => "FF:FF:FF:FF:FF:FF",
};

/// Strict parser for `XX:XX:XX:XX:XX:XX` MAC strings.
///
/// Requires exactly 6 colon-separated segments, each one or two hex digits,
/// each segment fitting in a `u8`.  Failing fast on malformed input is
/// preferable to silently substituting `0` for invalid hex — a typo there
/// would otherwise turn into a debugging black hole when frames go to
/// `00:00:00:00:00:00`.
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
    led.set_high()?; // LED off (active low)

    // ── ESP-NOW with channel scanning (retry until coordinator is found) ─
    let scan_config = ScanConfig::new(b"scout-probe");

    let espnow = EspIdfEspNow::init_with_radio(peripherals.modem, sys_loop, Some(nvs))?;

    loop {
        log::info!("Scanning for coordinator ...");
        match espnow.scan_for_peer(&coordinator_mac, &scan_config) {
            Ok(result) => {
                log::info!(
                    "Coordinator found on channel {} — starting send loop",
                    result.channel
                );
                led.set_low()?; // LED on — connected
                break;
            }
            Err(_) => {
                log::warn!("Coordinator not found — retrying in 2s ...");
                std::thread::sleep(std::time::Duration::from_secs(2));
            }
        }
    }

    // ── Main loop ───────────────────────────────────────────────────────
    let mut seq: u32 = 0;
    loop {
        seq += 1;
        let msg = format!("hello #{seq}");

        match espnow.send_and_wait(&coordinator_mac, msg.as_bytes(), 100) {
            Ok(()) => {
                log::info!("TX #{}: \"{}\" — ACK", seq, msg);
                led.set_low()?; // LED on
            }
            Err(e) => {
                log::warn!("TX #{} failed: {} — re-scanning ...", seq, e);
                led.set_high()?; // LED off during scan
                match espnow.scan_for_peer(&coordinator_mac, &scan_config) {
                    Ok(r) => {
                        log::info!("Re-scan: coordinator now on channel {}", r.channel);
                        led.set_low()?; // LED on — reconnected
                    }
                    Err(e) => log::error!("Re-scan failed: {}", e),
                }
            }
        }

        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}
