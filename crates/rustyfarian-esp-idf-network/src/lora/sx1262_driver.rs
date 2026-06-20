//! SX1262 radio driver for the Heltec WiFi LoRa 32 V3.
//!
//! [`EspIdfLoraRadio`] implements [`juggler::lora::LoraRadio`] using `sx126x 0.3` and
//! ESP-IDF SPI.
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
//! use esp_idf_hal::gpio::{InterruptType, PinDriver, Pull};
//!
//! pub static DIO1_FLAG: AtomicBool = AtomicBool::new(false);
//!
//! // Floating: DIO1 is driven by the SX1262 — no internal pull needed.
//! let mut dio1 = PinDriver::input(peripherals.pins.gpio14, Pull::Floating)?;
//! dio1.set_interrupt_type(InterruptType::PosEdge)?;
//! unsafe {
//!     dio1.subscribe(|| { DIO1_FLAG.store(true, Ordering::Release); })?;
//! }
//! dio1.enable_interrupt()?;
//! ```
//!
//! The main loop polls `DIO1_FLAG` and delivers `RadioEvent(Phy(()))` to the
//! lorawan-device state machine when it fires.
//!
//! # TCXO note
//!
//! The Heltec V3 uses a TCXO (32 MHz) powered from DIO3.
//! Without `tcxo_opts` in `Config`, `init()` succeeds silently but produces no RF output.
//! Always pass `tcxo_opts: Some((TcxoVoltage::Volt1_8, TcxoDelay::from_ms(5)))`.
//!
//! # IQ inversion
//!
//! LoRaWAN uplinks use standard IQ; downlinks use inverted IQ.
//! `prepare_rx` sets the inverted flag before each RX window.
//! `prepare_tx` restores standard IQ before each uplink.

use embedded_hal::{
    digital::OutputPin,
    spi::{ErrorType, Operation, SpiDevice},
};
use esp_idf_hal::{
    delay::FreeRtos,
    gpio::{GpioError, Input, Output, PinDriver},
    spi::{SpiDeviceDriver, SpiDriver},
};
use sx126x::{
    calc_rf_freq,
    op::{
        calib::{CalibImageFreq, CalibParam},
        irq::{IrqMask, IrqMaskBit},
        modulation::{LoRaBandWidth, LoRaSpreadFactor, LoraCodingRate, LoraModParams},
        packet::{LoRaCrcType, LoRaHeaderType, LoRaInvertIq, LoRaPacketParams, PacketType},
        rxtx::{DeviceSel, PaConfig, RampTime, RxTxTimeout, TxParams},
        tcxo::{TcxoDelay, TcxoVoltage},
        StandbyConfig,
    },
    SX126x,
};

use juggler::lora::config::LoraConfig;
use juggler::lora::{
    Bandwidth, CodingRate, LoraRadio, RxConfig, RxQuality, RxWindow, SpreadingFactor, TxConfig,
};

// ─── FullDuplexDevice SPI wrapper ─────────────────────────────────────────────

/// Adapts an `SpiDevice` for the ESP-IDF full-duplex constraint.
///
/// The ESP-IDF SPI master driver (`spi_master.c:check_trans_valid`) rejects any
/// transaction where `rxlength > txlength` in full-duplex mode.  When esp-idf-hal
/// translates an embedded-hal `Operation::Read(buf)` into an `spi_transaction_t`,
/// it sets `rx_buffer = buf` and `tx_buffer = NULL` while `length = rxlength =
/// buf.len() * 8`.  Logically `rxlength == length`, so the check should pass —
/// but in practice the ESP32-S3 SPI peripheral rejects the transaction with
/// `rx length > tx length in full duplex mode` (observed during `sx126x::init()`
/// at around t=404 ms from boot on Heltec WiFi LoRa 32 V3).
///
/// Root cause: the ESP-IDF C driver does not fully support `tx_buffer = NULL` with
/// `length > 0` in full-duplex mode on ESP32-S3.  A null TX buffer collapses the
/// effective TX length to zero, which then violates `rxlength <= txlength`.
///
/// Fix: translate every `Operation::Read(buf)` into
/// `Operation::Transfer(buf, &[0u8; buf.len()])`.  This provides a real (zeroed)
/// TX buffer of equal length to the RX buffer, satisfying the full-duplex
/// constraint.  The SX1262 ignores MOSI during read phases, so sending 0x00 bytes
/// on MOSI has no effect on correctness.
///
/// `TransferInPlace` (used by `sx126x` for `get_status`, `get_device_errors`, etc.)
/// already provides a real TX buffer and is not affected.
///
/// # Allocation bound
///
/// The slow path allocates a single scratch buffer per transaction, sized to the
/// largest `Read` in that transaction. The largest `Read` this driver issues is a
/// full 255-byte LoRaWAN downlink (`read_buffer`), so each allocation is bounded to
/// ≤ 256 bytes. `SpiDevice::transaction` takes `&mut self`, so transactions on a
/// given `FullDuplexDevice` are serialized by the borrow checker — scratch buffers
/// cannot accumulate concurrently across callers, and the allocation is freed when
/// the transaction returns.
pub(crate) struct FullDuplexDevice<SPI> {
    inner: SPI,
}

impl<SPI> FullDuplexDevice<SPI> {
    pub(crate) fn new(inner: SPI) -> Self {
        Self { inner }
    }
}

impl<SPI: ErrorType> ErrorType for FullDuplexDevice<SPI> {
    type Error = SPI::Error;
}

