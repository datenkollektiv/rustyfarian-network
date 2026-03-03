//! Platform-independent LoRa types, traits, and LoRaWAN protocol logic.
//!
//! # Architecture
//!
//! - [`LoraRadio`] — hardware-agnostic radio interface
//! - [`lorawan::LorawanDevice<R>`] — LoRaWAN Class A stack, generic over the radio
//! - [`mock::MockLoraRadio`] — test double for host-side unit tests
//!   (requires the `mock` feature or `#[cfg(test)]`)
//!
//! # Feature flags
//!
//! | Feature | What it enables                                              |
//! |:--------|:-------------------------------------------------------------|
//! | `mock`  | `MockLoraRadio` for downstream host-side tests               |

#![no_std]

pub mod commands;
pub mod config;
pub mod lorawan;

#[cfg(any(test, feature = "mock"))]
pub mod mock;

// Re-export top-level types for ergonomic imports.
pub use config::{HeltecV3Pins, LoraConfig, Region};
pub use lorawan::{
    Downlink, LorawanDevice, LorawanError, LorawanResponse, LorawanSessionData, LorawanState,
};

// ─── RX window timing defaults ────────────────────────────────────────────────

/// Default RX window opening offset in milliseconds.
///
/// Opening early compensates for runtime scheduling and driver initialisation latency.
/// HAL implementations may override [`LoraRadio::rx_window_offset_ms`] to tune this
/// for their hardware characteristics.
pub const RX_WINDOW_OFFSET_MS: i32 = -200;

/// Default RX window duration in milliseconds.
///
/// 500 ms gives enough time to detect a LoRaWAN preamble at any supported
/// data rate while remaining within LoRaWAN Class A timing constraints.
pub const RX_WINDOW_DURATION_MS: u32 = 500;

// ─── Core radio types ─────────────────────────────────────────────────────────

/// LoRa spreading factor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpreadingFactor {
    SF7,
    SF8,
    SF9,
    SF10,
    SF11,
    SF12,
}

/// LoRa signal bandwidth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bandwidth {
    BW125,
    BW250,
    BW500,
}

/// LoRa forward error correction coding rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodingRate {
    Cr45,
    Cr46,
    Cr47,
    Cr48,
}

/// TX configuration for a single LoRa frame.
///
/// Passed to [`LoraRadio::prepare_tx`] before calling [`LoraRadio::transmit`].
#[derive(Debug, Clone, Copy)]
pub struct TxConfig {
    /// Centre frequency in Hz (e.g. 868_100_000 for EU868 DR0 uplink).
    pub freq_hz: u32,
    pub sf: SpreadingFactor,
    pub bw: Bandwidth,
    pub cr: CodingRate,
    /// TX power in dBm. Typical range: 2–22 dBm for SX1262.
    pub power_dbm: i8,
}

/// RX configuration for a LoRa receive window.
///
/// Passed to [`LoraRadio::prepare_rx`] before calling [`LoraRadio::receive`].
#[derive(Debug, Clone, Copy)]
pub struct RxConfig {
    /// Centre frequency in Hz.
    pub freq_hz: u32,
    pub sf: SpreadingFactor,
    pub bw: Bandwidth,
    pub cr: CodingRate,
}

/// Which LoRaWAN receive window is being opened.
///
/// The `LoraRadioAdapter` uses this to select EU868 RX1 or RX2 frequency/DR.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RxWindow {
    /// First receive window: opens 1 s after end of TX (configurable via join accept).
    Rx1,
    /// Second receive window: opens 2 s after end of TX, fixed DR0 / 869.525 MHz in EU868.
    Rx2,
}

/// RSSI and SNR of the last received packet.
#[derive(Debug, Clone, Copy, Default)]
pub struct RxQuality {
    /// Received signal strength in dBm (typically –60 to –130).
    pub rssi: i16,
    /// Signal-to-noise ratio in dB (positive = good, negative = noisy).
    pub snr: i8,
}

// ─── LoraRadio trait ──────────────────────────────────────────────────────────

/// Hardware-agnostic LoRa radio interface.
///
/// Designed to mirror the needs of `lorawan-device`'s `PhyRxTx + Timings` traits
/// without exposing those crate-internal types on the public API boundary.
///
/// The split `prepare_*/trigger` model matches `lorawan-device`'s `PhyRxTx`:
/// configure the radio first, then trigger the operation non-blocking.
///
/// # Implementors
///
/// - `rustyfarian_esp_idf_lora::sx1262_driver::EspIdfLoraRadio` — hardware driver (ESP-IDF)
/// - `rustyfarian_esp_hal_lora::EspHalLoraRadio` — hardware driver (esp-hal, bare-metal)
/// - [`mock::MockLoraRadio`] — test double (behind `mock` feature / `#[cfg(test)]`)
pub trait LoraRadio {
    /// Radio-specific error type.
    type Error: core::fmt::Debug;

    /// Configure and pre-load the TX payload; set RF parameters from `config`.
    ///
    /// Must be called before [`transmit`][Self::transmit].
    /// `buf` is copied into the radio FIFO; the caller may drop it after this returns.
    fn prepare_tx(&mut self, config: TxConfig, buf: &[u8]) -> Result<(), Self::Error>;

    /// Trigger the uplink. Non-blocking — returns on-air time in ms on success.
    ///
    /// Call [`prepare_tx`][Self::prepare_tx] exactly once before polling begins.
    /// Returns `nb::Error::WouldBlock` while transmission is in progress
    /// (IRQ not yet fired, packet not yet on air). The caller must loop-call
    /// until `Ok(on_air_ms)` is returned or an error variant is received.
    fn transmit(&mut self) -> nb::Result<u32, Self::Error>;

    /// Configure the radio for the next receive window.
    ///
    /// Must be called exactly once before polling [`receive`][Self::receive].
    /// Implementations must ensure this can be called again after a timed-out
    /// or otherwise failed receive window, without a full reset.
    fn prepare_rx(&mut self, config: RxConfig, window: RxWindow) -> Result<(), Self::Error>;

    /// Poll for a received packet. Non-blocking.
    ///
    /// Call [`prepare_rx`][Self::prepare_rx] exactly once before polling begins.
    /// Returns `nb::Error::WouldBlock` while the radio is listening but no preamble
    /// has been detected yet. Returns `Ok((byte_count, RxQuality))` when a packet
    /// has been received and written into `buf`.
    fn receive(&mut self, buf: &mut [u8]) -> nb::Result<(usize, RxQuality), Self::Error>;

    /// Tune the radio to the given frequency in Hz. Synchronous.
    fn set_frequency(&mut self, freq_hz: u32) -> Result<(), Self::Error>;

    /// Return signal quality of the last successfully received packet.
    fn rx_quality(&self) -> RxQuality;

    /// Hardware-calibrated RX window opening offset in ms.
    ///
    /// A negative value opens the window earlier to compensate for hardware latency.
    /// The `LoraRadioAdapter` adds this to the LoRaWAN-specified window timing.
    fn rx_window_offset_ms(&self) -> i32;

    /// Duration the RX window stays open waiting for a preamble, in ms.
    fn rx_window_duration_ms(&self) -> u32;
}
