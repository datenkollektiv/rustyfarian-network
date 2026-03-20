//! Raw WiFi init test — matches upstream esp-radio v0.17 dhcp example.
//! Adds Rng::new() before esp_radio::init() to initialize the hardware RNG,
//! which the WiFi firmware blob requires.

#![no_std]
#![no_main]

extern crate alloc;

use esp_backtrace as _;
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::main;
use esp_hal::rng::Rng;
use esp_hal::timer::timg::TimerGroup;
use esp_println::println;

esp_bootloader_esp_idf::esp_app_desc!();

#[main]
fn main() -> ! {
    esp_println::logger::init_logger(log::LevelFilter::Info);
    let peripherals = esp_hal::init(esp_hal::Config::default());
    println!("hal_c6_wifi_raw: init done");

    esp_alloc::heap_allocator!(#[esp_hal::ram(reclaimed)] size: 64 * 1024);
    esp_alloc::heap_allocator!(size: 36 * 1024);
    println!("heap ready");

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_int = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);
    println!("rtos started");

    // Initialize the hardware RNG — the WiFi firmware blob uses it internally.
    let _rng = Rng::new();
    println!("rng ready");

    let esp_radio_ctrl = esp_radio::init().unwrap();
    println!("radio init done");

    println!("calling wifi::new...");
    let (_controller, _interfaces) =
        esp_radio::wifi::new(&esp_radio_ctrl, peripherals.WIFI, Default::default()).unwrap();
    println!("wifi::new done!");

    loop {}
}
