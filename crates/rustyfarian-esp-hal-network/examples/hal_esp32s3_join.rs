//! ESP32-S3 SX1262 Bring-Up — Heltec WiFi LoRa 32 V3
//!
//! Phase 5, Step 2: SX1262 hardware bring-up.
//!
//! Performs the minimal SPI sequence required to verify the radio is alive:
//!
//! 1. Hardware reset (GPIO 12, active-low)
//! 2. Poll BUSY (GPIO 13) until low — chip signals readiness
//! 3. `SetDIO3AsTCXOCtrl` — power the TCXO at 1.8 V via DIO3 (mandatory on this board)
//! 4. `SetDio2AsRfSwitchCtrl` — enable automatic RF switch control via DIO2
//! 5. `GetStatus` — read and decode the chip status byte
//!
//! Expected output after a clean reset:
//!
//! ```text
//! sx1262: reset complete, BUSY low
//! sx1262: TCXO enabled (DIO3 @ 1.8 V, 5 ms startup)
//! sx1262: RF switch control enabled (DIO2)
//! sx1262: GetStatus -> 0x22 (chip_mode=STDBY_RC, cmd_status=idle)
//! sx1262: bring-up complete
//! ```
//!
//! `cmd_status=idle` (0x1) is the normal post-reset state — the SX1262 datasheet
//! marks 0x1 as "reserved" but in practice it means "last configuration command
//! completed, no data to return".
//! `cmd_status=ok` (0x2) only appears after commands that return a data payload.
//! Both are healthy; `chip_mode=STDBY_RC` is the authoritative success indicator.
//!
//! If BUSY never clears or `GetStatus` returns `0x00`, the most likely cause is that
//! `SetDIO3AsTCXOCtrl` was not sent — the TCXO has no power and the radio has no clock.
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

use embedded_hal::digital::InputPin;
use embedded_hal::spi::{Operation, SpiDevice};
use embedded_hal_bus::spi::ExclusiveDevice;
use esp_hal::delay::Delay;
use esp_hal::gpio::{Input, InputConfig, Level, Output, OutputConfig};
use esp_hal::main;
use esp_hal::spi::master::{Config as SpiConfig, Spi};
use esp_hal::spi::Mode;
use esp_hal::time::Rate;
use esp_println::println;

// Minimal panic handler — replace with `panic-halt` or `panic-probe` in production.
#[panic_handler]
fn panic(_: &core::panic::PanicInfo) -> ! {
    loop {}
}

/// Poll BUSY (active-high) until low, with a 10 ms time-based timeout.
///
/// Each iteration sleeps 10 µs so the total worst-case wait is bounded
/// at ~10 ms regardless of CPU frequency or compiler optimisation level.
/// Returns `true` if BUSY cleared, `false` on timeout.
fn wait_busy_low(busy: &mut impl InputPin, delay: &Delay) -> bool {
    for _ in 0..1_000 {
        if !busy.is_high().unwrap_or(true) {
            return true;
        }
        delay.delay_micros(10);
    }
    false
}

/// Decode the SX1262 status byte and return human-readable strings for each field.
fn decode_status(status: u8) -> (&'static str, &'static str) {
    let chip_mode = match (status >> 4) & 0x07 {
        0x2 => "STDBY_RC",
        0x3 => "STDBY_XOSC",
        0x4 => "FS",
        0x5 => "RX",
        0x6 => "TX",
        _ => "unknown",
    };
    let cmd_status = match (status >> 1) & 0x07 {
        // 0x0 and 0x1: datasheet says "reserved" but 0x1 is the normal state
        // after a write-only configuration command (no data payload to return).
        0x0 | 0x1 => "idle",
        0x2 => "data_available",
        0x3 => "timeout",
        0x5 => "error",
        0x6 => "fail_to_exec",
        0x7 => "tx_done",
        _ => "reserved",
    };
    (chip_mode, cmd_status)
}

