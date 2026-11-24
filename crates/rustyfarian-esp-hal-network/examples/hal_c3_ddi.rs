//! Bare-metal hardware example of the DDI OTA client on the ESP32-C3 Super Mini (`docs/features/ddi-client-v1.md`), the esp-hal twin of `idf_c3_ddi`.
//!
//! The device polls a hawkBit-DDI server (the fleet DDI profile 0.4.0) over plain HTTP, downloads and flashes the offered image, reboots into it and reports the result on the action's feedback endpoint.
//! An MQTT "poll now" nudge on `rustyfarian/ota/<controller-id>/nudge` wakes it early; the device publishes nothing under `rustyfarian/ota/#`.
//!
//! The body is shared with `hal_c6_ddi` (`examples/common/ddi_app.rs`); this file sets up the heap and the scheduler and names the board.
//! It wires the hook points of the runtime in the ESP-IDF example's order: `DdiRuntime::new`, `open_records` before Wi-Fi, `DdiRuntime::start` once Wi-Fi is up (its `DdiRunner` spawned as a task) and `run_health_policy` on the main task (healthy = Wi-Fi ready and MQTT connected).
//! The restart callback is the only place the device is ever reset.
//! This tier has no wall clock, so feedback carries no timestamp, and the server name is resolved through the DHCP-provided DNS server (IPv4 only).
//!
//! # Lab only
//!
//! The example enables `ota-ddi-insecure-http`, the plain-HTTP lab opt-in of the fleet DDI profile §1, and the library logs one WARN per boot for it.
//! Anyone on the segment can read the token and forge polls and feedback: run it only on an isolated IoT segment against a lab server.
//! A release image leaves the feature out (TLS is the next round, on both tiers), and `https://` is refused until then.
//! The token is compiled into the image: never flash a lab image outside the lab, and rotate a token that was used over HTTP.
//!
//! # Fleet-side prerequisites
//!
//! The same as for `idf_c3_ddi`: a DDI server reachable from the device segment with its plain-HTTP opt-in, a target `DDI_CONTROLLER_ID` with the token `DDI_TOKEN`, a software module with the metadata `chip` = `esp32c3`, and an MQTT broker with a user named `DDI_CONTROLLER_ID` on which only the server principal may publish the nudge.
//!
//! # Environment variables (set at compile time)
//!
//! | Variable | Default | Description |
//! |---|---|---|
//! | `WIFI_SSID` | (required) | Wi-Fi network name |
//! | `WIFI_PASS` | `""` | Wi-Fi password |
//! | `MQTT_HOST` | (required) | MQTT broker IPv4 literal or hostname |
//! | `MQTT_PORT` | `1883` | MQTT broker port; an invalid value falls back to 1883 with a warning |
//! | `MQTT_PASS` | unset | MQTT password of the device user; unset sends the username alone |
//! | `DDI_BASE_URL` | (required) | DDI base URL including the tenant, `http://<lab-host>:<port>/DEFAULT` |
//! | `DDI_CONTROLLER_ID` | (required) | Device id `^[a-z0-9][a-z0-9-]{2,19}$`, also the MQTT username and client id |
//! | `DDI_TOKEN` | (required) | Target token, 32 to 64 lowercase hex; never logged |
//! | `FIRMWARE_VERSION` | (required) | `MAJOR.MINOR.PATCH` of this build |
//! | `DDI_DEMO_UNHEALTHY` | unset | `1` makes the health predicate always false, so the image is rolled back at the failure deadline; any other value is refused at startup |
//!
//! A missing or invalid value refuses the start with one `[app] refusing to start` line, and the device restarts after 5 s (a new image is then rolled back by the bootloader).
//! `MQTT_CLIENT_ID` and `MQTT_USER` are ignored: the controller id is both.
//! `just` loads `.env` automatically (`set dotenv-load`); a shell export takes precedence over `.env`, so keep each key in one place.
//!
//! # Build
//!
//! ```sh
//! just build-example hal_c3_ddi
//! ```

#![no_std]
#![no_main]

extern crate alloc;

use embassy_executor::Spawner;
use embassy_time::Instant;
use esp_backtrace as _;
use esp_hal::timer::timg::TimerGroup;
use rustyfarian_esp_hal_network::ota::ddi::Board;

#[path = "common/ddi_app.rs"]
mod ddi_app;
#[path = "common/env.rs"]
mod env;

esp_bootloader_esp_idf::esp_app_desc!();

/// The board name the configData reports: fixed when the image is built, never provisioned.
const BOARD: &str = "c3-supermini";

const _: () = assert!(Board::is_valid(BOARD));

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    esp_println::logger::init_logger(log::LevelFilter::Info);
    log::info!("[app] start {} (esp-hal)", env!("CARGO_CRATE_NAME"));

    let peripherals = esp_hal::init(esp_hal::Config::default());

    // Same 72 KiB internal-RAM heap as the other ESP32-C3 examples; the DDI transport takes 8 KiB
    // of socket buffers from it, and each request up to 4 KiB of body and 8 KiB of response.
    esp_alloc::heap_allocator!(size: 72 * 1024);

    // The firmware owns the scheduler (ADR 021): started first, so the boot instant of the
    // health policy is taken before Wi-Fi.
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);
    let boot = Instant::now();

    ddi_app::run(spawner, boot, BOARD, peripherals.WIFI, peripherals.FLASH).await
}