impl<SPI: SpiDevice> SpiDevice for FullDuplexDevice<SPI> {
    fn transaction(&mut self, operations: &mut [Operation<'_, u8>]) -> Result<(), Self::Error> {
        // Fast path: if no Operation::Read is present, forward the original slice
        // directly without any heap allocation.
        //
        // This path is critical for correctness: sx126x::reset() calls
        // spi.transaction(&mut [Operation::DelayNs(200_000)]) inside
        // critical_section::with (interrupts disabled, FreeRTOS scheduler
        // suspended).  Heap allocation (Vec) inside an ESP-IDF critical section
        // deadlocks the main task — the heap lock is held by the allocator, and
        // no other task can release it while the scheduler is suspended.
        //
        // All Write, TransferInPlace, and DelayNs operations take this path and
        // pay zero allocation cost.
        if !operations.iter().any(|op| matches!(op, Operation::Read(_))) {
            return self.inner.transaction(operations);
        }

        // Slow path: at least one Read is present.  All Read call sites in
        // sx126x 0.3 (read_register, read_buffer, get_irq_status,
        // get_packet_status, get_rx_buffer_status, get_device_errors) are NOT
        // wrapped in critical_section::with, so Vec allocation here is safe.
        //
        // Allocate a single zeroed scratch buffer sized to the largest Read in
        // this transaction.  Every Read borrows this buffer immutably (it is
        // only ever written from the SX1262 side), so one allocation covers all
        // Reads.  Do NOT cap at a small constant — read_buffer() can fetch a
        // full 255-byte LoRaWAN downlink, and a TX slice shorter than the RX
        // buffer would re-trigger the `rx length > tx length` error this wrapper
        // exists to prevent.
        let max_read_len = operations
            .iter()
            .filter_map(|op| {
                if let Operation::Read(buf) = op {
                    Some(buf.len())
                } else {
                    None
                }
            })
            .max()
            .unwrap_or(0);
        let zeroes = vec![0u8; max_read_len];

        // Build a rewritten operation list.  Translate every Read → Transfer
        // with a zeroed TX slice to satisfy the ESP-IDF full-duplex constraint
        // (rxlength must not exceed txlength).  All other variants pass through.
        let mut rewritten: Vec<Operation<'_, u8>> = Vec::with_capacity(operations.len());
        for op in operations.iter_mut() {
            rewritten.push(match op {
                Operation::Read(buf) => Operation::Transfer(buf, &zeroes[..buf.len()]),
                Operation::Write(buf) => Operation::Write(buf),
                Operation::Transfer(r, w) => Operation::Transfer(r, w),
                Operation::TransferInPlace(buf) => Operation::TransferInPlace(buf),
                Operation::DelayNs(ns) => Operation::DelayNs(*ns),
            });
        }

        self.inner.transaction(&mut rewritten)
    }
}

/// Error type for [`EspIdfLoraRadio`] operations.
#[derive(Debug)]
pub enum LoraError {
    /// SPI bus initialisation failed.
    SpiInitFailed,
    /// Radio hardware not responding or configuration rejected.
    RadioInitFailed,
    /// RF configuration contains an unsupported spreading factor, bandwidth, or coding rate.
    InvalidRfConfig,
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
            Self::InvalidRfConfig => write!(f, "unsupported RF config (SF/BW/CR)"),
            Self::TransmitFailed => write!(f, "transmit failed"),
            Self::ReceiveFailed => write!(f, "receive failed"),
            Self::Timeout => write!(f, "radio timeout"),
            Self::BusyTimeout => write!(f, "radio busy timeout"),
            Self::IrqStatusReadFailed => write!(f, "IRQ status read failed"),
        }
    }
}

// ─── Type aliases ─────────────────────────────────────────────────────────────

type SpiBusInner<'d> = SpiDeviceDriver<'d, SpiDriver<'d>>;
type SpiBus<'d> = FullDuplexDevice<SpiBusInner<'d>>;
type RstPin<'d> = PinDriver<'d, Output>;
type BusyPin<'d> = PinDriver<'d, Input>;
type Dio1Pin<'d> = PinDriver<'d, Input>;

// ─── EspIdfLoraRadio ─────────────────────────────────────────────────────────

/// SX1262 radio driver for the Heltec WiFi LoRa 32 V3.
///
/// Owns the SX126x handle together with the SPI device driver.
/// `sx126x 0.3` takes ownership of the SPI device — owning everything in one
/// struct avoids borrow conflicts during `LoraRadio` method calls.
///
/// # Antenna pin (`ANT`)
///
/// The SX126x constructor requires an antenna GPIO, but the Heltec V3 controls
/// the RF switch internally via `SetDio2AsRfSwitchCtrl` (called by `init()`).
/// Pass any spare output GPIO held high — the pin value is never used by `init()`.
pub struct EspIdfLoraRadio<'d, ANT> {
    radio: SX126x<SpiBus<'d>, RstPin<'d>, BusyPin<'d>, ANT, Dio1Pin<'d>>,
    last_rssi: i16,
    last_snr: i8,
    /// Wall-clock time at which the last `set_tx()` was issued.
    /// Used to report measured on-air time when `TxDone` fires.
    tx_start: Option<std::time::Instant>,
}

