//! SX1262 radio driver for the Heltec WiFi LoRa 32 V3.
//!
//! [`EspIdfLoraRadio`] implements [`lora_pure::LoraRadio`] using `sx126x 0.3` and
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

use embedded_hal::digital::OutputPin;
use esp_idf_hal::{
    gpio::{GpioError, Input, Output, PinDriver},
    spi::{SpiDeviceDriver, SpiDriver},
};
use sx126x::{
    calc_rf_freq,
    conf::Config,
    op::{
        calib::CalibParam,
        irq::{IrqMask, IrqMaskBit},
        modulation::{LoRaBandWidth, LoRaSpreadFactor, LoraCodingRate, LoraModParams},
        packet::{LoRaCrcType, LoRaHeaderType, LoRaInvertIq, LoRaPacketParams, PacketType},
        rxtx::{DeviceSel, PaConfig, RampTime, RxTxTimeout, TxParams},
        tcxo::{TcxoDelay, TcxoVoltage},
        StandbyConfig,
    },
    SX126x,
};

use lora_pure::config::LoraConfig;
use lora_pure::{
    Bandwidth, CodingRate, LoraRadio, RxConfig, RxQuality, RxWindow, SpreadingFactor, TxConfig,
};

// ─── Error type ───────────────────────────────────────────────────────────────

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

type SpiBus<'d> = SpiDeviceDriver<'d, SpiDriver<'d>>;
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
        spi: SpiBus<'d>,
        rst: RstPin<'d>,
        busy: BusyPin<'d>,
        ant: ANT,
        dio1: Dio1Pin<'d>,
        _config: &LoraConfig,
    ) -> Result<Self, LoraError> {
        let mut radio = SX126x::new(spi, (rst, busy, ant, dio1));
        radio
            .init(heltec_v3_eu868_config())
            .map_err(|_| LoraError::RadioInitFailed)?;
        log::info!("SX1262 initialized");
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
        let _ = self.radio.set_standby(StandbyConfig::StbyRc);
        let _ = self.radio.clear_irq_status(IrqMask::all());
        self.tx_start = None;
    }
}

/// Build the EU868 `Config` for the Heltec WiFi LoRa 32 V3.
///
/// Initialises at DR0 (SF12/BW125/CR4-5) with TCXO enabled.
/// Callers may re-issue `set_mod_params` to switch data rate before each frame.
fn heltec_v3_eu868_config() -> Config {
    let irq_mask = IrqMask::none()
        .combine(IrqMaskBit::TxDone)
        .combine(IrqMaskBit::RxDone)
        .combine(IrqMaskBit::CrcErr)
        .combine(IrqMaskBit::Timeout);

    Config {
        packet_type: PacketType::LoRa,

        // LoRaWAN public network sync word for SX1262.
        // 0x3444 for SX1262 — NOT 0x34 as used by SX1276.
        sync_word: 0x3444,

        calib_param: CalibParam::all(),

        // EU868 DR0: SF12, BW125, CR4/5.
        // LDRO is mandatory at SF12/BW125 — symbol duration is ~524 ms (> 16 ms threshold).
        mod_params: LoraModParams::default()
            .set_spread_factor(LoRaSpreadFactor::SF12)
            .set_bandwidth(LoRaBandWidth::BW125)
            .set_coding_rate(LoraCodingRate::CR4_5)
            .set_low_dr_opt(true)
            .into(),

        pa_config: PaConfig::default()
            .set_pa_duty_cycle(0x04)
            .set_hp_max(0x07)
            .set_device_sel(DeviceSel::SX1262),

        // Initial packet params for uplinks (IQ=Standard).
        // Re-issue set_packet_params() with IQ=Inverted before each RX window.
        packet_params: Some(
            LoRaPacketParams::default()
                .set_preamble_len(8)
                .set_header_type(LoRaHeaderType::VarLen)
                .set_payload_len(255)
                .set_crc_type(LoRaCrcType::CrcOn)
                .set_invert_iq(LoRaInvertIq::Standard)
                .into(),
        ),

        tx_params: TxParams::default()
            .set_power_dbm(14)
            .set_ramp_time(RampTime::Ramp200u),

        dio1_irq_mask: irq_mask,
        dio2_irq_mask: IrqMask::none(),
        dio3_irq_mask: IrqMask::none(),

        // set_rf_frequency() takes a PLL register value — use calc_rf_freq(), never raw Hz.
        rf_freq: calc_rf_freq(868_100_000.0, 32_000_000.0),
        rf_frequency: 868_100_000,

        // CRITICAL: without tcxo_opts, init() succeeds but produces no RF output.
        tcxo_opts: Some((TcxoVoltage::Volt1_8, TcxoDelay::from_ms(5))),
    }
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

    fn prepare_rx(&mut self, config: RxConfig, window: RxWindow) -> Result<(), LoraError> {
        // `window` is informational only here: lorawan-device already encodes the correct
        // frequency and data rate for RX1 vs RX2 in `config`, so no further dispatch is needed.
        // The adapter passes it through for logging.
        log::debug!(
            "prepare_rx: window={:?} freq={}Hz sf={:?} bw={:?}",
            window,
            config.freq_hz,
            config.sf,
            config.bw
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
            .set_crc_type(LoRaCrcType::CrcOn)
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
            .set_rx(RxTxTimeout::from_ms(lora_pure::RX_WINDOW_DURATION_MS))
            .map_err(|_| LoraError::ReceiveFailed)?;

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
        lora_pure::RX_WINDOW_OFFSET_MS
    }

    fn rx_window_duration_ms(&self) -> u32 {
        lora_pure::RX_WINDOW_DURATION_MS
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
                // `rf_config`.  `RxWindow::Rx1` is passed as a placeholder for logging;
                // it does not affect hardware configuration.
                self.radio.prepare_rx(lp_rx, RxWindow::Rx1)?;
                self.op = RadioOp::Rxing;
                Ok(LdRadioResponse::Rxing)
            }

            LdRadioEvent::CancelRx => {
                // Stop any in-progress RX and clear stale packet data.
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
