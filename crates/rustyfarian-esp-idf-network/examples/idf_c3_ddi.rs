//! Hardware example of the DDI OTA client on the ESP32-C3 Super Mini (`docs/features/ddi-client-v1.md`).
//!
//! The device polls a hawkBit-DDI server (the fleet DDI profile 0.4.0) over plain HTTP, downloads and flashes the offered image, reboots into it and reports the result on the action's feedback endpoint.
//! An MQTT "poll now" nudge on `rustyfarian/ota/<controller-id>/nudge` wakes it early; the device publishes nothing under `rustyfarian/ota/#`.
//!
//! The body is shared with `idf_c6_ddi` (`examples/common/ddi_app.rs`); this file only names the board.
//! It wires the four hook points of the runtime: `open_records` before Wi-Fi, `DdiRuntime::new`, `DdiRuntime::start` once Wi-Fi is up and `run_health_policy` on the main thread (healthy = Wi-Fi ready and MQTT connected).
//! The restart callback is the only place the device is ever reset, and the app starts SNTP.
//!
//! # Lab only
//!
//! The example enables `ota-ddi-insecure-http`, the plain-HTTP lab opt-in of the fleet DDI profile §1, and the library logs one WARN per boot for it.
//! Anyone on the segment can read the token and forge polls and feedback: run it only on an isolated IoT segment against a lab server.
//! A release image leaves the feature out (TLS is the next round), and `https://` is refused until then.
//! The token is compiled into the image: never flash a lab image outside the lab, and rotate a token that was used over HTTP.
//!
//! # Fleet-side prerequisites
//!
//! - A DDI server reachable from the device segment, started with its plain-HTTP opt-in, serving `<DDI_BASE_URL>/controller/v1/<DDI_CONTROLLER_ID>`; the base URL includes the tenant, e.g. `http://<lab-host>:<port>/DEFAULT`.
//! - A target with the id `DDI_CONTROLLER_ID` and the token `DDI_TOKEN` (32 to 64 lowercase hex, provisioned out of band).
//! - A software module with the metadata `chip` = `esp32c3` (and `downgrade=allowed` only for an operator downgrade), fed from the release bundle `just ddi-bundle idf_c3_ddi <version>` writes to `tmp/ddi-bundle/`.
//! - An MQTT broker with a user named `DDI_CONTROLLER_ID` (client id bound to the username, no anonymous access) on which only the server principal may publish the nudge.
//!
//! # Environment variables (set at compile time)
//!
//! | Variable | Default | Description |
//! |---|---|---|
//! | `WIFI_SSID` | (required) | Wi-Fi network name |
//! | `WIFI_PASS` | `""` | Wi-Fi password |
//! | `MQTT_HOST` | (required) | MQTT broker IP or hostname |
//! | `MQTT_PORT` | `1883` | MQTT broker port; an invalid value falls back to 1883 with a warning |
//! | `MQTT_PASS` | unset | MQTT password of the device user; unset sends the username alone |
//! | `DDI_BASE_URL` | (required) | DDI base URL including the tenant, `http://<lab-host>:<port>/DEFAULT` |
//! | `DDI_CONTROLLER_ID` | (required) | Device id `^[a-z0-9][a-z0-9-]{2,19}$`, also the MQTT username and client id |
//! | `DDI_TOKEN` | (required) | Target token, 32 to 64 lowercase hex; never logged |
//! | `FIRMWARE_VERSION` | (required) | `MAJOR.MINOR.PATCH` of this build; `just ddi-bundle` sets it |
//! | `DDI_DEMO_UNHEALTHY` | unset | `1` makes the health predicate always false, so the image is rolled back at the failure deadline; any other value is refused at startup |
//!
//! A missing or invalid value refuses the start with one `[app] refusing to start` line, and the device restarts after 5 s (a new image is then rolled back by the bootloader).
//! `MQTT_CLIENT_ID` and `MQTT_USER` are ignored: the controller id is both.
//! `just` loads `.env` automatically (`set dotenv-load`); a shell export takes precedence over `.env`, so keep each key in one place.
//!
//! # Build, bundle and flash
//!
//! ```sh
//! just build-example idf_c3_ddi
//! just ddi-bundle idf_c3_ddi 1.0.0
//! just flash idf_c3_ddi
//! ```

use std::time::Instant;

use rustyfarian_esp_idf_network::ota::ddi::Board;

#[path = "common/ddi_app.rs"]
mod ddi_app;
#[path = "common/env.rs"]
mod env;

/// The board name the configData reports: fixed when the image is built, never provisioned.
const BOARD: &str = "c3-supermini";

const _: () = assert!(Board::is_valid(BOARD));

fn main() -> anyhow::Result<()> {
    let boot = Instant::now();
    ddi_app::run(boot, BOARD)
}
