//! [`LorawanPhy`] — the `lorawan-device` `PhyRxTx + Timings` bridge over a [`LoraRadio`].
//!
//! `lorawan-device`'s `nb_device::Device` drives the radio through
//! `PhyRxTx::handle_event`. This bridge translates those events into
//! [`LoraRadio`] port calls and owns the RX buffer that `get_received_packet`
//! hands back. Both tiers use it: the ESP-IDF tier names it `LoraRadioAdapter`.
//!
//! [`LorawanPhy::phy_due`] is the filter the event loop asks before it delivers a `Phy` event:
//! `lorawan-device` 0.12 panics on any response but `TxDone` while a TX is on air, so an IRQ
//! that does not end the TX is withheld, and a stray event that gets through anyway is
//! rejected with [`RadioErrorKind::tx_in_progress`] instead of a `Txing` response.
//!
//! A `Phy` event that arrives while no TX or RX is in progress is out of phase.
//! Internally it is a `PhyOutcome::Ignored`, distinct from a closed RX window;
//! both become `Response::Idle` only at the `lorawan-device` boundary, and
//! [`LorawanPhy::ignored_events`] counts the ignored ones.

use lorawan_device::{
    nb_device::radio::{
        Bandwidth as LdBandwidth, CodingRate as LdCodingRate, Event as LdRadioEvent, PhyRxTx,
        Response as LdRadioResponse, RfConfig, RxQuality as LdRxQuality,
        SpreadingFactor as LdSpreadingFactor,
    },
    Timings,
};

use super::{
    Bandwidth, CodingRate, LoraRadio, RadioErrorKind, RxConfig, RxWindow, SpreadingFactor, TxConfig,
};

/// Size of the RX buffer: the LoRa PHY maximum payload plus one spare byte.
const RX_BUF_LEN: usize = 256;

/// Map a `lorawan-device` RF configuration to `juggler::lora` types.
///
/// Returns `(frequency_hz, sf, bw, cr)`, or `None` for a spreading factor or
/// bandwidth that has no [`SpreadingFactor`] / [`Bandwidth`] equivalent.
pub fn map_rf_config(rf: &RfConfig) -> Option<(u32, SpreadingFactor, Bandwidth, CodingRate)> {
    let sf = match rf.bb.sf {
        LdSpreadingFactor::_7 => SpreadingFactor::SF7,
        LdSpreadingFactor::_8 => SpreadingFactor::SF8,
        LdSpreadingFactor::_9 => SpreadingFactor::SF9,
        LdSpreadingFactor::_10 => SpreadingFactor::SF10,
        LdSpreadingFactor::_11 => SpreadingFactor::SF11,
        LdSpreadingFactor::_12 => SpreadingFactor::SF12,
        _ => return None,
    };
    let bw = match rf.bb.bw {
        LdBandwidth::_125KHz => Bandwidth::BW125,
        LdBandwidth::_250KHz => Bandwidth::BW250,
        LdBandwidth::_500KHz => Bandwidth::BW500,
        _ => return None,
    };
    let cr = match rf.bb.cr {
        LdCodingRate::_4_5 => CodingRate::Cr45,
        LdCodingRate::_4_6 => CodingRate::Cr46,
        LdCodingRate::_4_7 => CodingRate::Cr47,
        LdCodingRate::_4_8 => CodingRate::Cr48,
    };
    Some((rf.frequency, sf, bw, cr))
}

/// The radio operation in progress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RadioOp {
    Idle,
    Txing,
    Rxing,
}

/// What a `Phy` event did, before it is mapped to a `lorawan-device` response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PhyOutcome {
    /// The TX finished after `on_air_ms`.
    TxDone(u32),
    /// The RX window is still listening.
    StillRxing,
    /// A packet was received with this RSSI (dBm) and SNR (dB).
    RxDone { rssi: i16, snr: i8 },
    /// The RX window closed with no packet.
    WindowClosed,
    /// No TX or RX was in progress: the event was out of phase.
    Ignored,
}

