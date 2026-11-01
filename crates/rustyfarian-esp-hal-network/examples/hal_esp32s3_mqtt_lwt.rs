//! Bare-metal MQTT example for ESP32-S3 (CrowPanel 1.28" HMI): LWT, retained online flag, reconnect.
//!
//! Console output goes to the USB-Serial-JTAG port (the CrowPanel has no UART bridge);
//! the example build selects `esp-println/auto` through the crate's dev-dependency.
//! The firmware starts the `esp-rtos` scheduler itself in `main` and hands Wi-Fi only
//! the `WIFI` peripheral (`with_wifi_scheduler_running`), the boot shape of a firmware
//! that must also run without Wi-Fi (rustyfarian-knob, request U2e).
//! Joins Wi-Fi, then drives the [`MqttRunner`] from its own task and uses the
//! [`MqttHandle`] from the main loop:
//!
//! * CONNECT carries an LWT `offline` (retained, QoS 1) on `<prefix>/status`.
//! * Every (re)connect publishes the retained startup message `online` there.
//! * `<prefix>/cmd` is subscribed; each inbound message is logged by `rx_task`.
//! * Every 5 s the main loop `try_publish`es a tick on `<prefix>/tick`;
//!   `WouldBlock` is logged and skipped, never retried in place.
//! * Free heap is logged after connect and once a minute for soak runs.
//! * With `MQTT_SUSPEND_DEMO=1` the radio is suspended for 20 s every 60 s
//!   (`WifiLink::suspend` / `resume`), the cycle a deep-sleep firmware runs when
//!   a sleep attempt is aborted; the broker sees the will, then `online` again.
//!
//! Build-time environment (all `option_env!`, see `.env.example`):
//!
//! | Variable          | Default                | Meaning                               |
//! |:------------------|:-----------------------|:--------------------------------------|
//! | `WIFI_SSID`       | (required)             | Wi-Fi SSID                            |
//! | `WIFI_PASS`       | (required)             | Wi-Fi password                        |
//! | `MQTT_HOST`       | (required)             | Broker **IPv4 literal** (no DNS yet)  |
//! | `MQTT_PORT`       | `1883`                 | Broker port                           |
//! | `MQTT_CLIENT_ID`  | `hal-s3-mqtt`          | Client id (≤ 23 bytes)                |
//! | `MQTT_USER`       | unset                  | Optional username                     |
//! | `MQTT_PASS`       | unset                  | Optional password                     |
//! | `MQTT_TOPIC_PREFIX` | `rustyfarian/hal-mqtt` | Topic prefix for status/cmd/tick   |
//! | `MQTT_SUSPEND_DEMO` | unset                  | `1`: suspend 20 s every 60 s       |
//!
//! # Build and flash
//!
//! ```sh
//! just build-example hal_esp32s3_mqtt_lwt
//! just flash hal_esp32s3_mqtt_lwt
//! ```

#![no_std]
#![no_main]

extern crate alloc;

use embassy_executor::Spawner;
use embassy_time::{Duration, Instant, Timer};
use esp_backtrace as _;
use esp_hal::timer::timg::TimerGroup;
use esp_radio::wifi::Interface;
use rustyfarian_esp_hal_network::mqtt::{
    LwtConfig, MqttBuilder, MqttHandle, MqttRunner, QoS, TryPublishError,
};
use rustyfarian_esp_hal_network::wifi::{
    link_task, AsyncWifiHandle, WiFiConfig, WiFiConfigExt, WiFiManager, WifiLink,
    SUSPEND_DISCONNECT_TIMEOUT_MS,
};
use static_cell::StaticCell;

#[path = "common/env.rs"]
mod env;

esp_bootloader_esp_idf::esp_app_desc!();

static LINK: WifiLink = WifiLink::new();

const TICK_SECS: u64 = 5;
const HEAP_LOG_SECS: u64 = 60;
const SUSPEND_EVERY_SECS: u64 = 60;
const SUSPEND_FOR_SECS: u64 = 20;
const KEEP_ALIVE_SECS: u64 = 30;