impl<'d, ANT> EspIdfLoraRadio<'d, ANT>
where
    ANT: OutputPin<Error = GpioError>,
{
    /// Initialise the SX1262 and return a ready-to-use radio driver.
    ///
    /// Uses a step-by-step instrumented init sequence instead of the monolithic
    /// `sx126x::init()` so that:
    ///
    /// 1. Every command is logged before it is issued — the **last log line printed
    ///    before a watchdog fire identifies the stalling command**.
    /// 2. BUSY waits are bounded and yield to the FreeRTOS scheduler, preventing
    ///    `IDLE0` starvation and the 5-second task-watchdog fires.
    /// 3. The TCXO is brought up in the order mandated by the SX1262 datasheet
    ///    (§9.6 + §13.3.6): `SetDIO3AsTCXOCtrl` → `ClearDeviceErrors` → `Calibrate`.
    ///
    /// # Why the order matters
    ///
    /// At power-on the SX1262 runs its auto-calibration with the RC oscillator.
    /// When TCXO mode is later selected via `SetDIO3AsTCXOCtrl`, the chip sets the
    /// `XOSC_START_ERR` flag in the DeviceErrors register (bit 5) because the
    /// crystal has not yet had time to stabilise — this is **expected behaviour**
    /// (datasheet §13.3.6, Rev. 2.2).  Calling `Calibrate` while `XOSC_START_ERR`
    /// is set causes the calibration FSM to abort immediately, which in turn leaves
    /// BUSY asserted indefinitely.
    ///
    /// The fix is: after `SetDIO3AsTCXOCtrl`, call `ClearDeviceErrors` to acknowledge
    /// the expected `XOSC_START_ERR`, then call `Calibrate` so the FSM uses the now-
    /// powered TCXO.  This sequence is confirmed by the official Semtech reference
    /// driver (SX126x HAL) and by community reports (RadioLib issue #89).
    ///
    /// # Bounded BUSY wait
    ///
    /// The sx126x crate's `wait_on_busy()` spins forever.  This driver replaces every
    /// busy-wait with `wait_busy_spi()`, which:
    ///   - Polls `get_status()` over SPI (returns `0x_2_` in STDBY_RC / active modes,
    ///     and the cmd_status field indicates whether a command completed).
    ///   - Calls `FreeRtos::delay_ms(1)` between polls so the scheduler runs.
    ///   - Caps at `BUSY_POLL_MAX_ITER` iterations (500 ms total) and, on timeout,
    ///     reads `get_device_errors()` and returns `LoraError::BusyTimeout`.
    ///
    /// # Parameters
    ///
    /// - `spi` — SPI device driver configured for SPI mode 0 at 8 MHz with GPIO 8 as CS.
    /// - `rst` — GPIO 12 configured as output (active-low reset).
    /// - `busy` — GPIO 13 configured as input.
    /// - `ant` — spare output GPIO held high; DIO2 controls the RF switch internally.
    /// - `dio1` — GPIO 14 configured as input; the caller's ISR sets a shared flag on
    ///   rising edge, which the main loop delivers as a `Phy(())` event.
    /// - `_config` — LoRaWAN credentials (used by the LoRaWAN layer, not the radio driver).
    pub fn new(
        spi: SpiDeviceDriver<'d, SpiDriver<'d>>,
        rst: RstPin<'d>,
        busy: BusyPin<'d>,
        ant: ANT,
        dio1: Dio1Pin<'d>,
        _config: &LoraConfig,
    ) -> Result<Self, LoraError> {
        // Wrap the SPI device in the full-duplex compatibility shim before handing it
        // to sx126x.  See `FullDuplexDevice` for a detailed explanation of why this
        // is required.
        let spi = FullDuplexDevice::new(spi);
        let mut radio = SX126x::new(spi, (rst, busy, ant, dio1));

        init_sx1262(&mut radio)?;

        // The caller (example/app) logs the user-facing "initialized" line; keep
        // the driver-internal one at debug to avoid a duplicate at info level.
        log::debug!("SX1262 initialized (driver)");
        Ok(Self {
            radio,
            last_rssi: 0,
            last_snr: 0,
            tx_start: None,
        })
    }

    /// Return the radio to RC-oscillator standby, aborting any in-progress RX or TX.
    ///
    /// Best-effort: errors are ignored since this is called during cancellation paths
    /// where we want to reset state regardless.
    pub fn cancel_rx(&mut self) {
        // Diagnostic: capture what the radio saw in the RX window being torn down,
        // BEFORE returning to standby (which aborts any in-progress reception).
        // This is the only point that observes the final (RX2) window, since the
        // software timer closes it before any hardware Timeout IRQ reaches DIO1.
        if let Ok(irq) = self.radio.get_irq_status() {
            log::debug!(
                "cancel_rx: RX teardown IRQ — preamble={} syncword={} header_valid={} \
                 header_err={} rx_done={} crc_err={} timeout={}",
                irq.preamble_detected(),
                irq.syncword_valid(),
                irq.header_valid(),
                irq.header_error(),
                irq.rx_done(),
                irq.crc_err(),
                irq.timeout(),
            );
        }
        let _ = self.radio.set_standby(StandbyConfig::StbyRc);
        let _ = self.radio.clear_irq_status(IrqMask::all());
        self.tx_start = None;
    }

    /// Return `true` if a DIO1-relevant IRQ (RxDone, TxDone, Timeout, CrcErr) is
    /// latched in the radio's IRQ register, WITHOUT clearing it.
    ///
    /// The event loop polls this over SPI instead of relying on the DIO1 GPIO
    /// edge interrupt. `esp-idf-hal` `PinDriver` interrupts are one-shot — they
    /// auto-disable on each firing and must be re-armed from a non-ISR context —
    /// and the DIO1 pin is owned by the inner `SX126x`, so it cannot be re-armed
    /// from the loop. The result: only the first edge (TxDone) is ever delivered
    /// by interrupt, and RxDone is missed. Polling `GetIrqStatus` is immune to
    /// this and is safe to issue while the radio is in RX.
    pub fn irq_pending(&mut self) -> bool {
        self.radio
            .get_irq_status()
            .map(|irq| irq.rx_done() || irq.tx_done() || irq.timeout() || irq.crc_err())
            .unwrap_or(false)
    }
}

// ─── Bounded busy-wait ───────────────────────────────────────────────────────

/// Maximum number of 1 ms polls before declaring a busy-wait timeout.
///
/// 500 iterations × 1 ms/iteration = 500 ms maximum wait per command.
/// The longest legitimate busy interval is `Calibrate` with a TCXO startup
/// delay (10 ms TCXO + calibration FSM ~3.5 ms) — well under 100 ms on a
/// healthy chip.  500 ms gives 5× safety margin while still being far shorter
/// than the 5-second task-watchdog period.
const BUSY_POLL_MAX_ITER: u32 = 500;

/// Poll `get_status()` over SPI until the chip reports a non-busy state, with
/// a 1 ms yield between each poll so the FreeRTOS scheduler can run.
///
/// Returns `Ok(())` when the chip is ready, or `Err(LoraError::BusyTimeout)`
/// after `BUSY_POLL_MAX_ITER` ms.  On timeout, device errors are read and
/// logged before returning.
///
/// # Why get_status() instead of the BUSY pin
///
/// The sx126x crate moves the BUSY `PinDriver` into `SX126x` and does not
/// expose it again.  `get_status()` is an SPI command (`0xC0 NOP`) that
/// returns the chip status byte; when the chip is busy it returns `0x00`
/// (no valid status) or leaves the data bus in a known state.  Concretely,
/// the status byte encodes `chip_mode` in bits [6:4]:
///   - `0x2` = STDBY_RC  — ready
///   - `0x3` = STDBY_XOSC — ready (XOSC / TCXO active)
///   - `0x4` = FS — ready
///   - `0x0` = chip is still busy / command in progress
///
/// Polling until `chip_mode != 0` is equivalent to polling BUSY low.
fn wait_busy_spi<ANT>(
    label: &str,
    radio: &mut SX126x<SpiBus<'_>, RstPin<'_>, BusyPin<'_>, ANT, Dio1Pin<'_>>,
) -> Result<(), LoraError>
where
    ANT: OutputPin<Error = GpioError>,
{
    for i in 0..BUSY_POLL_MAX_ITER {
        // Yield to FreeRTOS — feeds the task watchdog and lets other tasks run.
        FreeRtos::delay_ms(1);

        match radio.get_status() {
            Ok(status) => {
                // chip_mode() returns Some(_) for any valid non-busy state:
                // STDBY_RC (0x2), STDBY_XOSC (0x3), FS (0x4), RX (0x5), TX (0x6).
                // Returns None when the chip is busy (chip_mode bits = 0x0 or 0x1).
                if status.chip_mode().is_some() {
                    log::debug!(
                        "wait_busy_spi({}): ready after {} ms, status={:?}",
                        label,
                        i + 1,
                        status
                    );
                    return Ok(());
                }
            }
            Err(_) => {
                // SPI error during poll — log and keep trying; may self-clear.
                log::warn!("wait_busy_spi({}): SPI error at iter {}", label, i);
            }
        }
    }

    // Timeout — read device errors to help diagnose the cause.
    log::error!(
        "wait_busy_spi({}): BUSY did not clear after {} ms — reading device errors",
        label,
        BUSY_POLL_MAX_ITER
    );
    match radio.get_device_errors() {
        Ok(errs) => log::error!("wait_busy_spi: device errors = {:?}", errs),
        Err(_) => log::error!("wait_busy_spi: could not read device errors (SPI error)"),
    }
    Err(LoraError::BusyTimeout)
}

