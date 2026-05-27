//! MQTT subscriber with LED-grid feedback for ESP32-C3 Super Mini.
//!
//! Subscribes to `c3-button/events` and toggles two LEDs on every incoming message.
//! A third LED stays on whenever MQTT is connected.
//!
//! Designed to pair with `idf_c3_mqtt_button_oled`: pressing the button on that
//! device publishes a message that triggers the LED toggle here.
//!
//! # Hardware
//!
//! LEDs wired with resistors, cathodes to GND (active-high: GPIO high = LED on).
//!
//! | GPIO | Grid | Role |
//! |------|------|------|
//! | 0    | TL   | Connection indicator — on while MQTT is connected |
//! | 1    | TC   | Toggles on odd-numbered messages |
//! | 3    | TR   | Toggles on even-numbered messages (alternates with GPIO1) |
//!
//! # Environment variables (set at compile time)
//!
//! | Variable | Default | Description |
//! |---|---|---|
//! | `WIFI_SSID` | `""` | Wi-Fi network name |
//! | `WIFI_PASS` | `""` | Wi-Fi password |
//! | `MQTT_HOST` | (required) | MQTT broker IP or hostname |
//! | `MQTT_CLIENT_ID` | `c3-leds` | Unique device identifier |
//!
//! # Build and flash
//!
//! ```sh
//! WIFI_SSID="MyNetwork" WIFI_PASS="secret" MQTT_HOST=192.168.1.100 \
//!   just build-example idf_c3_mqtt_led_grid
//! just flash idf_c3_mqtt_led_grid
//! ```

use esp_idf_svc::{
    eventloop::EspSystemEventLoop,
    hal::{gpio::PinDriver, peripherals::Peripherals},
    mqtt::client::{EspMqttClient, QoS},
    nvs::EspDefaultNvsPartition,
};
use rustyfarian_esp_idf_mqtt::{MqttBuilder, MqttConfig};
use rustyfarian_esp_idf_wifi::{WiFiConfig, WiFiManager};
use std::{
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc,
    },
    time::Duration,
};

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
    None => "c3-leds",
};
const SUBSCRIBE_TOPIC: &str = "c3-button/events";

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    if MQTT_HOST.is_empty() {
        anyhow::bail!("MQTT_HOST not configured — set it at build time");
    }

    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    // ── LEDs (active high: set_high = ON, set_low = OFF) ──────────────────
    let mut led_conn = PinDriver::output(peripherals.pins.gpio0)?; // TL: connection
    let mut led_a = PinDriver::output(peripherals.pins.gpio1)?; // TC: toggle A
    let mut led_b = PinDriver::output(peripherals.pins.gpio3)?; // TR: toggle B
    led_conn.set_low()?;
    led_a.set_low()?;
    led_b.set_low()?;

    // ── Wi-Fi ─────────────────────────────────────────────────────────────
    let wifi = WiFiManager::new_without_led(
        peripherals.modem,
        sys_loop,
        Some(nvs),
        WiFiConfig::new(WIFI_SSID, WIFI_PASS),
    )?;
    match wifi.get_ip(10_000)? {
        Some(ip) => log::info!("Wi-Fi connected — {}", ip),
        None => log::warn!("Wi-Fi connected but no IP yet"),
    }

    // ── MQTT ──────────────────────────────────────────────────────────────
    let msg_count = Arc::new(AtomicU32::new(0));
    let msg_count_cb = Arc::clone(&msg_count);

    let handle = MqttBuilder::new(MqttConfig::new(MQTT_HOST, 1883, MQTT_CLIENT_ID))
        .subscribe(SUBSCRIBE_TOPIC, QoS::AtLeastOnce)
        .on_connect(|_client: &mut EspMqttClient<'_>, _is_clean: bool| {
            log::info!("[mqtt] connected");
            Ok(())
        })
        .on_disconnect(|| log::warn!("[mqtt] disconnected"))
        .on_message(move |topic, _payload| {
            let n = msg_count_cb.fetch_add(1, Ordering::Relaxed) + 1;
            log::info!("[mqtt] message #{} on '{}'", n, topic);
        })
        .build()?;

    log::info!("subscribed to '{}' — waiting for messages", SUBSCRIBE_TOPIC);

    let mut last_count: u32 = 0;

    loop {
        // Connection indicator: on while MQTT is live
        if handle.is_connected() {
            led_conn.set_high()?;
        } else {
            led_conn.set_low()?;
        }

        // Toggle LEDs ping-pong on each new message
        let count = msg_count.load(Ordering::Relaxed);
        if count != last_count {
            last_count = count;
            let a_on = !count.is_multiple_of(2);
            if a_on {
                led_a.set_high()?;
                led_b.set_low()?;
            } else {
                led_a.set_low()?;
                led_b.set_high()?;
            }
            log::info!("[led] msg #{}: a={}, b={}", count, a_on, !a_on);
        }

        std::thread::sleep(Duration::from_millis(50));
    }
}
