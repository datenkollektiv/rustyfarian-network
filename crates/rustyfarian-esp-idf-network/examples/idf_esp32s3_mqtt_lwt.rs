//! ESP-IDF MQTT example for ESP32-S3 (CrowPanel 1.28" HMI): LWT, retained online flag, reconnect.
//!
//! Twin of `hal_esp32s3_mqtt_lwt`: same environment keys, same topics, same 5 s tick, so the
//! two tiers can be compared on one broker (different default client id and prefix).
//! Wi-Fi joins in `ConnectMode::NonBlocking`; the library reconnects after any link loss
//! (1 s back-off doubling to 30 s) and esp-mqtt reconnects on its own once the link is back.
//!
//! * CONNECT carries an LWT `offline` (retained, QoS 1) on `<prefix>/status`.
//! * Every (re)connect publishes the retained `online` there from `on_connect`.
//! * `<prefix>/cmd` is subscribed; each inbound message is logged by `on_message`.
//! * Every 5 s the main loop `try_publish`es a tick on `<prefix>/tick`;
//!   `WouldBlock` (not connected or client busy) is logged and skipped.
//! * Wi-Fi link and MQTT state changes and the free heap (once a minute) are logged.
//! * With `MQTT_SUSPEND_DEMO=1` the radio is suspended for 20 s every 60 s
//!   (`WiFiManager::suspend` / `resume`), the cycle a deep-sleep firmware runs when
//!   a sleep attempt is aborted; the broker sees the will, then `online` again.
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

use std::time::Duration;

use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::mqtt::client::EspMqttClient;
use esp_idf_svc::{eventloop::EspSystemEventLoop, nvs::EspDefaultNvsPartition};
use rustyfarian_esp_idf_network::mqtt::{to_esp_qos, LwtConfig, MqttBuilder, QoS, TryPublishError};
use rustyfarian_esp_idf_network::wifi::{WiFiConfig, WiFiConfigExt, WiFiManager};

#[path = "common/env.rs"]
mod env;

const TICK_SECS: u64 = 5;
const HEAP_LOG_SECS: u64 = 60;
const KEEP_ALIVE_SECS: u64 = 30;
const WIFI_FIRST_CONNECT_WAIT_MS: u64 = 30_000;
const SUSPEND_EVERY_SECS: u64 = 60;
const SUSPEND_FOR_SECS: u64 = 20;

fn free_heap() -> u32 {
    // SAFETY: plain FFI query with no arguments or side effects.
    unsafe { esp_idf_svc::sys::esp_get_free_heap_size() }
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

    let online_topic = status_topic.clone();
    let mqtt = MqttBuilder::new(mqtt_config)
        .subscribe(&cmd_topic, QoS::AtLeastOnce)
        .on_connect(
            move |client: &mut EspMqttClient<'_>, session_present: bool| {
                client.enqueue(&online_topic, to_esp_qos(QoS::AtLeastOnce), true, b"online")?;
                log::info!(
                    "[mqtt] connected ({}) — published retained online",
                    if session_present {
                        "session resumed"
                    } else {
                        "clean session"
                    }
                );
                Ok(())
            },
        )
        .on_disconnect(|| log::warn!("[mqtt] disconnected — esp-mqtt reconnects on its own"))
        .on_message(|topic, payload| match std::str::from_utf8(payload) {
            Ok(text) => log::info!("[example] rx topic={} payload={}", topic, text),
            Err(_) => log::info!(
                "[example] rx topic={} payload={} bytes (binary)",
                topic,
                payload.len()
            ),
        })
        .build(&wifi)?;

    log::info!(
        "[example] topics: status={} cmd={} tick={}",
        status_topic,
        cmd_topic,
        tick_topic
    );

    let suspend_demo = env::mqtt_suspend_demo();
    let mut tick: u32 = 0;
    let mut last_link = wifi.state();
    let mut last_state = mqtt.state();
    let mut mqtt_was_up = false;
    let mut secs_since_heap_log: u64 = 0;
    loop {
        std::thread::sleep(Duration::from_secs(TICK_SECS));
        tick += 1;
        secs_since_heap_log += TICK_SECS;

        let uptime = tick as u64 * TICK_SECS;
        if suspend_demo && uptime % SUSPEND_EVERY_SECS == 0 {
            log::info!("[example] demo: suspend for {} s", SUSPEND_FOR_SECS);
            if let Err(e) = wifi.suspend() {
                log::warn!("[example] suspend failed: {:#}", e);
            }
        } else if suspend_demo && uptime % SUSPEND_EVERY_SECS == SUSPEND_FOR_SECS {
            log::info!("[example] demo: resume");
            if let Err(e) = wifi.resume() {
                log::warn!("[example] resume failed: {:#}", e);
            }
        }

        let link = wifi.state();
        if link != last_link {
            log::info!("[example] wifi link {:?} -> {:?}", last_link, link);
            last_link = link;
        }
        let state = mqtt.state();
        if state != last_state {
            log::info!("[example] mqtt state {:?} -> {:?}", last_state, state);
            last_state = state;
        }
        let mqtt_up = mqtt.is_connected();
        if mqtt_up && !mqtt_was_up {
            log::info!(
                "[example] connect #{} free heap: {} B",
                mqtt.connect_count(),
                free_heap()
            );
        }
        mqtt_was_up = mqtt_up;

        let payload = format!(
            "{{\"tick\":{},\"uptime_s\":{},\"free_heap\":{}}}",
            tick,
            uptime,
            free_heap()
        );
        match mqtt.try_publish(&tick_topic, &payload) {
            Ok(()) => log::info!("[example] tick #{} queued", tick),
            Err(TryPublishError::WouldBlock) => {
                log::info!("[example] tick #{} would block (mqtt {:?})", tick, state)
            }
            Err(TryPublishError::Other(e)) => {
                log::warn!("[example] tick #{} failed: {:#}", tick, e)
            }
        }

        if secs_since_heap_log >= HEAP_LOG_SECS {
            secs_since_heap_log = 0;
            log::info!(
                "[example] heap free: {} B, connects: {}",
                free_heap(),
                mqtt.connect_count()
            );
        }
    }
}