/// Bridges a [`LoraRadio`] to `lorawan-device`'s `PhyRxTx + Timings` traits.
pub struct LorawanPhy<R: LoraRadio> {
    radio: R,
    rx_buf: [u8; RX_BUF_LEN],
    rx_len: usize,
    op: RadioOp,
    ignored: u32,
    /// The finished TX poll made by [`LorawanPhy::phy_due`], consumed by the `Phy` event.
    tx_polled: Option<Result<u32, R::Error>>,
}

impl<R: LoraRadio> LorawanPhy<R> {
    /// Wrap an initialised radio.
    pub fn new(radio: R) -> Self {
        Self {
            radio,
            rx_buf: [0u8; RX_BUF_LEN],
            rx_len: 0,
            op: RadioOp::Idle,
            ignored: 0,
            tx_polled: None,
        }
    }

    /// Poll the radio for a latched IRQ — see [`LoraRadio::irq_pending`].
    ///
    /// The event loop calls this each tick; a failed IRQ read counts as "nothing pending".
    pub fn irq_pending(&mut self) -> bool {
        self.radio.irq_pending().unwrap_or(false)
    }

    /// Return `true` when the loop should deliver `Event::RadioEvent(Phy(()))` now.
    ///
    /// `hint` is any other sign of an IRQ, such as a DIO1 edge flag.
    /// While a TX is on air, a pending IRQ only counts once the radio reports the frame
    /// finished (or failed): `lorawan-device` 0.12 panics on any other response in that wait,
    /// so an IRQ that does not end the TX (stale, or one a TX does not wait for) is withheld.
    /// Otherwise it is [`irq_pending`][Self::irq_pending] or `hint`.
    pub fn phy_due(&mut self, hint: bool) -> bool {
        if self.tx_polled.is_some() {
            return true;
        }
        let pending = hint || self.irq_pending();
        if !pending || self.op != RadioOp::Txing {
            return pending;
        }
        match self.radio.transmit() {
            Ok(on_air_ms) => self.tx_polled = Some(Ok(on_air_ms)),
            Err(nb::Error::WouldBlock) => return false,
            Err(nb::Error::Other(e)) => self.tx_polled = Some(Err(e)),
        }
        true
    }

    /// Number of `Phy` events that arrived while no TX or RX was in progress.
    pub fn ignored_events(&self) -> u32 {
        self.ignored
    }

    /// Borrow the wrapped radio.
    pub fn radio(&self) -> &R {
        &self.radio
    }

    /// Mutably borrow the wrapped radio.
    pub fn radio_mut(&mut self) -> &mut R {
        &mut self.radio
    }

    /// Consume the bridge and return the radio.
    pub fn into_radio(self) -> R {
        self.radio
    }

    fn on_phy(&mut self) -> Result<PhyOutcome, R::Error>
    where
        R::Error: RadioErrorKind,
    {
        match self.op {
            RadioOp::Txing => {
                let polled = match self.tx_polled.take() {
                    Some(Ok(on_air_ms)) => Ok(on_air_ms),
                    Some(Err(e)) => Err(nb::Error::Other(e)),
                    None => self.radio.transmit(),
                };
                match polled {
                    Ok(on_air_ms) => {
                        self.op = RadioOp::Idle;
                        Ok(PhyOutcome::TxDone(on_air_ms))
                    }
                    // `lorawan-device` 0.12 panics on any success but `TxDone` while it waits
                    // for the end of a TX, so an early event is an error, never `Txing`.
                    Err(nb::Error::WouldBlock) => {
                        log::debug!("lorawan-phy: Phy event while the TX is on air — rejected");
                        Err(R::Error::tx_in_progress())
                    }
                    Err(nb::Error::Other(e)) => Err(e),
                }
            }
            RadioOp::Rxing => match self.radio.receive(&mut self.rx_buf) {
                Ok((len, quality)) => {
                    self.rx_len = len;
                    self.op = RadioOp::Idle;
                    Ok(PhyOutcome::RxDone {
                        rssi: quality.rssi,
                        snr: quality.snr,
                    })
                }
                Err(nb::Error::WouldBlock) => Ok(PhyOutcome::StillRxing),
                Err(nb::Error::Other(e)) if e.is_rx_timeout() => {
                    // Normal outcome: the window closed with no downlink, so
                    // lorawan-device can proceed (open RX2 or retry).
                    self.rx_len = 0;
                    self.op = RadioOp::Idle;
                    Ok(PhyOutcome::WindowClosed)
                }
                Err(nb::Error::Other(e)) => Err(e),
            },
            RadioOp::Idle => {
                self.ignored = self.ignored.saturating_add(1);
                log::debug!("lorawan-phy: Phy event while idle — ignored");
                Ok(PhyOutcome::Ignored)
            }
        }
    }
}