// ─── Step-by-step instrumented init ──────────────────────────────────────────

/// Perform the Heltec V3 / EU868 SX1262 bring-up sequence with per-command
/// logging and bounded busy-waits.
///
/// Every `log::info!` call is issued **before** the corresponding SPI command
/// so the last line printed before a watchdog fire names the stalling command.
///
/// # TCXO bring-up order (SX1262 datasheet §9.6 + §13.3.6)
///
/// At POR the chip auto-calibrates using the internal RC oscillator.  When
/// `SetDIO3AsTCXOCtrl` is later issued, the chip immediately asserts
/// `XOSC_START_ERR` (DeviceErrors bit 5) because the crystal has not had
/// time to stabilise — **this is expected and documented behaviour**.
///
/// Calling `Calibrate` while `XOSC_START_ERR` is set causes the calibration
/// FSM to abort, which leaves BUSY high indefinitely.  The mandatory fix:
///
/// ```text
/// SetDIO3AsTCXOCtrl(1.8 V, 10 ms)   // power the TCXO; XOSC_START_ERR is set
/// wait_busy                           // chip acknowledges the command
/// ClearDeviceErrors()                 // ← mandatory: clear XOSC_START_ERR
/// wait_busy
/// Calibrate(0x7F)                     // ← re-calibrate all blocks with the TCXO
/// wait_busy
/// ```
///
/// The `sx126x 0.3` crate's `init()` calls `set_dio3_as_tcxo_ctrl` then
/// immediately calls `calibrate` **without** the intervening `ClearDeviceErrors`.
/// That is the root cause of the BUSY-stuck-forever hang on this board.
fn init_sx1262<ANT>(
    radio: &mut SX126x<SpiBus<'_>, RstPin<'_>, BusyPin<'_>, ANT, Dio1Pin<'_>>,
) -> Result<(), LoraError>
where
    ANT: OutputPin<Error = GpioError>,
{
    // ── Step 1: hardware reset ────────────────────────────────────────────────
    log::debug!("sx1262_init: step 1 — hardware reset");
    radio.reset().map_err(|_| LoraError::RadioInitFailed)?;
    log::debug!("sx1262_init: waiting busy after reset");
    wait_busy_spi("reset", radio)?;

    // ── Step 2: enter STDBY_RC ────────────────────────────────────────────────
    log::debug!("sx1262_init: step 2 — SetStandby(STDBY_RC)");
    radio
        .set_standby(StandbyConfig::StbyRc)
        .map_err(|_| LoraError::RadioInitFailed)?;
    log::debug!("sx1262_init: waiting busy after SetStandby");
    wait_busy_spi("SetStandby", radio)?;

    // ── Step 3: set packet type ───────────────────────────────────────────────
    log::debug!("sx1262_init: step 3 — SetPacketType(LoRa)");
    radio
        .set_packet_type(PacketType::LoRa)
        .map_err(|_| LoraError::RadioInitFailed)?;
    log::debug!("sx1262_init: waiting busy after SetPacketType");
    wait_busy_spi("SetPacketType", radio)?;

    // ── Step 4: set RF frequency ──────────────────────────────────────────────
    // At this point no TCXO yet — this uses the RC oscillator frequency reference.
    // The frequency will be re-confirmed accurate after calibration.
    log::debug!("sx1262_init: step 4 — SetRfFrequency(868.1 MHz)");
    radio
        .set_rf_frequency(calc_rf_freq(868_100_000.0, 32_000_000.0))
        .map_err(|_| LoraError::RadioInitFailed)?;
    log::debug!("sx1262_init: waiting busy after SetRfFrequency");
    wait_busy_spi("SetRfFrequency", radio)?;

    // ── Step 5: enable TCXO via DIO3 ─────────────────────────────────────────
    //
    // After this command the SX1262 SETS DeviceErrors.XOSC_START_ERR (bit 5).
    // This is EXPECTED (datasheet §13.3.6 Rev 2.2): the chip flags that the
    // oscillator has not yet started because the startup delay has not elapsed.
    //
    // Using 10 ms (not 5 ms) as a conservative startup margin.  The TCXO on the
    // Heltec V3 is rated for ≤2 ms startup, but the SX1262 startup-delay field
    // sets how long the chip waits before driving any subsequent oscillator-
    // dependent operation.  10 ms gives 5× headroom against a cold-start jitter.
    log::debug!(
        "sx1262_init: step 5 — SetDIO3AsTCXOCtrl(1.8 V, 10 ms) \
         [XOSC_START_ERR will be set — this is expected]"
    );
    radio
        .set_dio3_as_tcxo_ctrl(TcxoVoltage::Volt1_8, TcxoDelay::from_ms(10))
        .map_err(|_| LoraError::RadioInitFailed)?;
    log::debug!("sx1262_init: waiting busy after SetDIO3AsTCXOCtrl");
    wait_busy_spi("SetDIO3AsTCXOCtrl", radio)?;

    // Read and log device errors — expect XOSC_START_ERR (bit 5) here.
    match radio.get_device_errors() {
        Ok(errs) => log::debug!(
            "sx1262_init: device errors after SetDIO3AsTCXOCtrl = {:?} \
             [XOSC_START_ERR=true is normal here]",
            errs
        ),
        Err(_) => log::warn!("sx1262_init: could not read device errors after SetDIO3AsTCXOCtrl"),
    }

    // ── Step 6: clear XOSC_START_ERR ─────────────────────────────────────────
    //
    // MANDATORY before Calibrate.  If XOSC_START_ERR remains set when Calibrate
    // is issued, the calibration FSM interprets it as "oscillator not ready" and
    // aborts immediately, leaving BUSY asserted indefinitely.
    log::debug!("sx1262_init: step 6 — ClearDeviceErrors (mandatory before Calibrate)");
    radio
        .clear_device_errors()
        .map_err(|_| LoraError::RadioInitFailed)?;
    log::debug!("sx1262_init: waiting busy after ClearDeviceErrors");
    wait_busy_spi("ClearDeviceErrors", radio)?;

    // ── Step 7: calibrate all blocks ──────────────────────────────────────────
    //
    // Now that XOSC_START_ERR is cleared and the TCXO startup delay has elapsed
    // (the 10 ms delay was encoded in the SetDIO3AsTCXOCtrl delay field — the chip
    // waits internally), Calibrate will use the TCXO as the reference oscillator.
    log::debug!("sx1262_init: step 7 — Calibrate(all blocks, 0x7F)");
    radio
        .calibrate(CalibParam::all())
        .map_err(|_| LoraError::RadioInitFailed)?;
    log::debug!("sx1262_init: waiting busy after Calibrate (may take up to ~10 ms)");
    wait_busy_spi("Calibrate (may take up to ~10 ms)", radio)?;

    // Read device errors after calibration — all flags should be clear.
    match radio.get_device_errors() {
        Ok(errs) => {
            // DeviceErrors has no Into<u16>; check each flag individually.
            let any_err = errs.rc64k_calib_err()
                || errs.rc13m_calib_err()
                || errs.pll_calib_err()
                || errs.adc_calib_err()
                || errs.img_calib_err()
                || errs.xosc_start_err()
                || errs.pll_lock_err()
                || errs.pa_ramp_err();
            if any_err {
                log::error!(
                    "sx1262_init: device errors after Calibrate = {:?} — \
                     non-zero means calibration failed",
                    errs
                );
                return Err(LoraError::RadioInitFailed);
            }
            log::debug!(
                "sx1262_init: device errors after Calibrate = {:?} (clean)",
                errs
            );
        }
        Err(_) => log::warn!("sx1262_init: could not read device errors after Calibrate"),
    }

    // ── Step 8: calibrate image (frequency-band-specific) ────────────────────
    log::debug!("sx1262_init: step 8 — CalibrateImage(EU868 band: 0xD7..0xD8)");
    radio
        .calibrate_image(CalibImageFreq::from_rf_frequency(868_100_000))
        .map_err(|_| LoraError::RadioInitFailed)?;
    log::debug!("sx1262_init: waiting busy after CalibrateImage");
    wait_busy_spi("CalibrateImage", radio)?;

    // ── Step 9: PA config ─────────────────────────────────────────────────────
    log::debug!("sx1262_init: step 9 — SetPaConfig(duty=0x04, hp_max=0x07, SX1262)");
    radio
        .set_pa_config(
            PaConfig::default()
                .set_pa_duty_cycle(0x04)
                .set_hp_max(0x07)
                .set_device_sel(DeviceSel::SX1262),
        )
        .map_err(|_| LoraError::RadioInitFailed)?;
    log::debug!("sx1262_init: waiting busy after SetPaConfig");
    wait_busy_spi("SetPaConfig", radio)?;

    // ── Step 10: TX params ────────────────────────────────────────────────────
    log::debug!("sx1262_init: step 10 — SetTxParams(14 dBm, ramp=200 µs)");
    radio
        .set_tx_params(
            TxParams::default()
                .set_power_dbm(14)
                .set_ramp_time(RampTime::Ramp200u),
        )
        .map_err(|_| LoraError::RadioInitFailed)?;
    log::debug!("sx1262_init: waiting busy after SetTxParams");
    wait_busy_spi("SetTxParams", radio)?;

    // ── Step 11: buffer base addresses ───────────────────────────────────────
    log::debug!("sx1262_init: step 11 — SetBufferBaseAddress(tx=0x00, rx=0x00)");
    radio
        .set_buffer_base_address(0x00, 0x00)
        .map_err(|_| LoraError::RadioInitFailed)?;
    log::debug!("sx1262_init: waiting busy after SetBufferBaseAddress");
    wait_busy_spi("SetBufferBaseAddress", radio)?;

    // ── Step 12: modulation params (DR0: SF12/BW125/CR4-5, LDRO on) ──────────
    log::debug!("sx1262_init: step 12 — SetModulationParams(SF12/BW125/CR4-5/LDRO)");
    radio
        .set_mod_params(
            LoraModParams::default()
                .set_spread_factor(LoRaSpreadFactor::SF12)
                .set_bandwidth(LoRaBandWidth::BW125)
                .set_coding_rate(LoraCodingRate::CR4_5)
                .set_low_dr_opt(true)
                .into(),
        )
        .map_err(|_| LoraError::RadioInitFailed)?;
    log::debug!("sx1262_init: waiting busy after SetModulationParams");
    wait_busy_spi("SetModulationParams", radio)?;

    // ── Step 13: packet params ────────────────────────────────────────────────
    log::debug!("sx1262_init: step 13 — SetPacketParams(preamble=8, VarLen, CRCon, IQ=Std)");
    radio
        .set_packet_params(
            LoRaPacketParams::default()
                .set_preamble_len(8)
                .set_header_type(LoRaHeaderType::VarLen)
                .set_payload_len(255)
                .set_crc_type(LoRaCrcType::CrcOn)
                .set_invert_iq(LoRaInvertIq::Standard)
                .into(),
        )
        .map_err(|_| LoraError::RadioInitFailed)?;
    log::debug!("sx1262_init: waiting busy after SetPacketParams");
    wait_busy_spi("SetPacketParams", radio)?;

    // ── Step 14: DIO IRQ params ───────────────────────────────────────────────
    // Bits routed to DIO1 — these drive the join state machine.
    let dio1_mask = IrqMask::none()
        .combine(IrqMaskBit::TxDone)
        .combine(IrqMaskBit::RxDone)
        .combine(IrqMaskBit::CrcErr)
        .combine(IrqMaskBit::Timeout);
    // Global IRQ-record mask is wider so `get_irq_status()` can report whether the
    // radio saw a preamble / sync word / header during an RX window, even though
    // those bits are NOT routed to DIO1 (they would otherwise fire early and
    // disrupt the state machine). Used by the RX diagnostics in cancel_rx/receive.
    let record_mask = dio1_mask
        .combine(IrqMaskBit::PreambleDetected)
        .combine(IrqMaskBit::SyncwordValid)
        .combine(IrqMaskBit::HeaderValid)
        .combine(IrqMaskBit::HeaderError);
    log::debug!(
        "sx1262_init: step 14 — SetDioIrqParams(record=rx-diag, DIO1=TxDone|RxDone|CrcErr|Timeout)"
    );
    radio
        .set_dio_irq_params(record_mask, dio1_mask, IrqMask::none(), IrqMask::none())
        .map_err(|_| LoraError::RadioInitFailed)?;
    log::debug!("sx1262_init: waiting busy after SetDioIrqParams");
    wait_busy_spi("SetDioIrqParams", radio)?;

    // ── Step 15: DIO2 as RF switch ────────────────────────────────────────────
    log::debug!("sx1262_init: step 15 — SetDio2AsRfSwitchCtrl(enable)");
    radio
        .set_dio2_as_rf_switch_ctrl(true)
        .map_err(|_| LoraError::RadioInitFailed)?;
    log::debug!("sx1262_init: waiting busy after SetDio2AsRfSwitchCtrl");
    wait_busy_spi("SetDio2AsRfSwitchCtrl", radio)?;

    // ── Step 16: LoRaWAN public network sync word ─────────────────────────────
    // 0x3444 for SX1262/SX1261 on public networks (TTN).
    // NOT 0x34 as used by SX1276/SX1278 — the SX126x uses a 16-bit sync word.
    log::debug!("sx1262_init: step 16 — SetSyncWord(0x3444, LoRaWAN public)");
    radio
        .set_sync_word(0x3444)
        .map_err(|_| LoraError::RadioInitFailed)?;
    log::debug!("sx1262_init: waiting busy after SetSyncWord");
    wait_busy_spi("SetSyncWord", radio)?;

    // ── Final status check ────────────────────────────────────────────────────
    match radio.get_status() {
        Ok(status) => {
            // Status has no Into<u8>; use Debug which prints chip_mode + command_status.
            log::debug!("sx1262_init: complete — GetStatus = {:?}", status);
        }
        Err(_) => log::warn!("sx1262_init: could not read final status"),
    }

    Ok(())
}

