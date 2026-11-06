//! SX1262 bring-up as a rung-1 generic driver (ADR 023): one step order, shared
//! by both tiers, generic over `embedded-hal` 1.0 through `sx126x` 0.3.
//!
//! The tier keeps what is tier-specific: the SPI device it hands to `sx126x`
//! (the ESP-IDF tier wraps it in `FullDuplexDevice` for the ESP32-S3 null-TX
//! quirk), the NRST pulse (`sx126x::reset` runs inside `critical_section::with`,
//! bug 007), and the delay provider.
//!
//! # TCXO bring-up order (SX1262 datasheet §9.6 + §13.3.6)
//!
//! At POR the chip auto-calibrates using the internal RC oscillator. When
//! `SetDIO3AsTCXOCtrl` is later issued, the chip immediately asserts
//! `XOSC_START_ERR` (DeviceErrors bit 5) because the crystal has not had time
//! to stabilise — **this is expected and documented behaviour**.
//!
//! Calling `Calibrate` while `XOSC_START_ERR` is set causes the calibration FSM
//! to abort, which leaves BUSY high indefinitely. The mandatory order:
//!
//! ```text
//! SetDIO3AsTCXOCtrl(1.8 V, 10 ms)   // power the TCXO; XOSC_START_ERR is set
//! wait_busy                          // chip acknowledges the command
//! ClearDeviceErrors()                // mandatory: clear XOSC_START_ERR
//! wait_busy
//! Calibrate(0x7F)                    // re-calibrate all blocks with the TCXO
//! wait_busy
//! ```
//!
//! `sx126x` 0.3's own `init()` calls `calibrate` right after
//! `set_dio3_as_tcxo_ctrl` without the `ClearDeviceErrors`; that is the root
//! cause of the BUSY-stuck-forever hang on the Heltec V3. The order is confirmed
//! by the Semtech reference driver and RadioLib issue #89.
//!
//! # Bounded BUSY wait
//!
//! `sx126x`'s `wait_on_busy()` spins forever. [`init`] instead polls
//! `GetStatus` over SPI (the BUSY pin is owned by `SX126x` and not exposed),
//! sleeps 1 ms through the caller's delay between polls so an RTOS scheduler can
//! run, and gives up after `busy_poll_bound` polls with [`InitError::BusyTimeout`].

use embedded_hal::delay::DelayNs;
use embedded_hal::digital::{InputPin, OutputPin};
use embedded_hal::spi::SpiDevice;
use sx126x::{
    calc_rf_freq,
    op::{
        calib::{CalibImageFreq, CalibParam},
        irq::{IrqMask, IrqMaskBit, IrqStatus},
        modulation::{LoRaBandWidth, LoRaSpreadFactor, LoraCodingRate, LoraModParams},
        packet::{LoRaCrcType, LoRaHeaderType, LoRaInvertIq, LoRaPacketParams, PacketType},
        rxtx::{DeviceSel, PaConfig, RampTime, TxParams},
        tcxo::{TcxoDelay, TcxoVoltage},
        StandbyConfig,
    },
};

/// The `sx126x` 0.3 driver type [`init`] runs on, re-exported so a tier or
/// firmware can build it without a direct `sx126x` dependency.
pub use sx126x::SX126x;

use super::{
    LoraConfig, IRQ_CRC_ERR, IRQ_RX_DONE, IRQ_TIMEOUT, IRQ_TX_DONE, LORAWAN_PREAMBLE_LEN,
    LORAWAN_PUBLIC_SYNC_WORD, RX_PAYLOAD_MAX,
};

/// SX1262 crystal / TCXO reference frequency on the Heltec V3, in Hz.
pub const XTAL_HZ: u32 = 32_000_000;

/// Default number of 1 ms BUSY polls before [`init`] declares a timeout.
///
/// 500 polls × 1 ms = 500 ms per command. The longest legitimate busy interval
/// is `Calibrate` after the TCXO startup delay (10 ms TCXO + ~3.5 ms calibration
/// FSM), well under 100 ms on a healthy chip; 500 ms gives a 5× margin and stays
/// far below the ESP-IDF 5 s task-watchdog period.
pub const DEFAULT_BUSY_POLL_BOUND: u32 = 500;