impl<R: LoraRadio> core::fmt::Debug for LorawanPhy<R> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LorawanPhy")
            .field("op", &self.op)
            .field("rx_len", &self.rx_len)
            .field("ignored", &self.ignored)
            .finish_non_exhaustive()
    }
}

impl<R> PhyRxTx for LorawanPhy<R>
where
    R: LoraRadio,
    R::Error: RadioErrorKind,
{
    /// The event loop delivers `Phy(())` when [`LorawanPhy::irq_pending`] is true.
    type PhyEvent = ();
    type PhyError = R::Error;
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
                let (freq_hz, sf, bw, cr) =
                    map_rf_config(&tx_config.rf).ok_or_else(R::Error::invalid_rf_config)?;
                let config = TxConfig {
                    freq_hz,
                    sf,
                    bw,
                    cr,
                    power_dbm: tx_config.pw,
                };
                self.radio.prepare_tx(config, buf)?;
                self.tx_polled = None;
                self.op = RadioOp::Txing;
                Ok(LdRadioResponse::Txing)
            }

            LdRadioEvent::RxRequest(rf_config) => {
                // Reset stale RX data from a previous window before opening a new one.
                self.rx_len = 0;
                let (freq_hz, sf, bw, cr) =
                    map_rf_config(&rf_config).ok_or_else(R::Error::invalid_rf_config)?;
                let config = RxConfig {
                    freq_hz,
                    sf,
                    bw,
                    cr,
                };
                // lorawan-device 0.12 `RxRequest` does not say which window (RX1/RX2)
                // is being opened; the frequency and DR in `rf_config` already encode it.
                // `RxWindow::Rx1` is a placeholder that no radio uses for configuration.
                self.radio.prepare_rx(config, RxWindow::Rx1)?;
                self.op = RadioOp::Rxing;
                Ok(LdRadioResponse::Rxing)
            }

            LdRadioEvent::CancelRx => {
                log::debug!("lorawan-phy: CancelRx — closing RX window");
                // Best effort: the window is torn down regardless of the result.
                if let Err(e) = self.radio.cancel_rx() {
                    log::debug!("lorawan-phy: cancel_rx failed: {:?}", e);
                }
                self.rx_len = 0;
                self.tx_polled = None;
                self.op = RadioOp::Idle;
                Ok(LdRadioResponse::Idle)
            }

            LdRadioEvent::Phy(()) => Ok(match self.on_phy()? {
                PhyOutcome::TxDone(on_air_ms) => LdRadioResponse::TxDone(on_air_ms),
                PhyOutcome::StillRxing => LdRadioResponse::Rxing,
                PhyOutcome::RxDone { rssi, snr } => {
                    LdRadioResponse::RxDone(LdRxQuality::new(rssi, snr))
                }
                PhyOutcome::WindowClosed | PhyOutcome::Ignored => LdRadioResponse::Idle,
            }),
        }
    }
}

impl<R: LoraRadio> Timings for LorawanPhy<R> {
    fn get_rx_window_offset_ms(&self) -> i32 {
        self.radio.rx_window_offset_ms()
    }