// ─── LoraRadio impl ───────────────────────────────────────────────────────────

impl<'d, ANT> LoraRadio for EspIdfLoraRadio<'d, ANT>
where
    ANT: OutputPin<Error = GpioError>,
{
    type Error = LoraError;

    fn prepare_tx(&mut self, config: TxConfig, buf: &[u8]) -> Result<(), LoraError> {
        let mod_params = LoraModParams::default()
            .set_spread_factor(sf_to_sx126x(config.sf))
            .set_bandwidth(bw_to_sx126x(config.bw))
            .set_coding_rate(cr_to_sx126x(config.cr))
            .set_low_dr_opt(ldro_required(config.sf, config.bw))
            .into();
        self.radio
            .set_mod_params(mod_params)
            .map_err(|_| LoraError::TransmitFailed)?;

        let pkt_params = LoRaPacketParams::default()
            .set_preamble_len(8)
            .set_header_type(LoRaHeaderType::VarLen)
            .set_payload_len(buf.len() as u8)
            .set_crc_type(LoRaCrcType::CrcOn)
            .set_invert_iq(LoRaInvertIq::Standard)
            .into();
        self.radio
            .set_packet_params(pkt_params)
            .map_err(|_| LoraError::TransmitFailed)?;

        let rf_freq = calc_rf_freq(config.freq_hz as f32, 32_000_000.0);
        self.radio
            .set_rf_frequency(rf_freq)
            .map_err(|_| LoraError::TransmitFailed)?;

        self.radio
            .write_buffer(0x00, buf)
            .map_err(|_| LoraError::TransmitFailed)?;

        // Apply TX power from the lorawan-device request (ADR / join / retry levels).
        let tx_params = TxParams::default()
            .set_power_dbm(config.power_dbm)
            .set_ramp_time(RampTime::Ramp200u);
        self.radio
            .set_tx_params(tx_params)
            .map_err(|_| LoraError::TransmitFailed)?;

        // Start TX.  DIO1 fires when transmission is complete.
        // 6 000 ms is a conservative upper bound that covers SF12/BW125 at maximum
        // LoRaWAN payload length with margin.  Using 0 (infinite) risks an unrecoverable
        // hang if DIO1 never fires due to a wiring or IRQ-mask issue.
        self.tx_start = Some(std::time::Instant::now());
        self.radio
            .set_tx(RxTxTimeout::from_ms(6_000))
            .map_err(|_| LoraError::TransmitFailed)?;

        Ok(())
    }

    /// Poll for TX completion.
    ///
    /// Called from the `PhyRxTx::handle_event(Phy(()))` handler after DIO1 fires.
    /// The main loop clears `DIO1_FLAG` before delivering the `Phy` event, so
    /// this function reads IRQ status directly rather than re-checking the flag.
    fn transmit(&mut self) -> nb::Result<u32, LoraError> {
        let irq = self
            .radio
            .get_irq_status()
            .map_err(|_| nb::Error::Other(LoraError::IrqStatusReadFailed))?;
        self.radio
            .clear_irq_status(IrqMask::all())
            .map_err(|_| nb::Error::Other(LoraError::TransmitFailed))?;

        if irq.timeout() {
            self.tx_start = None;
            return Err(nb::Error::Other(LoraError::Timeout));
        }
        if !irq.tx_done() {
            return Err(nb::Error::WouldBlock);
        }

        let on_air_ms = self
            .tx_start
            .take()
            .map(|t| t.elapsed().as_millis() as u32)
            .unwrap_or(0);
        Ok(on_air_ms)
    }

    fn prepare_rx(&mut self, config: RxConfig, _window: RxWindow) -> Result<(), LoraError> {
        // `_window` is not used for hardware dispatch: lorawan-device already encodes the
        // correct frequency and data rate for RX1 vs RX2 in `config`. It is NOT logged —
        // lorawan-device does not expose which window this is, so the adapter can only pass
        // a fixed placeholder, and logging it would falsely claim every RX is RX1. The freq
        // (868.x vs 869.525) is the honest discriminator and is logged instead.
        log::info!(
            "prepare_rx: freq={}Hz sf={:?} bw={:?} cr={:?}",
            config.freq_hz,
            config.sf,
            config.bw,
            config.cr
        );
        let mod_params = LoraModParams::default()
            .set_spread_factor(sf_to_sx126x(config.sf))
            .set_bandwidth(bw_to_sx126x(config.bw))
            .set_coding_rate(cr_to_sx126x(config.cr))
            .set_low_dr_opt(ldro_required(config.sf, config.bw))
            .into();
        self.radio
            .set_mod_params(mod_params)
            .map_err(|_| LoraError::ReceiveFailed)?;

        // Downlinks always use inverted IQ — this differentiates them from uplinks.
        let pkt_params = LoRaPacketParams::default()
            .set_preamble_len(8)
            .set_header_type(LoRaHeaderType::VarLen)
            .set_payload_len(255)
            // LoRaWAN downlinks carry NO PHY-layer CRC. With CRC on, the SX1262
            // either treats the missing CRC bytes as payload or flags crc_err and
            // drops the frame, so RX must use CrcOff (TX keeps CrcOn).
            .set_crc_type(LoRaCrcType::CrcOff)
            .set_invert_iq(LoRaInvertIq::Inverted)
            .into();
        self.radio
            .set_packet_params(pkt_params)
            .map_err(|_| LoraError::ReceiveFailed)?;

        let rf_freq = calc_rf_freq(config.freq_hz as f32, 32_000_000.0);
        self.radio
            .set_rf_frequency(rf_freq)
            .map_err(|_| LoraError::ReceiveFailed)?;

        // Open the RX window immediately.  DIO1 fires when a packet arrives or the
        // window times out (SX1262 hardware timeout after RX_WINDOW_DURATION_MS).
        self.radio
            .set_rx(RxTxTimeout::from_ms(juggler::lora::RX_WINDOW_DURATION_MS))
            .map_err(|_| LoraError::ReceiveFailed)?;
        log::debug!(
            "prepare_rx: set_rx issued — RX window open for {}ms",
            juggler::lora::RX_WINDOW_DURATION_MS
        );

        Ok(())
    }

    /// Poll for a received packet.
    ///
    /// Called from the `PhyRxTx::handle_event(Phy(()))` handler after DIO1 fires.
    /// The main loop clears `DIO1_FLAG` before delivering the `Phy` event, so
    /// this function reads IRQ status directly rather than re-checking the flag.
    fn receive(&mut self, buf: &mut [u8]) -> nb::Result<(usize, RxQuality), LoraError> {
        let irq = self
            .radio
            .get_irq_status()
            .map_err(|_| nb::Error::Other(LoraError::IrqStatusReadFailed))?;
        log::debug!(
            "receive: IRQ before clear — preamble={} syncword={} header_valid={} \
             header_err={} rx_done={} crc_err={} timeout={}",
            irq.preamble_detected(),
            irq.syncword_valid(),
            irq.header_valid(),
            irq.header_error(),
            irq.rx_done(),
            irq.crc_err(),
            irq.timeout()
        );
        self.radio
            .clear_irq_status(IrqMask::all())
            .map_err(|_| nb::Error::Other(LoraError::ReceiveFailed))?;

        if irq.timeout() {
            // RX window expired with no packet — terminal outcome for this window.
            return Err(nb::Error::Other(LoraError::Timeout));
        }
        if irq.crc_err() {
            return Err(nb::Error::Other(LoraError::ReceiveFailed));
        }
        if !irq.rx_done() {
            return Err(nb::Error::WouldBlock);
        }

        let status = self
            .radio
            .get_rx_buffer_status()
            .map_err(|_| nb::Error::Other(LoraError::ReceiveFailed))?;
        let len = status.payload_length_rx() as usize;
        let offset = status.rx_start_buffer_pointer();

        // Reject frames larger than the caller's buffer.  A truncated LoRaWAN frame
        // always fails its MIC check, so passing partial data upward has no value.
        // In practice the lorawan-device adapter always passes a 256-byte buffer
        // (the LoRaWAN max PHY payload), so this guard is purely defensive.
        if len > buf.len() {
            log::warn!(
                "receive: payload {} bytes exceeds buffer {} bytes — dropping frame",
                len,
                buf.len()
            );
            return Err(nb::Error::Other(LoraError::ReceiveFailed));
        }

        self.radio
            .read_buffer(offset, &mut buf[..len])
            .map_err(|_| nb::Error::Other(LoraError::ReceiveFailed))?;

        let (rssi, snr) = self
            .radio
            .get_packet_status()
            .ok()
            .map(|s| (s.rssi_pkt() as i16, s.snr_pkt() as i8))
            .unwrap_or((0, 0));
        self.last_rssi = rssi;
        self.last_snr = snr;
        log::info!(
            "receive: RxDone len={} offset={} rssi={} snr={}",
            len,
            offset,
            rssi,
            snr
        );

        Ok((len, RxQuality { rssi, snr }))
    }

    fn set_frequency(&mut self, freq_hz: u32) -> Result<(), LoraError> {
        let rf_freq = calc_rf_freq(freq_hz as f32, 32_000_000.0);
        self.radio
            .set_rf_frequency(rf_freq)
            .map_err(|_| LoraError::RadioInitFailed)
    }

    fn rx_quality(&self) -> RxQuality {
        RxQuality {
            rssi: self.last_rssi,
            snr: self.last_snr,
        }
    }

    fn rx_window_offset_ms(&self) -> i32 {
        juggler::lora::RX_WINDOW_OFFSET_MS
    }

    fn rx_window_duration_ms(&self) -> u32 {
        juggler::lora::RX_WINDOW_DURATION_MS
    }
}