/// TCXO startup delay encoded in `SetDIO3AsTCXOCtrl`, in ms.
///
/// The Heltec V3 TCXO is rated for ≤2 ms startup; 10 ms (not 5 ms) gives 5×
/// headroom against cold-start jitter.
pub const TCXO_STARTUP_MS: u32 = 10;

/// TX power programmed during bring-up, in dBm (each uplink sets its own).
const INIT_TX_POWER_DBM: i8 = 14;

/// One step of the SX1262 bring-up, in [`BRING_UP_ORDER`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitStep {
    /// Wait out the hardware reset the tier pulsed on NRST.
    Reset,
    /// `SetStandby(STDBY_RC)`.
    SetStandby,
    /// `SetPacketType(LoRa)`.
    SetPacketType,
    /// `SetRfFrequency` to the region's default RF.
    SetRfFrequency,
    /// `SetDIO3AsTCXOCtrl(1.8 V, 10 ms)`; sets `XOSC_START_ERR` by design.
    SetTcxo,
    /// `ClearDeviceErrors`, mandatory before `Calibrate`.
    ClearDeviceErrors,
    /// `Calibrate(all blocks)`.
    Calibrate,
    /// `CalibrateImage` for the region's band.
    CalibrateImage,
    /// `SetPaConfig` for the SX1262 high-power PA.
    SetPaConfig,
    /// `SetTxParams(14 dBm, 200 µs ramp)`.
    SetTxParams,
    /// `SetBufferBaseAddress(0, 0)`.
    SetBufferBaseAddress,
    /// `SetModulationParams` (DR0: SF12/BW125/CR4-5, LDRO on).
    SetModulationParams,
    /// `SetPacketParams` (LoRaWAN preamble, variable length, CRC on, standard IQ).
    SetPacketParams,
    /// `SetDioIrqParams` (DIO1 = TxDone | RxDone | CrcErr | Timeout).
    SetDioIrqParams,
    /// `SetDio2AsRfSwitchCtrl(enable)`.
    SetDio2AsRfSwitch,
    /// Write the LoRaWAN public sync word.
    SetSyncWord,
}

impl InitStep {
    /// The SX126x command opcode this step issues, or `None` for [`InitStep::Reset`].
    pub const fn opcode(self) -> Option<u8> {
        match self {
            Self::Reset => None,
            Self::SetStandby => Some(0x80),
            Self::SetPacketType => Some(0x8A),
            Self::SetRfFrequency => Some(0x86),
            Self::SetTcxo => Some(0x97),
            Self::ClearDeviceErrors => Some(0x07),
            Self::Calibrate => Some(0x89),
            Self::CalibrateImage => Some(0x98),
            Self::SetPaConfig => Some(0x95),
            Self::SetTxParams => Some(0x8E),
            Self::SetBufferBaseAddress => Some(0x8F),
            Self::SetModulationParams => Some(0x8B),
            Self::SetPacketParams => Some(0x8C),
            Self::SetDioIrqParams => Some(0x08),
            Self::SetDio2AsRfSwitch => Some(0x9D),
            // WriteRegister(LoRaSyncWordMsb).
            Self::SetSyncWord => Some(0x0D),
        }
    }
}

/// The bring-up order [`init`] runs, each step followed by a bounded BUSY wait.
pub const BRING_UP_ORDER: [InitStep; 16] = [
    InitStep::Reset,
    InitStep::SetStandby,
    InitStep::SetPacketType,
    InitStep::SetRfFrequency,
    InitStep::SetTcxo,
    InitStep::ClearDeviceErrors,
    InitStep::Calibrate,
    InitStep::CalibrateImage,
    InitStep::SetPaConfig,
    InitStep::SetTxParams,
    InitStep::SetBufferBaseAddress,
    InitStep::SetModulationParams,
    InitStep::SetPacketParams,
    InitStep::SetDioIrqParams,
    InitStep::SetDio2AsRfSwitch,
    InitStep::SetSyncWord,
];

/// Error returned by [`init`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitError {
    /// The SPI command of this step failed.
    Command(InitStep),
    /// BUSY did not clear within the poll bound after this step.
    BusyTimeout(InitStep),
    /// DeviceErrors reported a calibration fault after `Calibrate`.
    CalibrationFailed,
}