    fn get_rx_window_duration_ms(&self) -> u32 {
        self.radio.rx_window_duration_ms()
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lora::mock::{MockLoraRadio, MockRadioError};
    use crate::lora::RxQuality;
    use lorawan_device::nb_device::radio::{BaseBandModulationParams, TxConfig as LdTxConfig};

    fn rf(sf: LdSpreadingFactor, bw: LdBandwidth, cr: LdCodingRate) -> RfConfig {
        RfConfig {
            frequency: 868_100_000,
            bb: BaseBandModulationParams::new(sf, bw, cr),
        }
    }

    fn sf7() -> RfConfig {
        rf(
            LdSpreadingFactor::_7,
            LdBandwidth::_125KHz,
            LdCodingRate::_4_5,
        )
    }

    fn start_rx(phy: &mut LorawanPhy<MockLoraRadio>) {
        let r = phy.handle_event(LdRadioEvent::RxRequest(sf7())).unwrap();
        assert!(matches!(r, LdRadioResponse::Rxing));
    }

    #[test]
    fn tx_then_txdone_reports_tx_done() {
        let mut phy = LorawanPhy::new(MockLoraRadio::new());
        let tx = LdTxConfig { pw: 14, rf: sf7() };
        let r = phy
            .handle_event(LdRadioEvent::TxRequest(tx, &[0xAA, 0xBB]))
            .unwrap();
        assert!(matches!(r, LdRadioResponse::Txing));
        assert_eq!(phy.radio().tx_calls.len(), 1);
        assert_eq!(phy.radio().tx_calls[0].config.power_dbm, 14);

        let r = phy.handle_event(LdRadioEvent::Phy(())).unwrap();
        assert!(matches!(r, LdRadioResponse::TxDone(0)));
        assert_eq!(phy.op, RadioOp::Idle);
        assert_eq!(phy.ignored_events(), 0);
    }

    #[test]
    fn rx_then_packet_reports_rx_done_and_exposes_payload() {
        let mut phy = LorawanPhy::new(MockLoraRadio::new());
        start_rx(&mut phy);
        phy.radio_mut()
            .queue_rx_response(&[1, 2, 3], RxQuality { rssi: -40, snr: 9 })
            .unwrap();
        let r = phy.handle_event(LdRadioEvent::Phy(())).unwrap();
        match r {
            LdRadioResponse::RxDone(q) => {
                assert_eq!(q.rssi(), -40);
                assert_eq!(q.snr(), 9);
            }
            other => panic!("expected RxDone, got {:?}", other),
        }
        assert_eq!(phy.get_received_packet(), &[1, 2, 3]);
    }

    #[test]
    fn rx_then_timeout_returns_idle_and_op_idle() {
        let mut phy = LorawanPhy::new(MockLoraRadio::new());
        start_rx(&mut phy);
        phy.radio_mut().inject_rx_timeout();
        let r = phy.handle_event(LdRadioEvent::Phy(())).unwrap();
        assert!(matches!(r, LdRadioResponse::Idle));
        assert_eq!(phy.op, RadioOp::Idle);
        assert_eq!(phy.ignored_events(), 0, "a closed window is not ignored");
    }

    #[test]
    fn rx_still_listening_returns_rxing() {
        let mut phy = LorawanPhy::new(MockLoraRadio::new());
        start_rx(&mut phy);
        let r = phy.handle_event(LdRadioEvent::Phy(())).unwrap();
        assert!(matches!(r, LdRadioResponse::Rxing));
        assert_eq!(phy.op, RadioOp::Rxing);
    }

    #[test]
    fn cancel_rx_resets_rx_len() {
        let mut phy = LorawanPhy::new(MockLoraRadio::new());
        start_rx(&mut phy);
        phy.radio_mut()
            .queue_rx_response(&[9, 9], RxQuality::default())
            .unwrap();
        phy.handle_event(LdRadioEvent::Phy(())).unwrap();
        assert_eq!(phy.get_received_packet().len(), 2);

        start_rx(&mut phy);
        let r = phy.handle_event(LdRadioEvent::CancelRx).unwrap();
        assert!(matches!(r, LdRadioResponse::Idle));
        assert!(phy.get_received_packet().is_empty());
        assert_eq!(phy.op, RadioOp::Idle);
        assert_eq!(phy.radio().cancel_count, 1);
    }

    #[test]
    fn phy_event_while_idle_is_ignored_and_counted() {
        let mut phy = LorawanPhy::new(MockLoraRadio::new());
        assert_eq!(phy.on_phy(), Ok(PhyOutcome::Ignored));
        let r = phy.handle_event(LdRadioEvent::Phy(())).unwrap();
        assert!(matches!(r, LdRadioResponse::Idle));
        assert_eq!(phy.ignored_events(), 2);
        assert_eq!(phy.radio().transmit_count, 0);
    }

    #[test]
    fn rx_timeout_differs_from_ignored_internally() {
        let mut phy = LorawanPhy::new(MockLoraRadio::new());
        start_rx(&mut phy);
        phy.radio_mut().inject_rx_timeout();
        assert_eq!(phy.on_phy(), Ok(PhyOutcome::WindowClosed));
        assert_eq!(phy.on_phy(), Ok(PhyOutcome::Ignored));
    }

    #[test]
    fn unsupported_sf_is_invalid_rf_config() {
        let mut phy = LorawanPhy::new(MockLoraRadio::new());
        let bad = rf(
            LdSpreadingFactor::_5,
            LdBandwidth::_125KHz,
            LdCodingRate::_4_5,
        );
        let r = phy.handle_event(LdRadioEvent::RxRequest(bad));
        assert!(matches!(r, Err(MockRadioError::InvalidRfConfig)));
        assert_eq!(phy.op, RadioOp::Idle);
    }

    #[test]
    fn irq_pending_passes_through() {
        let mut phy = LorawanPhy::new(MockLoraRadio::new());
        assert!(!phy.irq_pending());
        phy.radio_mut().irq_latched = true;
        assert!(phy.irq_pending());
    }

    #[test]
    fn map_rf_config_round_trip() {
        let sfs = [
            (LdSpreadingFactor::_7, SpreadingFactor::SF7),
            (LdSpreadingFactor::_8, SpreadingFactor::SF8),
            (LdSpreadingFactor::_9, SpreadingFactor::SF9),
            (LdSpreadingFactor::_10, SpreadingFactor::SF10),
            (LdSpreadingFactor::_11, SpreadingFactor::SF11),
            (LdSpreadingFactor::_12, SpreadingFactor::SF12),
        ];
        let bws = [
            (LdBandwidth::_125KHz, Bandwidth::BW125),
            (LdBandwidth::_250KHz, Bandwidth::BW250),
            (LdBandwidth::_500KHz, Bandwidth::BW500),
        ];
        let crs = [
            (LdCodingRate::_4_5, CodingRate::Cr45),
            (LdCodingRate::_4_6, CodingRate::Cr46),
            (LdCodingRate::_4_7, CodingRate::Cr47),
            (LdCodingRate::_4_8, CodingRate::Cr48),
        ];
        for (ld_sf, sf) in sfs {
            for (ld_bw, bw) in bws {
                for (ld_cr, cr) in crs {
                    let mapped = map_rf_config(&rf(ld_sf, ld_bw, ld_cr));
                    assert_eq!(mapped, Some((868_100_000, sf, bw, cr)));
                }
            }
        }
    }

    #[test]
    fn map_rf_config_rejects_unsupported_values() {
        let narrow = rf(
            LdSpreadingFactor::_7,
            LdBandwidth::_62KHz,
            LdCodingRate::_4_5,
        );
        assert_eq!(map_rf_config(&narrow), None);
        let sf6 = rf(
            LdSpreadingFactor::_6,
            LdBandwidth::_125KHz,
            LdCodingRate::_4_5,
        );
        assert_eq!(map_rf_config(&sf6), None);
    }

    #[test]
    fn timings_come_from_the_radio() {
        let mut radio = MockLoraRadio::new();
        radio.rx_window_offset = -123;
        radio.rx_window_duration = 456;
        let phy = LorawanPhy::new(radio);
        assert_eq!(phy.get_rx_window_offset_ms(), -123);
        assert_eq!(phy.get_rx_window_duration_ms(), 456);
    }

    // ─── Driving the real `lorawan_device` Device ─────────────────────────────

    use lorawan_device::{
        default_crypto::DefaultFactory,
        nb_device::{Device, Event as DevEvent, Response as DevResponse},
        region::{Configuration, Region},
        AppSKey, DevAddr, JoinMode, NewSKey, RngCore,
    };

    struct TestRng(u64);

    impl RngCore for TestRng {
        fn next_u32(&mut self) -> u32 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1);
            (self.0 >> 32) as u32
        }

