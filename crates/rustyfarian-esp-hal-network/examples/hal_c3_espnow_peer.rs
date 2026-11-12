//! Bare-metal ESP-NOW peer for ESP32-C3: owned radio, no Wi-Fi association, no SSID.
//!
//! Mirrors the "matrix"/"nunchuk" role of a multi-board prop.
//! Twin of `idf_c3_espnow_peer`: both run the same application body,
//! `examples-shared/espnow_peer_app.rs`, on a [`PeerSession`] (ESP-NOW parity Step 7).
//! This file is setup, plus the gate scenarios below.
//!
//! * [`EspHalEspNow::new_owned`] starts an unassociated station (the chip keeps its STA MAC),
//!   with the TX power capped at 13 dBm (52 quarter-dBm) to avoid saturating a nearby receiver.
//! * The session scans for the coordinator (`hal_c6_espnow_sta_coordinator`) through
//!   [`EspHalEspNow::scan_for_peer`]: the scan sends the `0xF3` probe, the coordinator answers
//!   with a `0xF4` reply naming its channel, and a MAC-layer ACK alone is the fallback (ADR 025).
//!   The pinned channel of the last successful scan is kept across rescans and restored if a
//!   scan fails.
//! * The shared app sends an 8-byte frame (big-endian sequence counter) every 100 ms and counts
//!   its ACK; echoed frames are counted, not logged, so the output does not distort the
//!   measurement.
//!   Every 10 s the counters are logged with the failure rate; this is the traffic pattern of
//!   hardware gate G-E1 (30 minutes at 100 ms, failures below 1 %).
//! * After 20 consecutive unacknowledged sends the session rescans, which recovers from a
//!   channel change of the coordinator's access point; a failed scan is retried with a doubling
//!   backoff (`PeerLinkConfig`).
//!
//! Build-time environment (`option_env!`):
//!
//! | Variable          | Meaning                                                            |
//! |:------------------|:-------------------------------------------------------------------|
//! | `ESPNOW_PEER_MAC` | Coordinator STA MAC, `aa:bb:cc:dd:ee:ff` (required; printed by it)  |
//! | `ESPNOW_CHANNEL`  | Optional first guess for the pinned channel (1..=11, see below)    |
//! | `WIFI_COUNTRY`    | Optional Wi-Fi country, e.g. `DE`; unset means `01` (channels 1-11) |
//! | `ESPNOW_SCENARIO` | Optional gate scenario, see below (default `default`, the behaviour above) |
//!
//! No credentials are involved.
//!
//! The unassociated radio cannot transmit outside its country, so the scan skips channels the
//! country does not allow: set `WIFI_COUNTRY` to the country of a coordinator on channel 12 or
//! 13.
//! The radio starts under the world-safe default, so `ESPNOW_CHANNEL` must be within 1-11.
//!
//! # Gate scenarios
//!
//! `ESPNOW_SCENARIO` selects a hardware gate scenario of
//! `docs/runbooks/release-0.7.0-hardware-test.md`.
//! Each scenario logs lines that start with `ESPNOW-GATE <scenario> PASS` or `ESPNOW-GATE <scenario> FAIL`.
//! The scenarios drive the driver directly, without the session.
//!
//! | `ESPNOW_SCENARIO` | What it does                                                                  |
//! |:------------------|:------------------------------------------------------------------------------|
//! | `cancel`          | Drops `scan_for_peer` mid-run (delay cycles 3 to 250 ms), then scans again    |
//! | `flood`           | Eight concurrent `send_acked` (1 ms and 400 ms deadlines), then one more send |
//! | `shutdown`        | Queues sends, starts a scan, calls `shutdown` mid-scan, sends and scans again |
//! | `shutdown-queued` | Queues sends and calls `shutdown` before any scan, sends and scans again      |
//! | `txpower`         | Reads the radio TX power before and after each scan and compares them         |
//!
//! `shutdown` and `shutdown-queued` end the driver for good, so each runs one check and then
//! parks; reset the board to repeat it.
//! Run `cancel` first with the coordinator powered off to see the not-found scans, then power it on.
//!
//! # Build and flash
//!
//! ```sh
//! ESPNOW_PEER_MAC="aa:bb:cc:dd:ee:ff" just build-example hal_c3_espnow_peer
//! just flash hal_c3_espnow_peer
//! ```