// ─── SF/BW/CR mapping helpers ─────────────────────────────────────────────────

fn sf_to_sx126x(sf: SpreadingFactor) -> LoRaSpreadFactor {
    match sf {
        SpreadingFactor::SF7 => LoRaSpreadFactor::SF7,
        SpreadingFactor::SF8 => LoRaSpreadFactor::SF8,
        SpreadingFactor::SF9 => LoRaSpreadFactor::SF9,
        SpreadingFactor::SF10 => LoRaSpreadFactor::SF10,
        SpreadingFactor::SF11 => LoRaSpreadFactor::SF11,
        SpreadingFactor::SF12 => LoRaSpreadFactor::SF12,
    }
}

fn bw_to_sx126x(bw: Bandwidth) -> LoRaBandWidth {
    match bw {
        Bandwidth::BW125 => LoRaBandWidth::BW125,
        Bandwidth::BW250 => LoRaBandWidth::BW250,
        Bandwidth::BW500 => LoRaBandWidth::BW500,
    }
}

fn cr_to_sx126x(cr: CodingRate) -> LoraCodingRate {
    match cr {
        CodingRate::Cr45 => LoraCodingRate::CR4_5,
        CodingRate::Cr46 => LoraCodingRate::CR4_6,
        CodingRate::Cr47 => LoraCodingRate::CR4_7,
        CodingRate::Cr48 => LoraCodingRate::CR4_8,
    }
}

