//! Async Wi-Fi connect with WS2812 RGB LED feedback for ESP32-C6.
//!
//! Spawns a `led_task` that pulses the onboard WS2812 LED (GPIO8) blue
//! via [`PulseEffect`] while Wi-Fi is connecting, then holds a steady
//! dim green once an IP address is acquired.
//!
//! LED phases:
//!
//! * **Connecting** — blue pulse (~1.4 s cycle via `PulseEffect`)
//! * **Connected** — steady dim green (0, 20, 0)
//! * **Disconnected** — resumes blue pulse automatically
//!
//! `WIFI_SSID` and `WIFI_PASS` must be set as environment variables **at build
//! time**. Requires the `embassy` and `rustyfarian-esp-hal-ws2812` features.
//!
//! # Build and flash
//!
//! ```sh
//! WIFI_SSID="MyNetwork" WIFI_PASS="secret" just build-example hal_c6_connect_async_led
//! just flash hal_c6_connect_async_led
//! ```

#![no_std]
#![no_main]

extern crate alloc;

use core::sync::atomic::{AtomicBool, Ordering};

use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_hal::gpio::Level;
use esp_hal::rmt::{Rmt, TxChannelConfig, TxChannelCreator};
use esp_hal::time::Rate;
use esp_hal::Blocking;
use esp_println::println;
use esp_radio::wifi::{WifiController, WifiDevice, WifiEvent};
use led_effects::{PulseEffect, StatusLed};
use rgb::RGB8;
use rustyfarian_esp_hal_wifi::{AsyncWifiHandle, WiFiConfig, WiFiConfigExt, WiFiManager};
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

/// Shared flag: `false` = connecting (pulse), `true` = connected (steady green).
static CONNECTED: AtomicBool = AtomicBool::new(false);

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    esp_println::logger::init_logger(log::LevelFilter::Info);

    let peripherals = esp_hal::init(esp_hal::Config::default());

    // ESP32-C6 requires two heap regions: reclaimed IRAM for Wi-Fi DMA
    // buffers, and regular DRAM for general allocations.
    // (ESP32-C3 has contiguous SRAM and uses a single 72 KiB region instead.)
    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);

    // Set up the onboard WS2812 RGB LED on GPIO8 via RMT.
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
    let led = Ws2812Rmt::<Blocking, N>::new(channel);
    println!("LED ready");

    // WiFiManager::init_async() is synchronous (heap + RTOS + radio init).
    // In embassy's cooperative model the LED task cannot run until init
    // returns and main hits its first .await. Radio init is fast (~100 ms)
    // once the heap is correctly configured with reclaimed IRAM.
    println!("Initializing Wi-Fi...");

    let config = WiFiConfig::new(SSID, PASSWORD).with_peripherals(
        peripherals.TIMG0,
        peripherals.SW_INTERRUPT,
        peripherals.WIFI,
    );

    let handle = match WiFiManager::init_async(config) {
        Ok(h) => h,
        Err(e) => {
            println!("FATAL: Wi-Fi init failed: {}", e);
            loop {}
        }
    };
    println!("Wi-Fi radio ready");

    let AsyncWifiHandle {
        controller,
        stack,
        runner,
    } = handle;

    spawner.must_spawn(wifi_task(controller));
    spawner.must_spawn(net_task(runner));
    spawner.must_spawn(led_task(led));

    println!("Waiting for DHCPv4 lease (LED pulsing blue)...");
    stack.wait_config_up().await;
    let v4 = stack
        .config_v4()
        .expect("stack reports config up but has no IPv4 config");
    println!(
        "Wi-Fi connected — IP: {}  gateway: {:?}",
        v4.address, v4.gateway
    );

    CONNECTED.store(true, Ordering::Relaxed);

    loop {
        Timer::after(Duration::from_secs(10)).await;
    }
}

/// Pulses the WS2812 LED blue while connecting; holds dim green once connected.
///
/// Uses [`PulseEffect`] for smooth brightness animation at ~20 fps.
/// When `CONNECTED` transitions back to `false` (e.g. after disconnect),
/// the blue pulse resumes automatically.
#[embassy_executor::task]
async fn led_task(mut led: Ws2812Rmt<'static, Blocking, N>) {
    let mut pulse = PulseEffect::new();

    loop {
        if CONNECTED.load(Ordering::Relaxed) {
            let _ = led.set_color(RGB8::new(0, 20, 0));
            Timer::after(Duration::from_millis(100)).await;
        } else {
            let _ = led.set_color(pulse.update((0, 0, 255)));
            Timer::after(Duration::from_millis(50)).await;
        }
    }
}

// Initial association is started by `WiFiManager::init_async`; this task
// only handles reconnection after a disconnect event.
#[embassy_executor::task]
async fn wifi_task(mut controller: WifiController<'static>) {
    loop {
        controller.wait_for_event(WifiEvent::StaDisconnected).await;
        println!("Wi-Fi disconnected — attempting to reconnect");
        CONNECTED.store(false, Ordering::Relaxed);
        Timer::after(Duration::from_millis(500)).await;
        if let Err(e) = controller.connect() {
            println!("reconnect failed: {:?}", e);
        }
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, WifiDevice<'static>>) -> ! {
    runner.run().await
}
