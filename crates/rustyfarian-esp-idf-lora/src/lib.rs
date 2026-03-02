//! LoRa radio driver and LoRaWAN Class A protocol for the Rustyfarian ecosystem.
//!
//! # Architecture
//!
//! - [`LoraRadio`] — hardware-agnostic radio interface (analogous to `BatteryMonitor`)
//! - [`sx1262_driver::EspLoraRadio`] — implements `LoraRadio` via `sx126x` + ESP-IDF SPI
//!   (requires the `esp-idf` feature, enabled by default)
//! - [`lorawan::LorawanDevice<R>`] — LoRaWAN Class A stack, generic over the radio
//! - [`mock::MockLoraRadio`] — test double for host-side unit tests
//!   (requires the `mock` feature or `#[cfg(test)]`)
//!
//! # Feature flags
//!
//! | Feature   | What it enables                                              |
//! |:----------|:-------------------------------------------------------------|
//! | `esp-idf` | `EspLoraRadio`, SX1262 SPI driver (default, needs ESP toolchain) |
//! | `mock`    | `MockLoraRadio` for downstream host-side tests               |
//!
//! # no_std
//!
//! This crate is `no_std` when the `esp-idf` feature is disabled, making it
//! suitable for host-side unit testing without the ESP toolchain.

#![cfg_attr(not(feature = "esp-idf"), no_std)]

pub mod commands;
pub mod config;
pub mod lorawan;

#[cfg(feature = "esp-idf")]
pub mod sx1262_driver;

#[cfg(any(test, feature = "mock"))]
pub mod mock;

// Re-export top-level types for ergonomic `use rustyfarian_esp_idf_lora::LoraConfig;` imports.
pub use config::{HeltecV3Pins, LoraConfig, Region};
pub use lorawan::{
    Downlink, LorawanDevice, LorawanError, LorawanResponse, LorawanSessionData, LorawanState,
};

// ─── RX window timing defaults ────────────────────────────────────────────────

/// Default RX window opening offset in milliseconds.
///
/// Opening 200 ms early compensates for ESP-IDF scheduler jitter and SPI
/// initialisation latency.  Both [`sx1262_driver::EspLoraRadio`] and
/// [`mock::MockLoraRadio`] use this as their default value.
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
/// The `LoraRadioAdapter` (internal to `sx1262_driver`) builds this from the
/// `lorawan-device` `TxConfig` — see the mapping table in `sx1262_driver.rs`.
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
/// - [`sx1262_driver::EspLoraRadio`] — hardware driver (behind `esp-idf` feature)
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
    /// Returns `nb::Error::WouldBlock` while transmission is in progress.
    /// The caller must loop-call until `Ok(on_air_ms)` is returned.
    fn transmit(&mut self) -> nb::Result<u32, Self::Error>;

    /// Configure the radio for the next receive window.
    ///
    /// Must be called before [`receive`][Self::receive].
    fn prepare_rx(&mut self, config: RxConfig, window: RxWindow) -> Result<(), Self::Error>;

    /// Poll for a received packet. Non-blocking.
    ///
    /// Returns `(byte_count, RxQuality)` when a packet is ready.
    /// Returns `nb::Error::WouldBlock` if no packet has been received yet.
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
