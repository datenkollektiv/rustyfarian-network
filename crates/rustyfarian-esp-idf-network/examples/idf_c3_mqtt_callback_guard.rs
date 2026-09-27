//! Hardware check for the MQTT callback contract on ESP32-C3.
//!
//! Exercises both paths that deadlocked the client before 0.5.1:
//!
//! - **Connect path** — `with_startup_message()` plus an `on_connect` that publishes a retained "online" status through its `client` argument.
//!   Pass: `is_connected()` becomes `true` and the heartbeat loop starts.
//! - **Message path** — the device subscribes to `{client_id}/echo` and publishes to it from the main loop every 2 s.
//!   `on_message` then deliberately calls `MqttHandle::try_publish` and `MqttHandle::publish`, which must fail fast with `PublishAckError::WrongThread` instead of hanging.
//!   Pass: every status line shows `received` tracking `sent` and `guard_hits` growing by two per echo.
//!
//! No host-side MQTT client is needed — the echo loop runs on the device itself.
//!
//! # Environment variables (set at compile time)
//!
//! | Variable | Default | Description |
//! |---|---|---|
//! | `WIFI_SSID` | `""` | Wi-Fi network name |
//! | `WIFI_PASS` | `""` | Wi-Fi password |
//! | `MQTT_HOST` | (required) | MQTT broker IP or hostname |
//! | `MQTT_PORT` | `1883` | MQTT broker port |
//! | `MQTT_CLIENT_ID` | `esp32c3-guard` | Unique device identifier |
//!
//! # Build and flash
//!
//! ```sh
//! just build-example idf_c3_mqtt_callback_guard
//! ```
//!
//! ```sh
//! just flash idf_c3_mqtt_callback_guard
//! ```

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::mqtt::client::{EspMqttClient, QoS};
use esp_idf_svc::{eventloop::EspSystemEventLoop, nvs::EspDefaultNvsPartition};
use rustyfarian_esp_idf_network::mqtt::{
    LwtConfig, MqttBuilder, MqttHandle, PublishAckError, TryPublishError,
};
use rustyfarian_esp_idf_network::wifi::{WiFiConfig, WiFiManager};

#[path = "common/env.rs"]
mod env;

fn is_wrong_thread(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<PublishAckError>(),
        Some(PublishAckError::WrongThread)
    )
}

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let client_id = env::mqtt_client_id("esp32c3-guard");
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

    let wifi_config = WiFiConfig::new(env::WIFI_SSID, env::wifi_pass());
    let wifi = WiFiManager::new_without_led(peripherals.modem, sys_loop, Some(nvs), wifi_config)?;

    match wifi.get_ip(10_000)? {
        Some(ip) => log::info!("Wi-Fi connected — IP: {}", ip),
        None => log::warn!("Wi-Fi connected but IP not yet assigned"),
    }

    let status_topic = format!("{}/status", client_id);
    let lwt = LwtConfig::new(&status_topic, b"offline", QoS::AtLeastOnce, true);
    let mqtt_config = env::mqtt_config(client_id)?.with_lwt(lwt);

    let echo_topic = format!("{}/echo", client_id);
    let online_topic = status_topic.clone();

    // The handle only exists after build(), so on_message reads it from here.
    // Holding a clone inside the callback keeps the event loop alive for the
    // lifetime of the program, which is what this check wants.
    let handle_slot: Arc<OnceLock<MqttHandle>> = Arc::new(OnceLock::new());
    let handle_for_cb = Arc::clone(&handle_slot);
    let received = Arc::new(AtomicU32::new(0));
    let received_cb = Arc::clone(&received);
    let guard_hits = Arc::new(AtomicU32::new(0));
    let guard_hits_cb = Arc::clone(&guard_hits);
    let echo_for_cb = echo_topic.clone();

    let handle = MqttBuilder::new(mqtt_config)
        .subscribe(&echo_topic, QoS::AtLeastOnce)
        .with_startup_message()
        .on_connect(move |client: &mut EspMqttClient<'_>, is_clean: bool| {
            client.enqueue(&online_topic, QoS::AtLeastOnce, true, b"online")?;
            log::info!("[guard] on_connect published status=online (clean={is_clean})");
            Ok(())
        })
        .on_disconnect(|| log::warn!("[guard] disconnected — will reconnect"))
        .on_message(move |topic, _payload| {
            if topic != echo_for_cb {
                return;
            }
            received_cb.fetch_add(1, Ordering::Relaxed);
            let Some(handle) = handle_for_cb.get() else {
                return;
            };

            match handle.try_publish(&echo_for_cb, "from-callback") {
                Err(TryPublishError::Other(e)) if is_wrong_thread(&e) => {
                    guard_hits_cb.fetch_add(1, Ordering::Relaxed);
                }
                other => log::error!("[guard] try_publish from on_message: {other:?}"),
            }
            match handle.publish(&echo_for_cb, "from-callback") {
                Err(e) if is_wrong_thread(&e) => {
                    guard_hits_cb.fetch_add(1, Ordering::Relaxed);
                }
                other => log::error!("[guard] publish from on_message: {other:?}"),
            }
        })
        .build()?;

    if handle_slot.set(handle.clone()).is_err() {
        anyhow::bail!("handle slot already set");
    }

    log::info!("[guard] waiting for broker connection...");
    let connect_deadline = std::time::Instant::now() + Duration::from_secs(30);
    while !handle.is_connected() {
        if std::time::Instant::now() >= connect_deadline {
            anyhow::bail!(
                "[guard] not connected after 30 s — check MQTT_HOST / MQTT_PORT; \
                 a hang here after '[mqtt] connected' would be the connect-path deadlock"
            );
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    log::info!("[guard] connected — connect path did not deadlock");

    let mut sent: u32 = 0;
    loop {
        if handle.is_connected() {
            match handle.publish(&echo_topic, &sent.to_string()) {
                Ok(()) => sent += 1,
                Err(e) => log::warn!("[guard] echo publish failed: {e:#}"),
            }
        }
        std::thread::sleep(Duration::from_secs(2));
        log::info!(
            "[guard] sent={} received={} guard_hits={}",
            sent,
            received.load(Ordering::Relaxed),
            guard_hits.load(Ordering::Relaxed)
        );
    }
}