/// Compose the raw IRQ word from an `sx126x` status, for [`super::classify_irq`].
///
/// `IrqStatus` exposes only per-bit accessors; this keeps the four bits the
/// classification reads.
pub fn irq_flags(irq: IrqStatus) -> u16 {
    let mut flags = 0;
    if irq.tx_done() {
        flags |= IRQ_TX_DONE;
    }
    if irq.rx_done() {
        flags |= IRQ_RX_DONE;
    }
    if irq.crc_err() {
        flags |= IRQ_CRC_ERR;
    }
    if irq.timeout() {
        flags |= IRQ_TIMEOUT;
    }
    flags
}

/// Failure of [`take_irq_status`], by SPI command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TakeIrqError {
    /// `GetIrqStatus` failed.
    Read,
    /// `ClearIrqStatus` failed.
    Clear,
}

/// The mask of every flag set in `irq`, so a clear touches nothing else.
pub fn irq_read_mask(irq: IrqStatus) -> IrqMask {
    let bits = [
        (irq.tx_done(), IrqMaskBit::TxDone),
        (irq.rx_done(), IrqMaskBit::RxDone),
        (irq.preamble_detected(), IrqMaskBit::PreambleDetected),
        (irq.syncword_valid(), IrqMaskBit::SyncwordValid),
        (irq.header_valid(), IrqMaskBit::HeaderValid),
        (irq.header_error(), IrqMaskBit::HeaderError),
        (irq.crc_err(), IrqMaskBit::CrcErr),
        (irq.cad_done(), IrqMaskBit::CadDone),
        (irq.cad_detected(), IrqMaskBit::CadDetected),
        (irq.timeout(), IrqMaskBit::Timeout),
    ];
    bits.into_iter()
        .filter(|(set, _)| *set)
        .fold(IrqMask::none(), |mask, (_, bit)| mask.combine(bit))
}

/// Read the IRQ register and clear exactly the flags that were read.
///
/// Clearing with `IrqMask::all()` would also wipe a flag (for example `TxDone`)
/// that latches between the read and the clear, so a poll loop would never see
/// it. The returned status is what the caller classifies.
pub fn take_irq_status<SPI, NRST, BUSY, ANT, DIO1, PINERR>(
    radio: &mut SX126x<SPI, NRST, BUSY, ANT, DIO1>,
) -> Result<IrqStatus, TakeIrqError>
where
    SPI: SpiDevice,
    NRST: OutputPin<Error = PINERR>,
    BUSY: InputPin<Error = PINERR>,
    ANT: OutputPin<Error = PINERR>,
    DIO1: InputPin<Error = PINERR>,
    PINERR: core::fmt::Debug,
{
    let irq = radio.get_irq_status().map_err(|_| TakeIrqError::Read)?;
    radio
        .clear_irq_status(irq_read_mask(irq))
        .map_err(|_| TakeIrqError::Clear)?;
    Ok(irq)
}

/// RF frequency word for `SetRfFrequency`, for a frequency in Hz.
pub fn rf_freq_word(freq_hz: u32) -> u32 {
    calc_rf_freq(freq_hz as f32, XTAL_HZ as f32)
}

