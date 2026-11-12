//! ESP-NOW scout — **SoftAP channel-lock variant** (ESP32-C3 Super Mini).
//!
//! Starts the Wi-Fi radio in SoftAP mode (`init_with_radio`), scans the channels
//! the country allows (1-11 by default) to find the coordinator (the `0xF3` probe is answered with a `0xF4`
//! reply naming its channel, the MAC-layer ACK is the fallback, ADR 025), then
//! sends a message every second.  SoftAP beacon scheduling prevents the Wi-Fi driver from
//! autonomously hopping channels, so every send lands on the coordinator's
//! channel without any per-send re-pinning.  This is the race-free, recommended
//! approach for most deployments.
//!
//! See `idf_c3_espnow_scout_promisc` for the STA + promiscuous-bracket
//! alternative when SoftAP conflicts with BLE or a user-facing access point.
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
//! Set `WIFI_COUNTRY` (e.g. `DE`) when the coordinator's network runs on channel 12 or 13;
//! unset, the scout probes only the world-safe channels 1-11.
//! Set `ESPNOW_TX_POWER` (quarter dBm, `8..=84`, e.g. `80` for 20 dBm) to override the radio's TX
//! power; unset, the shared owned-radio default of 13 dBm applies (bisecting radio faults).
//!
//! # Gate scenarios
//!
//! `ESPNOW_SCENARIO` (build time, default `default` = the behaviour above) selects a hardware
//! gate scenario of `docs/runbooks/release-0.7.0-hardware-test.md`.
//! Each scenario drives the async API through `block_on` and logs lines that start with
//! `ESPNOW-GATE <scenario> PASS` or `ESPNOW-GATE <scenario> FAIL`.
//!
//! | `ESPNOW_SCENARIO` | What it does                                                                 |
//! |:------------------|:-----------------------------------------------------------------------------|
//! | `cancel`          | Drops `scan_for_peer` mid-run (delay cycles 3 to 250 ms), then scans again   |
//! | `flood`           | Eight concurrent `send_acked` (1 ms and 400 ms deadlines), then one more send |
//! | `shutdown`        | Queues sends, starts a scan, calls `shutdown` mid-scan, sends once more      |
//! | `shutdown-queued` | Queues sends and calls `shutdown` before any scan, sends once more           |
//! | `txpower`         | Reads the radio TX power before and after each scan and compares them        |
//!
//! `shutdown` and `shutdown-queued` end the driver for good, so each runs one check and then
//! parks; reset the board to repeat it.
//! Run `cancel` first with the coordinator powered off to see the not-found scans, then power it on.
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
use embassy_futures::join::{join3, join_array};
use embassy_futures::yield_now;
use embassy_time::{with_timeout, Duration as EmbassyDuration, Timer};
use rustyfarian_esp_idf_network::espnow::{
    Country, EspIdfEspNow, EspIdfEspNowError, EspNowConfig, MacAddress, ScanConfig, ScanError,
};

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::gpio::PinDriver;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::hal::task::block_on;
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

/// TX power override (`ESPNOW_TX_POWER`, quarter dBm); unset or empty keeps the 13 dBm default.
const TX_POWER_QUARTER_DBM: Option<i8> = match option_env!("ESPNOW_TX_POWER") {
    Some(v) if !v.is_empty() => Some(parse_quarter_dbm(v)),
    _ => None,
};

/// Decimal quarter dBm from the build environment; the constructor validates the range.
const fn parse_quarter_dbm(v: &str) -> i8 {
    let b = v.as_bytes();
    let mut i = 0;
    let mut n: i32 = 0;
    while i < b.len() {
        assert!(
            b[i].is_ascii_digit(),
            "ESPNOW_TX_POWER must be decimal quarter dBm"
        );
        n = n * 10 + (b[i] - b'0') as i32;
        assert!(n <= 127, "ESPNOW_TX_POWER is out of range");
        i += 1;
    }
    n as i8
}

const COORDINATOR_MAC_STR: &str = match option_env!("COORDINATOR_MAC") {
    Some(s) => s,
    None => "FF:FF:FF:FF:FF:FF",
};

/// Gate scenario selected at build time; `default` keeps the plain scout behaviour.
const SCENARIO: &str = match option_env!("ESPNOW_SCENARIO") {
    Some(s) => s,
    None => "default",
};

