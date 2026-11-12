//! ESP-IDF MQTT example for ESP32-S3 (CrowPanel 1.28" HMI): LWT, retained online flag, reconnect.
//!
//! Twin of `hal_esp32s3_mqtt_lwt`: same environment keys, same topics, same 5 s tick, and
//! the same application body — `examples-shared/lwt_app.rs` is compiled into both examples,
//! so the two tiers can be compared on one broker (different default client id and prefix).
//! This file is setup only: Wi-Fi, the MQTT builder and one `block_on` that runs the shared app.
//!
//! * CONNECT carries an LWT `offline` (retained, QoS 1) on `<prefix>/status`.
//! * Every (re)connect publishes the retained `online` there as the builder's startup message.
//! * `<prefix>/cmd` is subscribed; inbound messages reach `MqttHandle::receive`, which the
//!   shared app drains.
//! * Everything else — the 5 s tick on `<prefix>/tick`, link and MQTT state logs, the free
//!   heap, and the suspend demo — is the shared app, and its lines start with `[app]`.
//! * Measurement lines: after each `[app] connect #N free heap` line the
//!   shared app adds `[app] connect #N min free heap: Y B` and one `[app] stack unused <name>: N B`
//!   per task (`main`, `mqtt-events`, `mqtt-net`, `mqtt_task`), and after the minute heap line
//!   `[app] heap min free: Y B`. The library names its own tasks (`mqtt-events`, `mqtt-net`).
//! * `MQTT_ON_CONNECT_DEMO=1` registers an `on_connect` that enqueues one
//!   retained `on_connect` on `<prefix>/boot` and logs `[app] on_connect ran`, so the
//!   `on_connect` step can be measured; unset, no `on_connect` is registered.
//!
//! Build-time environment (all `option_env!`, see `.env.example`):
//!
//! | Variable               | Default                | Meaning                             |
//! |:-----------------------|:-----------------------|:------------------------------------|
//! | `WIFI_SSID`            | (required)             | Wi-Fi SSID                          |
//! | `WIFI_PASS`            | (required)             | Wi-Fi password                      |
//! | `MQTT_HOST`            | (required)             | Broker IP or hostname               |
//! | `MQTT_PORT`            | `1883`                 | Broker port                         |
//! | `MQTT_CLIENT_ID`       | `idf-s3-mqtt`          | Client id (≤ 23 bytes)              |
//! | `MQTT_USER`            | unset                  | Optional username                   |
//! | `MQTT_PASS`            | unset                  | Optional password                   |
//! | `MQTT_TOPIC_PREFIX`    | `rustyfarian/idf-mqtt` | Topic prefix for status/cmd/tick    |
//! | `MQTT_SUSPEND_DEMO`    | unset                  | `1`: suspend 20 s every 60 s        |
//! | `MQTT_ON_CONNECT_DEMO` | unset                  | `1`: measurement `on_connect`       |
//!
//! # Build and flash
//!
//! ```sh
//! just build-example idf_esp32s3_mqtt_lwt
//! just flash idf_esp32s3_mqtt_lwt
//! ```

use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::{eventloop::EspSystemEventLoop, nvs::EspDefaultNvsPartition};
use rustyfarian_esp_idf_network::mqtt::{to_esp_qos, LwtConfig, MqttBuilder, QoS};
use rustyfarian_esp_idf_network::wifi::{WiFiConfig, WiFiConfigExt, WiFiManager};

#[path = "common/env.rs"]
mod env;
#[path = "../../../examples-shared/lwt_app.rs"]
mod lwt_app;
#[allow(dead_code)] // only the stack helper is used here
#[path = "common/spike.rs"]
mod spike;

const KEEP_ALIVE_SECS: u64 = 30;
const WIFI_FIRST_CONNECT_WAIT_MS: u64 = 30_000;

