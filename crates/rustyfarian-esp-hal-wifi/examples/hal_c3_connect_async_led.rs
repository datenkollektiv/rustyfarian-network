//! Async Wi-Fi connect with onboard LED feedback for ESP32-C3 Super Mini.
//!
//! Extends `hal_c3_connect_async` with a spawned `led_task` that blinks
//! GPIO8 (the onboard active-low LED) while Wi-Fi is connecting, then
//! holds it steady once an IP address is acquired.
//!
//! The LED pattern is driven by a shared `AtomicBool` flag:
//!
//! * `false` (default) — LED blinks at ~2 Hz (connecting)
//! * `true` — LED stays on (connected)
//!
//! This pattern is reusable for any async application that needs status
//! feedback during a multi-step boot sequence.
//!
//! `WIFI_SSID` and `WIFI_PASS` must be set as environment variables **at build
//! time**. Requires the `embassy` Cargo feature.
//!
//! # Build and flash
//!
//! ```sh
//! WIFI_SSID="MyNetwork" WIFI_PASS="secret" just build-example hal_c3_connect_async_led
//! just flash hal_c3_connect_async_led
//! ```

#![no_std]
#![no_main]

extern crate alloc;

use core::sync::atomic::{AtomicBool, Ordering};

use embassy_executor::Spawner;
use embassy_time::{Duration, Timer};
use esp_backtrace as _;
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_println::println;
use esp_radio::wifi::{Interface, WifiController};
use rustyfarian_esp_hal_wifi::{AsyncWifiHandle, WiFiConfig, WiFiConfigExt, WiFiManager};

esp_bootloader_esp_idf::esp_app_desc!();

const SSID: &str = match option_env!("WIFI_SSID") {
    Some(s) => s,
    None => "",
};
const PASSWORD: &str = match option_env!("WIFI_PASS") {
    Some(s) => s,
    None => "",
};

/// Shared flag: `false` = no IPv4 config (LED blinks), `true` = config up (LED steady).
/// Owned by `link_status_task`, which watches `embassy_net::Stack::wait_config_up`
/// and `wait_config_down` so the LED tracks the current DHCP state — including
/// after the first disconnect/reconnect cycle.
static CONNECTED: AtomicBool = AtomicBool::new(false);

#[esp_rtos::main]
async fn main(spawner: Spawner) {
    esp_println::logger::init_logger(log::LevelFilter::Info);

    let peripherals = esp_hal::init(esp_hal::Config::default());

    esp_alloc::heap_allocator!(size: 72 * 1024);

    // GPIO8 is the onboard LED on ESP32-C3 Super Mini (active-low).
    // Start with LED off (pin high).
    let led = Output::new(peripherals.GPIO8, Level::High, OutputConfig::default());

    println!("Initializing Wi-Fi (async + LED)...");

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

    let AsyncWifiHandle {
        controller,
        stack,
        runner,
    } = handle;

    spawner.spawn(wifi_task(controller).unwrap());
    spawner.spawn(net_task(runner).unwrap());
    spawner.spawn(led_task(led).unwrap());
    spawner.spawn(link_status_task(stack).unwrap());

    println!("Waiting for DHCPv4 lease (LED blinking)...");
    stack.wait_config_up().await;
    let v4 = stack
        .config_v4()
        .expect("stack reports config up but has no IPv4 config");
    println!(
        "Wi-Fi connected — IP: {}  gateway: {:?}",
        v4.address, v4.gateway
    );

    loop {
        Timer::after(Duration::from_secs(10)).await;
    }
}

/// Owns the `CONNECTED` flag.  Toggling it on association events alone is not
/// enough — a brief disconnect/reconnect race can leave the controller saying
/// "connected" while the embassy-net stack is still waiting for a new DHCP
/// lease, so the user-visible signal is whether the IP-layer config is up.
/// Watches both edges (`wait_config_up` and `wait_config_down`) and updates
/// the flag every time DHCP comes back up after a drop, not just on first boot.
#[embassy_executor::task]
async fn link_status_task(stack: embassy_net::Stack<'static>) {
    loop {
        stack.wait_config_up().await;
        CONNECTED.store(true, Ordering::Relaxed);
        stack.wait_config_down().await;
        CONNECTED.store(false, Ordering::Relaxed);
    }
}

/// Blinks the onboard LED while connecting; holds steady once connected.
///
/// Uses active-low logic: `set_low()` = LED on, `set_high()` = LED off.
/// Blink rate is ~2 Hz (250 ms on, 250 ms off) for clear visibility.
///
/// When `CONNECTED` transitions back to `false` (e.g. after a disconnect
/// detected by `wifi_task`), the LED resumes blinking automatically.
#[embassy_executor::task]
async fn led_task(mut led: Output<'static>) {
    loop {
        if CONNECTED.load(Ordering::Relaxed) {
            // Connected: LED on (steady)
            led.set_low();
            Timer::after(Duration::from_millis(100)).await;
        } else {
            // Connecting: blink at ~2 Hz
            led.set_low();
            Timer::after(Duration::from_millis(250)).await;
            led.set_high();
            Timer::after(Duration::from_millis(250)).await;
        }
    }
}

// Initial association is started by `WiFiManager::init_async`; this task
// only handles reconnection after a disconnect event.
#[embassy_executor::task]
async fn wifi_task(mut controller: WifiController<'static>) {
    // `wait_for_disconnect_async` and `connect_async` replace the
    // sync `wait_for_event` + `connect` pair removed in esp-radio 0.18.
    // The LED `CONNECTED` flag is owned by `link_status_task` watching
    // `embassy_net::Stack` config-up/config-down edges — not toggled here,
    // because a successful L2 reconnect does not yet imply a new DHCP lease.
    loop {
        let _ = controller.wait_for_disconnect_async().await;
        println!("Wi-Fi disconnected — attempting to reconnect");
        Timer::after(Duration::from_millis(500)).await;
        if let Err(e) = controller.connect_async().await {
            println!("reconnect failed: {:?}", e);
        }
    }
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, Interface<'static>>) -> ! {
    runner.run().await
}
