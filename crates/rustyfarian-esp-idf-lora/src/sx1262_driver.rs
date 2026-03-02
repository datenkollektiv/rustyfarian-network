//! SX1262 radio driver for the Heltec WiFi LoRa 32 V3.
//!
//! [`EspLoraRadio`] implements [`crate::LoraRadio`] using `sx126x 0.3` and
//! ESP-IDF SPI.  An internal `LoraRadioAdapter` (commented out below) will
//! bridge `EspLoraRadio` to `lorawan-device`'s `PhyRxTx + Timings` traits
//! once the API is confirmed on hardware.
//!
//! # SPI2 pin assignments (Heltec WiFi LoRa 32 V3)
//!
//! | Signal | GPIO |
//! |:-------|-----:|
//! | NSS/CS |    8 |
//! | SCK    |    9 |
//! | MOSI   |   10 |
//! | MISO   |   11 |
//! | RST    |   12 |
//! | BUSY   |   13 |
//! | DIO1   |   14 |
//!
//! SPI mode 0 (CPOL=0, CPHA=0), 8 MHz clock.
//!
//! # DIO1 interrupt setup
//!
//! DIO1 is the SX1262's IRQ output. In `main.rs`:
//!
//! ```rust,ignore
//! use std::sync::atomic::{AtomicBool, Ordering};
//! use esp_idf_hal::gpio::{InterruptType, PinDriver};
//!
//! pub static DIO1_FLAG: AtomicBool = AtomicBool::new(false);
//!
//! let mut dio1 = PinDriver::input(peripherals.pins.gpio14)?;
//! dio1.set_interrupt_type(InterruptType::PosEdge)?;
//! unsafe {
//!     dio1.subscribe(|| { DIO1_FLAG.store(true, Ordering::Release); })?;
//! }
//! dio1.enable_interrupt()?;
//! // After DIO1 fires: re-arm with dio1.enable_interrupt() from task context.
//! ```
//!
//! # Critical mapping table (implement before RF testing)
//!
//! Both `lorawan-device` and `sx126x` define independent enums for SF, BW, CR.
//! Map them explicitly in the `prepare_tx` implementation — no automatic
//! conversion exists. EU868 DR0 = SF12, BW125, CR4/5.
//!
//! # Implementation status
//!
//! This driver returns `Err(LoraError::RadioInitFailed)` from all methods.
//! The constructor always returns `Err`.
//! Beekeeper wraps the result in `Option<>` and boots without LoRa until this
//! implementation is completed (see HIGH RISK note in `docs/phase-5-plan.md`).
//!
//! Implementation milestones:
//! 1. Verify SPI communication: read GetStatus register, confirm chip responds.
//! 2. Implement `prepare_tx` / `transmit` / `prepare_rx` / `receive`.
//! 3. Implement `LoraRadioAdapter` PhyRxTx bridge (read lorawan-device/tests/class_a.rs first).
//! 4. Wire up OTAA join in beekeeper main loop.

use crate::config::LoraConfig;
use crate::{LoraRadio, RxConfig, RxQuality, RxWindow, TxConfig};

// ─── Error type ───────────────────────────────────────────────────────────────

/// Error type for [`EspLoraRadio`] operations.
#[derive(Debug)]
pub enum LoraError {
    /// SPI bus initialisation failed.
    SpiInitFailed,
    /// Radio hardware not responding or configuration rejected.
    RadioInitFailed,
    /// Frame transmission failed.
    TransmitFailed,
    /// Frame reception failed.
    ReceiveFailed,
    /// Operation timed out waiting for a radio response.
    Timeout,
    /// Radio BUSY pin held high longer than expected.
    BusyTimeout,
    /// Could not read the IRQ status register after DIO1 fired.
    IrqStatusReadFailed,
}

impl core::fmt::Display for LoraError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::SpiInitFailed => write!(f, "SPI init failed"),
            Self::RadioInitFailed => write!(f, "radio init failed"),
            Self::TransmitFailed => write!(f, "transmit failed"),
            Self::ReceiveFailed => write!(f, "receive failed"),
            Self::Timeout => write!(f, "radio timeout"),
            Self::BusyTimeout => write!(f, "radio busy timeout"),
            Self::IrqStatusReadFailed => write!(f, "IRQ status read failed"),
        }
    }
}