/// Run the SX1262 bring-up in [`BRING_UP_ORDER`].
///
/// The tier must have pulsed NRST before calling. Every BUSY wait polls
/// `GetStatus` at most `busy_poll_bound` times, 1 ms apart via `delay`.
/// Frequencies come from `cfg.region` ([`super::Region::default_rf_hz`],
/// [`super::Region::image_calibration_hz`]).
pub fn init<SPI, NRST, BUSY, ANT, DIO1, PINERR, D>(
    radio: &mut SX126x<SPI, NRST, BUSY, ANT, DIO1>,
    delay: &mut D,
    cfg: &LoraConfig,
    busy_poll_bound: u32,
) -> Result<(), InitError>
where
    SPI: SpiDevice,
    NRST: OutputPin<Error = PINERR>,
    BUSY: InputPin<Error = PINERR>,
    ANT: OutputPin<Error = PINERR>,
    DIO1: InputPin<Error = PINERR>,
    PINERR: core::fmt::Debug,
    D: DelayNs,
{
    use InitStep as S;

    macro_rules! step {
        ($step:expr, $call:expr) => {{
            log::debug!("sx1262_init: {:?}", $step);
            $call.map_err(|_| InitError::Command($step))?;
            wait_busy($step, radio, delay, busy_poll_bound)?;
        }};
    }

    log::debug!("sx1262_init: waiting busy after hardware reset");
    wait_busy(S::Reset, radio, delay, busy_poll_bound)?;

    step!(S::SetStandby, radio.set_standby(StandbyConfig::StbyRc));
    step!(S::SetPacketType, radio.set_packet_type(PacketType::LoRa));

    // No TCXO yet — this uses the RC oscillator; calibration below re-references it.
    step!(
        S::SetRfFrequency,
        radio.set_rf_frequency(rf_freq_word(cfg.region.default_rf_hz()))
    );

    // XOSC_START_ERR is set by this command — expected, cleared next.
    step!(
        S::SetTcxo,
        radio.set_dio3_as_tcxo_ctrl(TcxoVoltage::Volt1_8, TcxoDelay::from_ms(TCXO_STARTUP_MS))
    );
    match radio.get_device_errors() {
        Ok(errs) => log::debug!(
            "sx1262_init: device errors after SetTcxo = {:?} [XOSC_START_ERR=true is normal here]",
            errs
        ),
        Err(_) => log::warn!("sx1262_init: could not read device errors after SetTcxo"),
    }

    // MANDATORY before Calibrate (see the module docs).
    step!(S::ClearDeviceErrors, radio.clear_device_errors());
    step!(S::Calibrate, radio.calibrate(CalibParam::all()));

    match radio.get_device_errors() {
        Ok(errs) => {
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
                    "sx1262_init: device errors after Calibrate = {:?} — calibration failed",
                    errs
                );
                return Err(InitError::CalibrationFailed);
            }
            log::debug!(
                "sx1262_init: device errors after Calibrate = {:?} (clean)",
                errs
            );
        }
        Err(_) => log::warn!("sx1262_init: could not read device errors after Calibrate"),
    }

    step!(
        S::CalibrateImage,
        radio.calibrate_image(CalibImageFreq::from_rf_frequency(
            cfg.region.image_calibration_hz()
        ))
    );
    step!(
        S::SetPaConfig,
        radio.set_pa_config(
            PaConfig::default()
                .set_pa_duty_cycle(0x04)
                .set_hp_max(0x07)
                .set_device_sel(DeviceSel::SX1262),
        )
    );
    step!(
        S::SetTxParams,
        radio.set_tx_params(
            TxParams::default()
                .set_power_dbm(INIT_TX_POWER_DBM)
                .set_ramp_time(RampTime::Ramp200u),
        )
    );
    step!(
        S::SetBufferBaseAddress,
        radio.set_buffer_base_address(0x00, 0x00)
    );
    step!(
        S::SetModulationParams,
        radio.set_mod_params(
            LoraModParams::default()
                .set_spread_factor(LoRaSpreadFactor::SF12)
                .set_bandwidth(LoRaBandWidth::BW125)
                .set_coding_rate(LoraCodingRate::CR4_5)
                .set_low_dr_opt(true)
                .into(),
        )
    );
    step!(
        S::SetPacketParams,
        radio.set_packet_params(
            LoRaPacketParams::default()
                .set_preamble_len(LORAWAN_PREAMBLE_LEN)
                .set_header_type(LoRaHeaderType::VarLen)
                .set_payload_len(RX_PAYLOAD_MAX)
                .set_crc_type(LoRaCrcType::CrcOn)
                .set_invert_iq(LoRaInvertIq::Standard)
                .into(),
        )
    );

    // Bits routed to DIO1 drive the LoRaWAN state machine. The IRQ-record mask is
    // wider so `GetIrqStatus` can report preamble / sync word / header during an
    // RX window without those bits firing DIO1 early.
    let dio1_mask = IrqMask::none()
        .combine(IrqMaskBit::TxDone)
        .combine(IrqMaskBit::RxDone)
        .combine(IrqMaskBit::CrcErr)
        .combine(IrqMaskBit::Timeout);
    let record_mask = dio1_mask
        .combine(IrqMaskBit::PreambleDetected)
        .combine(IrqMaskBit::SyncwordValid)
        .combine(IrqMaskBit::HeaderValid)
        .combine(IrqMaskBit::HeaderError);
    step!(
        S::SetDioIrqParams,
        radio.set_dio_irq_params(record_mask, dio1_mask, IrqMask::none(), IrqMask::none())
    );
    step!(S::SetDio2AsRfSwitch, radio.set_dio2_as_rf_switch_ctrl(true));
    step!(
        S::SetSyncWord,
        radio.set_sync_word(LORAWAN_PUBLIC_SYNC_WORD)
    );

    match radio.get_status() {
        Ok(status) => log::debug!("sx1262_init: complete — GetStatus = {:?}", status),
        Err(_) => log::warn!("sx1262_init: could not read final status"),
    }
    Ok(())
}