/// LDRO is required when: BW=125 kHz and SF≥11, or BW=250 kHz and SF=12.
fn ldro_required(sf: SpreadingFactor, bw: Bandwidth) -> bool {
    matches!(
        (sf, bw),
        (
            SpreadingFactor::SF11 | SpreadingFactor::SF12,
            Bandwidth::BW125
        ) | (SpreadingFactor::SF12, Bandwidth::BW250)
    )
}

// ─── LoraRadioAdapter (PhyRxTx bridge) ────────────────────────────────────────

use lorawan_device::{
    nb_device::radio::{
        Event as LdRadioEvent, PhyRxTx, Response as LdRadioResponse, RxQuality as LdRxQuality,
    },
    Timings,
};

/// Internal state of the radio operation currently in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RadioOp {
    Idle,
    Txing,
    Rxing,
}

/// Bridges [`EspIdfLoraRadio`] to [`lorawan_device`]'s `PhyRxTx + Timings` traits.
///
/// The `lorawan-device` nb state machine drives the radio via `PhyRxTx::handle_event`.
/// This adapter translates the lorawan-device event types to the lower-level
/// `LoraRadio` calls and manages the RX buffer used by `get_received_packet`.
pub struct LoraRadioAdapter<'d, ANT> {
    radio: EspIdfLoraRadio<'d, ANT>,
    rx_buf: [u8; 256],
    rx_len: usize,
    op: RadioOp,
}

