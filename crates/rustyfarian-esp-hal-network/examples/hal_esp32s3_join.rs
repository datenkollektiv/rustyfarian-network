//! ESP32-S3 SX1262 Bring-Up — Heltec WiFi LoRa 32 V3
//!
//! SX1262 hardware bring-up for the TTN EU868 validation.
//!
//! Runs the shared SX1262 bring-up, `juggler::lora::sx1262::init` (re-exported as
//! `rustyfarian_esp_hal_network::lora::sx1262`), the same step order the ESP-IDF
//! tier runs (ADR 023 rung 1):
//!
//! 1. Hardware reset (GPIO 12, active-low), driven here
//! 2. `SetStandby`, `SetPacketType(LoRa)`, `SetRfFrequency` (868.1 MHz, EU868)
//! 3. `SetDIO3AsTCXOCtrl` — power the TCXO at 1.8 V via DIO3, 10 ms startup
//! 4. `ClearDeviceErrors` — clear the expected `XOSC_START_ERR` before calibrating
//! 5. `Calibrate`, `CalibrateImage`, PA / TX / buffer / modulation / packet / IRQ setup
//! 6. `SetDio2AsRfSwitchCtrl` and the LoRaWAN public sync word
//! 7. `GetStatus` — read and decode the chip status
//!
//! Every BUSY wait is bounded and polls `GetStatus` over SPI 1 ms apart.
//!
//! Expected output after a clean reset:
//!
//! ```text
//! hal_esp32s3_join: SX1262 bring-up starting
//! sx1262: reset pulsed
//! sx1262: shared bring-up complete
//! sx1262: GetStatus -> chip_mode=Some(StbyRC)
//! sx1262: bring-up complete
//! ```
//!
//! If the bring-up reports `BusyTimeout(Reset)`, check wiring and power; a
//! `BusyTimeout` on a later step names the command that stalled.
//! See `docs/heltec-wifi-lora-32-v3.md` for the full pin reference and failure-mode table.
//!
//! ## Pin assignments (Heltec WiFi LoRa 32 V3)
//!
//! | Signal | GPIO |
//! |--------|------|
//! | NSS/CS | 8    |
//! | SCK    | 9    |
//! | MOSI   | 10   |
//! | MISO   | 11   |
//! | RESET  | 12   |
//! | BUSY   | 13   |
//! | DIO1   | 14   |
//!
//! Note: Heltec labels DIO1 as "DIO0" in their Arduino headers — GPIO 14 is the
//! SX1262 DIO1 pad regardless of that label.
//!
//! ## Build and run
//!
//! ```sh
//! just run hal_esp32s3_join
//! ```

#![no_std]
#![no_main]

esp_bootloader_esp_idf::esp_app_desc!();

use core::convert::Infallible;

use embedded_hal::digital::{ErrorType, OutputPin};
use embedded_hal_bus::spi::ExclusiveDevice;
use esp_hal::delay::Delay;
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig};
use esp_hal::main;
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::spi::Mode;
use esp_hal::time::Rate;
use esp_println::println;
use rustyfarian_esp_hal_network::lora::sx1262::{self, SX126x};
use rustyfarian_esp_hal_network::lora::{LoraConfig, Region};

// Minimal panic handler — replace with `panic-halt` or `panic-probe` in production.
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

/// The dev-dependency `juggler` (host-test features such as `ota-wire`) links
/// `alloc` into every example, which then needs a global allocator. This
/// bring-up never allocates, so an allocator that always fails is enough.
struct NoHeap;

// SAFETY: `alloc` returns null, which `GlobalAlloc` permits to signal failure;
// `dealloc` is never reached because no allocation ever succeeds.
unsafe impl core::alloc::GlobalAlloc for NoHeap {
    unsafe fn alloc(&self, _: core::alloc::Layout) -> *mut u8 {
        core::ptr::null_mut()
    }

    unsafe fn dealloc(&self, _: *mut u8, _: core::alloc::Layout) {}
}