#![no_std]
#![no_main]

extern crate alloc;

use alloc::string::String;
use core::fmt::Write as _;
use core::sync::atomic::{AtomicBool, Ordering};

use embassy_executor::Spawner;
use embassy_futures::join::{join3, join_array};
use embassy_time::{with_timeout, Duration, Instant, Timer};
use esp_backtrace as _;
use esp_hal::timer::timg::TimerGroup;
use esp_println::println;
use rustyfarian_esp_hal_network::espnow::{
    EmbassyClock, EspHalEspNow, EspHalEspNowError, EspNowConfig, EspNowDriver, MacAddress,
    PeerConfig, PeerLinkConfig, PeerSession, ScanConfig, ScanError, TxPowerLevel,
};
use rustyfarian_esp_hal_network::wifi::Country;
use static_cell::StaticCell;

#[path = "../../../examples-shared/espnow_peer_app.rs"]
mod espnow_peer_app;

esp_bootloader_esp_idf::esp_app_desc!();

/// Retry delay of the gate scenarios' first scan (the default scenario backs off in the session).
const RESCAN_BACKOFF_MS: u64 = 2000;

/// Set by `sender_task` once `run_sender` has returned.
static SENDER_RETURNED: AtomicBool = AtomicBool::new(false);

/// Gate scenario selected at build time; `default` keeps the plain peer behaviour.
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

/// Wi-Fi country from `WIFI_COUNTRY` (e.g. `DE`); unset or empty keeps the world-safe default
/// (`01`, channels 1-11). An unsupported code fails the build.
const COUNTRY: Option<Country> = match option_env!("WIFI_COUNTRY") {
    Some(code) if !code.is_empty() => match Country::parse(code) {
        Ok(country) => Some(country),
        Err(_) => panic!("WIFI_COUNTRY is not a supported Wi-Fi country code"),
    },
    _ => None,
};

const PEER_MAC: Option<MacAddress> = match option_env!("ESPNOW_PEER_MAC") {
    Some(s) => parse_mac(s),
    None => None,
};
const FIRST_CHANNEL: Option<u8> = match option_env!("ESPNOW_CHANNEL") {
    Some(s) => parse_channel(s),
    None => None,
};

/// Parses `aa:bb:cc:dd:ee:ff` at compile time; anything else is `None`.
const fn parse_mac(s: &str) -> Option<MacAddress> {
    let b = s.as_bytes();
    if b.len() != 17 {
        return None;
    }
    let mut out = [0u8; 6];
    let mut i = 0;
    while i < 6 {
        let hi = hex(b[i * 3]);
        let lo = hex(b[i * 3 + 1]);
        if hi > 15 || lo > 15 {
            return None;
        }
        if i < 5 && b[i * 3 + 2] != b':' {
            return None;
        }
        out[i] = (hi << 4) | lo;
        i += 1;
    }
    Some(out)
}

const fn hex(c: u8) -> u8 {
    match c {
        b'0'..=b'9' => c - b'0',
        b'a'..=b'f' => c - b'a' + 10,
        b'A'..=b'F' => c - b'A' + 10,
        _ => 0xFF,
    }
}