// ─── EspLoraRadio ─────────────────────────────────────────────────────────────

/// SX1262 radio driver for the Heltec WiFi LoRa 32 V3.
///
/// Owns the SX126x configuration handle and the SPI device driver together.
/// `sx126x 0.3` passes `&mut spi` on every call but `LoraRadio`/`PhyRxTx` methods
/// receive no SPI parameter — owning both in one struct avoids borrow conflicts.
///
/// # Implementation status
///
/// [`new`][Self::new] currently returns `Err(LoraError::RadioInitFailed)`.
/// The struct fields below (`_radio`, `_spi`) will become the live `sx126x::SX126x`
/// handle and `SpiDeviceDriver` once the hardware API is confirmed.
pub struct EspLoraRadio {
    // TODO: Replace these placeholders with the real driver fields:
    //   radio: sx126x::SX126x<PinDriver<'static, Gpio8, Output>, ...>,
    //   spi: SpiDeviceDriver<'static, SpiDriver<'static>>,
    last_rssi: i16,
    last_snr: i8,
}

impl EspLoraRadio {
    /// Attempt to initialise the SX1262.
    ///
    /// Currently returns `Err(LoraError::RadioInitFailed)` — see module-level
    /// implementation status note. Pass the live ESP-IDF peripherals once the
    /// `sx126x 0.3` constructor API is verified on hardware.
    ///
    /// Expected final signature (subject to sx126x 0.3 API verification):
    /// ```rust,ignore
    /// pub fn new(
    ///     spi: SpiDeviceDriver<'static, SpiDriver<'static>>,
    ///     cs:   PinDriver<'static, Gpio8,  Output>,
    ///     rst:  PinDriver<'static, Gpio12, Output>,
    ///     busy: PinDriver<'static, Gpio13, Input>,
    ///     dio1: PinDriver<'static, Gpio14, Input>,
    ///     config: &LoraConfig,
    /// ) -> Result<Self, LoraError>
    /// ```
    pub fn new(_config: &LoraConfig) -> Result<Self, LoraError> {
        // Step 1 (to implement): init SPI2 at SPI mode 0, 8 MHz.
        // Step 2 (to implement): call SX126x::new(cs, rst, busy, dio1).
        // Step 3 (to implement): call radio.init(&mut spi) to verify communication.
        //   Confirm by reading GetStatus — log the result before proceeding.
        // Step 4 (to implement): configure packet type, sync word, DIO1 IRQ mask.
        //
        // See docs/phase-5-plan.md §"sx1262_driver.rs" for the full init sequence.
        log::warn!(
            "SX1262 driver: hardware integration pending — \
             LoRa will be unavailable until the PhyRxTx bridge is implemented. \
             Wi-Fi OTA continues normally."
        );
        Err(LoraError::RadioInitFailed)
    }
}

impl LoraRadio for EspLoraRadio {
    type Error = LoraError;

    fn prepare_tx(&mut self, config: TxConfig, buf: &[u8]) -> Result<(), LoraError> {
        // TODO: Map `TxConfig` → sx126x `PacketConfig`.
        // See `sf_to_sx126x`, `bw_to_sx126x`, `cr_to_sx126x` helpers below.
        // Call: radio.set_packet_params(), radio.set_rf_frequency(),
        //        radio.set_tx_params(), radio.write_buffer()
        let _ = (config, buf);
        Err(LoraError::RadioInitFailed)
    }

    fn transmit(&mut self) -> nb::Result<u32, LoraError> {
        // TODO: radio.set_tx(timeout), poll DIO1_FLAG, return Ok(on_air_ms).
        Err(nb::Error::Other(LoraError::RadioInitFailed))
    }

    fn prepare_rx(&mut self, config: RxConfig, window: RxWindow) -> Result<(), LoraError> {
        // TODO: Map `RxConfig` → PacketConfig, set frequency, call radio.set_rx().
        let _ = (config, window);
        Err(LoraError::RadioInitFailed)
    }

    fn receive(&mut self, buf: &mut [u8]) -> nb::Result<(usize, RxQuality), LoraError> {
        // TODO: Check DIO1_FLAG, read IRQ status, read FIFO, get packet status.
        let _ = buf;
        Err(nb::Error::Other(LoraError::RadioInitFailed))
    }

