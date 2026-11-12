//! Bare-metal OTA round-trip example for ESP32-C3: join Wi-Fi, fetch an image over HTTP, flash it
//! into the inactive slot, reset into it, and mark it valid on the next boot.
//!
//! There is no OTA runtime or MQTT trigger on the esp-hal tier (ADR 015), so this is a direct
//! HTTP check of `EspHalOtaManager`: download, SHA-256 verification, erase-once and
//! `ChunkAligner` flash writes (bug 004), slot activation, and `mark_valid`.
//! The whole application body is `examples/common/ota_app.rs`, shared with the other chip's
//! OTA example; this file is setup only (heap, Wi-Fi, tasks).
//!
//! The server must be reachable from the device: the `iot` SSID isolates clients, so the Mac
//! cannot serve the image. Run the server on a 192.168.31.x host such as the broker host
//! 192.168.31.185 (copy `target/ota/` there and run `just ota-serve`).
//!
//! # Runbook
//!
//! The digest of the served image cannot be baked into that same image, so build the image
//! first and the baseline that fetches it second.
//!
//! ```sh
//! just ota-image 2.0.0 hal_c3_ota
//! just flash-ota-baseline hal_c3_ota 1.0.0 2.0.0 192.168.31.185
//! ```
//!
//! Boot 1 (1.0.0) marks itself valid, sees the newer target, downloads, verifies, activates and
//! resets. Boot 2 logs `[ota] running slot 1 version 2.0.0` (the other slot than boot 1) and
//! `running image marked valid`; with no target configured it then idles. Flashing the baseline
//! again erases `otadata` and returns to slot 0.
//!
//! The flash recipe installs a rollback-enabled bootloader, so an image that never reaches `mark_valid` is rolled back.
//!
//! Build-time environment (all `option_env!`, see `.env.example`):
//!
//! | Variable             | Default      | Meaning                                         |
//! |:---------------------|:-------------|:------------------------------------------------|
//! | `WIFI_SSID`          | (required)   | Wi-Fi SSID                                      |
//! | `WIFI_PASS`          | (required)   | Wi-Fi password                                  |
//! | `FIRMWARE_VERSION`   | package ver. | Version of this image (MAJOR.MINOR.PATCH)       |
//! | `OTA_HOST`           | unset        | Server **IPv4 literal** (no DNS on this tier)   |
//! | `OTA_PORT`           | `8000`       | Server port                                     |
//! | `OTA_PATH`           | unset        | Image file name on the server                   |
//! | `OTA_SHA256`         | unset        | Hex SHA-256 of that image                       |
//! | `OTA_TARGET_VERSION` | unset        | Version of the served image; fetched if newer   |

#![no_std]
#![no_main]

extern crate alloc;

use embassy_executor::Spawner;
use esp_backtrace as _;
use esp_radio::wifi::Interface;
use rustyfarian_esp_hal_network::wifi::{
    link_task, AsyncWifiHandle, WiFiConfig, WiFiConfigExt, WiFiManager,
};

#[path = "common/ota_app.rs"]
mod ota_app;

esp_bootloader_esp_idf::esp_app_desc!();

static LINK: WiFiManager = WiFiManager::new();

const SSID: &str = match option_env!("WIFI_SSID") {
    Some(s) => s,
    None => "",
};
// No literal fallback: an unset `WIFI_PASS` yields the `&str` type default (`""`),
// keeping CodeQL's hard-coded-credential query source-free.
fn password() -> &'static str {
    option_env!("WIFI_PASS").unwrap_or_default()
}

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    esp_println::logger::init_logger(log::LevelFilter::Info);
    log::info!("[app] start {} (esp-hal)", env!("CARGO_CRATE_NAME"));

    let peripherals = esp_hal::init(esp_hal::Config::default());

    // Same 72 KiB internal-RAM heap as the ESP32-C3 Wi-Fi example; the OTA path streams
    // through static socket buffers and a 512-byte chunk, so it adds almost nothing.
    esp_alloc::heap_allocator!(size: 72 * 1024);

    log::info!("Initializing Wi-Fi (async)...");
    let config = WiFiConfig::new(SSID, password()).with_peripherals(
        peripherals.TIMG0,
        peripherals.FROM_CPU_INTR0,
        peripherals.WIFI,
    );
    let handle = match WiFiManager::init_async(config) {
        Ok(h) => h,
        Err(e) => {
            log::error!("FATAL: Wi-Fi init failed: {}", e);
            loop {
                core::hint::spin_loop();
            }
        }
    };
    let AsyncWifiHandle {
        controller,
        stack,
        runner,
    } = handle;
    spawner.spawn(link_task(&LINK, controller, stack).unwrap());
    spawner.spawn(net_task(runner).unwrap());

    ota_app::run(&LINK, stack, peripherals.FLASH).await
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, Interface>) -> ! {
    runner.run().await
}