const fn parse_channel(s: &str) -> Option<u8> {
    let b = s.as_bytes();
    if b.is_empty() || b.len() > 2 {
        return None;
    }
    let mut v: u8 = 0;
    let mut i = 0;
    while i < b.len() {
        if b[i] < b'0' || b[i] > b'9' {
            return None;
        }
        v = v * 10 + (b[i] - b'0');
        i += 1;
    }
    if v >= 1 && v <= 13 {
        Some(v)
    } else {
        None
    }
}

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    esp_println::logger::init_logger(log::LevelFilter::Info);
    log::info!("[app] start {} (esp-hal)", env!("CARGO_CRATE_NAME"));

    let peripherals = esp_hal::init(esp_hal::Config::default());

    // ESP32-C3 has contiguous SRAM: one region serves the radio buffers and the heap.
    esp_alloc::heap_allocator!(size: 72 * 1024);

    // esp-radio needs the preemptive scheduler; there is no WiFiManager to start it here.
    // It also installs the time driver, so nothing may `.await` a timer before this point.
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    let Some(coordinator) = PEER_MAC else {
        println!("FATAL: set ESPNOW_PEER_MAC=aa:bb:cc:dd:ee:ff at build time");
        halt().await
    };

    // 13 dBm = 52 quarter-dBm (esp-radio starts at 5 dBm).
    let mut config = EspNowConfig::default().with_tx_power(TxPowerLevel::Medium);
    if let Some(channel) = FIRST_CHANNEL {
        config = config.with_channel(channel);
    }
    // Applied before the channel and the first scan: the allowed channels bound every scan.
    if let Some(country) = COUNTRY {
        config = config.with_country(country);
    }
    let espnow = match EspHalEspNow::new_owned(peripherals.WIFI, config) {
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
        "ESP-NOW up on owned radio, tx power {:?} quarter-dBm, own STA MAC {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        espnow.tx_power_quarter_dbm(),
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    );

    spawner.spawn(sender_task(espnow).unwrap());

    if !SCENARIOS.contains(&SCENARIO) {
        println!(
            "FATAL: unknown ESPNOW_SCENARIO={}, expected one of {:?}",
            SCENARIO, SCENARIOS
        );
        halt().await
    }
    println!("ESPNOW_SCENARIO={}", SCENARIO);

    if SCENARIO == "default" {
        // The session's scans register the coordinator as a peer; the shared app receives
        // through the session, so no separate drain task runs here.
        let session =
            PeerSession::new(espnow, coordinator, PeerLinkConfig::default(), EmbassyClock);
        let cfg = espnow_peer_app::PeerAppConfig {
            pinned_channel: &|| espnow.pinned_channel(),
            oversize_dropped: &|| espnow.oversize_dropped(),
        };
        espnow_peer_app::run(&session, &cfg).await;
        halt().await
    }

    // The gate scenarios drive the driver directly; a drain task keeps the receive queue empty.
    spawner.spawn(rx_task(espnow).unwrap());
    if SCENARIO == "cancel" {
        scenario_cancel(espnow, &coordinator).await
    }

    scan_until_found(espnow, &coordinator).await;

    match SCENARIO {
        "flood" => scenario_flood(espnow, &coordinator).await,
        "shutdown" => scenario_shutdown(espnow, &coordinator, true).await,
        "shutdown-queued" => scenario_shutdown(espnow, &coordinator, false).await,
        "txpower" => scenario_txpower(espnow, &coordinator).await,
        _ => halt().await,
    }
}

/// Owns every async ESP-NOW transmission (`send_acked` and scan probes); never cancelled.
#[embassy_executor::task]
async fn sender_task(espnow: &'static EspHalEspNow) {
    espnow.run_sender().await;
    SENDER_RETURNED.store(true, Ordering::Relaxed);
}

/// Drains the frames the coordinator echoes back during a gate scenario, so the 10-frame RX
/// queue does not overflow; the default scenario receives through the session instead.
/// Probe replies never arrive here: the scan takes them before `receive` sees them.
#[embassy_executor::task]
async fn rx_task(espnow: &'static EspHalEspNow) {
    loop {
        let _ = espnow.receive().await;
    }
}

/// Parks the task after a fatal error (the message is already logged).
async fn halt() -> ! {
    loop {
        Timer::after(Duration::from_secs(3600)).await;
    }
}

/// Scans until the coordinator answers, for the gate scenarios that need it found first; the
/// driver keeps the last good channel across attempts.
async fn scan_until_found(espnow: &EspHalEspNow, coordinator: &MacAddress) {
    let scan = ScanConfig::for_peer();
    loop {
        match espnow.scan_for_peer(coordinator, &scan).await {
            Ok(result) => {
                println!("[espnow] coordinator found on channel {}", result.channel);
                if !espnow.peer_exists(coordinator) {
                    // The scan leaves the peer registered; this keeps the example robust if not.
                    if let Err(e) = espnow.add_peer(&PeerConfig::new(*coordinator)) {
                        println!("[espnow] add_peer failed: {:?}", e);
                    }
                }
                return;
            }
            Err(e) => {
                println!("[espnow] scan failed: {} - retrying", e);
                Timer::after(Duration::from_millis(RESCAN_BACKOFF_MS)).await;
            }
        }
    }
}

