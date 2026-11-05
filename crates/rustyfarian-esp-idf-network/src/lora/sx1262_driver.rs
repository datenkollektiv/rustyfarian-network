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
//! # Bring-up
//!
//! The SX1262 bring-up order (TCXO → `ClearDeviceErrors` → `Calibrate`) lives in
//! [`juggler::lora::sx1262::init`], shared with the esp-hal tier (ADR 023 rung 1).
//! This adapter keeps the tier facts: the `FullDuplexDevice` SPI shim, the NRST
//! pulse outside a critical section (bug 007), and the FreeRTOS delay.
//!
//! # LoRaWAN PHY policy
//!
//! Preamble, IQ inversion, CRC and the RX payload limit come from the
//! `juggler::lora` constants (uplinks standard IQ with CRC, downlinks inverted
//! IQ without CRC); LDRO from [`juggler::lora::ldro_required`] and IRQ meaning
//! from [`juggler::lora::classify_irq`].
//!
//! # lorawan-device bridge
//!
//! [`LoraRadioAdapter`] is [`juggler::lora::LorawanPhy`] over this radio.

use embedded_hal::{
    digital::OutputPin,
    spi::{ErrorType, Operation, SpiDevice},
};
use esp_idf_hal::{
    delay::{Ets, FreeRtos},
    gpio::{GpioError, Input, Output, PinDriver},
    spi::{SpiDeviceDriver, SpiDriver},
};
use sx126x::{
    op::{
        irq::IrqMask,
        modulation::{LoRaBandWidth, LoRaSpreadFactor, LoraCodingRate, LoraModParams},
        packet::{LoRaCrcType, LoRaHeaderType, LoRaInvertIq, LoRaPacketParams},
        rxtx::{RampTime, RxTxTimeout, TxParams},
        StandbyConfig,
    },
    SX126x,
};