        fn next_u64(&mut self) -> u64 {
            (u64::from(self.next_u32()) << 32) | u64::from(self.next_u32())
        }

        fn fill_bytes(&mut self, dest: &mut [u8]) {
            rand_core::impls::fill_bytes_via_next(self, dest);
        }

        fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
            self.fill_bytes(dest);
            Ok(())
        }
    }

    type TestDevice = Device<LorawanPhy<MockLoraRadio>, DefaultFactory, TestRng, 256>;

    /// An ABP-joined device whose uplink is on air: the `SendingData` wait.
    fn device_with_uplink_on_air(busy_polls: usize) -> TestDevice {
        let mut radio = MockLoraRadio::new();
        radio.tx_busy_polls = busy_polls;
        let mut device = Device::new(
            Configuration::new(Region::EU868),
            LorawanPhy::new(radio),
            TestRng(7),
        );
        let joined = device.join(JoinMode::ABP {
            newskey: NewSKey::from([0u8; 16]),
            appskey: AppSKey::from([0u8; 16]),
            devaddr: DevAddr::from(0),
        });
        assert!(matches!(joined, Ok(DevResponse::JoinSuccess)));
        let sent = device.send(b"hi", 1, false);
        assert!(matches!(sent, Ok(DevResponse::UplinkSending(_))));
        device
    }

    fn deliver_phy(device: &mut TestDevice) -> bool {
        matches!(
            device.handle_event(DevEvent::RadioEvent(LdRadioEvent::Phy(()))),
            Ok(DevResponse::TimeoutRequest(_))
        )
    }

    #[test]
    fn a_premature_phy_event_during_an_uplink_does_not_panic_the_device() {
        let mut device = device_with_uplink_on_air(2);
        for _ in 0..2 {
            let early = device.handle_event(DevEvent::RadioEvent(LdRadioEvent::Phy(())));
            assert!(
                matches!(
                    early,
                    Err(lorawan_device::nb_device::Error::Radio(
                        MockRadioError::TxInProgress
                    ))
                ),
                "an early event is an error the device survives"
            );
        }
        assert!(
            deliver_phy(&mut device),
            "the real TxDone still completes the uplink and schedules RX1"
        );
    }

    #[test]
    fn the_loop_filter_withholds_an_irq_that_does_not_end_the_tx() {
        let mut device = device_with_uplink_on_air(1);
        device.get_radio().radio_mut().irq_latched = true;
        let phy = device.get_radio();
        assert!(
            !phy.phy_due(false),
            "a stale IRQ while the frame is on air is withheld"
        );
        assert!(phy.phy_due(true), "the next sample sees the TX finished");
        assert!(
            phy.phy_due(false),
            "the finished poll stays due until delivered"
        );
        assert!(deliver_phy(&mut device));
        assert_eq!(
            device.get_radio().radio().transmit_count,
            2,
            "the filter's poll is the one the event consumes, not a second transmit"
        );
    }

    #[test]
    fn the_loop_filter_passes_a_dio1_hint_only_once_the_tx_ended() {
        let mut device = device_with_uplink_on_air(1);
        let phy = device.get_radio();
        assert!(
            !phy.phy_due(true),
            "an edge hint during the TX is checked, not trusted"
        );
        assert!(phy.phy_due(true));
        assert!(deliver_phy(&mut device));
    }

    #[test]
    fn the_loop_filter_leaves_other_states_alone() {
        let mut phy = LorawanPhy::new(MockLoraRadio::new());
        assert!(!phy.phy_due(false));
        assert!(phy.phy_due(true));
        phy.radio_mut().irq_latched = true;
        assert!(phy.phy_due(false));
        start_rx(&mut phy);
        assert!(phy.phy_due(false));
        assert_eq!(phy.radio().transmit_count, 0);
    }
}