#[global_allocator]
static HEAP: NoHeap = NoHeap;

/// The antenna pin `SX126x` asks for. The Heltec V3 drives its RF switch from
/// DIO2 (`SetDio2AsRfSwitchCtrl`), so no GPIO is needed.
struct NoAntennaPin;

impl ErrorType for NoAntennaPin {
    type Error = Infallible;
}

impl OutputPin for NoAntennaPin {
    fn set_low(&mut self) -> Result<(), Infallible> {
        Ok(())
    }

    fn set_high(&mut self) -> Result<(), Infallible> {
        Ok(())
    }
}

#[main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default());
    let mut delay = Delay::new();

    println!("[app] start {} (esp-hal)", env!("CARGO_CRATE_NAME"));
    println!("hal_esp32s3_join: SX1262 bring-up starting");

    // --- GPIO setup ---------------------------------------------------------

    // CS starts high (deasserted); ExclusiveDevice takes ownership and drives it.
    let cs = Output::new(peripherals.GPIO8, Level::High, OutputConfig::default());

    // RESET is active-low; idle high.
    let mut rst = Output::new(peripherals.GPIO12, Level::High, OutputConfig::default());

    // BUSY is active-high — high means the chip is processing a command.
    // SX1262 drives this line; no pull resistor needed.
    let busy = Input::new(peripherals.GPIO13, InputConfig::default());

    // DIO1 is the join-accept / TX-done IRQ — not used in this bring-up step.
    // SX1262 drives this line; no pull resistor needed.
    let dio1 = Input::new(peripherals.GPIO14, InputConfig::default());

    // --- SPI bus setup ------------------------------------------------------

    // SX1262 supports up to 16 MHz; start at 1 MHz for initial bring-up.
    let spi_bus = Spi::new(
        peripherals.SPI2,
        SpiConfig::default()
            .with_frequency(Rate::from_mhz(1))
            .with_mode(Mode::_0),
    )
    .unwrap()
    .with_sck(peripherals.GPIO9)
    .with_mosi(peripherals.GPIO10)
    .with_miso(peripherals.GPIO11);

    // Wrap SpiBus + CS pin into an SpiDevice — ExclusiveDevice asserts/deasserts
    // NSS automatically around each transaction.
    let spi = ExclusiveDevice::new_no_delay(spi_bus, cs).unwrap();

    // --- Step 1: Hardware reset ---------------------------------------------
    //
    // Assert RESET low for 200 µs, then release (datasheet: at least 100 µs).
    // Driven here rather than through `SX126x::reset`, which wraps the pulse in
    // `critical_section::with` (bug 007); the shared bring-up waits out the reset.

    rst.set_low();
    delay.delay_micros(200);
    rst.set_high();
    println!("sx1262: reset pulsed");

    // --- Steps 2–6: shared bring-up -----------------------------------------

    let mut radio = SX126x::new(spi, (rst, busy, NoAntennaPin, dio1));
    let config = LoraConfig {
        region: Region::EU868,
        ..LoraConfig::default()
    };
    if let Err(e) = sx1262::init(
        &mut radio,
        &mut delay,
        &config,
        sx1262::DEFAULT_BUSY_POLL_BOUND,
    ) {
        println!("sx1262: ERROR — bring-up failed: {:?}", e);
        loop {}
    }
    println!("sx1262: shared bring-up complete");

    // --- Step 7: GetStatus --------------------------------------------------
    //
    // Expected chip_mode = StbyRC after the bring-up.

    match radio.get_status() {
        Ok(status) => println!("sx1262: GetStatus -> chip_mode={:?}", status.chip_mode()),
        Err(_) => println!("sx1262: WARNING — GetStatus failed (SPI wiring?)"),
    }

    // --- Done ---------------------------------------------------------------

    println!("sx1262: bring-up complete");
    println!("Next step: implement EspHalLoraRadio on the shared bring-up and attempt OTAA join");
    loop {}
}