// ── Gate scenarios (test harness, not library) ──────────────────────────────

/// Logs the single PASS or FAIL line a runbook gate checks.
fn gate(id: &str, ok: bool, detail: core::fmt::Arguments<'_>) {
    println!(
        "ESPNOW-GATE {} {}: {}",
        id,
        if ok { "PASS" } else { "FAIL" },
        detail
    );
}

/// Short name of a send result for the scenario logs.
fn class(result: &Result<(), EspHalEspNowError>) -> &'static str {
    match result {
        Ok(()) => "Ok",
        Err(EspHalEspNowError::SendFailed) => "SendFailed",
        Err(EspHalEspNowError::Timeout) => "Timeout",
        Err(EspHalEspNowError::Busy) => "Busy",
        Err(EspHalEspNowError::ScanInProgress) => "ScanInProgress",
        Err(EspHalEspNowError::Shutdown) => "Shutdown",
        Err(EspHalEspNowError::NoSenderRunner) => "NoSenderRunner",
        Err(_) => "Other",
    }
}

/// Reads the radio's maximum TX power in quarter dBm, `None` when the driver refuses.
fn radio_tx_power() -> Option<i8> {
    extern "C" {
        fn esp_wifi_get_max_tx_power(power: *mut i8) -> i32;
    }
    let mut power: i8 = 0;
    // SAFETY: `esp_wifi_get_max_tx_power` is provided by esp-wifi-sys, always linked by every
    // esp-radio build, and writes the current level to the valid `&mut i8` passed in.
    // It fails with an error code (read as `None`) when Wi-Fi is not started.
    let rc = unsafe { esp_wifi_get_max_tx_power(&mut power) };
    (rc == 0).then_some(power)
}

/// The radio state a cancelled or not-found scan must leave as it was.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
struct RadioState {
    pinned: Option<u8>,
    shadow_power: Option<i8>,
    radio_power: Option<i8>,
    peer: bool,
}

fn radio_state(espnow: &EspHalEspNow, coordinator: &MacAddress) -> RadioState {
    RadioState {
        pinned: espnow.pinned_channel(),
        shadow_power: espnow.tx_power_quarter_dbm(),
        radio_power: radio_tx_power(),
        peer: espnow.peer_exists(coordinator),
    }
}

/// `cancel`: scan, cancel a scan by dropping its future, scan again.
///
/// The cancel delay cycles through short values so that some cancels land mid-probe.
/// A cancelled or not-found scan must leave the pinned channel, TX power and peer as they were
/// (after the deferred teardown of a probe on the air had time to run), and a scan after a cancel
/// must find the coordinator on the channel it was found on before.
async fn scenario_cancel(espnow: &EspHalEspNow, coordinator: &MacAddress) -> ! {
    const DELAYS_MS: [u64; 6] = [3, 10, 25, 60, 120, 250];
    let scan = ScanConfig::for_peer();
    let mut known: Option<u8> = None;
    let mut cycle = 0usize;
    loop {
        let delay = DELAYS_MS[cycle % DELAYS_MS.len()];
        cycle += 1;

        let before = radio_state(espnow, coordinator);
        let cancelled = with_timeout(
            Duration::from_millis(delay),
            espnow.scan_for_peer(coordinator, &scan),
        )
        .await;
        match cancelled {
            Err(_) => {
                // A probe on the air defers the teardown to the sender runner.
                Timer::after(Duration::from_millis(50)).await;
                let after = radio_state(espnow, coordinator);
                gate(
                    "cancel",
                    before == after,
                    format_args!(
                        "scan cancelled after {} ms, state {:?} -> {:?}",
                        delay, before, after
                    ),
                );
            }
            Ok(_) => println!(
                "[cancel] scan ended within {} ms, before the cancel; not a cancel sample",
                delay
            ),
        }

        let before = radio_state(espnow, coordinator);
        match espnow.scan_for_peer(coordinator, &scan).await {
            Ok(found) => {
                let ok = known.is_none_or(|k| k == found.channel);
                gate(
                    "cancel",
                    ok,
                    format_args!(
                        "scan after cancel found the coordinator on channel {} (earlier {:?})",
                        found.channel, known
                    ),
                );
                known = Some(found.channel);
            }
            Err(EspHalEspNowError::Scan(e @ ScanError::NotFound { .. })) => {
                let after = radio_state(espnow, coordinator);
                gate(
                    "cancel",
                    before == after,
                    format_args!(
                        "not-found scan ended ({:?}), state {:?} -> {:?}, coordinator earlier on {:?}",
                        e, before, after, known
                    ),
                );
            }
            Err(e) => gate(
                "cancel",
                false,
                format_args!("scan after cancel failed: {}", e),
            ),
        }
        Timer::after(Duration::from_secs(2)).await;
    }
}

