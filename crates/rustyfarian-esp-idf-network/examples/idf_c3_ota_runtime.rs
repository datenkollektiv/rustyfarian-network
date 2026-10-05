//! Hardware example of the OTA consumer runtime on ESP32-C3.
//!
//! It shows the four hook points an app wires up: the MQTT command intake (`on_message` forwards to the runtime), the update worker started by `OtaRuntime::start`, the health policy (`run_health_policy` with the app's predicate, here "MQTT is connected") and the restart callback, the only place the device is ever reset.
//!
//! Never publish commands retained: a retained command would run again on every reconnect and boot.
//! The status topic has no last-will message.
//!
//! Topics: commands are published to `<OTA_TOPIC_PREFIX>/command` without the retain flag, statuses appear on `<OTA_TOPIC_PREFIX>/status`; the default prefix is `rustyfarian/example/ota`.
//!
//! # Environment variables (set at compile time)
//!
//! | Variable | Default | Description |
//! |---|---|---|
//! | `WIFI_SSID` | `""` | Wi-Fi network name |
//! | `WIFI_PASS` | `""` | Wi-Fi password |
//! | `MQTT_HOST` | (required) | MQTT broker IP or hostname |
//! | `MQTT_PORT` | `1883` | MQTT broker port |
//! | `MQTT_CLIENT_ID` | `esp32c3-ota` | Unique device identifier, at most 23 bytes |
//! | `FIRMWARE_VERSION` | `0.1.0` | `MAJOR.MINOR.PATCH` of this build |
//! | `OTA_TOPIC_PREFIX` | `rustyfarian/example/ota` | Prefix of the command and status topics |
//! | `OTA_DEMO_UNHEALTHY` | unset | `1` makes the health predicate always false, so the failure deadline expires and the image is rolled back; any other value is refused at startup |
//! | `OTA_FAILURE_DEADLINE_SECS` | `120` | Decimal seconds overriding the health policy failure deadline; must exceed the 30 s healthy dwell |
//!
//! Hardware test procedure: [`docs/runbooks/ota-hardware-test.md`](../../../docs/runbooks/ota-hardware-test.md)

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::mqtt::client::QoS;
use esp_idf_svc::{eventloop::EspSystemEventLoop, nvs::EspDefaultNvsPartition};
use rustyfarian_esp_idf_network::mqtt::MqttBuilder;
use rustyfarian_esp_idf_network::ota::runtime::{channel, open_records, OtaConfig, RestartFn};
use rustyfarian_esp_idf_network::ota::Version;
use rustyfarian_esp_idf_network::wifi::{WiFiConfig, WiFiManager};

#[path = "common/env.rs"]
mod env;

const FIRMWARE_VERSION: &str = match option_env!("FIRMWARE_VERSION") {
    Some(version) => version,
    None => "0.1.0",
};

const TOPIC_PREFIX: &str = match option_env!("OTA_TOPIC_PREFIX") {
    Some(prefix) => prefix,
    None => "rustyfarian/example/ota",
};

const DEMO_UNHEALTHY: Option<&str> = option_env!("OTA_DEMO_UNHEALTHY");

const FAILURE_DEADLINE_SECS: Option<&str> = option_env!("OTA_FAILURE_DEADLINE_SECS");

/// Reads `OTA_DEMO_UNHEALTHY`: unset or empty is off, `1` is on, anything else is a startup error.
fn demo_unhealthy() -> anyhow::Result<bool> {
    match DEMO_UNHEALTHY {
        None | Some("") => Ok(false),
        Some("1") => Ok(true),
        Some(other) => anyhow::bail!("OTA_DEMO_UNHEALTHY must be 1 or unset, got '{other}'"),
    }
}

/// Reads `OTA_FAILURE_DEADLINE_SECS`: `None` keeps the library default, garbage is a startup error.
fn failure_deadline_override() -> anyhow::Result<Option<Duration>> {
    match FAILURE_DEADLINE_SECS {
        None | Some("") => Ok(None),
        Some(text) => {
            let secs: u64 = text.parse().map_err(|_| {
                anyhow::anyhow!(
                    "OTA_FAILURE_DEADLINE_SECS '{text}' is not a whole number of seconds"
                )
            })?;
            Ok(Some(Duration::from_secs(secs)))
        }
    }
}

fn main() -> anyhow::Result<()> {
    let boot = Instant::now();
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let client_id = env::mqtt_client_id("esp32c3-ota");
    env::log_config(client_id);
    env::mqtt_host()?;
    if client_id.len() > 23 {
        anyhow::bail!(
            "MQTT_CLIENT_ID '{}' is {} bytes — MQTT 3.1.1 maximum is 23",
            client_id,
            client_id.len()
        );
    }
    let unhealthy = demo_unhealthy()?;
    let deadline_override = failure_deadline_override()?;
    let version = Version::parse(FIRMWARE_VERSION)
        .map_err(|e| anyhow::anyhow!("FIRMWARE_VERSION is not MAJOR.MINOR.PATCH: {e}"))?;
    log::info!("OTA example {version} starting");

    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    let records = open_records(nvs.clone()).context("the OTA record store cannot be opened")?;

    let wifi_config = WiFiConfig::new(env::WIFI_SSID, env::wifi_pass());
    let wifi = WiFiManager::new_without_led(peripherals.modem, sys_loop, Some(nvs), wifi_config)?;
    match wifi.get_ip(10_000)? {
        Some(ip) => log::info!("Wi-Fi connected — IP: {ip}"),
        None => log::warn!("Wi-Fi connected but IP not yet assigned"),
    }

    let restart: RestartFn = Arc::new(|| {
        esp_idf_svc::hal::reset::restart();
    });
    let mut config = OtaConfig::new(
        format!("{TOPIC_PREFIX}/command"),
        format!("{TOPIC_PREFIX}/status"),
        version,
        restart,
    )
    .context("invalid OTA configuration")?;
    if let Some(deadline) = deadline_override {
        let dwell = config.settings.timings.healthy_dwell;
        if deadline <= dwell {
            anyhow::bail!(
                "OTA_FAILURE_DEADLINE_SECS {}s must exceed the healthy dwell of {}s",
                deadline.as_secs(),
                dwell.as_secs()
            );
        }
        config.settings.timings.failure_deadline = deadline;
        log::warn!("failure deadline overridden to {}s", deadline.as_secs());
    }
    config.validate().context("invalid OTA timings")?;
    if unhealthy {
        log::error!("*** OTA_DEMO_UNHEALTHY=1: the health predicate always fails; this image will be rolled back at the failure deadline ({}s) ***", config.settings.timings.failure_deadline.as_secs());
    }
    let (submitter, runtime) = channel(config);

    let callback_submitter = submitter.clone();
    let mqtt = MqttBuilder::new(env::mqtt_config(client_id)?)
        .subscribe(submitter.command_topic(), QoS::AtLeastOnce)
        .on_message(move |topic, payload| {
            callback_submitter.handle(topic, payload);
        })
        .build()?;

    let ota = runtime.start(mqtt.clone(), records)?;

    let health_mqtt = mqtt.clone();
    let verdict = ota.run_health_policy(boot, move || !unhealthy && health_mqtt.is_connected());
    log::info!("health policy finished: {verdict:?}");

    loop {
        std::thread::sleep(Duration::from_secs(30));
        log::info!(
            "OTA: admission open {}, stack marks {:?}, dropped rejections {}",
            ota.flags().admission_open(),
            ota.stack_high_water(),
            submitter.rejects_dropped()
        );
    }
}