/// Logs a start-up failure and parks; an example has nothing to fall back to.
fn fatal(what: &str, reason: &dyn core::fmt::Display) -> ! {
    log::error!("FATAL: {}: {}", what, reason);
    loop {}
}

/// Builds `<prefix>/<suffix>` once into static storage so the builder can keep it.
fn static_topic(
    cell: &'static StaticCell<heapless::String<64>>,
    prefix: &str,
    suffix: &str,
) -> &'static str {
    let mut topic = heapless::String::<64>::new();
    let _ = topic.push_str(prefix);
    let _ = topic.push('/');
    let _ = topic.push_str(suffix);
    cell.init(topic).as_str()
}

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    esp_println::logger::init_logger(log::LevelFilter::Info);

    let client_id = env::mqtt_client_id("hal-s3-mqtt");
    let prefix = env::mqtt_topic_prefix("rustyfarian/hal-mqtt");
    env::log_config(client_id);
    if let Err(e) = env::mqtt_host() {
        fatal("configuration", &e);
    }
    if client_id.len() > 23 {
        fatal(
            "MQTT_CLIENT_ID is longer than the MQTT 3.1.1 maximum of 23 bytes",
            &client_id,
        );
    }

    let peripherals = esp_hal::init(esp_hal::Config::default());

    // Same 72 KiB internal-RAM heap as the S3 Wi-Fi example; the MQTT client
    // adds no heap traffic (all its buffers are static).
    esp_alloc::heap_allocator!(size: 72 * 1024);

    // The firmware owns the scheduler (ADR 021): start it here, once per boot,
    // as a firmware that may boot without Wi-Fi would, so its embassy timers
    // work whether or not Wi-Fi comes up afterwards.
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    log::info!("Initializing Wi-Fi (async, scheduler already running)...");
    let config = WiFiConfig::new(env::WIFI_SSID, env::wifi_pass())
        .with_wifi_scheduler_running(peripherals.WIFI);
    let handle = match WiFiManager::init_async(config) {
        Ok(h) => h,
        Err(e) => fatal("Wi-Fi init failed", &e),
    };
    let AsyncWifiHandle {
        controller,
        stack,
        runner,
    } = handle;
    spawner.spawn(link_task(&LINK, controller, stack).unwrap());
    spawner.spawn(net_task(runner).unwrap());

    log::info!("Waiting for the link to be ready (associated with an IP)...");
    LINK.wait_connected().await;
    let v4 = stack
        .config_v4()
        .expect("stack reports config up but has no IPv4 config");
    log::info!(
        "Wi-Fi connected — IP: {}  gateway: {:?}",
        v4.address,
        v4.gateway
    );

    static STATUS_TOPIC: StaticCell<heapless::String<64>> = StaticCell::new();
    static CMD_TOPIC: StaticCell<heapless::String<64>> = StaticCell::new();
    static TICK_TOPIC: StaticCell<heapless::String<64>> = StaticCell::new();
    let status_topic = static_topic(&STATUS_TOPIC, prefix, "status");
    let cmd_topic = static_topic(&CMD_TOPIC, prefix, "cmd");
    let tick_topic = static_topic(&TICK_TOPIC, prefix, "tick");

    let lwt = LwtConfig::new(status_topic, b"offline", QoS::AtLeastOnce, true);
    let mut mqtt_config = match env::mqtt_config(client_id) {
        Ok(c) => c.with_keep_alive(KEEP_ALIVE_SECS).with_lwt(lwt),
        Err(e) => fatal("configuration", &e),
    };
    if let Some(user) = env::MQTT_USER {
        mqtt_config = match env::mqtt_pass() {
            Some(pass) => mqtt_config.with_auth(user, pass),
            None => mqtt_config.with_username_only(user),
        };
    }
    let (mqtt, mqtt_runner) = match MqttBuilder::new(mqtt_config)
        .with_startup_message(status_topic, b"online", QoS::AtLeastOnce, true)
        .subscribe(cmd_topic, QoS::AtLeastOnce)
        .build(stack)
    {
        Ok(pair) => pair,
        Err(e) => fatal("MQTT build failed", &e),
    };
    spawner.spawn(mqtt_task(mqtt_runner).unwrap());
    spawner.spawn(rx_task(mqtt).unwrap());

    log::info!(
        "[example] topics: status={} cmd={} tick={}",
        status_topic,
        cmd_topic,
        tick_topic
    );

    let suspend_demo = env::mqtt_suspend_demo();
    let mut tick: u32 = 0;
    let mut last_link = LINK.state();
    let mut last_state = mqtt.state();
    let mut was_connected = false;
    let mut secs_since_heap_log: u64 = 0;
    loop {
        Timer::after(Duration::from_secs(TICK_SECS)).await;
        tick += 1;
        secs_since_heap_log += TICK_SECS;

        let uptime = tick as u64 * TICK_SECS;
        if suspend_demo && uptime % SUSPEND_EVERY_SECS == 0 {
            log::info!("[example] demo: suspend for {} s", SUSPEND_FOR_SECS);
            LINK.suspend();
            // The deep-sleep recipe: wait, bounded, until the station is down.
            let deadline = Instant::now() + Duration::from_millis(SUSPEND_DISCONNECT_TIMEOUT_MS);
            while !LINK.is_suspended() && Instant::now() < deadline {
                Timer::after(Duration::from_millis(10)).await;
            }
            log::info!("[example] demo: is_suspended = {}", LINK.is_suspended());
        } else if suspend_demo && uptime % SUSPEND_EVERY_SECS == SUSPEND_FOR_SECS {
            log::info!("[example] demo: resume");
            LINK.resume();
        }

        let link = LINK.state();
        if link != last_link {
            log::info!("[example] wifi link {:?} -> {:?}", last_link, link);
            last_link = link;
        }
        let state = mqtt.state();
        if state != last_state {
            log::info!("[example] mqtt state {:?} -> {:?}", last_state, state);
            last_state = state;
        }
        if mqtt.is_connected() && !was_connected {
            log::info!(
                "[example] connect #{} free heap: {} B",
                mqtt.connect_count(),
                esp_alloc::HEAP.free()
            );
        }
        was_connected = mqtt.is_connected();

        let mut payload = heapless::String::<96>::new();
        let _ = core::fmt::write(
            &mut payload,
            format_args!(
                "{{\"tick\":{},\"uptime_s\":{},\"free_heap\":{}}}",
                tick,
                uptime,
                esp_alloc::HEAP.free()
            ),
        );
        match mqtt.try_publish(tick_topic, payload.as_bytes()) {
            Ok(()) => log::info!("[example] tick #{} queued", tick),
            Err(TryPublishError::WouldBlock) => {
                log::info!("[example] tick #{} would block (mqtt {:?})", tick, state)
            }
            Err(TryPublishError::Other(e)) => log::warn!("[example] tick #{} failed: {}", tick, e),
        }

        if secs_since_heap_log >= HEAP_LOG_SECS {
            secs_since_heap_log = 0;
            log::info!(
                "[example] heap free: {} B, connects: {}, dropped rx/tx: {}/{}",
                esp_alloc::HEAP.free(),
                mqtt.connect_count(),
                mqtt.dropped_inbound(),
                mqtt.dropped_publish()
            );
        }
    }
}

#[embassy_executor::task]
async fn mqtt_task(runner: MqttRunner) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn rx_task(mqtt: MqttHandle) {
    loop {
        let msg = mqtt.receive().await;
        match core::str::from_utf8(msg.payload.as_slice()) {
            Ok(text) => log::info!(
                "[example] rx topic={} payload={} retained={}",
                msg.topic.as_str(),
                text,
                msg.retained
            ),
            Err(_) => log::info!(
                "[example] rx topic={} payload={} bytes (binary)",
                msg.topic.as_str(),
                msg.payload.len()
            ),
        }
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, Interface>) -> ! {
    runner.run().await
}