    fn set_frequency(&mut self, freq_hz: u32) -> Result<(), LoraError> {
        // TODO: radio.set_rf_frequency(freq_hz, &mut self.spi)
        let _ = freq_hz;
        Err(LoraError::RadioInitFailed)
    }

    fn rx_quality(&self) -> RxQuality {
        RxQuality {
            rssi: self.last_rssi,
            snr: self.last_snr,
        }
    }

    fn rx_window_offset_ms(&self) -> i32 {
        crate::RX_WINDOW_OFFSET_MS
    }

    fn rx_window_duration_ms(&self) -> u32 {
        crate::RX_WINDOW_DURATION_MS
    }
}

// ─── SF/BW/CR mapping helpers ─────────────────────────────────────────────────
//
// These will be called from `prepare_tx` once sx126x 0.3 is wired up.
// EU868 DR0: SF12, BW125, CR4/5.
// Verify the sx126x crate's exact enum variants before use — they differ from
// other LoRa crates and must not be assumed.

use crate::{Bandwidth, CodingRate, SpreadingFactor};

/// Map our `SpreadingFactor` to the numeric SF value for sx126x.
///
/// Replace the `u8` return with the `sx126x` SF enum type once the API is confirmed.
#[allow(dead_code)]
fn sf_to_sx126x(sf: SpreadingFactor) -> u8 {
    match sf {
        SpreadingFactor::SF7 => 7,
        SpreadingFactor::SF8 => 8,
        SpreadingFactor::SF9 => 9,
        SpreadingFactor::SF10 => 10,
        SpreadingFactor::SF11 => 11,
        SpreadingFactor::SF12 => 12,
    }
}

/// Map our `Bandwidth` to Hz for sx126x (which typically uses Hz values).
///
/// Replace `u32` with the `sx126x` BW enum type once the API is confirmed.
#[allow(dead_code)]
fn bw_to_sx126x(bw: Bandwidth) -> u32 {
    match bw {
        Bandwidth::BW125 => 125_000,
        Bandwidth::BW250 => 250_000,
        Bandwidth::BW500 => 500_000,
    }
}

/// Map our `CodingRate` to the sx126x coding rate discriminant.
///
/// Replace `u8` with the `sx126x` CR enum type once the API is confirmed.
#[allow(dead_code)]
fn cr_to_sx126x(cr: CodingRate) -> u8 {
    match cr {
        CodingRate::Cr45 => 1,
        CodingRate::Cr46 => 2,
        CodingRate::Cr47 => 3,
        CodingRate::Cr48 => 4,
    }
}

// ─── LoraRadioAdapter (PhyRxTx bridge) ────────────────────────────────────────
//
// This internal type bridges `LoraRadio` to `lorawan-device`'s `PhyRxTx + Timings`.
// It is NOT part of the public crate API.
//
// Implementation approach:
// 1. Read `lorawan-device/tests/class_a.rs` to understand the exact `PhyRxTx` API.
// 2. Implement `PhyRxTx::handle_event(Event::RadioEvent(...))` by checking the
//    DIO1 flag and routing to the appropriate `LoraRadio` method.
// 3. Implement `Timings::get_rx_window_offset_ms()` by delegating to `LoraRadio`.
//
// If the `PhyRxTx` bridge proves unworkable (HIGH RISK), fall back to
// `lorawan-encoding` with a hand-written Class A state machine.
//
// #[allow(dead_code)]
// struct LoraRadioAdapter<R: LoraRadio>(R);
//
// impl<R: LoraRadio> PhyRxTx for LoraRadioAdapter<R> {
//     type PhyError = R::Error;
//     fn handle_event(&mut self, event: Event<Self>) -> Result<Response<Self>, ...> {
//         match event {
//             Event::TimeoutFired => { /* advance timing */ }
//             Event::RadioEvent(phy_response) => { /* route DIO1 */ }
//         }
//     }
// }
//
// impl<R: LoraRadio> Timings for LoraRadioAdapter<R> {
//     fn get_rx_window_offset_ms(&self) -> i32 { self.0.rx_window_offset_ms() }
//     fn get_rx_window_duration_ms(&self) -> u32 { self.0.rx_window_duration_ms() }
// }
