//! [`MockLoraRadio`] — a test double for host-side unit tests.
//!
//! Enable with `features = ["mock"]` in `dev-dependencies`, or it is automatically
//! available inside `#[cfg(test)]` blocks within this crate.
//!
//! # Example
//!
//! ```rust,ignore
//! use lora_pure::mock::MockLoraRadio;
//! use lora_pure::{LoraConfig, LorawanDevice};
//!
//! let radio = MockLoraRadio::new();
//! let config = LoraConfig::default();
//! let mut device = LorawanDevice::new(radio, config);
//! device.join().unwrap();
//! ```

use crate::{LoraRadio, RxConfig, RxQuality, RxWindow, TxConfig};
use heapless::Vec;

/// A recorded `prepare_tx` call, available for inspection in tests.
#[derive(Debug, Clone)]
pub struct RecordedTx {
    pub config: TxConfig,
    pub payload: Vec<u8, 256>,
}

/// A pre-programmed RX response to inject into [`MockLoraRadio::receive`].
#[derive(Debug, Clone)]
pub struct RxResponse {
    /// Bytes to return as the received payload.
    pub payload: Vec<u8, 256>,
    /// Signal quality to attach to this response.
    pub quality: RxQuality,
}

/// Mock implementation of [`LoraRadio`] for host-side unit tests.
///
/// - Records all `prepare_tx` + `transmit` calls for assertion.
/// - Serves pre-programmed RX responses (via [`queue_rx_response`][Self::queue_rx_response]).
/// - `receive()` returns `nb::Error::WouldBlock` until a response is queued.
/// - `transmit()` immediately returns `Ok(0)` (zero on-air ms).
pub struct MockLoraRadio {
    /// All `prepare_tx` calls in order.
    pub tx_calls: Vec<RecordedTx, 16>,
    /// Number of `transmit()` calls made.
    pub transmit_count: usize,
    /// Pre-programmed responses for `receive()`. Consumed FIFO-style.
    rx_responses: Vec<RxResponse, 16>,
    /// Signal quality returned by `rx_quality()`.
    pub configured_quality: RxQuality,
    /// Simulated `rx_window_offset_ms` value.
    pub rx_window_offset: i32,
    /// Simulated `rx_window_duration_ms` value.
    pub rx_window_duration: u32,
    current_freq_hz: u32,
}

impl MockLoraRadio {
    /// Create a new mock radio with no pre-programmed responses.
    pub fn new() -> Self {
        Self {
            tx_calls: Vec::new(),
            transmit_count: 0,
            rx_responses: Vec::new(),
            configured_quality: RxQuality { rssi: -90, snr: 5 },
            rx_window_offset: crate::RX_WINDOW_OFFSET_MS,
            rx_window_duration: crate::RX_WINDOW_DURATION_MS,
            current_freq_hz: 0,
        }
    }

    /// Queue a packet that `receive()` will return on the next call.
    ///
    /// Responses are consumed in FIFO order. If the queue is empty,
    /// `receive()` returns `nb::Error::WouldBlock`.
    ///
    /// Returns `Err(MockRadioError::CapacityExhausted)` if the payload exceeds
    /// 256 bytes or the response queue (16 entries) is full — so test failures
    /// are surfaced immediately rather than silently dropped.
    pub fn queue_rx_response(
        &mut self,
        payload: &[u8],
        quality: RxQuality,
    ) -> Result<(), MockRadioError> {
        let mut p = Vec::new();
        p.extend_from_slice(payload)
            .map_err(|_| MockRadioError::CapacityExhausted)?;
        self.rx_responses
            .push(RxResponse {
                payload: p,
                quality,
            })
            .map_err(|_| MockRadioError::CapacityExhausted)
    }

    /// Return `true` if all queued RX responses have been consumed.
    pub fn rx_queue_empty(&self) -> bool {
        self.rx_responses.is_empty()
    }

    /// The frequency last set via `set_frequency()`, in Hz.
    pub fn current_freq_hz(&self) -> u32 {
        self.current_freq_hz
    }
}

impl Default for MockLoraRadio {
    fn default() -> Self {
        Self::new()
    }
}

/// Error type for [`MockLoraRadio`] — infallible by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockRadioError {
    /// Triggered when `tx_calls` or `rx_responses` Vec capacity is exhausted.
    CapacityExhausted,
}

impl core::fmt::Display for MockRadioError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "mock radio capacity exhausted")
    }
}

impl LoraRadio for MockLoraRadio {
    type Error = MockRadioError;

    fn prepare_tx(&mut self, config: TxConfig, buf: &[u8]) -> Result<(), MockRadioError> {
        let mut payload = Vec::new();
        payload
            .extend_from_slice(buf)
            .map_err(|_| MockRadioError::CapacityExhausted)?;
        self.tx_calls
            .push(RecordedTx { config, payload })
            .map_err(|_| MockRadioError::CapacityExhausted)?;
        Ok(())
    }

