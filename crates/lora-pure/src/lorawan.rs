//! LoRaWAN Class A device and session persistence types.
//!
//! [`LorawanDevice`] is generic over any [`crate::LoraRadio`] implementation,
//! so it can be driven by a [`crate::mock::MockLoraRadio`] in host-side tests
//! or by `rustyfarian_esp_idf_lora::sx1262_driver::EspIdfLoraRadio` on the Heltec V3.
//!
//! # Implementation status
//!
//! The public API and all persistent state types are fully defined here.
//! The internal state machine wiring to `lorawan-device`'s `nb_device` module
//! is a HIGH RISK item (see `docs/ROADMAP.md`): the exact `PhyRxTx` bridge
//! API needs to be verified against `lorawan-device 0.12` on hardware before the
//! `process()` implementation can advance the real LoRaWAN state machine.
//!
//! Until then, `join()`, `send()`, and `process()` compile and return sensible
//! sentinel values so that beekeeper can boot, log the situation, and continue
//! operating with Wi-Fi OTA while LoRa integration is completed.

use crate::{LoraConfig, LoraRadio};
use heapless::Vec;

// ─── Session data ─────────────────────────────────────────────────────────────

/// Persistent LoRaWAN session state saved across deep sleep cycles (Phase 7).
///
/// All fields that the OTAA join accept frame populates are included so that
/// `restore_from_sleep` can reconstruct the full session without a re-join.
/// Omitting any of these fields would cause the device to silently use
/// hard-coded defaults after wake, which breaks EU868 DR offsets and RX timing.
///
/// Layout: 56 bytes, align 4.
/// `repr(C)` makes the layout predictable and documents intent for RTC memory placement.
#[repr(C)]
#[derive(Copy, Clone)]
pub struct LorawanSessionData {
    /// 4-byte device address assigned by the network server.
    pub dev_addr: [u8; 4],
    /// Network session key (for MAC layer integrity).
    pub nwk_skey: [u8; 16],
    /// Application session key (for payload encryption).
    pub app_skey: [u8; 16],
    /// Uplink frame counter.
    pub fcnt_up: u32,
    /// Downlink frame counter.
    pub fcnt_down: u32,
    /// Data-rate offset for the RX1 window (from join accept, default 0).
    pub rx1_dr_offset: u8,
    /// Default data rate for the RX2 window (from join accept, EU868 default DR0).
    pub rx2_datarate: u8,
    /// Delay in seconds from end of TX to RX1 window open (from join accept, default 1).
    pub rx1_delay_s: u8,
    /// [`Region`][crate::config::Region] enum discriminant — needed to reconstruct
    /// region configuration on wake without re-storing the full `LoraConfig`.
    pub region: u8,
    /// `1` after a successful join, `0` on cold boot.
    /// Using `u8` rather than `bool` avoids undefined behaviour when reading from
    /// uninitialised or corrupt RTC memory — any byte value other than 0 or 1 is
    /// safe to compare with `== 1`.
    pub valid: u8,
    pub _pad: [u8; 3],
    /// CRC-32 over key session fields (Phase 7: integrity check in RTC memory).
    /// Initialised to 0 now; the Phase 7 implementation writes the real checksum.
    pub crc32: u32,
}

impl LorawanSessionData {
    /// Returns a zero-initialised, invalid session for use on cold boot.
    ///
    /// Do not rely on hardware RTC reset state — it is undefined for slow RTC memory.
    /// Always call this on first boot, not `unsafe { core::mem::zeroed() }`.
    pub const fn empty() -> Self {
        Self {
            dev_addr: [0u8; 4],
            nwk_skey: [0u8; 16],
            app_skey: [0u8; 16],
            fcnt_up: 0,
            fcnt_down: 0,
            rx1_dr_offset: 0,
            rx2_datarate: 0,
            rx1_delay_s: 1,
            region: 0,
            valid: 0,
            _pad: [0u8; 3],
            crc32: 0,
        }
    }
}

impl core::fmt::Debug for LorawanSessionData {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LorawanSessionData")
            .field("dev_addr", &self.dev_addr)
            .field("nwk_skey", &"<redacted>")
            .field("app_skey", &"<redacted>")
            .field("fcnt_up", &self.fcnt_up)
            .field("fcnt_down", &self.fcnt_down)
            .field("valid", &self.valid)
            .field("crc32", &self.crc32)
            .finish()
    }
}

// Verify the struct is exactly 56 bytes at compile time.
const _: () = assert!(core::mem::size_of::<LorawanSessionData>() == 56);

// ─── Error type ───────────────────────────────────────────────────────────────

/// Errors returned by [`LorawanDevice`] operations.
///
/// Generic over the radio error type `E` so that `LorawanDevice<MockLoraRadio>`
/// and `LorawanDevice<EspIdfLoraRadio>` each carry their own concrete error without boxing.
#[derive(Debug)]
pub enum LorawanError<E: core::fmt::Debug> {
    /// A radio operation failed. Wraps the radio's own error type.
    Radio(E),
    /// OTAA join request was rejected or timed out.
    JoinFailed,
    /// The LoRaWAN session has expired (frame counter exhausted or invalidated).
    SessionExpired,
    /// The uplink frame counter has exhausted its range.
    /// LoRaWAN 1.0/1.1 uses 32-bit counters (FCntUp); the exact rollover
    /// policy depends on the network server configuration.
    FrameCounterExhausted,
    /// Protocol-level error (malformed downlink, unexpected state transition).
    Protocol,
}

