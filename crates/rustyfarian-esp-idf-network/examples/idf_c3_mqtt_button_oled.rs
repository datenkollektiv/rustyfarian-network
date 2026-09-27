//! MQTT publisher with button trigger and optional OLED status display for ESP32-C3 Super Mini.
//!
//! Publishes `"pressed"` to `c3-button/events` whenever the push button is pressed.
//! If an SSD1306 OLED answers on I2C at boot it shows Wi-Fi status, MQTT connection state,
//! and the cumulative press count; if nothing answers, the example logs a warning once and
//! runs headless with the same behaviour on the serial console.
//!
//! Designed to pair with `idf_c3_mqtt_led_grid`: button presses on this device
//! trigger LED toggles on the other.
//!
//! # Hardware
//!
//! | Component | GPIO |
//! |-----------|------|
//! | B3F push button (other leg to 3V3) | 4 |
//! | SSD1306 128×64 OLED SDA (optional) | 8 |
//! | SSD1306 128×64 OLED SCL (optional) | 9 |
//!
//! # Environment variables (set at compile time)
//!
//! | Variable | Default | Description |
//! |---|---|---|
//! | `WIFI_SSID` | `""` | Wi-Fi network name |
//! | `WIFI_PASS` | `""` | Wi-Fi password |
//! | `MQTT_HOST` | (required) | MQTT broker IP or hostname |
//! | `MQTT_PORT` | `1883` | MQTT broker port |
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
use rustyfarian_esp_idf_network::mqtt::MqttBuilder;
use rustyfarian_esp_idf_network::wifi::{WiFiConfig, WiFiManager};
use ssd1306::{mode::BufferedGraphicsMode, prelude::*, I2CDisplayInterface, Ssd1306};
use std::time::{Duration, Instant};

#[path = "common/env.rs"]
mod env;

const EVENTS_TOPIC: &str = "c3-button/events";

type Oled = Ssd1306<
    I2CInterface<I2cDriver<'static>>,
    DisplaySize128x64,
    BufferedGraphicsMode<DisplaySize128x64>,
>;

/// Redraws the OLED with one text line per entry, 12 px apart; a no-op when no
/// display was detected at boot.
fn show(display: &mut Option<Oled>, lines: &[&str]) {
    let Some(display) = display.as_mut() else {
        return;
    };
    let style = MonoTextStyleBuilder::new()
        .font(&FONT_6X10)
        .text_color(BinaryColor::On)
        .build();
    let _ = display.clear(BinaryColor::Off);
    for (i, line) in lines.iter().enumerate() {
        let y = 12 * (i as i32 + 1);
        let _ = Text::new(line, Point::new(0, y), style).draw(display);
    }
    let _ = display.flush();
}

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let client_id = env::mqtt_client_id("c3-button");
    env::log_config(client_id);
    env::mqtt_host()?;

    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    // ── OLED display (SDA=GPIO8, SCL=GPIO9), optional ─────────────────────
    // `init()` fails with an I2C error when no SSD1306 answers on the bus; the
    // example then runs headless instead of aborting.
    let i2c = I2cDriver::new(
        peripherals.i2c0,
        peripherals.pins.gpio8,
        peripherals.pins.gpio9,
        &I2cConfig::new().baudrate(Hertz(400_000)),
    )?;
    let mut oled = Ssd1306::new(
        I2CDisplayInterface::new(i2c),
        DisplaySize128x64,
        DisplayRotation::Rotate180,
    )
    .into_buffered_graphics_mode();
    let mut display: Option<Oled> = match oled.init() {
        Ok(()) => Some(oled),
        Err(e) => {
            log::warn!(
                "no SSD1306 OLED detected on SDA=GPIO8/SCL=GPIO9 ({:?}) — running headless",
                e
            );
            None
        }
    };

    show(&mut display, &["WiFi connecting..."]);

    // ── Button (GPIO4, active high with internal pull-down; other leg to 3V3) ──
    let button = PinDriver::input(peripherals.pins.gpio4, Pull::Down)?;

    // ── Wi-Fi (blocking connect) ──────────────────────────────────────────
    let wifi = WiFiManager::new_without_led(
        peripherals.modem,
        sys_loop,
        Some(nvs),
        WiFiConfig::new(env::WIFI_SSID, env::wifi_pass()),
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

    show(&mut display, &["WiFi OK", &ip_str, "MQTT connecting..."]);

    // ── MQTT (non-blocking) ───────────────────────────────────────────────
    let handle = MqttBuilder::new(env::mqtt_config(client_id)?)
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

        // Refresh OLED at ~5 Hz (no-op when running headless)
        if display.is_some() && last_display.elapsed() > Duration::from_millis(200) {
            last_display = Instant::now();
            let mqtt_line = if handle.is_connected() {
                "MQTT: connected"
            } else {
                "MQTT: ---"
            };
            let btn_line = format!("Presses: {}", press_count);
            show(&mut display, &["WiFi OK", &ip_str, mqtt_line, &btn_line]);
        }

        std::thread::sleep(Duration::from_millis(20));
    }
}