use juggler::lora::config::LoraConfig;
use juggler::lora::sx1262::{self, irq_flags, rf_freq_word, InitError};
use juggler::lora::{
    classify_irq, ldro_required, Bandwidth, CodingRate, IrqOutcome, LoraRadio, LorawanPhy,
    RadioErrorKind, RxConfig, RxQuality, RxWindow, SpreadingFactor, TxConfig, DOWNLINK_CRC_ON,
    DOWNLINK_IQ_INVERTED, LORAWAN_PREAMBLE_LEN, RX_PAYLOAD_MAX, UPLINK_CRC_ON, UPLINK_IQ_INVERTED,
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
        // No transaction reaches this wrapper from inside critical_section::with:
        // the only critical section in sx126x 0.3 is reset(), which this driver
        // never calls (see `pulse_nrst`).  The firmware picks the provider —
        // esp-idf-hal's is a recursive FreeRTOS mutex, others mask interrupts —
        // and under a masking provider neither the blocking SPI bus acquire nor
        // the slow path's allocation is allowed, so SPI stays outside them.
        //
        // All Write, TransferInPlace, and DelayNs operations take this path and
        // pay zero allocation cost.
        if !operations.iter().any(|op| matches!(op, Operation::Read(_))) {
            return self.inner.transaction(operations);
        }

        // Slow path: at least one Read is present.  All Read call sites in
        // sx126x 0.3 (read_register, read_buffer, get_irq_status,
        // get_packet_status, get_rx_buffer_status, get_device_errors) run
        // outside critical_section::with, so Vec allocation here is safe.
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

impl RadioErrorKind for LoraError {
    fn invalid_rf_config() -> Self {
        Self::InvalidRfConfig
    }

    fn is_rx_timeout(&self) -> bool {
        matches!(self, Self::Timeout)
    }
}

impl From<InitError> for LoraError {
    fn from(e: InitError) -> Self {
        match e {
            InitError::BusyTimeout(_) => Self::BusyTimeout,
            InitError::Command(_) | InitError::CalibrationFailed => Self::RadioInitFailed,
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
    /// Pulses NRST, then runs the shared bring-up [`juggler::lora::sx1262::init`]:
    /// the TCXO → `ClearDeviceErrors` → `Calibrate` order (datasheet §9.6 + §13.3.6),
    /// with every BUSY wait bounded to [`sx1262::DEFAULT_BUSY_POLL_BOUND`] polls
    /// that yield to the FreeRTOS scheduler through `FreeRtos::delay_ms(1)`, so
    /// `IDLE0` is never starved and the 5-second task watchdog never fires.
    /// Each step is logged at debug level before it is issued, so the last line
    /// printed before a stall names the stalling command.
    ///
    /// # Parameters
    ///
    /// - `spi` — SPI device driver configured for SPI mode 0 at 8 MHz with GPIO 8 as CS.
    /// - `rst` — GPIO 12 configured as output (active-low reset).
    /// - `busy` — GPIO 13 configured as input.
    /// - `ant` — spare output GPIO held high; DIO2 controls the RF switch internally.
    /// - `dio1` — GPIO 14 configured as input; the caller's ISR sets a shared flag on
    ///   rising edge, which the main loop delivers as a `Phy(())` event.
    /// - `config` — its `region` selects the bring-up RF and image-calibration band;
    ///   the credentials are used by the LoRaWAN layer, not the radio driver.
    pub fn new(
        spi: SpiDeviceDriver<'d, SpiDriver<'d>>,
        mut rst: RstPin<'d>,
        busy: BusyPin<'d>,
        ant: ANT,
        dio1: Dio1Pin<'d>,
        config: &LoraConfig,
    ) -> Result<Self, LoraError> {
        // Wrap the SPI device in the full-duplex compatibility shim before handing it
        // to sx126x.  See `FullDuplexDevice` for a detailed explanation of why this
        // is required.
        let spi = FullDuplexDevice::new(spi);
        pulse_nrst(&mut rst)?;
        let mut radio = SX126x::new(spi, (rst, busy, ant, dio1));

        sx1262::init(
            &mut radio,
            &mut FreeRtos,
            config,
            sx1262::DEFAULT_BUSY_POLL_BOUND,
        )?;

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
    /// Best effort: errors are ignored.
    #[deprecated(
        since = "0.7.0",
        note = "use the LoraRadio trait method, `LoraRadio::cancel_rx(&mut radio)`, which \
                reports the error"
    )]
    pub fn cancel_rx(&mut self) {
        let _ = LoraRadio::cancel_rx(self);
    }

    /// Return `true` if a TxDone, RxDone, Timeout or CrcErr IRQ is latched in the
    /// radio's IRQ register, WITHOUT clearing it; `false` when the read fails.
    #[deprecated(
        since = "0.7.0",
        note = "use LorawanPhy::irq_pending on the LoraRadioAdapter, or the LoraRadio trait \
                method `LoraRadio::irq_pending(&mut radio)`, which reports the error"
    )]
    pub fn irq_pending(&mut self) -> bool {
        LoraRadio::irq_pending(self).unwrap_or(false)
    }
}

// ─── Hardware reset ──────────────────────────────────────────────────────────

/// Reset the SX1262 by pulling NRST low for 200 µs (datasheet §8.1: typically 100 µs).
///
/// Replaces `sx126x::reset()`, which wraps an SPI `DelayNs` transaction in
/// `critical_section::with`.  The esp-idf-hal SPI transaction blocks on
/// `spi_device_acquire_bus`, which must not run inside an interrupt-masking
/// critical section, so the pulse is driven here without one (bug 007).
fn pulse_nrst(rst: &mut RstPin<'_>) -> Result<(), LoraError> {
    rst.set_low().map_err(|_| LoraError::RadioInitFailed)?;
    Ets::delay_us(200);
    rst.set_high().map_err(|_| LoraError::RadioInitFailed)
}

// ─── LorawanPhy bridge ───────────────────────────────────────────────────────

