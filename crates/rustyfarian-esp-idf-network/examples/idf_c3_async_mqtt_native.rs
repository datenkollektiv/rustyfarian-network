//! Tier-parity spike, variant B (ADR 022, plan Step 2): the same app on `esp-idf-svc`'s
//! native async types, `AsyncWifi` and `EspAsyncMqttClient`.
//!
//! Nothing of this crate's Wi-Fi or MQTT runtime is used; the example does what a tier
//! port built on the async types would have to do itself:
//!
//! * `join_with_backoff` joins and waits for the netif before the MQTT client exists
//!   (bug 010), and `keep_link` rejoins after every drop with the shared back-off
//!   constants (1 s doubling to 30 s);
//! * `mqtt_events` pumps `EspAsyncMqttConnection`, feeds the host-tested
//!   `juggler::mqtt::SessionPhase`, advances a `ConnectionEpoch` on every `Connected`
//!   and `Disconnected`, and hands new sessions to `sessions` through a signal; it never
//!   awaits anything but `next()`, because esp-mqtt holds its API lock until the next
//!   `next()` call releases the event (the ADR 017 deadlock, here between the pump and
//!   any publish);
//! * `sessions` subscribes and publishes the retained `online` for each new session,
//!   then confirms the epoch token, so a session that dropped meanwhile is never
//!   reported ready (the library's own pattern).
//!
//! esp-idf-svc 0.53 has no non-blocking publish on the async client: ticks await
//! `publish`, which returns once esp-mqtt has sent the message (QoS 1 does not wait
//! for the PUBACK), and the log reports how long that took ("published in N ms").
//!
//! Same app and the same `common/spike.rs` workload as variant A (`idf_c3_async_mqtt`):
//! LWT `offline` and retained `online` on `<prefix>/status`, `<prefix>/cmd` subscribed,
//! a tick every 5 s, state changes logged, and once a minute heap and per-task stack
//! high-water marks.
//!
//! Build-time environment as in `idf_esp32s3_mqtt_lwt` (see `.env.example`); default
//! client id `idf-c3-async-b`, prefix `rustyfarian/idf-async-b`.
//!
//! ```sh
//! just build-example idf_c3_async_mqtt_native
//! just flash idf_c3_async_mqtt_native
//! ```

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use embassy_futures::join::join5;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_sync::signal::Signal;
use embassy_time::{Duration, Instant, Timer};
use esp_idf_svc::hal::{peripherals::Peripherals, task::block_on};
use esp_idf_svc::mqtt::client::{
    EspAsyncMqttClient, EspAsyncMqttConnection, EventPayload, LwtConfiguration,
    MqttClientConfiguration, QoS as EspQoS,
};
use esp_idf_svc::timer::EspTaskTimerService;
use esp_idf_svc::wifi::{AsyncWifi, ClientConfiguration, Configuration, EspWifi};
use esp_idf_svc::{eventloop::EspSystemEventLoop, nvs::EspDefaultNvsPartition};
use juggler::mqtt::{
    format_broker_url, ConnectionEpoch, EpochToken, MqttConnectionState, SessionPhase,
};
use juggler::wifi::{RECONNECT_BACKOFF_BASE_MS, RECONNECT_BACKOFF_MAX_MS};
use rustyfarian_esp_idf_network::mqtt::DEFAULT_MQTT_TASK_STACK_SIZE;

#[path = "common/env.rs"]
mod env;
#[path = "common/spike.rs"]
mod spike;

const TICK_SECS: u64 = 5;
const RESOURCE_LOG_SECS: u64 = 60;
const KEEP_ALIVE_SECS: u64 = 30;
/// Worker tasks of this variant: esp-mqtt's task and the async client's sender thread,
/// whose name `MQTT Sending task` FreeRTOS stores cut to 15 characters.
const WORKER_TASKS: [&std::ffi::CStr; 2] = [c"mqtt_task", c"MQTT Sending ta"];

/// Station link up (associated and the netif has an address).
static LINK_UP: AtomicBool = AtomicBool::new(false);
/// Sessions confirmed since boot.
static CONNECTS: AtomicU32 = AtomicU32::new(0);
static PHASE: SessionPhase = SessionPhase::new();
/// Token of the latest `Connected`, handed from the pump to `sessions`.
static NEW_SESSION: Signal<CriticalSectionRawMutex, EpochToken> = Signal::new();

