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
//!
//! Build-time environment (all `option_env!`, see `.env.example`):
//!
//! | Variable            | Default                | Meaning                               |
//! |:--------------------|:-----------------------|:--------------------------------------|
//! | `WIFI_SSID`         | (required)             | Wi-Fi SSID                            |
//! | `WIFI_PASS`         | (required)             | Wi-Fi password                        |
//! | `MQTT_HOST`         | (required)             | Broker IP or hostname                 |
//! | `MQTT_PORT`         | `1883`                 | Broker port                           |
//! | `MQTT_CLIENT_ID`    | `idf-s3-mqtt`          | Client id (≤ 23 bytes)                |
//! | `MQTT_USER`         | unset                  | Optional username                     |
//! | `MQTT_PASS`         | unset                  | Optional password                     |
//! | `MQTT_TOPIC_PREFIX` | `rustyfarian/idf-mqtt` | Topic prefix for status/cmd/tick      |
//! | `MQTT_SUSPEND_DEMO` | unset                  | `1`: suspend 20 s every 60 s          |
//!
//! # Build and flash
//!
//! ```sh
//! just build-example idf_esp32s3_mqtt_lwt
//! just flash idf_esp32s3_mqtt_lwt
//! ```

use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::{eventloop::EspSystemEventLoop, nvs::EspDefaultNvsPartition};
use rustyfarian_esp_idf_network::mqtt::{LwtConfig, MqttBuilder, QoS};
use rustyfarian_esp_idf_network::wifi::{WiFiConfig, WiFiConfigExt, WiFiManager};

#[path = "common/env.rs"]
mod env;
#[path = "../../../examples-shared/lwt_app.rs"]
mod lwt_app;

const KEEP_ALIVE_SECS: u64 = 30;
const WIFI_FIRST_CONNECT_WAIT_MS: u64 = 30_000;

fn free_heap() -> usize {
    // SAFETY: plain FFI query with no arguments or side effects.
    unsafe { esp_idf_svc::sys::esp_get_free_heap_size() as usize }
}

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

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
    let mqtt = MqttBuilder::new(mqtt_config)
        .subscribe(&cmd_topic, QoS::AtLeastOnce)
        .with_startup_message(status_topic_static, b"online", QoS::AtLeastOnce, true)
        .build(&wifi)?;

    log::info!(
        "[example] topics: status={} cmd={} tick={}",
        status_topic,
        cmd_topic,
        tick_topic
    );

    let cfg = lwt_app::LwtAppConfig {
        tick_topic: &tick_topic,
        suspend_demo: env::mqtt_suspend_demo(),
        free_heap,
    };
    esp_idf_svc::hal::task::block_on(lwt_app::run(&wifi, &mqtt, &cfg));
    Ok(())
}