const SCENARIOS: [&str; 6] = [
    "default",
    "cancel",
    "flood",
    "shutdown",
    "shutdown-queued",
    "txpower",
];

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

    // ── Onboard LED (GPIO 8, active low) ────────────────────────────────
    let mut led = PinDriver::output(peripherals.pins.gpio8)?;
    led.set_high()?; // LED off (active low)

    // ── ESP-NOW with channel scanning (retry until coordinator is found) ─
    let scan_config = ScanConfig::for_peer();

    let espnow = EspIdfEspNow::init_with_radio_config(
        peripherals.modem,
        sys_loop,
        Some(nvs),
        EspNowConfig::default()
            .with_country(COUNTRY)
            .with_tx_power_quarter_dbm(TX_POWER_QUARTER_DBM),
    )?;
    log::info!("radio TX power {:?} quarter dBm", radio_tx_power());

    anyhow::ensure!(
        SCENARIOS.contains(&SCENARIO),
        "unknown ESPNOW_SCENARIO={SCENARIO:?}, expected one of {SCENARIOS:?}"
    );
    log::info!("ESPNOW_SCENARIO={SCENARIO}");
    if SCENARIO == "cancel" {
        scenario_cancel(&espnow, &coordinator_mac, &scan_config);
    }

    loop {
        log::info!("Scanning for coordinator ...");
        match espnow.scan_for_peer_blocking(&coordinator_mac, &scan_config) {
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

    match SCENARIO {
        "flood" => scenario_flood(&espnow, &coordinator_mac),
        "shutdown" => scenario_shutdown(&espnow, &coordinator_mac, true),
        "shutdown-queued" => scenario_shutdown(&espnow, &coordinator_mac, false),
        "txpower" => scenario_txpower(&espnow, &coordinator_mac, &scan_config),
        _ => {}
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
                match espnow.scan_for_peer_blocking(&coordinator_mac, &scan_config) {
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

// ── Gate scenarios (test harness, not library) ──────────────────────────────

/// Logs the single PASS or FAIL line a runbook gate checks.
fn gate(id: &str, ok: bool, detail: &str) {
    if ok {
        log::info!("ESPNOW-GATE {id} PASS: {detail}");
    } else {
        log::error!("ESPNOW-GATE {id} FAIL: {detail}");
    }
}

/// Parks the thread once a one-shot scenario has reported.
/// Parks the scenario after its gate line.
///
/// The gate line is the last console write, and the USB Serial/JTAG console only hands a partial
/// 64-byte packet to the host on a flush; `fsync` flushes it and waits for the host, and
/// the heartbeat flushes again should that wait have timed out.
fn park_forever() -> ! {
    // SAFETY: `fsync` on the console file descriptor has no memory-safety preconditions.
    unsafe {
        esp_idf_svc::sys::fsync(1);
    }
    loop {
        std::thread::sleep(std::time::Duration::from_secs(30));
        log::info!("[app] parked after the scenario");
    }
}

/// Short name of a send result for the scenario logs.
fn class(result: &Result<(), EspIdfEspNowError>) -> &'static str {
    match result {
        Ok(()) => "Ok",
        Err(EspIdfEspNowError::NoAck) => "NoAck",
        Err(EspIdfEspNowError::Timeout) => "Timeout",
        Err(EspIdfEspNowError::Busy) => "Busy",
        Err(EspIdfEspNowError::ScanInProgress) => "ScanInProgress",
        Err(EspIdfEspNowError::Shutdown) => "Shutdown",
        Err(_) => "Other",
    }
}

/// `cancel`: scan, cancel a scan by dropping its future, scan again.
///
/// The cancel delay cycles through short values so that some cancels land mid-probe.
/// A cancelled or not-found scan must leave the pinned channel alone, and a scan after a
/// cancel must find the coordinator on the channel it was found on before.
fn scenario_cancel(espnow: &EspIdfEspNow, mac: &MacAddress, config: &ScanConfig<'_>) -> ! {
    const DELAYS_MS: [u64; 6] = [3, 10, 25, 60, 120, 250];
    let mut known: Option<u8> = None;
    let mut cycle = 0usize;
    loop {
        let delay = DELAYS_MS[cycle % DELAYS_MS.len()];
        cycle += 1;

        let pinned_before = espnow.pinned_channel();
        let cancelled = block_on(with_timeout(
            EmbassyDuration::from_millis(delay),
            espnow.scan_for_peer(mac, config),
        ));
        match cancelled {
            Err(_) => {
                let pinned_after = espnow.pinned_channel();
                gate(
                    "cancel",
                    pinned_before == pinned_after,
                    &format!(
                        "scan cancelled after {delay} ms, pinned channel {pinned_before:?} -> {pinned_after:?}"
                    ),
                );
            }
            Ok(_) => log::info!(
                "[cancel] scan ended within {delay} ms, before the cancel; not a cancel sample"
            ),
        }

        let pinned_before = espnow.pinned_channel();
        match block_on(espnow.scan_for_peer(mac, config)) {
            Ok(found) => {
                let ok = known.is_none_or(|k| k == found.channel);
                gate(
                    "cancel",
                    ok,
                    &format!(
                        "scan after cancel found the coordinator on channel {} (earlier {known:?})",
                        found.channel
                    ),
                );
                known = Some(found.channel);
            }
            Err(EspIdfEspNowError::Scan(e @ ScanError::NotFound { .. })) => {
                let pinned_after = espnow.pinned_channel();
                gate(
                    "cancel",
                    pinned_before == pinned_after,
                    &format!(
                        "not-found scan ended ({e}), pinned channel {pinned_before:?} -> {pinned_after:?}, coordinator earlier on {known:?}"
                    ),
                );
            }
            Err(e) => gate("cancel", false, &format!("scan after cancel failed: {e}")),
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

/// One send of the flood: returns its index, deadline, elapsed time and result.
async fn timed_send(
    espnow: &EspIdfEspNow,
    mac: &MacAddress,
    idx: usize,
    timeout_ms: u64,
) -> (usize, u64, u128, Result<(), EspIdfEspNowError>) {
    let start = std::time::Instant::now();
    let result = espnow
        .send_acked(mac, b"flood", std::time::Duration::from_millis(timeout_ms))
        .await;
    (idx, timeout_ms, start.elapsed().as_millis(), result)
}

/// `flood`: more concurrent `send_acked` futures than queue slots, with short and long deadlines.
///
/// Passes when the flood ends without a hang, every result is a real result or a `Timeout`
/// delivered close to its deadline, and a later send still runs.
fn scenario_flood(espnow: &EspIdfEspNow, mac: &MacAddress) -> ! {
    const FLOOD: usize = juggler::espnow::TX_SLOTS * 2;
    const SLACK_MS: u128 = 100;
    loop {
        let flood = block_on(with_timeout(
            EmbassyDuration::from_secs(10),
            join_array::<_, FLOOD>(core::array::from_fn(|i| {
                let timeout_ms = if i % 2 == 0 { 400 } else { 1 };
                timed_send(espnow, mac, i, timeout_ms)
            })),
        ));
        match flood {
            Err(_) => gate("flood", false, "the flood did not end within 10 s (hang)"),
            Ok(results) => {
                let mut bad = Vec::new();
                let mut summary = String::new();
                for (idx, timeout_ms, elapsed, result) in &results {
                    let c = class(result);
                    summary.push_str(&format!(" #{idx}:{c}@{elapsed}ms/{timeout_ms}"));
                    let allowed = matches!(c, "Ok" | "NoAck" | "Timeout" | "Busy");
                    let late = c == "Timeout" && *elapsed > u128::from(*timeout_ms) + SLACK_MS;
                    if !allowed || late {
                        bad.push(format!(
                            "#{idx} {c} after {elapsed} ms (deadline {timeout_ms} ms)"
                        ));
                    }
                }
                let after = block_on(espnow.send_acked(
                    mac,
                    b"after-flood",
                    std::time::Duration::from_millis(500),
                ));
                let alive = matches!(class(&after), "Ok" | "NoAck");
                if !alive {
                    bad.push(format!("send after the flood ended {}", class(&after)));
                }
                gate(
                    "flood",
                    bad.is_empty(),
                    &format!(
                        "flood of {FLOOD}:{summary}; after-flood send {}; problems {bad:?}",
                        class(&after)
                    ),
                );
            }
        }
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}

/// `shutdown` and `shutdown-queued`: shut the driver down with sends queued (and, with `with_scan`, a scan running).
///
/// A send that was queued when the scan began is dropped by the scan as `ScanInProgress`;
/// that is accepted in the `shutdown` variant, and `shutdown-queued` expects `Shutdown`.
/// A frame that was already on the air may report its real result.
fn scenario_shutdown(espnow: &EspIdfEspNow, mac: &MacAddress, with_scan: bool) -> ! {
    let shutdown_tag = if with_scan {
        "shutdown"
    } else {
        "shutdown-queued"
    };
    const SENDS: usize = 8;
    const CHANNELS: [u8; 65] = {
        let mut list = [0u8; 65];
        let mut i = 0;
        while i < list.len() {
            list[i] = (i % 13) as u8 + 1;
            i += 1;
        }
        list
    };
    // No peer owns this address, so every probe fails and the scan keeps going.
    let silent: MacAddress = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
    let scan_config = ScanConfig::for_peer().with_channels(&CHANNELS);

    let pinned_before = espnow.pinned_channel();
    let outcome = block_on(with_timeout(EmbassyDuration::from_secs(10), async {
        let sends =
            join_array::<_, SENDS>(core::array::from_fn(|i| timed_send(espnow, mac, i, 5000)));
        let scan = async {
            if with_scan {
                Some(espnow.scan_for_peer(&silent, &scan_config).await)
            } else {
                None
            }
        };
        let killer = async {
            if with_scan {
                Timer::after(EmbassyDuration::from_millis(15)).await;
            } else {
                yield_now().await;
            }
            espnow.shutdown();
        };
        join3(sends, scan, killer).await
    }));
    let Ok((sends, scan, ())) = outcome else {
        gate(
            shutdown_tag,
            false,
            "queued requests did not end within 10 s of shutdown (hang)",
        );
        park_forever()
    };

    let mut bad = Vec::new();
    let mut summary = String::new();
    for (idx, _, elapsed, result) in &sends {
        let c = class(result);
        summary.push_str(&format!(" #{idx}:{c}@{elapsed}ms"));
        let ok = match c {
            "Shutdown" => true,
            "ScanInProgress" => with_scan,
            "Ok" | "NoAck" => *idx == 0,
            _ => false,
        };
        if !ok {
            bad.push(format!("#{idx} ended {c}"));
        }
    }
    if let Some(scan) = &scan {
        // The worker publishes `Aborted`; a caller that sees the shutdown flag first gets
        // `Shutdown` (the same pair the juggler host test accepts).
        let aborted = matches!(
            scan,
            Err(EspIdfEspNowError::Scan(ScanError::Aborted)) | Err(EspIdfEspNowError::Shutdown)
        );
        if !aborted {
            bad.push(format!("scan did not end Aborted or Shutdown: {scan:?}"));
        }
    }
    let pinned = espnow.pinned_channel();
    if pinned != pinned_before {
        bad.push(format!(
            "pinned channel changed {pinned_before:?} -> {pinned:?}"
        ));
    }
    let later_send =
        block_on(espnow.send_acked(mac, b"late", std::time::Duration::from_millis(100)));
    if !matches!(later_send, Err(EspIdfEspNowError::Shutdown)) {
        bad.push(format!("later send ended {}", class(&later_send)));
    }
    espnow.shutdown();
    gate(
        shutdown_tag,
        bad.is_empty(),
        &format!(
            "shutdown{}: sends{summary}; scan {:?}; pinned channel {pinned_before:?} -> {pinned:?}; later send {}; second shutdown returned; problems {bad:?}",
            if with_scan { " mid-scan" } else { " with queued sends" },
            scan.as_ref().map(|r| r.as_ref().map(|_| ())),
            class(&later_send),
        ),
    );
    park_forever()
}

/// Reads the radio's maximum TX power in quarter dBm.
fn radio_tx_power() -> Option<i8> {
    let mut level: i8 = 0;
    // SAFETY: `esp_wifi_get_max_tx_power` is an FFI call into the started Wi-Fi driver; it
    // writes the level to the valid `&mut i8` passed in and returns a non-zero error code
    // (read as `None`) when it cannot.
    let rc = unsafe { esp_idf_svc::sys::esp_wifi_get_max_tx_power(&mut level) };
    (rc == 0).then_some(level)
}

/// `txpower`: the radio TX power must be the same before and after each scan.
fn scenario_txpower(espnow: &EspIdfEspNow, mac: &MacAddress, config: &ScanConfig<'_>) -> ! {
    let mut scans = 0u32;
    loop {
        scans += 1;
        let before = radio_tx_power();
        let result = block_on(espnow.scan_for_peer(mac, config));
        let after = radio_tx_power();
        let ok = before.is_some() && before == after && result.is_ok();
        gate(
            "txpower",
            ok,
            &format!(
                "scan {scans} ({}): TX power {before:?} -> {after:?} quarter dBm",
                match &result {
                    Ok(r) => format!("found on channel {}", r.channel),
                    Err(e) => format!("failed: {e}"),
                }
            ),
        );
        std::thread::sleep(std::time::Duration::from_secs(2));
    }
}
