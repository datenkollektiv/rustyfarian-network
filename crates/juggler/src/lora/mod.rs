//! Platform-independent LoRa types, traits, and LoRaWAN protocol logic.
//!
//! # Architecture
//!
//! The domain follows the shared-logic ladder (ADR 023):
//!
//! - Flat rules: [`Region::default_rf_hz`], [`ldro_required`], [`classify_irq`] and
//!   the LoRaWAN PHY constants below.
//! - Rung 1: [`sx1262::init`] — the SX1262 bring-up order, generic over
//!   `embedded-hal` and shared by both tiers.
//! - [`LorawanPhy<R>`] — the `lorawan-device` `PhyRxTx + Timings` bridge over a
//!   [`LoraRadio`] port.
//! - Flat rules: [`LorawanTimeline`], [`requested_deadline`] and [`classify_response`] decide the events
//!   that drive `lorawan-device`'s `nb_device::Device`.
//! - [`LoraRadio`] — the narrow, hardware-agnostic radio port
//! - [`mock::MockLoraRadio`] — test double for host-side unit tests
//!   (requires the `mock` feature or `#[cfg(test)]`)
//!
//! [`lorawan::LorawanDevice<R>`] is a deprecated stub that no code drives.
//!
//! # Feature flags
//!
//! | Feature | What it enables                                              |
//! |:--------|:-------------------------------------------------------------|
//! | `mock`  | `MockLoraRadio` for downstream host-side tests               |

pub mod commands;
pub mod config;
pub mod lorawan;
pub mod phy;
pub mod sx1262;
pub mod ticker;

#[cfg(any(test, feature = "mock"))]
pub mod mock;

// Re-export top-level types for ergonomic imports.
pub use config::{HeltecV3Pins, LoraConfig, Region};
#[allow(deprecated)]
pub use lorawan::{
    Downlink, LorawanDevice, LorawanError, LorawanResponse, LorawanSessionData, LorawanState,
};
pub use phy::{map_rf_config, LorawanPhy};
pub use ticker::{classify_response, requested_deadline, LorawanTimeline, Outcome, TickEvent};

// ─── RX window timing defaults ────────────────────────────────────────────────

/// Default RX window opening offset in milliseconds.
///
/// Opening early compensates for runtime scheduling and driver initialisation latency.
/// A negative value means "open this many ms before the LoRaWAN-specified window time".
///
/// HAL implementations may override [`LoraRadio::rx_window_offset_ms`] to tune this
/// for their hardware characteristics.
pub const RX_WINDOW_OFFSET_MS: i32 = -500;

/// Default RX window duration in milliseconds.
///
/// `lorawan-device` uses this as the software window-close timeout: it keeps the
/// window open for this long and then issues `CancelRx`, which aborts reception
/// **even if the radio is mid-packet**. It must therefore exceed the worst-case
/// time from window-open to `RxDone` for the slowest data rate — it is NOT merely
/// a preamble-detection margin.
///
/// At SF12/BW125 a TTN join-accept with CFList is ≈ 1.8 s of airtime. With the
/// −500 ms early open plus the ≈ 500 ms until the downlink actually starts,
/// `RxDone` lands ≈ 2.3–2.8 s after the window opens. 3000 ms covers this with
/// margin. (An 800 ms window detects the preamble but is torn down long before
/// `RxDone`, so the join-accept is never delivered — observed on hardware.)
///
/// # No RX1/RX2 overlap despite the long duration
///
/// `lorawan-device` caps the RX1 close at the inter-window gap — `RxWindow1` uses
/// `min(duration, t_rx2 − t_rx1)` (= 1000 ms for EU868 OTAA join), so RX1 always
/// closes before RX2 opens. Only RX2, which has no following window, uses the full
/// duration. See `nb_device::state::WaitingForRx::handle_event`.
///
/// HAL implementations may override [`LoraRadio::rx_window_duration_ms`].
pub const RX_WINDOW_DURATION_MS: u32 = 3000;