impl<E: core::fmt::Debug + core::fmt::Display> core::fmt::Display for LorawanError<E> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Radio(e) => write!(f, "radio error: {}", e),
            Self::JoinFailed => write!(f, "OTAA join failed"),
            Self::SessionExpired => write!(f, "session expired"),
            Self::FrameCounterExhausted => write!(f, "frame counter exhausted"),
            Self::Protocol => write!(f, "protocol error"),
        }
    }
}

// ─── Response type ────────────────────────────────────────────────────────────

/// Response from a [`LorawanDevice::process`] tick.
///
/// Mirrors the relevant variants of `lorawan-device`'s `nb_device::Response`
/// without exposing that crate's types on the public API boundary.
// `DownlinkReceived(Downlink)` contains a heapless::Vec<u8, 222> which is
// stack-allocated and cannot be boxed in a no_std context without alloc.
// The enum is only ever returned from a function, never stored in collections.
#[allow(clippy::large_enum_variant)]
#[derive(Debug)]
pub enum LorawanResponse {
    /// Tick again in `ms` milliseconds. Cap at 100 ms during active TX/RX phases.
    TimeoutRequest(u32),
    /// OTAA join was accepted by the network server.
    JoinSuccess,
    /// A downlink was received.
    DownlinkReceived(Downlink),
    /// OTAA join attempt failed (no response from network server).
    JoinFailed,
    /// No state change this tick. Default idle interval applies (100 ms).
    NoUpdate,
}

// ─── Supporting types ─────────────────────────────────────────────────────────

/// Current LoRaWAN device state, queryable from the main loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LorawanState {
    Idle,
    Joining,
    Joined,
    JoinFailed,
}

/// A received LoRaWAN downlink payload.
#[derive(Debug)]
pub struct Downlink {
    /// LoRaWAN application port (1–223).
    pub port: u8,
    /// Payload bytes (max 222 bytes at DR5; fewer at lower data rates).
    pub data: Vec<u8, 222>,
    /// RSSI in dBm of the received packet.
    pub rssi: i16,
}

// ─── Device ───────────────────────────────────────────────────────────────────

/// LoRaWAN Class A device, generic over the radio driver.
///
/// # Integration status
///
/// The internal wiring to `lorawan-device`'s `nb_device::Device` is pending
/// hardware verification (see HIGH RISK note in `docs/ROADMAP.md`).
/// The current `process()` implementation returns [`LorawanResponse::NoUpdate`]
/// until the `PhyRxTx` bridge is completed.
/// All other parts of the beekeeper firmware continue to function normally.
pub struct LorawanDevice<R: LoraRadio> {
    radio: R,
    config: LoraConfig,
    state: LorawanState,
}

impl<R: LoraRadio> LorawanDevice<R> {
    /// Create a new [`LorawanDevice`] with the given radio and application config.
    pub fn new(radio: R, config: LoraConfig) -> Self {
        Self {
            radio,
            config,
            state: LorawanState::Idle,
        }
    }

    /// Queue an OTAA join request.
    ///
    /// The join exchange completes asynchronously over several calls to [`process`][Self::process].
    pub fn join(&mut self) -> Result<(), LorawanError<R::Error>> {
        log::info!(
            "LoRaWAN: queuing OTAA join (region={:?}) [stub — LoRaWAN stack not yet wired]",
            self.config.region
        );
        self.state = LorawanState::Joining;
        // TODO: call lorawan-device nb_device join request
        // When the PhyRxTx bridge is implemented, this will delegate to:
        //   self.inner.join(OTAA, &config).map_err(LorawanError::Protocol)?;
        Ok(())
    }

    /// Queue an uplink on the given port.
    ///
    /// The frame is transmitted on the next call to [`process`][Self::process] when
    /// the state machine is in the TX phase.
    pub fn send(
        &mut self,
        port: u8,
        data: &[u8],
        confirmed: bool,
    ) -> Result<(), LorawanError<R::Error>> {
        if self.state != LorawanState::Joined {
            log::warn!("LoRaWAN: cannot send — not joined (state={:?})", self.state);
            return Err(LorawanError::Protocol);
        }
        log::info!(
            "LoRaWAN: queuing uplink port={} len={} confirmed={} [stub — LoRaWAN stack not yet wired]",
            port,
            data.len(),
            confirmed
        );
        // TODO: delegate to lorawan-device nb_device send
        Ok(())
    }

