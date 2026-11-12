//! Shared application body of the bare-metal OTA examples (`hal_c3_ota`, `hal_esp32s3_ota`).
//!
//! The chip examples are setup only (heap, Wi-Fi, tasks); the one-shot OTA round trip lives
//! here, so both chips run exactly the same logic.
//! Each example includes this file with `#[path = "common/ota_app.rs"] mod ota_app;`.
//!
//! The flow, per boot:
//!
//! 1. Log the selected slot and the running version.
//! 2. Wait for the Wi-Fi link (associated with an IP), then mark the running image valid
//!    (the health check is "Wi-Fi came up", so a rollback-capable bootloader keeps this slot).
//!    An image built with `OTA_DEMO_UNHEALTHY=1` (`just ota-image-unhealthy`) never marks itself
//!    valid, so the next reset makes the bootloader roll it back.
//! 3. Compare the running version with `OTA_TARGET_VERSION`: only a strictly newer target is
//!    fetched (`decide_update`), so the updated image does not update itself again.
//!    After a rollback boot (the other slot's otadata entry is `Aborted` or `Invalid`) the
//!    update is skipped, so the baseline does not fetch the rolled-back image again.
//! 4. Download `http://OTA_HOST:OTA_PORT/OTA_PATH` into the inactive slot, verify `OTA_SHA256`,
//!    activate the slot and reset.
//!
//! The digest cannot be baked into the image it describes, so the image served is built first
//! (`just ota-image`), and the baseline firmware that fetches it is built second with that
//! digest; see the example's doc comment for the full runbook.

use alloc::format;
use embassy_net::tcp::TcpSocket;
use embassy_net::Stack;
use embassy_time::{Duration, Timer};
use esp_hal::peripherals::FLASH;
use rustyfarian_esp_hal_network::ota::{
    decide_update, hex_to_bytes, EspHalOtaManager, OtaManagerConfig, UpdateDecision, Version,
};
use rustyfarian_esp_hal_network::wifi::WiFiManager;
use static_cell::StaticCell;

/// Version of the running image; `FIRMWARE_VERSION` (set by `just ota-image` and the flash
/// recipes) wins over the package version.
const RUNNING_VERSION: &str = match option_env!("FIRMWARE_VERSION") {
    Some(v) => v,
    None => env!("CARGO_PKG_VERSION"),
};

/// OTA server address; an IPv4 literal on this tier (no DNS, ADR 019).
const OTA_HOST: &str = match option_env!("OTA_HOST") {
    Some(v) => v,
    None => "",
};

/// OTA server port.
const OTA_PORT: &str = match option_env!("OTA_PORT") {
    Some(v) => v,
    None => "8000",
};

/// Path of the image on the OTA server, for example `hal_c3_ota-v2.0.0.bin`.
const OTA_PATH: &str = match option_env!("OTA_PATH") {
    Some(v) => v,
    None => "",
};

/// Lowercase hex SHA-256 of the image to fetch (printed by `just ota-image`).
const OTA_SHA256: &str = match option_env!("OTA_SHA256") {
    Some(v) => v,
    None => "",
};

/// Version of the image on the server; the update runs only when it is newer than the running one.
const OTA_TARGET_VERSION: &str = match option_env!("OTA_TARGET_VERSION") {
    Some(v) => v,
    None => "",
};

/// `OTA_DEMO_UNHEALTHY`: `1` makes this image skip `mark_valid`, so the bootloader rolls it back
/// on the next reset; unset or empty is healthy, anything else is a startup error (the same
/// contract as the ESP-IDF example `idf_c3_ota_runtime`).
const DEMO_UNHEALTHY: Option<&str> = option_env!("OTA_DEMO_UNHEALTHY");

const SOCKET_RX_LEN: usize = 4096;
const SOCKET_TX_LEN: usize = 1024;

/// Seconds the link must be up before the running image is marked valid.
const HEALTH_DWELL_SECS: u64 = 5;

