//! Bare-metal MQTT example for classic ESP32: LWT, retained online flag, reconnect.
//!
//! Console output goes to UART0 (the classic ESP32 has no USB-Serial-JTAG); the example
//! build selects `esp-println/auto` through the crate's dev-dependency.
//! The firmware starts the `esp-rtos` scheduler itself in `main` and hands Wi-Fi only
//! the `WIFI` peripheral (`with_wifi_scheduler_running`), the boot shape of a firmware
//! that must also run without Wi-Fi (rustyfarian-knob, request U2e).
//!
//! Sibling of `idf_esp32s3_mqtt_lwt`: same environment keys, same topics, same 5 s tick, and
//! the same application body — `examples-shared/lwt_app.rs` is compiled into both examples,
//! so the two tiers can be compared on one broker (different default client id and prefix).
//! This file is setup only: Wi-Fi, the MQTT runner task and the shared app as the main task.
//!
//! * CONNECT carries an LWT `offline` (retained, QoS 1) on `<prefix>/status`.
//! * Every (re)connect publishes the retained startup message `online` there.
//! * `<prefix>/cmd` is subscribed; inbound messages reach `MqttHandle::receive`, which the
//!   shared app drains.
//! * Everything else — the 5 s tick on `<prefix>/tick`, link and MQTT state logs, the free
//!   heap, and the suspend demo — is the shared app, and its lines start with `[app]`.
//!
//! Build-time environment (all `option_env!`, see `.env.example`):
//!
//! | Variable          | Default                | Meaning                               |
//! |:------------------|:-----------------------|:--------------------------------------|
//! | `WIFI_SSID`       | (required)             | Wi-Fi SSID                            |
//! | `WIFI_PASS`       | (required)             | Wi-Fi password                        |
//! | `MQTT_HOST`       | (required)             | Broker IPv4 literal or DNS name (IPv6 not supported) |
//! | `MQTT_PORT`       | `1883`                 | Broker port                           |
//! | `MQTT_CLIENT_ID`  | `hal-esp32-mqtt`       | Client id (≤ 23 bytes)                |
//! | `MQTT_USER`       | unset                  | Optional username                     |
//! | `MQTT_PASS`       | unset                  | Optional password                     |
//! | `MQTT_TOPIC_PREFIX` | `rustyfarian/hal-mqtt` | Topic prefix for status/cmd/tick   |
//! | `MQTT_SUSPEND_DEMO` | unset                  | `1`: suspend 20 s every 60 s       |
//!
//! # Build and flash
//!
//! ```sh
//! just build-example hal_esp32_mqtt_lwt
//! just flash hal_esp32_mqtt_lwt
//! ```

#![no_std]
#![no_main]

extern crate alloc;

use embassy_executor::Spawner;
use esp_backtrace as _;
use esp_hal::timer::timg::TimerGroup;
use esp_radio::wifi::Interface;
use rustyfarian_esp_hal_network::mqtt::{LwtConfig, MqttBuilder, MqttRunner, QoS};
use rustyfarian_esp_hal_network::wifi::{
    link_task, AsyncWifiHandle, WiFiConfig, WiFiConfigExt, WiFiManager, WifiLink,
};
use static_cell::StaticCell;

#[path = "common/env.rs"]
mod env;
#[path = "../../../examples-shared/lwt_app.rs"]
mod lwt_app;

esp_bootloader_esp_idf::esp_app_desc!();

static LINK: WifiLink = WifiLink::new();

const KEEP_ALIVE_SECS: u64 = 30;

fn free_heap() -> usize {
    esp_alloc::HEAP.free()
}

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

    let client_id = env::mqtt_client_id("hal-esp32-mqtt");
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

    // Same 72 KiB internal-RAM heap as the classic ESP32 Wi-Fi example; the MQTT client
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

    log::info!(
        "[example] topics: status={} cmd={} tick={}",
        status_topic,
        cmd_topic,
        tick_topic
    );

    let cfg = lwt_app::LwtAppConfig {
        tick_topic,
        suspend_demo: env::mqtt_suspend_demo(),
        free_heap,
    };
    lwt_app::run(&LINK, &mqtt, &cfg).await;
}

#[embassy_executor::task]
async fn mqtt_task(runner: MqttRunner) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, Interface>) -> ! {
    runner.run().await
}
