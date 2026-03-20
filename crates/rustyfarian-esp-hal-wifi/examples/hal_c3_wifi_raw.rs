//! Raw WiFi connect + DHCP test for ESP32-C3 Super Mini with onboard LED.
//!
//! Blinks GPIO8 (onboard LED) during WiFi association, then prints the
//! DHCP-assigned IP address.
//!
//! `WIFI_SSID` and `WIFI_PASS` must be set as environment variables **at build time**.

#![no_std]
#![no_main]

extern crate alloc;

use esp_backtrace as _;
use esp_hal::delay::Delay;
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::interrupt::software::SoftwareInterruptControl;
use esp_hal::main;
use esp_hal::rng::Rng;
use esp_hal::timer::timg::TimerGroup;
use esp_println::println;
use esp_radio::wifi::{ClientConfig, ModeConfig, PowerSaveMode};
use smoltcp::iface::{Config as IfaceConfig, Interface, SocketSet};
use smoltcp::socket::dhcpv4;
use smoltcp::wire::{EthernetAddress, HardwareAddress};

esp_bootloader_esp_idf::esp_app_desc!();

const SSID: &str = match option_env!("WIFI_SSID") {
    Some(s) => s,
    None => "",
};
const PASSWORD: &str = match option_env!("WIFI_PASS") {
    Some(s) => s,
    None => "",
};

fn smoltcp_now() -> smoltcp::time::Instant {
    smoltcp::time::Instant::from_millis(
        esp_hal::time::Instant::now()
            .duration_since_epoch()
            .as_millis() as i64,
    )
}

#[main]
fn main() -> ! {
    esp_println::logger::init_logger(log::LevelFilter::Info);
    let peripherals = esp_hal::init(esp_hal::Config::default());
    println!("hal_c3_wifi_raw: init done (SSID len={})", SSID.len());

    let mut led = Output::new(peripherals.GPIO8, Level::High, OutputConfig::default());
    let delay = Delay::new();

    // Blink twice to confirm board is alive.
    for _ in 0..2 {
        led.set_low();
        delay.delay_millis(200);
        led.set_high();
        delay.delay_millis(200);
    }

    esp_alloc::heap_allocator!(size: 72 * 1024);

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let sw_int = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_int.software_interrupt0);

    let _rng = Rng::new();
    let esp_radio_ctrl = esp_radio::init().unwrap();

    let (mut controller, interfaces) =
        esp_radio::wifi::new(&esp_radio_ctrl, peripherals.WIFI, Default::default()).unwrap();
    println!("wifi::new done");

    // Configure STA mode.
    let client_config = ClientConfig::default()
        .with_ssid(SSID.into())
        .with_password(PASSWORD.into());
    controller
        .set_config(&ModeConfig::Client(client_config))
        .unwrap();
    controller.start().unwrap();
    controller.set_power_saving(PowerSaveMode::None).unwrap();
    controller.connect().unwrap();
    println!("Connecting to '{}'...", SSID);

    // Blink LED while waiting for L2 association.
    loop {
        match controller.is_connected() {
            Ok(true) => break,
            Ok(false) => {}
            Err(e) => {
                println!("WiFi error: {:?}", e);
                loop {}
            }
        }
        led.toggle();
        delay.delay_millis(100);
    }
    led.set_low(); // LED on = associated
    println!("Associated");

    // Run DHCP.
    let mut device = interfaces.sta;
    let mac = esp_radio::wifi::sta_mac();
    let hw_addr = HardwareAddress::Ethernet(EthernetAddress(mac));
    let mut iface = Interface::new(IfaceConfig::new(hw_addr), &mut device, smoltcp_now());

    let mut socket_storage = [smoltcp::iface::SocketStorage::EMPTY; 1];
    let mut sockets = SocketSet::new(&mut socket_storage[..]);
    let dhcp_handle = sockets.add(dhcpv4::Socket::new());

    println!("Waiting for DHCP...");
    loop {
        iface.poll(smoltcp_now(), &mut device, &mut sockets);

        let socket = sockets.get_mut::<dhcpv4::Socket>(dhcp_handle);
        if let Some(dhcpv4::Event::Configured(config)) = socket.poll() {
            let ip = config.address.address();
            println!("DHCP assigned IP: {}", ip);
            break;
        }
        delay.delay_millis(50);
    }

    // LED off — done.
    led.set_high();
    println!("Done. Looping.");

    loop {
        delay.delay_millis(1_000);
    }
}