impl<'d, ANT> LoraRadioAdapter<'d, ANT>
where
    ANT: OutputPin<Error = GpioError>,
{
    /// Wrap an initialised [`EspIdfLoraRadio`] in the lorawan-device adapter.
    pub fn new(radio: EspIdfLoraRadio<'d, ANT>) -> Self {
        Self {
            radio,
            rx_buf: [0u8; 256],
            rx_len: 0,
            op: RadioOp::Idle,
        }
    }

    /// Poll the radio for a pending DIO1-relevant IRQ — see
    /// [`EspIdfLoraRadio::irq_pending`]. The event loop calls this each tick so
    /// it does not depend on the one-shot DIO1 GPIO interrupt.
    pub fn irq_pending(&mut self) -> bool {
        self.radio.irq_pending()
    }
}

impl<'d, ANT> core::fmt::Debug for LoraRadioAdapter<'d, ANT>
where
    ANT: OutputPin<Error = GpioError>,
{
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LoraRadioAdapter")
            .field("op", &self.op)
            .field("rx_len", &self.rx_len)
            .finish_non_exhaustive()
    }
}

/// Map `lora-modulation` types (used by lorawan-device) to `lora_pure` types.
///
/// Returns `Err(LoraError::InvalidRfConfig)` for any unrecognised parameter value.
fn map_rf_config(
    rf: &lorawan_device::nb_device::radio::RfConfig,
) -> Result<(u32, SpreadingFactor, Bandwidth, CodingRate), LoraError> {
    use lora_modulation::{Bandwidth as LmBw, CodingRate as LmCr, SpreadingFactor as LmSf};

    let sf = match rf.bb.sf {
        LmSf::_7 => SpreadingFactor::SF7,
        LmSf::_8 => SpreadingFactor::SF8,
        LmSf::_9 => SpreadingFactor::SF9,
        LmSf::_10 => SpreadingFactor::SF10,
        LmSf::_11 => SpreadingFactor::SF11,
        LmSf::_12 => SpreadingFactor::SF12,
        _ => return Err(LoraError::InvalidRfConfig),
    };
    let bw = match rf.bb.bw {
        LmBw::_125KHz => Bandwidth::BW125,
        LmBw::_250KHz => Bandwidth::BW250,
        LmBw::_500KHz => Bandwidth::BW500,
        _ => return Err(LoraError::InvalidRfConfig),
    };
    let cr = match rf.bb.cr {
        LmCr::_4_5 => CodingRate::Cr45,
        LmCr::_4_6 => CodingRate::Cr46,
        LmCr::_4_7 => CodingRate::Cr47,
        LmCr::_4_8 => CodingRate::Cr48,
    };
    Ok((rf.frequency, sf, bw, cr))
}

impl<'d, ANT> PhyRxTx for LoraRadioAdapter<'d, ANT>
where
    ANT: OutputPin<Error = GpioError>,
{
    /// The DIO1 interrupt fires with `()` — the adapter clears the flag internally.
    type PhyEvent = ();
    type PhyError = LoraError;
    type PhyResponse = ();

    const MAX_RADIO_POWER: u8 = 22;

    fn get_mut_radio(&mut self) -> &mut Self {
        self
    }

    fn get_received_packet(&mut self) -> &mut [u8] {
        &mut self.rx_buf[..self.rx_len]
    }

    fn handle_event(
        &mut self,
        event: LdRadioEvent<Self>,
    ) -> Result<LdRadioResponse<Self>, Self::PhyError> {
        match event {
            LdRadioEvent::TxRequest(tx_config, buf) => {
                let (freq, sf, bw, cr) = map_rf_config(&tx_config.rf)?;
                let lp_tx = TxConfig {
                    freq_hz: freq,
                    sf,
                    bw,
                    cr,
                    power_dbm: tx_config.pw,
                };
                self.radio.prepare_tx(lp_tx, buf)?;
                self.op = RadioOp::Txing;
                Ok(LdRadioResponse::Txing)
            }

            LdRadioEvent::RxRequest(rf_config) => {
                // Reset stale RX data from a previous window before opening a new one.
                self.rx_len = 0;
                let (freq, sf, bw, cr) = map_rf_config(&rf_config)?;
                let lp_rx = RxConfig {
                    freq_hz: freq,
                    sf,
                    bw,
                    cr,
                };
                // lorawan-device 0.12 `RxRequest` does not expose which window (RX1/RX2)
                // is being opened — the correct frequency and DR are already encoded in
                // `rf_config`.  `RxWindow::Rx1` is an unused placeholder (`prepare_rx`
                // ignores it and does not log it); it does not affect hardware configuration.
                self.radio.prepare_rx(lp_rx, RxWindow::Rx1)?;
                self.op = RadioOp::Rxing;
                Ok(LdRadioResponse::Rxing)
            }

            LdRadioEvent::CancelRx => {
                // Stop any in-progress RX and clear stale packet data.
                log::debug!("adapter: CancelRx — closing RX window");
                self.radio.cancel_rx();
                self.rx_len = 0;
                self.op = RadioOp::Idle;
                Ok(LdRadioResponse::Idle)
            }

            LdRadioEvent::Phy(()) => match self.op {
                RadioOp::Txing => match self.radio.transmit() {
                    Ok(on_air_ms) => {
                        self.op = RadioOp::Idle;
                        Ok(LdRadioResponse::TxDone(on_air_ms))
                    }
                    Err(nb::Error::WouldBlock) => Ok(LdRadioResponse::Txing),
                    Err(nb::Error::Other(e)) => Err(e),
                },
                RadioOp::Rxing => match self.radio.receive(&mut self.rx_buf) {
                    Ok((len, quality)) => {
                        self.rx_len = len;
                        self.op = RadioOp::Idle;
                        Ok(LdRadioResponse::RxDone(LdRxQuality::new(
                            quality.rssi,
                            quality.snr,
                        )))
                    }
                    Err(nb::Error::WouldBlock) => Ok(LdRadioResponse::Rxing),
                    Err(nb::Error::Other(LoraError::Timeout)) => {
                        // Normal outcome: RX window closed with no downlink received.
                        // Return Idle so lorawan-device can proceed (e.g. open RX2 or retry).
                        self.rx_len = 0;
                        self.op = RadioOp::Idle;
                        Ok(LdRadioResponse::Idle)
                    }
                    Err(nb::Error::Other(e)) => Err(e),
                },
                RadioOp::Idle => Ok(LdRadioResponse::Idle),
            },
        }
    }
}

impl<'d, ANT> Timings for LoraRadioAdapter<'d, ANT>
where
    ANT: OutputPin<Error = GpioError>,
{
    fn get_rx_window_offset_ms(&self) -> i32 {
        self.radio.rx_window_offset_ms()
    }

    fn get_rx_window_duration_ms(&self) -> u32 {
        self.radio.rx_window_duration_ms()
    }
}
