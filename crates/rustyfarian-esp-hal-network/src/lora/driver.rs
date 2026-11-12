//! SX1262 radio driver stub for ESP-HAL targets.
//!
//! [`EspHalLoraRadio`] will use `esp-hal` SPI + GPIO once the hardware API
//! is verified on a Heltec V3 board. All methods currently return stub errors
//! matching the operation attempted (e.g. [`LoraError::TransmitFailed`] from TX methods).
//!
//! When implemented, the bring-up calls the shared [`juggler::lora::sx1262::init`]
//! (ADR 023 rung 1) with the esp-hal `Delay`, exactly as the ESP-IDF tier and the
//! `hal_esp32s3_join` example do, and the radio plugs into the shared
//! [`juggler::lora::LorawanPhy`] bridge through [`RadioErrorKind`].

use juggler::lora::config::LoraConfig;
use juggler::lora::{LoraRadio, RadioErrorKind, RxConfig, RxQuality, RxWindow, TxConfig};
use pennant::StatusLed;

/// Error type for [`EspHalLoraRadio`] operations.
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
    /// A `Phy` event arrived while the frame was still on air (not a radio fault).
    TxInProgress,
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
            Self::TxInProgress => write!(f, "TX still on air"),
        }
    }
}

impl RadioErrorKind for LoraError {
    fn invalid_rf_config() -> Self {
        Self::InvalidRfConfig
    }

    fn tx_in_progress() -> Self {
        Self::TxInProgress
    }

    fn is_rx_timeout(&self) -> bool {
        matches!(self, Self::Timeout)
    }
}

/// SX1262 radio driver for ESP-HAL targets.
///
/// Accepts a `StatusLed` implementation for visual feedback:
/// - Use [`pennant::NoLed`] for headless configurations.
/// - Use `rustyfarian_esp_hal_ws2812::Ws2812RmtDriver` for WS2812 LED feedback.
///
/// # Implementation status
///
/// [`new`][Self::new] currently returns `Ok` with a stub instance.
/// Hardware integration is planned as part of TTN v3 (EU868) validation.
pub struct EspHalLoraRadio<S: StatusLed> {
    _led: S,
    last_rssi: i16,
    last_snr: i8,
}

impl<S: StatusLed> EspHalLoraRadio<S> {
    /// Attempt to initialise the SX1262 using ESP-HAL SPI.
    ///
    /// Currently returns `Ok` with a stub that returns errors from all radio operations.
    /// Hardware integration pending.
    pub fn new(_config: &LoraConfig, led: S) -> Result<Self, LoraError> {
        log::warn!(
            "EspHalLoraRadio: hardware integration pending — \
             LoRa unavailable until esp-hal SPI driver is implemented."
        );
        Ok(Self {
            _led: led,
            last_rssi: 0,
            last_snr: 0,
        })
    }
}

impl<S: StatusLed> LoraRadio for EspHalLoraRadio<S> {
    type Error = LoraError;

    fn prepare_tx(&mut self, config: TxConfig, buf: &[u8]) -> Result<(), LoraError> {
        let _ = (config, buf);
        Err(LoraError::TransmitFailed)
    }

    fn transmit(&mut self) -> nb::Result<u32, LoraError> {
        Err(nb::Error::Other(LoraError::TransmitFailed))
    }

    fn prepare_rx(&mut self, config: RxConfig, window: RxWindow) -> Result<(), LoraError> {
        let _ = (config, window);
        Err(LoraError::ReceiveFailed)
    }

    fn receive(&mut self, buf: &mut [u8]) -> nb::Result<(usize, RxQuality), LoraError> {
        let _ = buf;
        Err(nb::Error::Other(LoraError::ReceiveFailed))
    }

    fn set_frequency(&mut self, freq_hz: u32) -> Result<(), LoraError> {
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
        juggler::lora::RX_WINDOW_OFFSET_MS
    }

    fn rx_window_duration_ms(&self) -> u32 {
        juggler::lora::RX_WINDOW_DURATION_MS
    }

    fn cancel_rx(&mut self) -> Result<(), LoraError> {
        Err(LoraError::ReceiveFailed)
    }

    fn irq_pending(&mut self) -> Result<bool, LoraError> {
        Err(LoraError::IrqStatusReadFailed)
    }
}