    /// Advance the LoRaWAN state machine by one tick.
    ///
    /// Call at ≤100 ms intervals during join and active TX/RX phases.
    /// Returns the suggested delay until the next call.
    ///
    /// # Implementation status
    ///
    /// This currently returns [`LorawanResponse::NoUpdate`] — the `PhyRxTx`
    /// bridge to `lorawan-device`'s `nb_device::Device` is pending hardware
    /// verification. The LoRaWAN stack will be active once the bridge is wired up.
    pub fn process(&mut self) -> Result<LorawanResponse, LorawanError<R::Error>> {
        // TODO: Replace this stub with the real nb_device tick:
        //
        //   let event = if DIO1_FLAG.swap(false, Ordering::AcqRel) {
        //       nb_device::Event::RadioEvent(self.radio.get_phy_response())
        //   } else {
        //       nb_device::Event::TimeoutFired
        //   };
        //   match self.inner.handle_event(event) { ... }
        //
        // See docs/ROADMAP.md §"Main Loop Timing — Critical Constraint".
        Ok(LorawanResponse::NoUpdate)
    }

    /// Return the current device state.
    pub fn state(&self) -> LorawanState {
        self.state
    }

    /// Return `true` if the device has completed OTAA and has an active session.
    pub fn is_joined(&self) -> bool {
        self.state == LorawanState::Joined
    }

    /// Consume the device, returning its session data and radio for deep sleep.
    ///
    /// # Caller responsibility
    ///
    /// No RX window must be open when this is called.
    /// Ensure the current TX/RX cycle is complete before calling `prepare_sleep`.
    /// The returned [`LorawanSessionData`] must be written to RTC memory before sleep.
    pub fn prepare_sleep(self) -> (LorawanSessionData, R) {
        // TODO: extract real session keys from the lorawan-device state machine.
        // For now, return an empty (invalid) session — the device will re-join after wake.
        let session = LorawanSessionData::empty();
        (session, self.radio)
    }

    /// Reconstruct a [`LorawanDevice`] from session data saved before deep sleep.
    ///
    /// If `session.valid` is `0` (cold boot or expired session), the device
    /// is initialised in `Idle` state and must call `join()` before sending.
    ///
    /// Use [`LorawanSessionData::empty()`] on cold boot to guarantee a clean
    /// zero-initialised session.
    /// Never pass uninitialised RTC memory — always zero-initialise with `empty()` first.
    pub fn restore_from_sleep(radio: R, session: LorawanSessionData, config: LoraConfig) -> Self {
        let state = if session.valid == 1 {
            log::info!("LoRaWAN: restoring joined session from RTC memory");
            LorawanState::Joined
        } else {
            log::info!("LoRaWAN: no valid session in RTC memory — will join on next cycle");
            LorawanState::Idle
        };
        Self {
            radio,
            config,
            state,
        }
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_data_size() {
        assert_eq!(core::mem::size_of::<LorawanSessionData>(), 56);
    }

    #[test]
    fn session_data_empty_is_invalid() {
        let s = LorawanSessionData::empty();
        assert_eq!(s.valid, 0);
        assert_eq!(s.crc32, 0);
        assert_eq!(s.dev_addr, [0u8; 4]);
        assert_eq!(s.rx1_delay_s, 1);
    }

    #[cfg(feature = "mock")]
    mod with_mock {
        use super::*;
        use crate::config::Region;
        use crate::mock::MockLoraRadio;

        fn make_device() -> LorawanDevice<MockLoraRadio> {
            let radio = MockLoraRadio::new();
            let config = LoraConfig {
                region: Region::EU868,
                ..LoraConfig::default()
            };
            LorawanDevice::new(radio, config)
        }

        #[test]
        fn initially_not_joined() {
            let device = make_device();
            assert!(!device.is_joined());
            assert_eq!(device.state(), LorawanState::Idle);
        }

        #[test]
        fn join_transitions_to_joining() {
            let mut device = make_device();
            device.join().unwrap();
            assert_eq!(device.state(), LorawanState::Joining);
        }

        #[test]
        fn send_fails_when_not_joined() {
            let mut device = make_device();
            let result = device.send(10, &[0x01], false);
            assert!(result.is_err());
        }

        #[test]
        fn process_returns_no_update_on_stub() {
            let mut device = make_device();
            let response = device.process().unwrap();
            assert!(matches!(response, LorawanResponse::NoUpdate));
        }

        #[test]
        fn prepare_sleep_returns_invalid_session_stub() {
            let device = make_device();
            let (session, _radio) = device.prepare_sleep();
            assert_eq!(session.valid, 0);
        }

        #[test]
        fn restore_from_sleep_with_invalid_session_is_idle() {
            let radio = MockLoraRadio::new();
            let config = LoraConfig::default();
            let session = LorawanSessionData::empty();
            let device = LorawanDevice::restore_from_sleep(radio, session, config);
            assert_eq!(device.state(), LorawanState::Idle);
        }

        #[test]
        fn restore_from_sleep_with_valid_session_is_joined() {
            let radio = MockLoraRadio::new();
            let config = LoraConfig::default();
            let mut session = LorawanSessionData::empty();
            session.valid = 1;
            let device = LorawanDevice::restore_from_sleep(radio, session, config);
            assert_eq!(device.state(), LorawanState::Joined);
            assert!(device.is_joined());
        }
    }
}
