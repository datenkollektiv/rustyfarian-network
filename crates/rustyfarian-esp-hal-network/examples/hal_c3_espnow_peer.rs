//! Bare-metal ESP-NOW peer for ESP32-C3: owned radio, no Wi-Fi association, no SSID.
//!
//! Mirrors the "matrix"/"nunchuk" role of a multi-board prop:
//!
//! * [`EspHalEspNow::new_owned`] starts an unassociated station (the chip keeps its STA MAC),
//!   with the TX power capped at 13 dBm (52 quarter-dBm) to avoid saturating a nearby receiver.
//! * [`EspHalEspNow::scan_for_peer`] finds the channel on which the coordinator
//!   (`hal_c6_espnow_sta_coordinator`) answers with a MAC-layer ACK. The pinned channel of the last
//!   successful scan is kept across rescans and restored if a scan fails.
//! * Every 100 ms an 8-byte frame (big-endian sequence counter) is sent and its ACK counted.
//!   Echoed frames are counted, not logged, so the output does not distort the measurement.
//!   Every 10 s the counters are logged with the failure rate; this is the traffic pattern of
//!   hardware gate G-E1 (30 minutes at 100 ms, failures below 1 %).
//! * After `RESCAN_AFTER_FAILURES` consecutive unacknowledged sends the peer rescans, which
//!   recovers from a channel change of the coordinator's access point.
//!
//! Build-time environment (`option_env!`):
//!
//! | Variable          | Meaning                                                            |
//! |:------------------|:-------------------------------------------------------------------|
//! | `ESPNOW_PEER_MAC` | Coordinator STA MAC, `aa:bb:cc:dd:ee:ff` (required; printed by it)  |
//! | `ESPNOW_CHANNEL`  | Optional first guess for the pinned channel (1..=13)               |
//!
//! No credentials are involved.
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

use core::sync::atomic::{AtomicU32, Ordering};

use embassy_executor::Spawner;
use embassy_time::{Duration, Instant, Ticker, Timer};
use esp_backtrace as _;
use esp_hal::timer::timg::TimerGroup;
use esp_println::println;
use rustyfarian_esp_hal_network::espnow::{
    EspHalEspNow, EspHalEspNowError, EspNowConfig, EspNowDriver, MacAddress, PeerConfig,
    ScanConfig, TxPowerLevel,
};
use static_cell::StaticCell;

esp_bootloader_esp_idf::esp_app_desc!();

const SEND_PERIOD_MS: u64 = 100;
const ACK_TIMEOUT: core::time::Duration = core::time::Duration::from_millis(50);
const STATS_SECS: u64 = 10;
const RESCAN_AFTER_FAILURES: u32 = 20;
const RESCAN_BACKOFF_MS: u64 = 2000;
/// Probe payload sent during the scan; any short frame the coordinator accepts will do.
const PROBE: &[u8] = b"scan";

/// Echoed frames received; written by `rx_task` only (ESP32-C3 has no atomic read-modify-write).
static RX_ECHOES: AtomicU32 = AtomicU32::new(0);

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
    spawner.spawn(rx_task(espnow).unwrap());

    let mut pinned: Option<u8> = FIRST_CHANNEL;
    scan_until_found(espnow, &coordinator, &mut pinned).await;

    let mut ticker = Ticker::every(Duration::from_millis(SEND_PERIOD_MS));
    let mut seq: u32 = 0;
    let (mut sent, mut acked) = (0u32, 0u32);
    let mut consecutive_failures: u32 = 0;
    let mut last_stats = Instant::now();
    loop {
        ticker.next().await;
        seq = seq.wrapping_add(1);
        let mut frame = [0u8; 8];
        frame[..4].copy_from_slice(&seq.to_be_bytes());
        frame[4..].copy_from_slice(&(Instant::now().as_millis() as u32).to_be_bytes());

        sent += 1;
        match espnow
            .send_async_timeout(&coordinator, &frame, ACK_TIMEOUT)
            .await
        {
            Ok(()) => {
                acked += 1;
                consecutive_failures = 0;
            }
            Err(EspHalEspNowError::SendFailed | EspHalEspNowError::Timeout) => {
                consecutive_failures += 1;
            }
            Err(e) => {
                consecutive_failures += 1;
                println!("[espnow] send error: {}", e);
            }
        }

        if last_stats.elapsed() >= Duration::from_secs(STATS_SECS) {
            last_stats = Instant::now();
            let failed = sent - acked;
            // Failure rate in hundredths of a percent, integer only.
            let per_10k = (failed as u64 * 10_000) / sent.max(1) as u64;
            println!(
                "[espnow] sent={} acked={} failed={} fail_rate={}.{:02}% echoes={} channel={:?} oversize_dropped={}",
                sent,
                acked,
                failed,
                per_10k / 100,
                per_10k % 100,
                RX_ECHOES.load(Ordering::Relaxed),
                pinned,
                espnow.oversize_dropped()
            );
        }

        if consecutive_failures >= RESCAN_AFTER_FAILURES {
            println!(
                "[espnow] {} sends unacknowledged - rescanning",
                consecutive_failures
            );
            scan_until_found(espnow, &coordinator, &mut pinned).await;
            consecutive_failures = 0;
            ticker.reset();
        }
    }
}

/// Scans until the coordinator answers; `pinned` keeps the last good channel across attempts.
async fn scan_until_found(
    espnow: &EspHalEspNow,
    coordinator: &MacAddress,
    pinned: &mut Option<u8>,
) {
    let scan = ScanConfig::new(PROBE);
    loop {
        match espnow.scan_for_peer(coordinator, &scan, pinned).await {
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

/// Owns every async ESP-NOW transmission (`send_async*` and scan probes); never cancelled.
#[embassy_executor::task]
async fn sender_task(espnow: &'static EspHalEspNow) -> ! {
    espnow.run_sender().await
}

/// Counts frames the coordinator echoes back (reported in the periodic stats, never logged per
/// frame); draining keeps the 10-frame RX queue from overflowing.
#[embassy_executor::task]
async fn rx_task(espnow: &'static EspHalEspNow) {
    loop {
        match espnow.recv_async().await {
            Ok(_) => {
                let n = RX_ECHOES.load(Ordering::Relaxed);
                RX_ECHOES.store(n.wrapping_add(1), Ordering::Relaxed);
            }
            Err(e) => {
                println!("[espnow] recv failed: {}", e);
                Timer::after(Duration::from_millis(100)).await;
            }
        }
    }
}

/// Parks the task after a fatal error (the message is already logged).
async fn halt() -> ! {
    loop {
        Timer::after(Duration::from_secs(3600)).await;
    }
}