fn mqtt_state(epoch: &ConnectionEpoch) -> MqttConnectionState {
    PHASE.state(epoch.is_connected())
}

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    let boot = Instant::now();

    let client_id = env::mqtt_client_id("idf-c3-async-b");
    let prefix = env::mqtt_topic_prefix("rustyfarian/idf-async-b");
    env::log_config(client_id);
    let host = env::mqtt_host()?;
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
    let timer_service = EspTaskTimerService::new()?;

    let mut wifi = AsyncWifi::wrap(
        EspWifi::new(peripherals.modem, sys_loop.clone(), Some(nvs))?,
        sys_loop,
        timer_service,
    )?;
    wifi.set_configuration(&Configuration::Client(ClientConfiguration {
        ssid: env::WIFI_SSID
            .try_into()
            .map_err(|_| anyhow::anyhow!("WIFI_SSID is longer than 32 bytes"))?,
        password: env::wifi_pass()
            .try_into()
            .map_err(|_| anyhow::anyhow!("WIFI_PASS is longer than 64 bytes"))?,
        ..Default::default()
    }))?;

    let status_topic = format!("{prefix}/status");
    let cmd_topic = format!("{prefix}/cmd");
    let tick_topic = format!("{prefix}/tick");

    let mqtt_config = MqttClientConfiguration {
        client_id: Some(client_id),
        keep_alive_interval: Some(std::time::Duration::from_secs(KEEP_ALIVE_SECS)),
        // `None` would disable esp-mqtt's auto-reconnect; `ZERO` keeps its 10 s default.
        reconnect_timeout: Some(std::time::Duration::ZERO),
        task_stack: DEFAULT_MQTT_TASK_STACK_SIZE,
        lwt: Some(LwtConfiguration {
            topic: &status_topic,
            payload: b"offline",
            qos: EspQoS::AtLeastOnce,
            retain: true,
        }),
        username: env::MQTT_USER,
        password: env::mqtt_pass(),
        ..Default::default()
    };
    let url = format_broker_url(host, env::mqtt_port());

    block_on(async {
        // Proper order (bug 010): MQTT starts only once the station has an IP.
        wifi.start().await?;
        join_with_backoff(&mut wifi).await;
        log::info!("[example] wifi up after {} ms", boot.elapsed().as_millis());

        let (client, mut connection) = EspAsyncMqttClient::new(&url, &mqtt_config)?;
        let client = Mutex::<CriticalSectionRawMutex, _>::new(client);
        let epoch = ConnectionEpoch::new();
        log::info!(
            "[example] variant B, topics: status={} cmd={} tick={}",
            status_topic,
            cmd_topic,
            tick_topic
        );

        join5(
            keep_link(&mut wifi),
            mqtt_events(&mut connection, &epoch),
            sessions(boot, &client, &epoch, &status_topic, &cmd_topic),
            app(&client, &epoch, &tick_topic),
            spike::rx(),
        )
        .await;
        Ok(())
    })
}

/// Joins and waits for the netif, retrying with the shared back-off until it works.
async fn join_with_backoff(wifi: &mut AsyncWifi<EspWifi<'static>>) {
    let mut backoff_ms = RECONNECT_BACKOFF_BASE_MS;
    loop {
        let joined = match wifi.connect().await {
            Ok(()) => wifi.wait_netif_up().await,
            Err(e) => Err(e),
        };
        match joined {
            Ok(()) => {
                LINK_UP.store(true, Ordering::Release);
                return;
            }
            Err(e) => {
                log::warn!(
                    "[example] wifi connect failed: {:?}, retry in {} ms",
                    e,
                    backoff_ms
                );
                Timer::after(Duration::from_millis(backoff_ms as u64)).await;
                backoff_ms = (backoff_ms * 2).min(RECONNECT_BACKOFF_MAX_MS);
            }
        }
    }
}

