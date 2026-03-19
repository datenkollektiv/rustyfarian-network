//! Non-blocking Wi-Fi connect example for ESP32-C6 with onboard WS2812 RGB LED.
//!
//! Demonstrates [`WiFiConfig::connect_nonblocking`] with a seamless LED animation
//! driven entirely by the example — no hand-off gap between library and caller:
//!
//! - Blue pulse while associating
//! - Smooth blue-to-green transition on success (continuous pulse, no interruption)
//! - Dim green for 5 seconds, then fade to black (power saving)
//! - LED is free for other uses after fade-out
//!
//! This example uses non-blocking mode so it owns the LED throughout the entire
//! sequence.
//! WiFiManager initiates association in the background; the example polls
//! `is_connected()` while driving the animation.
//!
//! `WIFI_SSID` and `WIFI_PASS` must be set as environment variables **at build time**.
//! With [direnv](https://direnv.net/) and a populated `.envrc`, these are set automatically.
//!
//! # Components
//!
//! - ESP32-C6 development board with onboard WS2812 LED on GPIO8
//! - USB cable
//!
//! # Build and Flash
//!
//! ```sh
//! just build-example idf_c6_connect_nonblocking_rgb
//! just flash idf_c6_connect_nonblocking_rgb
//! ```

use std::thread;
use std::time::Duration;

use led_effects::{PulseEffect, StatusLed};
use rgb::RGB8;
use rustyfarian_esp_idf_wifi::{WiFiConfig, WiFiConfigExt, WiFiManager};
use rustyfarian_esp_idf_ws2812::WS2812RMT;

use esp_idf_svc::eventloop::EspSystemEventLoop;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::nvs::EspDefaultNvsPartition;

const SSID: &str = match option_env!("WIFI_SSID") {
    Some(s) => s,
    None => "",
};
const PASSWORD: &str = match option_env!("WIFI_PASS") {
    Some(s) => s,
    None => "",
};

const FRAME_MS: u64 = 50;

fn main() -> anyhow::Result<()> {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();

    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    let mut led = WS2812RMT::new(peripherals.pins.gpio8)?;
    let mut pulse = PulseEffect::new();

    let config = WiFiConfig::new(SSID, PASSWORD)
        .connect_nonblocking()
        .with_peripherals(peripherals.modem, sys_loop, Some(nvs));
    let wifi = WiFiManager::init(config)?;
    log::info!("Wi-Fi connect initiated");

    // Phase 1: blue pulse while waiting for association.
    loop {
        if wifi.is_connected()? {
            break;
        }
        led.set_color(pulse.update((0, 0, 255)))?;
        thread::sleep(Duration::from_millis(FRAME_MS));
    }
    log::info!("Wi-Fi connected");

    // Phase 2: seamless blue-to-green transition (same pulse, no reset).
    let transition_frames: u16 = 40;
    for i in 0..transition_frames {
        let t = i * 255 / transition_frames;
        let blue = (255 - t) as u8;
        let green = t as u8;
        led.set_color(pulse.update((0, green, blue)))?;
        thread::sleep(Duration::from_millis(FRAME_MS));
    }

    // Phase 3: steady dim green while waiting for DHCP.
    led.set_color(RGB8::new(0, 20, 0))?;
    let ip = wifi.wait_connected(30_000)?;
    log::info!("Connected — IP: {}", ip);

    thread::sleep(Duration::from_secs(5));

    // Phase 4: fade green to black.
    for brightness in (0..=20).rev() {
        led.set_color(RGB8::new(0, brightness, 0))?;
        thread::sleep(Duration::from_millis(FRAME_MS));
    }
    led.set_color(RGB8::new(0, 0, 0))?;
    log::info!("LED off — power saving mode");

    Ok(())
}