/// One send of the flood: returns its index, deadline, elapsed time and result.
async fn timed_send(
    espnow: &EspHalEspNow,
    coordinator: &MacAddress,
    idx: usize,
    timeout_ms: u64,
) -> (usize, u64, u64, Result<(), EspHalEspNowError>) {
    let start = Instant::now();
    let result = espnow
        .send_acked(
            coordinator,
            b"flood",
            core::time::Duration::from_millis(timeout_ms),
        )
        .await;
    (idx, timeout_ms, start.elapsed().as_millis(), result)
}

/// `flood`: more concurrent `send_acked` futures than queue slots, with short and long deadlines.
///
/// Passes when the flood ends without a hang, every result is a real result or a `Timeout`
/// delivered close to its deadline, and a later send still runs, which shows the sender runner
/// is alive.
async fn scenario_flood(espnow: &EspHalEspNow, coordinator: &MacAddress) -> ! {
    // Twice the driver's four send slots.
    const FLOOD: usize = 8;
    const SLACK_MS: u64 = 100;
    loop {
        let flood = with_timeout(
            Duration::from_secs(10),
            join_array::<_, FLOOD>(core::array::from_fn(|i| {
                let timeout_ms = if i % 2 == 0 { 400 } else { 1 };
                timed_send(espnow, coordinator, i, timeout_ms)
            })),
        )
        .await;
        match flood {
            Err(_) => gate(
                "flood",
                false,
                format_args!("the flood did not end within 10 s (hang)"),
            ),
            Ok(results) => {
                let mut problems = String::new();
                let mut summary = String::new();
                for (idx, timeout_ms, elapsed, result) in &results {
                    let c = class(result);
                    let _ = write!(summary, " #{}:{}@{}ms/{}", idx, c, elapsed, timeout_ms);
                    let allowed = matches!(c, "Ok" | "SendFailed" | "Timeout" | "Busy");
                    let late = c == "Timeout" && *elapsed > *timeout_ms + SLACK_MS;
                    if !allowed || late {
                        let _ = write!(
                            problems,
                            " #{} {} after {} ms (deadline {} ms);",
                            idx, c, elapsed, timeout_ms
                        );
                    }
                }
                let after = espnow
                    .send_acked(
                        coordinator,
                        b"after-flood",
                        core::time::Duration::from_millis(500),
                    )
                    .await;
                if !matches!(class(&after), "Ok" | "SendFailed") {
                    let _ = write!(problems, " send after the flood ended {};", class(&after));
                }
                gate(
                    "flood",
                    problems.is_empty(),
                    format_args!(
                        "flood of {}:{}; after-flood send {}; problems:{}",
                        FLOOD,
                        summary,
                        class(&after),
                        problems
                    ),
                );
            }
        }
        Timer::after(Duration::from_secs(2)).await;
    }
}