fn free_heap() -> usize {
    // SAFETY: plain FFI query with no arguments or side effects.
    unsafe { esp_idf_svc::sys::esp_get_free_heap_size() as usize }
}

fn min_free_heap() -> usize {
    // SAFETY: plain FFI query with no arguments or side effects.
    unsafe { esp_idf_svc::sys::esp_get_minimum_free_heap_size() as usize }
}

/// Logs the unused stack of the main task, the library's tasks and esp-mqtt's task.
fn stack_report() {
    for name in [c"main", c"mqtt-events", c"mqtt-net", c"mqtt_task"] {
        match spike::stack_high_water(name) {
            Some(unused) => log::info!(
                "[app] stack unused {}: {} B",
                name.to_string_lossy(),
                unused
            ),
            None => log::info!(
                "[app] stack unused {}: no such task",
                name.to_string_lossy()
            ),
        }
    }
}

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    log::info!("[app] start {} (esp-idf)", env!("CARGO_CRATE_NAME"));

    let client_id = env::mqtt_client_id("idf-s3-mqtt");
    let prefix = env::mqtt_topic_prefix("rustyfarian/idf-mqtt");
    env::log_config(client_id);
    env::mqtt_host()?;
    if client_id.len() > 23 {
        anyhow::bail!(
            "MQTT_CLIENT_ID '{}' is {} bytes — MQTT 3.1.1 maximum is 23",
            client_id,
            client_id.len()
        );
    }

    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    let wifi_config = WiFiConfig::new(env::WIFI_SSID, env::wifi_pass())
        .connect_nonblocking()
        .with_peripherals(peripherals.modem, sys_loop, Some(nvs));
    let wifi = WiFiManager::init(wifi_config)?;
    match wifi.get_ip(WIFI_FIRST_CONNECT_WAIT_MS)? {
        Some(ip) => log::info!("Wi-Fi connected — IP: {}", ip),
        None => log::warn!("Wi-Fi not up yet — the manager keeps reconnecting in the background"),
    }

    let status_topic = format!("{prefix}/status");
    let cmd_topic = format!("{prefix}/cmd");
    let tick_topic = format!("{prefix}/tick");

    let lwt = LwtConfig::new(&status_topic, b"offline", QoS::AtLeastOnce, true);
    let mut mqtt_config = env::mqtt_config(client_id)?
        .with_keep_alive(KEEP_ALIVE_SECS)
        .with_lwt(lwt);
    if let Some(user) = env::MQTT_USER {
        mqtt_config = match env::mqtt_pass() {
            Some(pass) => mqtt_config.with_auth(user, pass),
            None => mqtt_config.with_username_only(user),
        };
    }

    let status_topic_static: &'static str = format!("{prefix}/status").leak();
    let mut builder = MqttBuilder::new(mqtt_config)
        .subscribe(&cmd_topic, QoS::AtLeastOnce)
        .with_startup_message(status_topic_static, b"online", QoS::AtLeastOnce, true);
    if env::mqtt_on_connect_demo() {
        let boot_topic = format!("{prefix}/boot");
        builder = builder.on_connect(move |client, session_present| {
            client.enqueue(
                &boot_topic,
                to_esp_qos(QoS::AtLeastOnce),
                true,
                b"on_connect",
            )?;
            log::info!(
                "[app] on_connect ran (session present: {})",
                session_present
            );
            Ok(())
        });
    }
    let mqtt = builder.build(&wifi)?;

    log::info!(
        "[app] topics: status={} cmd={} tick={}",
        status_topic,
        cmd_topic,
        tick_topic
    );

    let cfg = lwt_app::LwtAppConfig {
        tick_topic: &tick_topic,
        suspend_demo: env::mqtt_suspend_demo(),
        free_heap,
        min_free_heap: Some(min_free_heap),
        stack_report: Some(stack_report),
    };
    esp_idf_svc::hal::task::block_on(lwt_app::run(&wifi, &mqtt, &cfg));
    Ok(())
}
