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
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_hal::timer::timg::TimerGroup;
use esp_println::println;
use esp_radio::wifi::{Interface, WifiController};
use rustyfarian_esp_hal_network::mqtt::{
    LwtConfig, MqttBuilder, MqttConfig, MqttHandle, MqttRunner, QoS, TryPublishError,
};
use rustyfarian_esp_hal_network::wifi::{AsyncWifiHandle, WiFiConfig, WiFiConfigExt, WiFiManager};
use static_cell::StaticCell;

esp_bootloader_esp_idf::esp_app_desc!();

const RECONNECT_DELAY_MS: u64 = 500;
const CONNECT_BACKOFF_MS: u64 = 5000;
const TICK_SECS: u64 = 5;
const HEAP_LOG_SECS: u64 = 60;

const SSID: &str = match option_env!("WIFI_SSID") {
    Some(s) => s,
    None => "",
};
// No literal fallback: an unset secret yields the `&str` type default (`""`),
// keeping CodeQL's hard-coded-credential query source-free.
fn wifi_password() -> &'static str {
    option_env!("WIFI_PASS").unwrap_or_default()
}
fn mqtt_password() -> Option<&'static str> {
    option_env!("MQTT_PASS")
}

const MQTT_HOST: &str = match option_env!("MQTT_HOST") {
    Some(s) => s,
    None => "",
};
const MQTT_PORT: u16 = parse_port(match option_env!("MQTT_PORT") {
    Some(s) => s,
    None => "1883",
});
const MQTT_CLIENT_ID: &str = match option_env!("MQTT_CLIENT_ID") {
    Some(s) => s,
    None => "hal-s3-mqtt",
};
const MQTT_USER: Option<&str> = option_env!("MQTT_USER");
const TOPIC_PREFIX: &str = match option_env!("MQTT_TOPIC_PREFIX") {
    Some(s) => s,
    None => "rustyfarian/hal-mqtt",
};

/// Parses a decimal port at compile time; a malformed value falls back to 1883.
const fn parse_port(s: &str) -> u16 {
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut value: u32 = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b < b'0' || b > b'9' || value > 6553 {
            return 1883;
        }
        value = value * 10 + (b - b'0') as u32;
        i += 1;
    }
    if value == 0 || value > 65535 {
        1883
    } else {
        value as u16
    }
}

