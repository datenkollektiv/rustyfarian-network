//! Tier-parity spike, variant A (ADR 022, plan Step 2): async app over the existing
//! `WiFiManager` and `MqttHandle` worker threads.
//!
//! The application is plain `async` code run by `esp_idf_svc::hal::task::block_on`;
//! the library's threads (the Wi-Fi link supervisor and the MQTT event loop) stay as
//! they are, and the example adds the async glue a tier port would provide:
//!
//! * `wait_until` polls a readiness getter every 10 ms with `embassy_time::Timer` (the
//!   real port would wake on a signal set by the worker thread);
//! * `on_message` hands each inbound message to the shared bounded queue that the
//!   async `rx` future drains, the shape of an async `receive()`.
//!
//! Ticks use the library's non-blocking `try_publish`, which hands the message to
//! esp-mqtt's outbox and never waits for the network ("queued").
//!
//! Same app and the same `common/spike.rs` workload as variant B
//! (`idf_c3_async_mqtt_native`): LWT `offline` and retained `online` on
//! `<prefix>/status`, `<prefix>/cmd` subscribed, a tick every 5 s, state changes logged,
//! and once a minute heap and per-task stack high-water marks.
//!
//! Build-time environment as in `idf_esp32s3_mqtt_lwt` (see `.env.example`); default
//! client id `idf-c3-async-a`, prefix `rustyfarian/idf-async-a`.
//!
//! ```sh
//! just build-example idf_c3_async_mqtt
//! just flash idf_c3_async_mqtt
//! ```

use embassy_futures::join::join;
use embassy_time::{Duration, Instant, Timer};
use esp_idf_svc::hal::{peripherals::Peripherals, task::block_on};
use esp_idf_svc::mqtt::client::EspMqttClient;
use esp_idf_svc::{eventloop::EspSystemEventLoop, nvs::EspDefaultNvsPartition};
use rustyfarian_esp_idf_network::mqtt::{
    to_esp_qos, LwtConfig, MqttBuilder, MqttHandle, QoS, TryPublishError,
};
use rustyfarian_esp_idf_network::wifi::{WiFiConfig, WiFiConfigExt, WiFiManager};

#[path = "common/env.rs"]
mod env;
#[path = "common/spike.rs"]
mod spike;

const TICK_SECS: u64 = 5;
const RESOURCE_LOG_SECS: u64 = 60;
const KEEP_ALIVE_SECS: u64 = 30;
/// Poll period of the spike's `wait_until`; a real port wakes on a signal instead.
const READY_POLL_MS: u64 = 10;
/// Slice of the blocking wait for the first IP; the loop waits as long as it takes,
/// and `get_ip` warns once per elapsed slice.
const WIFI_IP_WAIT_SLICE_MS: u64 = 30_000;
/// Worker tasks of this variant: `wifi-link` named through `spike::name_next_threads`,
/// `mqtt-events` and `mqtt-net` by the library.
const WORKER_TASKS: [&std::ffi::CStr; 4] =
    [c"wifi-link", c"mqtt-events", c"mqtt-net", c"mqtt_task"];

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    log::info!("[app] start {} (esp-idf)", env!("CARGO_CRATE_NAME"));
    let boot = Instant::now();

    let client_id = env::mqtt_client_id("idf-c3-async-a");
    let prefix = env::mqtt_topic_prefix("rustyfarian/idf-async-a");
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
    spike::name_next_threads(Some(c"wifi-link"));
    let wifi = WiFiManager::init(wifi_config)?;
    spike::name_next_threads(None);

    // Proper order: MQTT starts only once the station has an IP.
    let ip = loop {
        if let Some(ip) = wifi.get_ip(WIFI_IP_WAIT_SLICE_MS)? {
            break ip;
        }
    };
    log::info!(
        "[app] wifi up after {} ms, ip {}",
        boot.elapsed().as_millis(),
        ip
    );

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
    spike::name_next_threads(Some(c"mqtt-events"));
    let mqtt = MqttBuilder::new(mqtt_config)
        .subscribe(&cmd_topic, QoS::AtLeastOnce)
        .on_connect(
            move |client: &mut EspMqttClient<'_>, _session_present: bool| {
                client.enqueue(&online_topic, to_esp_qos(QoS::AtLeastOnce), true, b"online")?;
                Ok(())
            },
        )
        .on_message(spike::offer_inbound)
        .build(&wifi)?;
    spike::name_next_threads(None);

    log::info!(
        "[app] variant A, topics: status={} cmd={} tick={}",
        status_topic,
        cmd_topic,
        tick_topic
    );

    block_on(join(app(boot, &wifi, &mqtt, &tick_topic), spike::rx()));
    Ok(())
}

/// Spike adapter: waits until `ready` holds, polling every [`READY_POLL_MS`].
async fn wait_until(mut ready: impl FnMut() -> bool) {
    while !ready() {
        Timer::after(Duration::from_millis(READY_POLL_MS)).await;
    }
}

async fn app(boot: Instant, wifi: &WiFiManager, mqtt: &MqttHandle, tick_topic: &str) {
    wait_until(|| mqtt.is_connected()).await;
    log::info!("[app] mqtt up after {} ms", boot.elapsed().as_millis());

    let mut tick: u32 = 0;
    let mut last_link = wifi.state();
    let mut last_state = mqtt.state();
    let mut secs_since_resource_log: u64 = 0;
    loop {
        Timer::after(Duration::from_secs(TICK_SECS)).await;
        tick += 1;
        secs_since_resource_log += TICK_SECS;

        let link = wifi.state();
        if link != last_link {
            log::info!("[app] wifi link {:?} -> {:?}", last_link, link);
            last_link = link;
        }
        let state = mqtt.state();
        if state != last_state {
            log::info!("[app] mqtt state {:?} -> {:?}", last_state, state);
            last_state = state;
        }

        let payload = format!(
            "{{\"tick\":{},\"free_heap\":{},\"variant\":\"A\"}}",
            tick,
            spike::heap_free()
        );
        match mqtt.try_publish(tick_topic, &payload) {
            Ok(()) => log::info!("[app] tick #{} queued", tick),
            Err(TryPublishError::WouldBlock) => {
                log::info!("[app] tick #{} would block (mqtt {:?})", tick, state)
            }
            Err(TryPublishError::Other(e)) => {
                log::warn!("[app] tick #{} failed: {:#}", tick, e)
            }
        }

        if secs_since_resource_log >= RESOURCE_LOG_SECS {
            secs_since_resource_log = 0;
            spike::log_resources("A", mqtt.connect_count(), &WORKER_TASKS);
        }
    }
}