#[main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default());
    let delay = Delay::new();

    println!("hal_esp32s3_join: SX1262 bring-up starting");

    // --- GPIO setup ---------------------------------------------------------

    // CS starts high (deasserted); ExclusiveDevice takes ownership and drives it.
    let cs = Output::new(peripherals.GPIO8, Level::High, OutputConfig::default());

    // RESET is active-low; idle high.
    let mut rst = Output::new(peripherals.GPIO12, Level::High, OutputConfig::default());

    // BUSY is active-high — high means the chip is processing a command.
    // SX1262 drives this line; no pull resistor needed.
    let mut busy = Input::new(peripherals.GPIO13, InputConfig::default());

    // DIO1 is the join-accept / TX-done IRQ — not used in this bring-up step.
    // SX1262 drives this line; no pull resistor needed.
    let _dio1 = Input::new(peripherals.GPIO14, InputConfig::default());

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
    let mut spi = ExclusiveDevice::new_no_delay(spi_bus, cs).unwrap();

    // --- Step 1: Hardware reset ---------------------------------------------
    //
    // Assert RESET low for 200 µs, then release.
    // SX1262 datasheet requires at least 100 µs; 200 µs gives comfortable margin.

    rst.set_low();
    delay.delay_micros(200);
    rst.set_high();

    // Wait for BUSY to go low — chip signals readiness within ~3 ms after reset.
    if !wait_busy_low(&mut busy, &delay) {
        println!("sx1262: ERROR — BUSY did not clear after reset (check wiring or power)");
        loop {}
    }
    println!("sx1262: reset complete, BUSY low");

    // --- Step 2: SetDIO3AsTCXOCtrl (mandatory on Heltec V3) ----------------
    //
    // The Heltec V3 TCXO is powered from the SX1262 DIO3 output pin, not from a
    // fixed supply rail.  Without this command the radio has no reference clock
    // and every subsequent command will stall or return garbage.
    //
    // Parameters:
    //   0x97        = SetDIO3AsTCXOCtrl opcode
    //   0x02        = 1.8 V output (register code)
    //   0x00 0x01 0x40 = 320 ticks × 15.625 µs/tick = 5 ms startup delay

    spi.transaction(&mut [Operation::Write(&[0x97, 0x02, 0x00, 0x01, 0x40])])
        .unwrap();

    if !wait_busy_low(&mut busy, &delay) {
        println!("sx1262: ERROR — BUSY did not clear after SetDIO3AsTCXOCtrl");
        loop {}
    }
    println!("sx1262: TCXO enabled (DIO3 @ 1.8 V, 5 ms startup)");

    // --- Step 3: SetDio2AsRfSwitchCtrl -------------------------------------
    //
    // Configures DIO2 to drive the onboard RF switch automatically:
    //   high during TX, low otherwise.
    // Without this the RF switch position is undefined and TX/RX is unreliable.
    //
    // Parameters:
    //   0x9D = SetDio2AsRfSwitchCtrl opcode
    //   0x01 = enable

    spi.transaction(&mut [Operation::Write(&[0x9D, 0x01])])
        .unwrap();

    if !wait_busy_low(&mut busy, &delay) {
        println!("sx1262: ERROR — BUSY did not clear after SetDio2AsRfSwitchCtrl");
        loop {}
    }
    println!("sx1262: RF switch control enabled (DIO2)");

    // --- Step 4: GetStatus --------------------------------------------------
    //
    // Reads the chip status byte to confirm the radio is alive and in STDBY_RC.
    //
    // SPI frame:
    //   MOSI: [0xC0, 0x00]   (opcode + NOP)
    //   MISO: [?,    status] (status returned during the NOP byte)
    //
    // Expected status byte = 0x22:
    //   bits 6:4 = 0x2 → chip_mode = STDBY_RC
    //   bits 3:1 = 0x1 → cmd_status = idle (normal after a configuration command)

    let mut rx = [0u8; 2];
    spi.transaction(&mut [Operation::Transfer(&mut rx, &[0xC0, 0x00])])
        .unwrap();

    if !wait_busy_low(&mut busy, &delay) {
        println!("sx1262: ERROR — BUSY did not clear after GetStatus");
        loop {}
    }

    let status = rx[1];
    let (chip_mode, cmd_status) = decode_status(status);
    println!(
        "sx1262: GetStatus -> 0x{:02x} (chip_mode={}, cmd_status={})",
        status, chip_mode, cmd_status
    );

    if status == 0x00 {
        println!(
            "sx1262: WARNING — status is 0x00; likely TCXO not powered \
             or SPI wiring issue"
        );
    }

    // --- Done ---------------------------------------------------------------

    println!("sx1262: bring-up complete");
    println!("Next step: implement full SX1262 init in EspHalLoraRadio and attempt OTAA join");
    loop {}
}