/// Poll `GetStatus` until the chip reports a mode, at most `bound` times, 1 ms apart.
///
/// The status byte encodes `chip_mode` in bits [6:4]; `chip_mode()` is `Some`
/// for STDBY_RC, STDBY_XOSC, FS, RX and TX, and `None` while a command is still
/// in progress, so polling until it is `Some` is equivalent to polling BUSY low.
fn wait_busy<SPI, NRST, BUSY, ANT, DIO1, PINERR, D>(
    step: InitStep,
    radio: &mut SX126x<SPI, NRST, BUSY, ANT, DIO1>,
    delay: &mut D,
    bound: u32,
) -> Result<(), InitError>
where
    SPI: SpiDevice,
    NRST: OutputPin<Error = PINERR>,
    BUSY: InputPin<Error = PINERR>,
    ANT: OutputPin<Error = PINERR>,
    DIO1: InputPin<Error = PINERR>,
    PINERR: core::fmt::Debug,
    D: DelayNs,
{
    for i in 0..bound {
        // Yield — feeds an RTOS task watchdog and lets other tasks run.
        delay.delay_ms(1);
        match radio.get_status() {
            Ok(status) if status.chip_mode().is_some() => {
                log::debug!(
                    "wait_busy({:?}): ready after {} ms, status={:?}",
                    step,
                    i + 1,
                    status
                );
                return Ok(());
            }
            Ok(_) => {}
            // An SPI error during a poll may self-clear; keep trying.
            Err(_) => log::warn!("wait_busy({:?}): SPI error at iter {}", step, i),
        }
    }

    log::error!(
        "wait_busy({:?}): BUSY did not clear after {} ms — reading device errors",
        step,
        bound
    );
    match radio.get_device_errors() {
        Ok(errs) => log::error!("wait_busy: device errors = {:?}", errs),
        Err(_) => log::error!("wait_busy: could not read device errors (SPI error)"),
    }
    Err(InitError::BusyTimeout(step))
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lora::config::Region;
    use alloc::vec::Vec;
    use core::convert::Infallible;
    use embedded_hal::spi::{ErrorType, Operation};

    const GET_STATUS: u8 = 0xC0;
    const GET_DEVICE_ERRORS: u8 = 0x17;
    /// STDBY_RC (chip_mode 0x2).
    const STATUS_READY: u8 = 0x22;

    /// Records each transaction's written bytes and answers status reads.
    struct MockSpi {
        transactions: Vec<Vec<u8>>,
        status: u8,
    }

    impl MockSpi {
        fn new(status: u8) -> Self {
            Self {
                transactions: Vec::new(),
                status,
            }
        }

        /// Opcodes of the configuration commands, without status / error polls.
        fn command_opcodes(&self) -> Vec<u8> {
            self.transactions
                .iter()
                .map(|t| t[0])
                .filter(|op| *op != GET_STATUS && *op != GET_DEVICE_ERRORS)
                .collect()
        }

        fn payload_of(&self, opcode: u8) -> Vec<u8> {
            self.transactions
                .iter()
                .find(|t| t[0] == opcode)
                .map(|t| t[1..].to_vec())
                .expect("command was issued")
        }
    }

    impl ErrorType for MockSpi {
        type Error = Infallible;
    }

    impl SpiDevice for MockSpi {
        fn transaction(&mut self, operations: &mut [Operation<'_, u8>]) -> Result<(), Infallible> {
            let mut written = Vec::new();
            for op in operations.iter_mut() {
                match op {
                    Operation::Write(buf) => written.extend_from_slice(buf),
                    Operation::TransferInPlace(buf) => {
                        written.extend_from_slice(buf);
                        if buf[0] == GET_STATUS {
                            buf[1] = self.status;
                        } else {
                            // GetDeviceErrors and friends: all clear.
                            for b in buf[1..].iter_mut() {
                                *b = 0;
                            }
                        }
                    }
                    Operation::Transfer(read, write) => {
                        written.extend_from_slice(write);
                        read.fill(0);
                    }
                    Operation::Read(buf) => buf.fill(0),
                    Operation::DelayNs(_) => {}
                }
            }
            if !written.is_empty() {
                self.transactions.push(written);
            }
            Ok(())
        }
    }

    /// A pin that is never read or driven by [`init`] (BUSY is polled over SPI).
    struct MockPin;

    impl embedded_hal::digital::ErrorType for MockPin {
        type Error = Infallible;
    }

    impl OutputPin for MockPin {
        fn set_low(&mut self) -> Result<(), Infallible> {
            Ok(())
        }
        fn set_high(&mut self) -> Result<(), Infallible> {
            Ok(())
        }
    }

    impl InputPin for MockPin {
        fn is_high(&mut self) -> Result<bool, Infallible> {
            Ok(false)
        }
        fn is_low(&mut self) -> Result<bool, Infallible> {
            Ok(true)
        }
    }

    #[derive(Default)]
    struct MockDelay {
        total_ns: u64,
    }

    impl DelayNs for MockDelay {
        fn delay_ns(&mut self, ns: u32) {
            self.total_ns += u64::from(ns);
        }
    }

    fn cfg(region: Region) -> LoraConfig {
        LoraConfig {
            region,
            ..LoraConfig::default()
        }
    }

    /// Run [`init`] over a borrowed [`MockSpi`], since `SX126x` does not hand its
    /// SPI device back.
    fn run_init(status: u8, region: Region, bound: u32) -> (Result<(), InitError>, MockSpi, u64) {
        let mut spi_log = MockSpi::new(status);
        let mut delay = MockDelay::default();
        let result = {
            let spi = RecordingSpi(&mut spi_log);
            let mut radio = SX126x::new(spi, (MockPin, MockPin, MockPin, MockPin));
            init(&mut radio, &mut delay, &cfg(region), bound)
        };
        (result, spi_log, delay.total_ns)
    }

    /// Forwards to a borrowed [`MockSpi`] so the test can inspect it afterwards.
    struct RecordingSpi<'a>(&'a mut MockSpi);

    impl ErrorType for RecordingSpi<'_> {
        type Error = Infallible;
    }

    impl SpiDevice for RecordingSpi<'_> {
        fn transaction(&mut self, operations: &mut [Operation<'_, u8>]) -> Result<(), Infallible> {
            self.0.transaction(operations)
        }
    }

    #[test]
    fn init_command_order_matches_reference() {
        let (result, spi, _) = run_init(STATUS_READY, Region::EU868, DEFAULT_BUSY_POLL_BOUND);
        assert_eq!(result, Ok(()));
        let expected: [u8; 15] = [
            0x80, // SetStandby
            0x8A, // SetPacketType
            0x86, // SetRfFrequency
            0x97, // SetDIO3AsTCXOCtrl
            0x07, // ClearDeviceErrors
            0x89, // Calibrate
            0x98, // CalibrateImage
            0x95, // SetPaConfig
            0x8E, // SetTxParams
            0x8F, // SetBufferBaseAddress
            0x8B, // SetModulationParams
            0x8C, // SetPacketParams
            0x08, // SetDioIrqParams
            0x9D, // SetDio2AsRfSwitchCtrl
            0x0D, // WriteRegister (sync word)
        ];
        assert_eq!(spi.command_opcodes(), expected);

        let from_table: Vec<u8> = BRING_UP_ORDER.iter().filter_map(|s| s.opcode()).collect();
        assert_eq!(from_table, expected);

        let opcodes = spi.command_opcodes();
        let clear = opcodes.iter().position(|o| *o == 0x07).unwrap();
        let calibrate = opcodes.iter().position(|o| *o == 0x89).unwrap();
        let tcxo = opcodes.iter().position(|o| *o == 0x97).unwrap();
        assert!(tcxo < clear && clear < calibrate);
    }

    #[test]
    fn init_writes_the_public_sync_word_and_tcxo_delay() {
        let (_, spi, _) = run_init(STATUS_READY, Region::EU868, DEFAULT_BUSY_POLL_BOUND);
        // WriteRegister(0x0740 LoRaSyncWordMsb, 0x34 0x44).
        assert_eq!(spi.payload_of(0x0D), [0x07, 0x40, 0x34, 0x44]);
        // 1.8 V, 10 ms = 640 ticks of 15.625 µs.
        assert_eq!(spi.payload_of(0x97), [0x02, 0x00, 0x02, 0x80]);
    }

    #[test]
    fn busy_stuck_returns_error_after_bound() {
        let bound = 7;
        let (result, spi, delay_ns) = run_init(0x00, Region::EU868, bound);
        assert_eq!(result, Err(InitError::BusyTimeout(InitStep::Reset)));
        let polls = spi
            .transactions
            .iter()
            .filter(|t| t[0] == GET_STATUS)
            .count();
        assert_eq!(polls, bound as usize);
        assert!(spi.command_opcodes().is_empty());
        assert_eq!(delay_ns, u64::from(bound) * 1_000_000);
    }

    #[test]
    fn region_frequency_used_for_rf_and_calibration() {
        let (_, eu, _) = run_init(STATUS_READY, Region::EU868, DEFAULT_BUSY_POLL_BOUND);
        assert_eq!(eu.payload_of(0x86), rf_freq_word(868_100_000).to_be_bytes());
        let eu_band: [u8; 2] = CalibImageFreq::from_rf_frequency(868_100_000).into();
        assert_eq!(eu.payload_of(0x98), eu_band);

        let (_, us, _) = run_init(STATUS_READY, Region::US915, DEFAULT_BUSY_POLL_BOUND);
        assert_eq!(
            us.payload_of(0x86),
            rf_freq_word(Region::US915.default_rf_hz()).to_be_bytes()
        );
        let us_band: [u8; 2] = CalibImageFreq::from_rf_frequency(915_000_000).into();
        assert_eq!(us.payload_of(0x98), us_band);
        assert_ne!(eu_band, us_band);
    }

    #[test]
    fn irq_flags_round_trip_through_classify() {
        use crate::lora::{classify_irq, IrqOutcome};
        let irq = IrqStatus::from(IRQ_CRC_ERR | IRQ_RX_DONE | (1 << 2));
        assert_eq!(irq_flags(irq), IRQ_CRC_ERR | IRQ_RX_DONE);
        assert_eq!(classify_irq(irq_flags(irq)), IrqOutcome::CrcError);
    }

    /// SPI model of the IRQ register: `latch_after_read` is set into the
    /// register right after each `GetIrqStatus`, like a flag that latches
    /// between the read and the clear.
    struct IrqSpi {
        reg: u16,
        latch_after_read: u16,
    }

    impl ErrorType for IrqSpi {
        type Error = Infallible;
    }

    impl SpiDevice for IrqSpi {
        fn transaction(&mut self, operations: &mut [Operation<'_, u8>]) -> Result<(), Infallible> {
            let mut opcode = 0;
            for op in operations.iter_mut() {
                match op {
                    Operation::Write(buf) if buf.len() == 1 => opcode = buf[0],
                    Operation::Write(buf) if opcode == 0x02 => {
                        self.reg &= !u16::from_be_bytes([buf[0], buf[1]]);
                    }
                    Operation::Read(buf) if opcode == 0x12 => {
                        buf[1..3].copy_from_slice(&self.reg.to_be_bytes());
                        self.reg |= self.latch_after_read;
                        self.latch_after_read = 0;
                    }
                    _ => {}
                }
            }
            Ok(())
        }
    }

    #[test]
    fn take_irq_status_keeps_flag_latched_between_read_and_clear() {
        let spi = IrqSpi {
            reg: 1 << 2,
            latch_after_read: IRQ_TX_DONE,
        };
        let mut radio = SX126x::new(spi, (MockPin, MockPin, MockPin, MockPin));
        let first = take_irq_status(&mut radio).unwrap();
        assert!(first.preamble_detected());
        assert!(!first.tx_done());
        let next = take_irq_status(&mut radio).unwrap();
        assert!(next.tx_done(), "TxDone latched mid-poll was cleared");
    }
}