/// Runs the OTA round trip once, then idles; never returns.
pub async fn run(link: &'static WiFiManager, stack: Stack<'static>, flash: FLASH<'static>) -> ! {
    let mut manager = match EspHalOtaManager::new(OtaManagerConfig { timeout_secs: 30 }, flash) {
        Ok(m) => m.with_deadline(Duration::from_secs(300)),
        Err(e) => fatal("OTA manager", &e),
    };

    match manager.selected_slot() {
        Ok(slot) => log::info!(
            "[ota] running slot {} version {} (target {:?})",
            slot.0,
            RUNNING_VERSION,
            OTA_TARGET_VERSION
        ),
        Err(e) => log::warn!("[ota] could not read the selected slot: {}", e),
    }

    let unhealthy = match DEMO_UNHEALTHY {
        None | Some("") => false,
        Some("1") => true,
        Some(other) => fatal(
            "OTA_DEMO_UNHEALTHY",
            &format_args!("must be 1 or unset, got '{other}'"),
        ),
    };

    log::info!("[ota] waiting for the link (associated with an IP)...");
    link.wait_connected().await;
    stack.wait_config_up().await;
    Timer::after(Duration::from_secs(HEALTH_DWELL_SECS)).await;
    if unhealthy {
        log::error!(
            "[ota] *** OTA_DEMO_UNHEALTHY=1: this image never marks itself valid; reset the board and the bootloader rolls it back ***"
        );
    } else {
        match manager.mark_valid() {
            Ok(()) => log::info!("[ota] running image marked valid"),
            Err(e) => log::warn!(
                "[ota] mark_valid failed (first boot of a fresh table?): {}",
                e
            ),
        }
    }

    let rolled_back = match manager.previous_update_rolled_back() {
        Ok(rolled_back) => rolled_back,
        Err(e) => {
            log::warn!("[ota] could not read the rollback state: {}", e);
            false
        }
    };
    if rolled_back {
        log::warn!(
            "[ota] previous update was rolled back; not retrying {}",
            OTA_TARGET_VERSION
        );
    }

    match plan(rolled_back) {
        Plan::Idle(why) => log::info!("[ota] nothing to do: {}", why),
        Plan::Fetch { url, sha256 } => {
            static RX: StaticCell<[u8; SOCKET_RX_LEN]> = StaticCell::new();
            static TX: StaticCell<[u8; SOCKET_TX_LEN]> = StaticCell::new();
            let rx = RX.init([0; SOCKET_RX_LEN]);
            let tx = TX.init([0; SOCKET_TX_LEN]);
            let mut socket = TcpSocket::new(stack, rx, tx);
            log::info!("[ota] fetching {}", url);
            match manager.fetch_and_apply(&mut socket, &url, &sha256).await {
                Ok(()) => {
                    log::info!("[ota] new image activated — resetting into it");
                    Timer::after(Duration::from_secs(1)).await;
                    esp_hal::system::software_reset();
                }
                Err(e) => log::error!("[ota] update failed, boot slot unchanged: {}", e),
            }
        }
    }

    loop {
        Timer::after(Duration::from_secs(30)).await;
        log::info!(
            "[ota] idle: version {} link ups {} free heap {} B",
            RUNNING_VERSION,
            link.connect_count(),
            esp_alloc::HEAP.free()
        );
    }
}

/// What this boot does about OTA.
enum Plan {
    /// Nothing to fetch, with the reason.
    Idle(&'static str),
    /// Fetch `url` and check it against `sha256`.
    Fetch {
        url: alloc::string::String,
        sha256: [u8; 32],
    },
}

/// Decides from the build-time configuration whether to fetch.
fn plan(rolled_back: bool) -> Plan {
    if rolled_back {
        return Plan::Idle("the previous update was rolled back");
    }
    if OTA_HOST.is_empty() || OTA_PATH.is_empty() || OTA_SHA256.is_empty() {
        return Plan::Idle("OTA_HOST, OTA_PATH and OTA_SHA256 are not configured");
    }
    let Ok(running) = Version::parse(RUNNING_VERSION) else {
        return Plan::Idle("FIRMWARE_VERSION is not MAJOR.MINOR.PATCH");
    };
    let Ok(target) = Version::parse(OTA_TARGET_VERSION) else {
        return Plan::Idle("OTA_TARGET_VERSION is unset or not MAJOR.MINOR.PATCH");
    };
    match decide_update(running, target) {
        UpdateDecision::Apply => {}
        UpdateDecision::Skip => return Plan::Idle("already running the target version"),
        UpdateDecision::Reject => {
            return Plan::Idle("target version is older than the running one")
        }
    }
    let sha256 = match hex_to_bytes(OTA_SHA256) {
        Ok(d) => d,
        Err(_) => return Plan::Idle("OTA_SHA256 is not 64 hex characters"),
    };
    Plan::Fetch {
        url: format!(
            "http://{}:{}/{}",
            OTA_HOST,
            OTA_PORT,
            OTA_PATH.trim_start_matches('/')
        ),
        sha256,
    }
}

/// Logs a start-up failure and parks; an example has nothing to fall back to.
fn fatal(what: &str, reason: &dyn core::fmt::Display) -> ! {
    log::error!("FATAL: {}: {}", what, reason);
    loop {
        core::hint::spin_loop();
    }
}