// ─── LoRaWAN PHY policy ───────────────────────────────────────────────────────

/// LoRaWAN public-network sync word for the SX126x family (TTN and other public networks).
///
/// The SX126x uses a 16-bit sync word; `0x34` is the SX127x spelling of the same value.
pub const LORAWAN_PUBLIC_SYNC_WORD: u16 = 0x3444;

/// LoRaWAN preamble length in symbols, for uplinks and downlinks alike.
pub const LORAWAN_PREAMBLE_LEN: u16 = 8;

/// Uplinks use standard (non-inverted) IQ.
pub const UPLINK_IQ_INVERTED: bool = false;

/// Downlinks use inverted IQ, which keeps devices from hearing each other's uplinks.
pub const DOWNLINK_IQ_INVERTED: bool = true;

/// Uplinks carry a PHY-layer CRC.
pub const UPLINK_CRC_ON: bool = true;

/// Downlinks carry NO PHY-layer CRC.
///
/// With CRC on, the SX1262 either treats the missing CRC bytes as payload or
/// flags `crc_err` and drops the frame, so RX must run with CRC off.
pub const DOWNLINK_CRC_ON: bool = false;

/// Largest payload the radio accepts in an RX window, in bytes (the LoRa PHY maximum).
pub const RX_PAYLOAD_MAX: u8 = 255;

/// Return `true` when low-data-rate optimisation (LDRO) is required.
///
/// LDRO is required when: BW=125 kHz and SF≥11, or BW=250 kHz and SF=12.
pub fn ldro_required(sf: SpreadingFactor, bw: Bandwidth) -> bool {
    matches!(
        (sf, bw),
        (
            SpreadingFactor::SF11 | SpreadingFactor::SF12,
            Bandwidth::BW125
        ) | (SpreadingFactor::SF12, Bandwidth::BW250)
    )
}

// ─── IRQ classification ───────────────────────────────────────────────────────

/// SX126x IRQ register bit: transmission finished.
pub const IRQ_TX_DONE: u16 = 1 << 0;
/// SX126x IRQ register bit: a packet was received.
pub const IRQ_RX_DONE: u16 = 1 << 1;
/// SX126x IRQ register bit: the received packet failed its CRC.
pub const IRQ_CRC_ERR: u16 = 1 << 6;
/// SX126x IRQ register bit: the RX or TX hardware timeout expired.
pub const IRQ_TIMEOUT: u16 = 1 << 9;

/// What a latched IRQ word means for the operation in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqOutcome {
    /// No relevant bit is set: the operation is still in progress.
    None,
    /// Transmission finished.
    TxDone,
    /// A packet was received.
    RxDone,
    /// The hardware timeout expired.
    Timeout,
    /// A packet arrived with a bad CRC.
    CrcError,
}

/// Classify a raw SX126x IRQ word.
///
/// Priority: `Timeout` > `CrcError` > `RxDone` > `TxDone` > `None`, the order in
/// which the radio's receive and transmit polls test the bits. A `crc_err`
/// latched together with `rx_done` is therefore a `CrcError`, never a packet.
/// Bits other than the four named ones (preamble, sync word, header) are ignored.
pub fn classify_irq(flags: u16) -> IrqOutcome {
    if flags & IRQ_TIMEOUT != 0 {
        IrqOutcome::Timeout
    } else if flags & IRQ_CRC_ERR != 0 {
        IrqOutcome::CrcError
    } else if flags & IRQ_RX_DONE != 0 {
        IrqOutcome::RxDone
    } else if flags & IRQ_TX_DONE != 0 {
        IrqOutcome::TxDone
    } else {
        IrqOutcome::None
    }
}

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
/// `lorawan-device` already encodes the RX1 or RX2 frequency and data rate in the
/// RF config, so [`LorawanPhy`] passes a fixed placeholder.
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
/// - `rustyfarian_esp_idf_network::lora::sx1262_driver::EspIdfLoraRadio` — hardware driver (ESP-IDF)
/// - `rustyfarian_esp_hal_network::lora::EspHalLoraRadio` — hardware driver (esp-hal, bare-metal)
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
    /// [`LorawanPhy`] reports this to `lorawan-device` as the window timing offset.
    fn rx_window_offset_ms(&self) -> i32;

    /// Duration the RX window stays open waiting for a preamble, in ms.
    fn rx_window_duration_ms(&self) -> u32;

    /// Abort any RX or TX in progress and return the radio to standby.
    ///
    /// Called when `lorawan-device` closes an RX window (`CancelRx`).
    fn cancel_rx(&mut self) -> Result<(), Self::Error>;

    /// Return `true` if a TX-done, RX-done, timeout or CRC-error IRQ is latched,
    /// without clearing it.
    ///
    /// The event loop polls this instead of relying on a DIO1 edge interrupt.
    fn irq_pending(&mut self) -> Result<bool, Self::Error>;
}

