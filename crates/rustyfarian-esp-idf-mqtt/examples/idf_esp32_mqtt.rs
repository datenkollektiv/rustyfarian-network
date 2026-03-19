//! MQTT client example for ESP32 using the `MqttBuilder` API.
//!
//! Demonstrates all three `MqttBuilder` callbacks:
//!
//! - `on_connect` — subscribes to the command topic and publishes a retained "online" status
//! - `on_disconnect` — logs the disconnection so reconnect attempts are visible in the TTY
//! - `on_message` — dispatches incoming commands by matching on the topic suffix
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
//! | `MQTT_CLIENT_ID` | `esp32-demo` | Unique device identifier |
//!
//! With [direnv](https://direnv.net/) and a populated `.envrc`, all variables are set automatically.
//!
//! # Build and flash
//!
//! ```sh
//! WIFI_SSID="MyNetwork" WIFI_PASS="<your-password>" just build-example idf_esp32_mqtt
//! ```
//!
//! ```sh
//! just flash idf_esp32_mqtt
//! ```

use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::mqtt::client::{EspMqttClient, QoS};
use esp_idf_svc::{eventloop::EspSystemEventLoop, nvs::EspDefaultNvsPartition};
use rustyfarian_esp_idf_mqtt::{LwtConfig, MqttBuilder, MqttConfig};
use rustyfarian_esp_idf_wifi::{WiFiConfig, WiFiManager};

const WIFI_SSID: &str = match option_env!("WIFI_SSID") {
    Some(s) => s,
    None => "",
};
const WIFI_PASS: &str = match option_env!("WIFI_PASS") {
    Some(s) => s,
    None => "",
};
const MQTT_HOST: &str = match option_env!("MQTT_HOST") {
    Some(h) => h,
    None => "",
};
const MQTT_CLIENT_ID: &str = match option_env!("MQTT_CLIENT_ID") {
    Some(id) => id,
    None => "esp32-mqtt",
};

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    log::info!(
        "Config — ssid={} mqtt_host={} client_id={}",
        WIFI_SSID,
        MQTT_HOST,
        MQTT_CLIENT_ID
    );
    if MQTT_HOST.is_empty() {
        anyhow::bail!(
            "MQTT_HOST not configured — set it at build time, e.g.:\n  MQTT_HOST=192.168.1.100 cargo build ...\nSee .env.example for all available variables."
        );
    }
    if MQTT_CLIENT_ID.len() > 23 {
        anyhow::bail!(
            "MQTT_CLIENT_ID '{}' is {} bytes — MQTT 3.1.1 maximum is 23",
            MQTT_CLIENT_ID,
            MQTT_CLIENT_ID.len()
        );
    }

    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    let wifi_config = WiFiConfig::new(WIFI_SSID, WIFI_PASS);
    let wifi = WiFiManager::new_without_led(peripherals.modem, sys_loop, Some(nvs), wifi_config)?;

    match wifi.get_ip(10_000)? {
        Some(ip) => log::info!("Wi-Fi connected — IP: {}", ip),
        None => log::warn!("Wi-Fi connected but IP not yet assigned"),
    }

    let lwt_topic = format!("{}/status", MQTT_CLIENT_ID);
    let lwt = LwtConfig::new(&lwt_topic, b"offline", QoS::AtLeastOnce, true);
    let mqtt_config = MqttConfig::new(MQTT_HOST, 1883, MQTT_CLIENT_ID).with_lwt(lwt);

    let status_topic = format!("{}/status", MQTT_CLIENT_ID);
    let commands_topic = format!("{}/commands/#", MQTT_CLIENT_ID);

    let handle = MqttBuilder::new(mqtt_config)
        .on_connect(move |client: &mut EspMqttClient<'_>, is_clean: bool| {
            if is_clean {
                log::info!("[mqtt] connected — clean session");
            } else {
                log::info!("[mqtt] connected — session resumed");
            }
            client.subscribe(&commands_topic, QoS::AtLeastOnce)?;
            log::info!("[mqtt] subscribed to {}", commands_topic);
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

    let heartbeat_topic = format!("{}/heartbeat", MQTT_CLIENT_ID);
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