/// `shutdown` and `shutdown-queued`: shut the driver down with sends queued (and, with `with_scan`, a scan running).
///
/// A send that was queued when the scan began is dropped by the scan as `ScanInProgress`;
/// that is accepted in the `shutdown` variant, and `shutdown-queued` expects `Shutdown`.
/// A frame that was already on the air may report its real result.
/// Afterwards a send must return `Shutdown`, a scan must end without touching the radio, and
/// `run_sender` must have returned.
async fn scenario_shutdown(espnow: &EspHalEspNow, coordinator: &MacAddress, with_scan: bool) -> ! {
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

    let outcome = with_timeout(Duration::from_secs(10), async {
        let sends = join_array::<_, SENDS>(core::array::from_fn(|i| {
            timed_send(espnow, coordinator, i, 5000)
        }));
        let scan = async {
            if with_scan {
                Some(espnow.scan_for_peer(&silent, &scan_config).await)
            } else {
                None
            }
        };
        let killer = async {
            if with_scan {
                Timer::after(Duration::from_millis(15)).await;
            } else {
                embassy_futures::yield_now().await;
            }
            espnow.shutdown();
        };
        join3(sends, scan, killer).await
    })
    .await;
    let Ok((sends, scan, ())) = outcome else {
        gate(
            shutdown_tag,
            false,
            format_args!("queued requests did not end within 10 s of shutdown (hang)"),
        );
        halt().await
    };

    let mut problems = String::new();
    let mut summary = String::new();
    for (idx, _, elapsed, result) in &sends {
        let c = class(result);
        let _ = write!(summary, " #{}:{}@{}ms", idx, c, elapsed);
        let ok = match c {
            "Shutdown" => true,
            "ScanInProgress" => with_scan,
            "Ok" | "SendFailed" => *idx == 0,
            _ => false,
        };
        if !ok {
            let _ = write!(problems, " #{} ended {};", idx, c);
        }
    }
    if let Some(scan) = &scan {
        if !matches!(scan, Err(EspHalEspNowError::Scan(ScanError::Aborted))) {
            let _ = write!(problems, " scan did not end Aborted: {:?};", scan.is_ok());
        }
    }

    let before = radio_state(espnow, coordinator);
    let started = Instant::now();
    let later_send = espnow
        .send_acked(coordinator, b"late", core::time::Duration::from_millis(100))
        .await;
    let send_ms = started.elapsed().as_millis();
    if !matches!(later_send, Err(EspHalEspNowError::Shutdown)) || send_ms > 50 {
        let _ = write!(
            problems,
            " later send ended {} after {} ms;",
            class(&later_send),
            send_ms
        );
    }
    let later_scan = espnow
        .scan_for_peer(coordinator, &ScanConfig::for_peer())
        .await;
    let after = radio_state(espnow, coordinator);
    if later_scan.is_ok() || before != after {
        let _ = write!(
            problems,
            " later scan ok={} state {:?} -> {:?};",
            later_scan.is_ok(),
            before,
            after
        );
    }
    // Give the runner time to finish the frame on the air.
    Timer::after(Duration::from_millis(500)).await;
    if !SENDER_RETURNED.load(Ordering::Relaxed) {
        let _ = write!(problems, " run_sender did not return;");
    }
    espnow.shutdown();
    gate(
        shutdown_tag,
        problems.is_empty(),
        format_args!(
            "shutdown{}: sends{}; later send {}; later scan {}; run_sender returned={}; problems:{}",
            if with_scan {
                " mid-scan"
            } else {
                " with queued sends"
            },
            summary,
            class(&later_send),
            if later_scan.is_err() { "refused" } else { "ran" },
            SENDER_RETURNED.load(Ordering::Relaxed),
            problems
        ),
    );
    halt().await
}

/// `txpower`: the radio TX power must be the same before and after each scan.
async fn scenario_txpower(espnow: &EspHalEspNow, coordinator: &MacAddress) -> ! {
    let scan = ScanConfig::for_peer();
    let mut scans = 0u32;
    loop {
        scans += 1;
        let shadow_before = espnow.tx_power_quarter_dbm();
        let before = radio_tx_power();
        let result = espnow.scan_for_peer(coordinator, &scan).await;
        let after = radio_tx_power();
        let shadow_after = espnow.tx_power_quarter_dbm();
        let ok =
            before.is_some() && before == after && shadow_before == shadow_after && result.is_ok();
        gate(
            "txpower",
            ok,
            format_args!(
                "scan {} ({}): radio TX power {:?} -> {:?}, handle {:?} -> {:?} quarter dBm",
                scans,
                if result.is_ok() { "found" } else { "failed" },
                before,
                after,
                shadow_before,
                shadow_after
            ),
        );
        Timer::after(Duration::from_secs(2)).await;
    }
}