/// Bridges [`EspIdfLoraRadio`] to `lorawan-device`'s `PhyRxTx + Timings` traits.
///
/// The shared [`juggler::lora::LorawanPhy`] over this radio; kept at this path so
/// existing firmware keeps compiling.
pub type LoraRadioAdapter<'d, ANT> = LorawanPhy<EspIdfLoraRadio<'d, ANT>>;

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
            .set_preamble_len(LORAWAN_PREAMBLE_LEN)
            .set_header_type(LoRaHeaderType::VarLen)
            .set_payload_len(buf.len() as u8)
            .set_crc_type(crc_type(UPLINK_CRC_ON))
            .set_invert_iq(invert_iq(UPLINK_IQ_INVERTED))
            .into();
        self.radio
            .set_packet_params(pkt_params)
            .map_err(|_| LoraError::TransmitFailed)?;

        let rf_freq = rf_freq_word(config.freq_hz);
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

        match classify_irq(irq_flags(irq)) {
            IrqOutcome::Timeout => {
                self.tx_start = None;
                return Err(nb::Error::Other(LoraError::Timeout));
            }
            IrqOutcome::TxDone => {}
            _ => return Err(nb::Error::WouldBlock),
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

        // Downlinks use inverted IQ and carry no PHY-layer CRC (see `DOWNLINK_CRC_ON`).
        let pkt_params = LoRaPacketParams::default()
            .set_preamble_len(LORAWAN_PREAMBLE_LEN)
            .set_header_type(LoRaHeaderType::VarLen)
            .set_payload_len(RX_PAYLOAD_MAX)
            .set_crc_type(crc_type(DOWNLINK_CRC_ON))
            .set_invert_iq(invert_iq(DOWNLINK_IQ_INVERTED))
            .into();
        self.radio
            .set_packet_params(pkt_params)
            .map_err(|_| LoraError::ReceiveFailed)?;

        let rf_freq = rf_freq_word(config.freq_hz);
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

        match classify_irq(irq_flags(irq)) {
            // RX window expired with no packet — terminal outcome for this window.
            IrqOutcome::Timeout => return Err(nb::Error::Other(LoraError::Timeout)),
            IrqOutcome::CrcError => return Err(nb::Error::Other(LoraError::ReceiveFailed)),
            IrqOutcome::RxDone => {}
            IrqOutcome::None | IrqOutcome::TxDone => return Err(nb::Error::WouldBlock),
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
        let rf_freq = rf_freq_word(freq_hz);
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

    /// Return the radio to RC-oscillator standby, aborting any in-progress RX or TX.
    fn cancel_rx(&mut self) -> Result<(), LoraError> {
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
        // Both commands are issued even if the first fails.
        let standby = self.radio.set_standby(StandbyConfig::StbyRc);
        let clear = self.radio.clear_irq_status(IrqMask::all());
        self.tx_start = None;
        standby.and(clear).map_err(|_| LoraError::ReceiveFailed)
    }

    /// Return `true` if a TxDone, RxDone, Timeout or CrcErr IRQ is latched in the
    /// radio's IRQ register, WITHOUT clearing it.
    ///
    /// The event loop polls this over SPI instead of relying on the DIO1 GPIO
    /// edge interrupt. `esp-idf-hal` `PinDriver` interrupts are one-shot — they
    /// auto-disable on each firing and must be re-armed from a non-ISR context —
    /// and the DIO1 pin is owned by the inner `SX126x`, so it cannot be re-armed
    /// from the loop. The result: only the first edge (TxDone) is ever delivered
    /// by interrupt, and RxDone is missed. Polling `GetIrqStatus` is immune to
    /// this and is safe to issue while the radio is in RX.
    fn irq_pending(&mut self) -> Result<bool, LoraError> {
        self.radio
            .get_irq_status()
            .map(|irq| classify_irq(irq_flags(irq)) != IrqOutcome::None)
            .map_err(|_| LoraError::IrqStatusReadFailed)
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

fn crc_type(on: bool) -> LoRaCrcType {
    if on {
        LoRaCrcType::CrcOn
    } else {
        LoRaCrcType::CrcOff
    }
}

fn invert_iq(inverted: bool) -> LoRaInvertIq {
    if inverted {
        LoRaInvertIq::Inverted
    } else {
        LoRaInvertIq::Standard
    }
}