/// Keeps the link up: waits for it to drop, then rejoins.
async fn keep_link(wifi: &mut AsyncWifi<EspWifi<'static>>) {
    loop {
        if let Err(e) = wifi.wifi_wait(|w| w.is_connected(), None).await {
            log::warn!("[example] wifi wait failed: {:?}", e);
        }
        LINK_UP.store(false, Ordering::Release);
        log::warn!("[example] wifi link down");
        join_with_backoff(wifi).await;
        log::info!("[example] wifi link up");
    }
}

/// Pumps esp-mqtt's events; awaits nothing but `next()` (see the module docs).
async fn mqtt_events(connection: &mut EspAsyncMqttConnection, epoch: &ConnectionEpoch) {
    loop {
        let event = match connection.next().await {
            Ok(event) => event,
            Err(_) => {
                log::info!("[example] mqtt connection closed");
                return;
            }
        };
        match event.payload() {
            EventPayload::BeforeConnect => PHASE.attempt_started(),
            EventPayload::Connected(_) => NEW_SESSION.signal(epoch.advance()),
            EventPayload::Disconnected => {
                // Recorded before `advance()` clears the ready bit, as in the library.
                PHASE.session_lost();
                epoch.advance();
            }
            EventPayload::Received {
                topic: Some(topic),
                data,
                ..
            } => spike::offer_inbound(topic, data),
            _ => {}
        }
    }
}

/// Does each new session's connect work, then confirms it if it is still current.
async fn sessions(
    boot: Instant,
    client: &Mutex<CriticalSectionRawMutex, EspAsyncMqttClient>,
    epoch: &ConnectionEpoch,
    status_topic: &str,
    cmd_topic: &str,
) {
    loop {
        let token = NEW_SESSION.wait().await;
        if !epoch.is_current(token) {
            continue;
        }
        let mut client = client.lock().await;
        let subscribed = client.subscribe(cmd_topic, EspQoS::AtLeastOnce).await;
        let online = client
            .publish(status_topic, EspQoS::AtLeastOnce, true, b"online")
            .await;
        drop(client);
        if let (Err(e), _) | (_, Err(e)) = (&subscribed, &online) {
            log::warn!("[example] connect work failed: {:?}", e);
            continue;
        }
        if epoch.confirm(token) {
            let connects = CONNECTS.fetch_add(1, Ordering::AcqRel) + 1;
            if connects == 1 {
                log::info!("[example] mqtt up after {} ms", boot.elapsed().as_millis());
            }
        } else {
            log::info!("[example] session dropped during its connect work, not marked ready");
        }
    }
}

async fn app(
    client: &Mutex<CriticalSectionRawMutex, EspAsyncMqttClient>,
    epoch: &ConnectionEpoch,
    tick_topic: &str,
) {
    let mut tick: u32 = 0;
    let mut last_link = LINK_UP.load(Ordering::Acquire);
    let mut last_state = mqtt_state(epoch);
    let mut secs_since_resource_log: u64 = 0;
    loop {
        Timer::after(Duration::from_secs(TICK_SECS)).await;
        tick += 1;
        secs_since_resource_log += TICK_SECS;

        let link = LINK_UP.load(Ordering::Acquire);
        if link != last_link {
            log::info!("[example] wifi link up {} -> {}", last_link, link);
            last_link = link;
        }
        let state = mqtt_state(epoch);
        if state != last_state {
            log::info!("[example] mqtt state {:?} -> {:?}", last_state, state);
            last_state = state;
        }

        if state == MqttConnectionState::Connected {
            let payload = format!(
                "{{\"tick\":{},\"free_heap\":{},\"variant\":\"B\"}}",
                tick,
                spike::heap_free()
            );
            let started = Instant::now();
            let published = client
                .lock()
                .await
                .publish(tick_topic, EspQoS::AtLeastOnce, false, payload.as_bytes())
                .await;
            match published {
                Ok(_) => log::info!(
                    "[example] tick #{} published in {} ms",
                    tick,
                    started.elapsed().as_millis()
                ),
                Err(e) => log::warn!("[example] tick #{} failed: {:?}", tick, e),
            }
        } else {
            log::info!("[example] tick #{} would block (mqtt {:?})", tick, state);
        }

        if secs_since_resource_log >= RESOURCE_LOG_SECS {
            secs_since_resource_log = 0;
            spike::log_resources("B", CONNECTS.load(Ordering::Relaxed), &WORKER_TASKS);
        }
    }
}