/// Classifies a radio error for [`LorawanPhy`]: one error mapping per method.
///
/// Implemented by each tier's radio error type so the shared bridge can tell a
/// closed RX window apart from a real failure.
pub trait RadioErrorKind {
    /// The error for an RF configuration the radio cannot represent.
    fn invalid_rf_config() -> Self;

    /// Return `true` when the error is an RX window that closed with no packet.
    fn is_rx_timeout(&self) -> bool;
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ldro_table_for_sf11_sf12_at_125k() {
        assert!(ldro_required(SpreadingFactor::SF11, Bandwidth::BW125));
        assert!(ldro_required(SpreadingFactor::SF12, Bandwidth::BW125));
        assert!(!ldro_required(SpreadingFactor::SF10, Bandwidth::BW125));
        assert!(!ldro_required(SpreadingFactor::SF7, Bandwidth::BW125));
    }

    #[test]
    fn ldro_table_at_250k_and_500k() {
        assert!(ldro_required(SpreadingFactor::SF12, Bandwidth::BW250));
        assert!(!ldro_required(SpreadingFactor::SF11, Bandwidth::BW250));
        assert!(!ldro_required(SpreadingFactor::SF12, Bandwidth::BW500));
    }

    #[test]
    fn classify_irq_priority() {
        assert_eq!(
            classify_irq(IRQ_CRC_ERR | IRQ_RX_DONE),
            IrqOutcome::CrcError
        );
        assert_eq!(classify_irq(IRQ_TIMEOUT), IrqOutcome::Timeout);
        assert_eq!(
            classify_irq(IRQ_TIMEOUT | IRQ_CRC_ERR | IRQ_RX_DONE),
            IrqOutcome::Timeout
        );
        assert_eq!(classify_irq(0), IrqOutcome::None);
        assert_eq!(classify_irq(IRQ_TX_DONE), IrqOutcome::TxDone);
        assert_eq!(classify_irq(IRQ_RX_DONE), IrqOutcome::RxDone);
    }

    #[test]
    fn classify_irq_ignores_diagnostic_bits() {
        let preamble_sync_header = (1 << 2) | (1 << 3) | (1 << 4) | (1 << 5);
        assert_eq!(classify_irq(preamble_sync_header), IrqOutcome::None);
    }

    #[test]
    fn lorawan_phy_policy_values() {
        assert_eq!(LORAWAN_PUBLIC_SYNC_WORD, 0x3444);
        assert_eq!(LORAWAN_PREAMBLE_LEN, 8);
        assert_eq!(
            (
                UPLINK_IQ_INVERTED,
                DOWNLINK_IQ_INVERTED,
                UPLINK_CRC_ON,
                DOWNLINK_CRC_ON
            ),
            (false, true, true, false)
        );
        assert_eq!(RX_PAYLOAD_MAX, 255);
    }
}