/// Builds `<prefix>/<suffix>` once into static storage so the builder can keep it.
fn static_topic(cell: &'static StaticCell<heapless::String<64>>, suffix: &str) -> &'static str {
    let mut topic = heapless::String::<64>::new();
    let _ = topic.push_str(TOPIC_PREFIX);
    let _ = topic.push('/');
    let _ = topic.push_str(suffix);
    cell.init(topic).as_str()
}

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    esp_println::logger::init_logger(log::LevelFilter::Info);

    let peripherals = esp_hal::init(esp_hal::Config::default());

    // Same 72 KiB internal-RAM heap as the S3 Wi-Fi example; the MQTT client
    // adds no heap traffic (all its buffers are static).
    esp_alloc::heap_allocator!(size: 72 * 1024);

    // The firmware owns the scheduler (ADR 021): start it here, once per boot,
    // as a firmware that may boot without Wi-Fi would, so its embassy timers
    // work whether or not Wi-Fi comes up afterwards.
    let timg0 = TimerGroup::new(peripherals.TIMG0);
    esp_rtos::start(timg0.timer0, peripherals.FROM_CPU_INTR0);

    println!("Initializing Wi-Fi (async, scheduler already running)...");
    let config =
        WiFiConfig::new(SSID, wifi_password()).with_wifi_scheduler_running(peripherals.WIFI);
    let handle = match WiFiManager::init_async(config) {
        Ok(h) => h,
        Err(e) => {
            println!("FATAL: Wi-Fi init failed: {}", e);
            loop {}
        }
    };
    let AsyncWifiHandle {
        controller,
        stack,
        runner,
    } = handle;
    spawner.spawn(wifi_task(controller).unwrap());
    spawner.spawn(net_task(runner).unwrap());

    println!("Waiting for DHCPv4 lease...");
    stack.wait_config_up().await;
    let v4 = stack
        .config_v4()
        .expect("stack reports config up but has no IPv4 config");
    println!(
        "Wi-Fi connected — IP: {}  gateway: {:?}",
        v4.address, v4.gateway
    );

    static STATUS_TOPIC: StaticCell<heapless::String<64>> = StaticCell::new();
    static CMD_TOPIC: StaticCell<heapless::String<64>> = StaticCell::new();
    static TICK_TOPIC: StaticCell<heapless::String<64>> = StaticCell::new();
    let status_topic = static_topic(&STATUS_TOPIC, "status");
    let cmd_topic = static_topic(&CMD_TOPIC, "cmd");
    let tick_topic = static_topic(&TICK_TOPIC, "tick");

    let lwt = LwtConfig::new(status_topic, b"offline", QoS::AtLeastOnce, true);
    let mut mqtt_config = MqttConfig::new(MQTT_HOST, MQTT_PORT, MQTT_CLIENT_ID)
        .with_keep_alive(30)
        .with_lwt(lwt);
    if let Some(user) = MQTT_USER {
        mqtt_config = match mqtt_password() {
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
        Err(e) => {
            println!("FATAL: MQTT build failed: {}", e);
            loop {}
        }
    };
    spawner.spawn(mqtt_task(mqtt_runner).unwrap());
    spawner.spawn(rx_task(mqtt).unwrap());

    println!(
        "[example] topics: status={} cmd={} tick={}",
        status_topic, cmd_topic, tick_topic
    );

    let mut tick: u32 = 0;
    let mut last_state = mqtt.state();
    let mut was_connected = false;
    let mut secs_since_heap_log: u64 = 0;
    loop {
        Timer::after(Duration::from_secs(TICK_SECS)).await;
        tick += 1;
        secs_since_heap_log += TICK_SECS;

        let state = mqtt.state();
        if state != last_state {
            println!("[example] mqtt state {:?} -> {:?}", last_state, state);
            last_state = state;
        }
        if mqtt.is_connected() && !was_connected {
            println!(
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
                tick as u64 * TICK_SECS,
                esp_alloc::HEAP.free()
            ),
        );
        match mqtt.try_publish(tick_topic, payload.as_bytes()) {
            Ok(()) => println!("[example] tick #{} queued", tick),
            Err(TryPublishError::WouldBlock) => {
                println!("[example] tick #{} would block (mqtt {:?})", tick, state)
            }
            Err(TryPublishError::Other(e)) => println!("[example] tick #{} failed: {}", tick, e),
        }

        if secs_since_heap_log >= HEAP_LOG_SECS {
            secs_since_heap_log = 0;
            println!(
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
            Ok(text) => println!(
                "[example] rx topic={} payload={} retained={}",
                msg.topic.as_str(),
                text,
                msg.retained
            ),
            Err(_) => println!(
                "[example] rx topic={} payload={} bytes (binary)",
                msg.topic.as_str(),
                msg.payload.len()
            ),
        }
    }
}

// Handles both the initial association and any subsequent reconnects.
#[embassy_executor::task]
async fn wifi_task(mut controller: WifiController<'static>) {
    let mut cycle: u32 = 0;
    loop {
        match controller.connect_async().await {
            Ok(_) => {
                cycle += 1;
                println!("Wi-Fi link up #{cycle}");
                let _ = controller.wait_for_disconnect_async().await;
                println!("Wi-Fi disconnected #{cycle} — reconnecting...");
                Timer::after(Duration::from_millis(RECONNECT_DELAY_MS)).await;
            }
            Err(e) => {
                println!("connect failed: {:?}", e);
                Timer::after(Duration::from_millis(CONNECT_BACKOFF_MS)).await;
            }
        }
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, Interface>) -> ! {
    runner.run().await
}
