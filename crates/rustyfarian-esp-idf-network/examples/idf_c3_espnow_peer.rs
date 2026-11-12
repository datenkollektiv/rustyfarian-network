//! ESP-NOW peer for the ESP32-C3 Super Mini (ESP-IDF): owned SoftAP radio, no AP connection.
//!
//! ESP-IDF twin of `hal_c3_espnow_peer`: both run the same application body,
//! `examples-shared/espnow_peer_app.rs`, on a [`PeerSession`].
//! This file is setup only: the radio, ESP-NOW, the session and one `block_on`.
//!
//! * [`EspIdfEspNow::init_with_radio`] starts the Wi-Fi radio as a hidden SoftAP on channel 1;
//!   beacon scheduling keeps the driver from hopping channels on its own, so every send lands on
//!   the pinned channel (ADR 012, the same setup as `idf_c3_espnow_scout`).
//! * The session scans for the coordinator (`idf_c6_espnow_coordinator`,
//!   `idf_c3_espnow_coordinator` or `hal_c6_espnow_sta_coordinator`): the scan sends the `0xF3`
//!   probe, the coordinator answers with a `0xF4` reply naming its channel, and a MAC-layer ACK
//!   alone is the fallback (ADR 025).
//!   After 20 consecutive unacknowledged sends the session rescans, which recovers from a channel
//!   change of the coordinator's access point; a failed scan is retried with a doubling backoff.
//! * The shared app sends an 8-byte frame every 100 ms, counts ACKs and echoes, and logs the
//!   counters every 10 s (`[espnow] sent=… acked=… failed=… fail_rate=… echoes=… channel=…
//!   oversize_dropped=… rescans=… local_failures=…`).
//!
//! The scouts (`idf_c3_espnow_scout*`) stay as demos of the blocking API; this example is the
//! async one.
//!
//! Build-time environment (`option_env!`):
//!
//! | Variable          | Meaning                                                                    |
//! |:------------------|:---------------------------------------------------------------------------|
//! | `ESPNOW_PEER_MAC` | Coordinator STA MAC, `aa:bb:cc:dd:ee:ff` (required; printed by it)          |
//! | `COORDINATOR_MAC` | Accepted as a fallback for `ESPNOW_PEER_MAC` (the scouts' variable name)   |
//! | `ESPNOW_SCENARIO` | Only `default` exists here; the gate scenarios live in `idf_c3_espnow_scout` |
//! | `WIFI_COUNTRY`    | Wi-Fi country, e.g. `DE` for a coordinator on channel 12 or 13; unset = `01` (1-11) |
//!
//! No credentials are involved.
//!
//! # Components
//!
//! - ESP32-C3 Super Mini
//! - USB cable
//!
//! # Build and Flash
//!
//! ```sh
//! ESPNOW_PEER_MAC="aa:bb:cc:dd:ee:ff" just build-example idf_c3_espnow_peer
//! ```
//!
//! ```sh
//! just flash idf_c3_espnow_peer
//! ```

use anyhow::Context as _;
use rustyfarian_esp_idf_network::espnow::{
    Country, EmbassyClock, EspIdfEspNow, EspNowConfig, MacAddress, PeerLinkConfig, PeerSession,
};

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::hal::task::block_on;
use esp_idf_svc::nvs::EspDefaultNvsPartition;

#[path = "../../../examples-shared/espnow_peer_app.rs"]
mod espnow_peer_app;

/// `ESPNOW_PEER_MAC` as on the hal peer, `COORDINATOR_MAC` as on the scouts.
/// Wi-Fi country (`WIFI_COUNTRY`, e.g. `DE`); unset or empty is the world-safe `01` (channels 1-11).
/// A code the Wi-Fi driver does not support fails the build.
const COUNTRY: Country = match option_env!("WIFI_COUNTRY") {
    Some(code) if !code.is_empty() => match Country::parse(code) {
        Ok(country) => country,
        Err(_) => panic!("WIFI_COUNTRY is not a supported country code"),
    },
    _ => Country::WORLD,
};

const PEER_MAC_STR: Option<&str> = match option_env!("ESPNOW_PEER_MAC") {
    Some(s) => Some(s),
    None => option_env!("COORDINATOR_MAC"),
};

/// Only the default scenario exists on this example.
const SCENARIO: &str = match option_env!("ESPNOW_SCENARIO") {
    Some(s) => s,
    None => "default",
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

/// Reads the radio's maximum TX power in quarter dBm, `None` when the driver refuses.
fn radio_tx_power() -> Option<i8> {
    let mut level: i8 = 0;
    // SAFETY: `esp_wifi_get_max_tx_power` is an FFI call into the started Wi-Fi driver; it
    // writes the level to the valid `&mut i8` passed in and returns a non-zero error code
    // (read as `None`) when it cannot.
    let rc = unsafe { esp_idf_svc::sys::esp_wifi_get_max_tx_power(&mut level) };
    (rc == 0).then_some(level)
}

/// The station MAC of the chip (the frames leave the SoftAP interface, whose MAC is derived
/// from it); `None` when the driver refuses.
fn sta_mac() -> Option<MacAddress> {
    let mut mac = [0u8; 6];
    // SAFETY: the Wi-Fi driver is initialised by `init_with_radio`; `mac` is a 6-byte buffer.
    let rc = unsafe {
        esp_idf_svc::sys::esp_wifi_get_mac(
            esp_idf_svc::sys::wifi_interface_t_WIFI_IF_STA,
            mac.as_mut_ptr(),
        )
    };
    (rc == esp_idf_svc::sys::ESP_OK).then_some(mac)
}

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    log::info!("[app] start {} (esp-idf)", env!("CARGO_CRATE_NAME"));

    let Some(peer_mac_str) = PEER_MAC_STR else {
        anyhow::bail!("set ESPNOW_PEER_MAC=aa:bb:cc:dd:ee:ff at build time");
    };
    let coordinator = parse_mac(peer_mac_str)
        .with_context(|| format!("failed to parse ESPNOW_PEER_MAC={peer_mac_str:?}"))?;
    log::info!(
        "Target coordinator MAC: {:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}",
        coordinator[0],
        coordinator[1],
        coordinator[2],
        coordinator[3],
        coordinator[4],
        coordinator[5],
    );
    anyhow::ensure!(
        SCENARIO == "default",
        "ESPNOW_SCENARIO={SCENARIO:?} is not supported here; the gate scenarios live in idf_c3_espnow_scout"
    );

    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    // ── ESP-NOW on the owned SoftAP radio ───────────────────────────────
    let espnow = EspIdfEspNow::init_with_radio_config(
        peripherals.modem,
        sys_loop,
        Some(nvs),
        EspNowConfig::default().with_country(COUNTRY),
    )?;

    let mac = sta_mac().unwrap_or_default();
    log::info!(
        "ESP-NOW up on owned radio, tx power {:?} quarter-dBm, own STA MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        radio_tx_power(),
        mac[0],
        mac[1],
        mac[2],
        mac[3],
        mac[4],
        mac[5]
    );
    log::info!("ESPNOW_SCENARIO={SCENARIO}");

    // ── Peer session and the shared app ─────────────────────────────────
    // The session's scans register the coordinator as a peer; the app sends nothing before
    // the first find.
    let session = PeerSession::new(
        &espnow,
        coordinator,
        PeerLinkConfig::default(),
        EmbassyClock,
    );
    let cfg = espnow_peer_app::PeerAppConfig {
        pinned_channel: &|| espnow.pinned_channel(),
        oversize_dropped: &|| espnow.oversize_dropped(),
    };
    block_on(espnow_peer_app::run(&session, &cfg));
    Ok(())
}
