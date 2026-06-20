//! MQTT publisher with button trigger and OLED status display for ESP32-C3 Super Mini.
//!
//! Publishes `"pressed"` to `c3-button/events` whenever the push button is pressed.
//! The SSD1306 OLED shows Wi-Fi status, MQTT connection state, and cumulative press count.
//!
//! Designed to pair with `idf_c3_mqtt_led_grid`: button presses on this device
//! trigger LED toggles on the other.
//!
//! # Hardware
//!
//! | Component | GPIO |
//! |-----------|------|
//! | B3F push button (other leg to 3V3) | 4 |
//! | SSD1306 128×64 OLED SDA | 8 |
//! | SSD1306 128×64 OLED SCL | 9 |
//!
//! # Environment variables (set at compile time)
//!
//! | Variable | Default | Description |
//! |---|---|---|
//! | `WIFI_SSID` | `""` | Wi-Fi network name |
//! | `WIFI_PASS` | `""` | Wi-Fi password |
//! | `MQTT_HOST` | (required) | MQTT broker IP or hostname |
//! | `MQTT_CLIENT_ID` | `c3-button` | Unique device identifier |
//!
//! # Build and flash
//!
//! ```sh
//! WIFI_SSID="MyNetwork" WIFI_PASS="secret" MQTT_HOST=192.168.1.100 \
//!   just build-example idf_c3_mqtt_button_oled
//! just flash idf_c3_mqtt_button_oled
//! ```

use embedded_graphics::{
    mono_font::{ascii::FONT_6X10, MonoTextStyleBuilder},
    pixelcolor::BinaryColor,
    prelude::*,
    text::Text,
};
use esp_idf_svc::{
    eventloop::EspSystemEventLoop,
    hal::{
        gpio::{PinDriver, Pull},
        i2c::{I2cConfig, I2cDriver},
        peripherals::Peripherals,
        units::Hertz,
    },
    nvs::EspDefaultNvsPartition,
};
use rustyfarian_esp_idf_network::mqtt::{MqttBuilder, MqttConfig};
use rustyfarian_esp_idf_network::wifi::{WiFiConfig, WiFiManager};
use ssd1306::{prelude::*, I2CDisplayInterface, Ssd1306};
use std::time::{Duration, Instant};

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
    None => "c3-button",
};
const EVENTS_TOPIC: &str = "c3-button/events";

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    if MQTT_HOST.is_empty() {
        anyhow::bail!("MQTT_HOST not configured — set it at build time");
    }

    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    // ── OLED display (SDA=GPIO8, SCL=GPIO9) ──────────────────────────────
    let i2c = I2cDriver::new(
        peripherals.i2c0,
        peripherals.pins.gpio8,
        peripherals.pins.gpio9,
        &I2cConfig::new().baudrate(Hertz(400_000)),
    )?;
    let mut display = Ssd1306::new(
        I2CDisplayInterface::new(i2c),
        DisplaySize128x64,
        DisplayRotation::Rotate180,
    )
    .into_buffered_graphics_mode();
    display
        .init()
        .map_err(|e| anyhow::anyhow!("OLED init: {:?}", e))?;

    let text_style = MonoTextStyleBuilder::new()
        .font(&FONT_6X10)
        .text_color(BinaryColor::On)
        .build();

    let _ = display.clear(BinaryColor::Off);
    let _ = Text::new("WiFi connecting...", Point::new(0, 12), text_style).draw(&mut display);
    let _ = display.flush();

    // ── Button (GPIO4, active high with internal pull-down; other leg to 3V3) ──
    let button = PinDriver::input(peripherals.pins.gpio4, Pull::Down)?;

    // ── Wi-Fi (blocking connect) ──────────────────────────────────────────
    let wifi = WiFiManager::new_without_led(
        peripherals.modem,
        sys_loop,
        Some(nvs),
        WiFiConfig::new(WIFI_SSID, WIFI_PASS),
    )?;
    let ip_str = match wifi.get_ip(10_000)? {
        Some(ip) => {
            log::info!("Wi-Fi connected — {}", ip);
            ip.to_string()
        }
        None => {
            log::warn!("Wi-Fi connected but no IP yet");
            "no IP yet".to_string()
        }
    };

    let _ = display.clear(BinaryColor::Off);
    let _ = Text::new("WiFi OK", Point::new(0, 12), text_style).draw(&mut display);
    let _ = Text::new(&ip_str, Point::new(0, 24), text_style).draw(&mut display);
    let _ = Text::new("MQTT connecting...", Point::new(0, 36), text_style).draw(&mut display);
    let _ = display.flush();

    // ── MQTT (non-blocking) ───────────────────────────────────────────────
    let handle = MqttBuilder::new(MqttConfig::new(MQTT_HOST, 1883, MQTT_CLIENT_ID))
        .on_connect(|_client, _is_clean| {
            log::info!("[mqtt] connected");
            Ok(())
        })
        .on_disconnect(|| log::warn!("[mqtt] disconnected"))
        .build()?;

    log::info!(
        "ready — press GPIO4 button to publish to '{}'",
        EVENTS_TOPIC
    );

    let mut press_count: u32 = 0;
    let mut last_periodic = Instant::now() - Duration::from_secs(10);
    let mut last_display = Instant::now();
    // Stability filter: require 8 consecutive HIGH samples (8 × 20 ms = 160 ms) before
    // registering a press. A floating/bouncing pin won't sustain HIGH that long.
    // Counter resets to 0 on any LOW reading, requiring full release before next press.
    let mut stable_high: u8 = 0;
    const STABLE_THRESHOLD: u8 = 8;

    loop {
        if button.is_high() {
            stable_high = stable_high.saturating_add(1);
        } else {
            stable_high = 0;
        }

        if stable_high == STABLE_THRESHOLD {
            press_count += 1;
            if handle.is_connected() {
                let payload = format!("pressed #{}", press_count);
                match handle.publish(EVENTS_TOPIC, &payload) {
                    Ok(()) => log::info!("[btn] published: {}", payload),
                    Err(e) => log::warn!("[btn] publish failed: {:#}", e),
                }
            } else {
                log::warn!("[btn] press #{} dropped — MQTT not connected", press_count);
            }
        }

        // Periodic publish every 10 s
        if last_periodic.elapsed() >= Duration::from_secs(10) {
            last_periodic = Instant::now();
            if handle.is_connected() {
                let payload = format!("heartbeat presses={}", press_count);
                match handle.publish(EVENTS_TOPIC, &payload) {
                    Ok(()) => log::info!("[periodic] published: {}", payload),
                    Err(e) => log::warn!("[periodic] publish failed: {:#}", e),
                }
            }
        }

        // Refresh OLED at ~5 Hz
        if last_display.elapsed() > Duration::from_millis(200) {
            last_display = Instant::now();
            let mqtt_line = if handle.is_connected() {
                "MQTT: connected"
            } else {
                "MQTT: ---"
            };
            let btn_line = format!("Presses: {}", press_count);
            let _ = display.clear(BinaryColor::Off);
            let _ = Text::new("WiFi OK", Point::new(0, 12), text_style).draw(&mut display);
            let _ = Text::new(&ip_str, Point::new(0, 24), text_style).draw(&mut display);
            let _ = Text::new(mqtt_line, Point::new(0, 36), text_style).draw(&mut display);
            let _ = Text::new(&btn_line, Point::new(0, 48), text_style).draw(&mut display);
            let _ = display.flush();
        }

        std::thread::sleep(Duration::from_millis(20));
    }
}
