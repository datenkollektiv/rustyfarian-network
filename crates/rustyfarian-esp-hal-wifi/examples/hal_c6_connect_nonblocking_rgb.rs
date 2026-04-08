//! Non-blocking Wi-Fi connect example for ESP32-C6 with onboard WS2812 RGB LED.
//!
//! Bare-metal (no_std) counterpart of `idf_c6_connect_nonblocking_rgb`.
//! The example owns the LED and drives the animation directly while polling
//! [`WiFiManager::is_connected`] — no hand-off gap between library and caller:
//!
//! - Blue pulse while associating
//! - Smooth blue-to-green transition on success
//! - Dim green while waiting for DHCP
//! - Fade green to black after IP assignment (power saving)
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
//! WIFI_SSID="MyNetwork" WIFI_PASS="secret" just build-example hal_c6_connect_nonblocking_rgb
//! just flash hal_c6_connect_nonblocking_rgb
//! ```

#![no_std]
#![no_main]

extern crate alloc;

use esp_backtrace as _;
use esp_hal::delay::Delay;
use esp_hal::gpio::Level;
use esp_hal::main;
use esp_hal::rmt::{Rmt, TxChannelConfig, TxChannelCreator};
use esp_hal::time::Rate;
use esp_hal::Blocking;
use esp_println::println;
use led_effects::{PulseEffect, StatusLed};
use rgb::RGB8;
use rustyfarian_esp_hal_wifi::{WiFiConfig, WiFiConfigExt, WiFiManager, WifiDriver, WifiError};
use rustyfarian_esp_hal_ws2812::{buffer_size, Ws2812Rmt, RMT_CLK_DIV};

esp_bootloader_esp_idf::esp_app_desc!();

const SSID: &str = match option_env!("WIFI_SSID") {
    Some(s) => s,
    None => "",
};
const PASSWORD: &str = match option_env!("WIFI_PASS") {
    Some(s) => s,
    None => "",
};

const NUM_LEDS: usize = 1;
const N: usize = buffer_size(NUM_LEDS);
const FRAME_MS: u32 = 50;
const TIMEOUT_MS: u64 = 30_000;

fn run() -> Result<(), WifiError> {
    let peripherals = esp_hal::init(esp_hal::Config::default());
    println!(
        "hal_c6_connect_nonblocking_rgb starting (SSID len={})",
        SSID.len()
    );
    let delay = Delay::new();

    // ESP32-C6 requires two heap regions: reclaimed IRAM for Wi-Fi DMA + DRAM.
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    // Init Wi-Fi first — RMT can be set up after.
    let wifi_config = WiFiConfig::new(SSID, PASSWORD).with_peripherals(
        peripherals.TIMG0,
        peripherals.SW_INTERRUPT,
        peripherals.WIFI,
    );
    let mut wifi = WiFiManager::init(wifi_config)?;
    println!("Wi-Fi connect initiated");

    // Set up the onboard WS2812 RGB LED on GPIO8 (after WiFi init).
    let rmt = Rmt::new(peripherals.RMT, Rate::from_mhz(80)).unwrap();
    let rmt_config = TxChannelConfig::default()
        .with_clk_divider(RMT_CLK_DIV)
        .with_idle_output_level(Level::Low)
        .with_idle_output(true)
        .with_carrier_modulation(false);
    let channel = rmt
        .channel0
        .configure_tx(peripherals.GPIO8, rmt_config)
        .unwrap();
    let mut led = Ws2812Rmt::<Blocking, N>::new(channel);
    let mut pulse = PulseEffect::new();
    println!("LED ready");

    // Phase 1: blue pulse while waiting for L2 association.
    let start = esp_hal::time::Instant::now();
    loop {
        if wifi.is_connected()? {
            break;
        }
        let elapsed_ms = (esp_hal::time::Instant::now() - start).as_millis();
        if elapsed_ms >= TIMEOUT_MS {
            println!("Wi-Fi association timeout");
            for _ in 0..20 {
                let _ = led.set_color(pulse.update((255, 0, 0)));
                delay.delay_millis(FRAME_MS);
            }
            return Err(WifiError::ConnectFailed);
        }
        let _ = led.set_color(pulse.update((0, 0, 255)));
        delay.delay_millis(FRAME_MS);
    }
    println!("Wi-Fi associated");

    // Phase 2: seamless blue-to-green transition (same pulse, no reset).
    let transition_frames: u16 = 40;
    for i in 0..transition_frames {
        let t = i * 255 / transition_frames;
        let blue = (255 - t) as u8;
        let green = t as u8;
        let _ = led.set_color(pulse.update((0, green, blue)));
        delay.delay_millis(FRAME_MS);
    }

    // Phase 3: steady dim green while waiting for DHCP.
    let _ = led.set_color(RGB8::new(0, 20, 0));
    let ip = wifi.wait_connected(30_000)?;
    println!("Wi-Fi connected — IP: {}", ip);

    delay.delay_millis(5_000);

    // Phase 4: fade green to black.
    for brightness in (0..=20).rev() {
        let _ = led.set_color(RGB8::new(0, brightness, 0));
        delay.delay_millis(FRAME_MS);
    }
    let _ = led.set_color(RGB8::new(0, 0, 0));
    println!("LED off — power saving mode");

    Ok(())
}

#[main]
fn main() -> ! {
    esp_println::logger::init_logger_from_env();
    if let Err(e) = run() {
        println!("FATAL: {}", e);
    }
    loop {}
}