    fn transmit(&mut self) -> nb::Result<u32, MockRadioError> {
        self.transmit_count += 1;
        Ok(0) // 0 ms on-air (instant in mock)
    }

    fn prepare_rx(&mut self, _config: RxConfig, _window: RxWindow) -> Result<(), MockRadioError> {
        Ok(())
    }

    fn receive(&mut self, buf: &mut [u8]) -> nb::Result<(usize, RxQuality), MockRadioError> {
        if self.rx_responses.is_empty() {
            return Err(nb::Error::WouldBlock);
        }
        // Shift the first response out of the queue.
        let response = {
            let first = &self.rx_responses[0];
            RxResponse {
                payload: first.payload.clone(),
                quality: first.quality,
            }
        };
        // Remove first element by rotating left by 1 and popping.
        // heapless::Vec has no remove() at arbitrary index; this is the workaround.
        for i in 1..self.rx_responses.len() {
            let val = self.rx_responses[i].clone();
            self.rx_responses[i - 1] = val;
        }
        self.rx_responses.truncate(self.rx_responses.len() - 1);

        let len = response.payload.len().min(buf.len());
        buf[..len].copy_from_slice(&response.payload[..len]);
        self.configured_quality = response.quality;
        Ok((len, response.quality))
    }

    fn set_frequency(&mut self, freq_hz: u32) -> Result<(), MockRadioError> {
        self.current_freq_hz = freq_hz;
        Ok(())
    }

    fn rx_quality(&self) -> RxQuality {
        self.configured_quality
    }

    fn rx_window_offset_ms(&self) -> i32 {
        self.rx_window_offset
    }

    fn rx_window_duration_ms(&self) -> u32 {
        self.rx_window_duration
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_radio_has_empty_tx_log() {
        let radio = MockLoraRadio::new();
        assert!(radio.tx_calls.is_empty());
        assert_eq!(radio.transmit_count, 0);
    }

    #[test]
    fn prepare_tx_records_call() {
        let mut radio = MockLoraRadio::new();
        let config = TxConfig {
            freq_hz: 868_100_000,
            sf: crate::SpreadingFactor::SF12,
            bw: crate::Bandwidth::BW125,
            cr: crate::CodingRate::Cr45,
            power_dbm: 14,
        };
        radio.prepare_tx(config, &[0x01, 0x02]).unwrap();
        assert_eq!(radio.tx_calls.len(), 1);
        assert_eq!(&radio.tx_calls[0].payload[..], &[0x01, 0x02]);
        assert_eq!(radio.tx_calls[0].config.freq_hz, 868_100_000);
    }

    #[test]
    fn transmit_increments_counter() {
        let mut radio = MockLoraRadio::new();
        radio.transmit().unwrap();
        radio.transmit().unwrap();
        assert_eq!(radio.transmit_count, 2);
    }

    #[test]
    fn receive_returns_would_block_when_empty() {
        let mut radio = MockLoraRadio::new();
        let mut buf = [0u8; 32];
        assert!(matches!(
            radio.receive(&mut buf),
            Err(nb::Error::WouldBlock)
        ));
    }

    #[test]
    fn receive_returns_queued_response() {
        let mut radio = MockLoraRadio::new();
        radio
            .queue_rx_response(&[0x01, 0x02, 0x03], RxQuality { rssi: -80, snr: 8 })
            .unwrap();
        let mut buf = [0u8; 32];
        let result = radio.receive(&mut buf);
        assert!(result.is_ok());
        let (len, _quality) = result.unwrap();
        assert_eq!(len, 3);
        assert_eq!(&buf[..3], &[0x01, 0x02, 0x03]);
    }

    #[test]
    fn receive_consumes_responses_in_fifo_order() {
        let mut radio = MockLoraRadio::new();
        radio
            .queue_rx_response(&[0xAA], RxQuality::default())
            .unwrap();
        radio
            .queue_rx_response(&[0xBB], RxQuality::default())
            .unwrap();

        let mut buf = [0u8; 32];
        radio.receive(&mut buf).unwrap();
        assert_eq!(buf[0], 0xAA);

        radio.receive(&mut buf).unwrap();
        assert_eq!(buf[0], 0xBB);

        // Queue is now empty
        assert!(radio.rx_queue_empty());
        assert!(matches!(
            radio.receive(&mut buf),
            Err(nb::Error::WouldBlock)
        ));
    }

    #[test]
    fn set_frequency_stores_value() {
        let mut radio = MockLoraRadio::new();
        radio.set_frequency(869_525_000).unwrap();
        assert_eq!(radio.current_freq_hz(), 869_525_000);
    }

    #[test]
    fn rx_quality_returns_last_configured() {
        let radio = MockLoraRadio::new();
        let q = radio.rx_quality();
        assert_eq!(q.rssi, -90);
        assert_eq!(q.snr, 5);
    }

    #[test]
    fn timing_defaults() {
        let radio = MockLoraRadio::new();
        assert_eq!(radio.rx_window_offset_ms(), -200);
        assert_eq!(radio.rx_window_duration_ms(), 500);
    }
}
