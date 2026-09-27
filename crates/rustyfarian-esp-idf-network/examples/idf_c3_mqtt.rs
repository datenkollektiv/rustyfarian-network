//! MQTT client example for ESP32-C3 using the `MqttBuilder` API.
//!
//! Demonstrates all three `MqttBuilder` callbacks:
//!
//! - `on_connect` — publishes a retained "online" status through its `client` argument (safe: it runs on the per-connect helper thread, not on the event loop)
//! - `on_disconnect` — logs the disconnection so reconnect attempts are visible in the TTY
//! - `on_message` — dispatches incoming commands by matching on the topic suffix
//!
//! The command subscription is registered with [`MqttBuilder::subscribe`](rustyfarian_esp_idf_network::mqtt::MqttBuilder::subscribe); no callback ever calls a [`MqttHandle`](rustyfarian_esp_idf_network::mqtt::MqttHandle) method.
//!
//! The LWT configuration ensures the broker publishes `{client_id}/status = "offline"` if
//! the device disconnects unexpectedly (e.g. power loss, crash, network failure).
//! A clean `DISCONNECT` suppresses the LWT.
//!
//! # Prerequisites
//!
//! - A running MQTT broker reachable from the device (Mosquitto or compatible)
//! - A USB/TTY connection for reading log output
//!
//! # Environment variables (set at compile time)
//!
//! | Variable | Default | Description |
//! |---|---|---|
//! | `WIFI_SSID` | `""` | Wi-Fi network name |
//! | `WIFI_PASS` | `""` | Wi-Fi password |
//! | `MQTT_HOST` | (required) | MQTT broker IP or hostname |
//! | `MQTT_PORT` | `1883` | MQTT broker port |
//! | `MQTT_CLIENT_ID` | `esp32c3-demo` | Unique device identifier |
//!
//! `just` loads a populated `.env` automatically (`set dotenv-load`); a shell export, e.g. from a
//! direnv `.envrc`, takes precedence over `.env`, so keep each key in one place.
//!
//! # Build and flash
//!
//! ```sh
//! WIFI_SSID="MyNetwork" WIFI_PASS="<your-password>" just build-example idf_c3_mqtt
//! ```
//!
//! ```sh
//! just flash idf_c3_mqtt
//! ```

use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::mqtt::client::{EspMqttClient, QoS};
use esp_idf_svc::{eventloop::EspSystemEventLoop, nvs::EspDefaultNvsPartition};
use rustyfarian_esp_idf_network::mqtt::{LwtConfig, MqttBuilder};
use rustyfarian_esp_idf_network::wifi::{WiFiConfig, WiFiManager};

#[path = "common/env.rs"]
mod env;

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let client_id = env::mqtt_client_id("esp32c3-mqtt");
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

    let wifi_config = WiFiConfig::new(env::WIFI_SSID, env::WIFI_PASS);
    let wifi = WiFiManager::new_without_led(peripherals.modem, sys_loop, Some(nvs), wifi_config)?;

    match wifi.get_ip(10_000)? {
        Some(ip) => log::info!("Wi-Fi connected — IP: {}", ip),
        None => log::warn!("Wi-Fi connected but IP not yet assigned"),
    }

    let lwt_topic = format!("{}/status", client_id);
    let lwt = LwtConfig::new(&lwt_topic, b"offline", QoS::AtLeastOnce, true);
    let mqtt_config = env::mqtt_config(client_id)?.with_lwt(lwt);

    let status_topic = format!("{}/status", client_id);
    let commands_topic = format!("{}/commands/#", client_id);

    let handle = MqttBuilder::new(mqtt_config)
        .subscribe(&commands_topic, QoS::AtLeastOnce)
        .on_connect(move |client: &mut EspMqttClient<'_>, is_clean: bool| {
            if is_clean {
                log::info!("[mqtt] connected — clean session");
            } else {
                log::info!("[mqtt] connected — session resumed");
            }
            client.enqueue(&status_topic, QoS::AtLeastOnce, true, b"online")?;
            log::info!("[mqtt] published retained status=online");
            Ok(())
        })
        .on_disconnect(|| {
            log::warn!("[mqtt] disconnected — will reconnect automatically");
        })
        .on_message(|topic, payload| {
            let body = std::str::from_utf8(payload).unwrap_or("<non-utf8>");
            log::info!("[mqtt] message on '{}': {}", topic, body);
            let suffix = topic.rsplit('/').next().unwrap_or(topic);
            match suffix {
                "reboot" => log::info!("[cmd] reboot requested"),
                "ping" => log::info!("[cmd] ping received"),
                _ => log::warn!("[cmd] unknown command: {}", suffix),
            }
        })
        .build()?;

    log::info!("MQTT handle ready — waiting for broker connection...");

    let heartbeat_topic = format!("{}/heartbeat", client_id);
    let mut counter: u64 = 0;

    loop {
        if !handle.is_connected() {
            std::thread::sleep(std::time::Duration::from_secs(1));
            continue;
        }

        let payload = counter.to_string();
        if let Err(e) = handle.publish(&heartbeat_topic, &payload) {
            log::warn!("[mqtt] heartbeat publish failed: {:#}", e);
        } else {
            log::info!("[mqtt] heartbeat {} sent", counter);
        }
        counter += 1;
        std::thread::sleep(std::time::Duration::from_secs(5));
    }
}
