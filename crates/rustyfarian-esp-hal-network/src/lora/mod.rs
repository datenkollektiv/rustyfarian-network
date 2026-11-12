//! LoRa radio driver for ESP-HAL projects (bare-metal, no_std).
//!
//! This crate provides [`EspHalLoraRadio`], an implementation of the
//! [`juggler::lora::LoraRadio`] trait using `esp-hal` SPI and GPIO peripherals.
//!
//! The shared surface — config, LoRaWAN types, the [`LorawanPhy`] bridge, the
//! [`LorawanTimeline`], the [`sx1262`] bring-up and the PHY rules — is re-exported
//! from [`juggler::lora`] under the same paths as `rustyfarian_esp_idf_network::lora`
//! (ADR 022), so consumer code swaps between the tiers.
//!
//! # Status
//!
//! This driver is a stub.
//! Each method returns the most appropriate error variant for the operation:
//! TX methods return [`driver::LoraError::TransmitFailed`],
//! RX methods return [`driver::LoraError::ReceiveFailed`], and
//! configuration methods return [`driver::LoraError::RadioInitFailed`].
//! Hardware integration is planned as part of the TTN EU868 OTAA validation.
//!
//! # LED status
//!
//! [`EspHalLoraRadio`] accepts a generic `S: StatusLed` at construction time,
//! enabling WS2812 RGB LED feedback (join, uplink, downlink states) or
//! [`pennant::NoLed`] for headless configurations.

pub use juggler::lora::commands;
pub use juggler::lora::config;
pub use juggler::lora::lorawan;
pub use juggler::lora::sx1262;
pub use juggler::lora::{
    classify_irq, classify_response, ldro_required, map_rf_config, requested_deadline, IrqOutcome,
    LorawanPhy, LorawanTimeline, Outcome, RadioErrorKind, TickEvent, DOWNLINK_CRC_ON,
    DOWNLINK_IQ_INVERTED, IRQ_CRC_ERR, IRQ_RX_DONE, IRQ_TIMEOUT, IRQ_TX_DONE, LORAWAN_PREAMBLE_LEN,
    LORAWAN_PUBLIC_SYNC_WORD, RX_PAYLOAD_MAX, UPLINK_CRC_ON, UPLINK_IQ_INVERTED,
};
pub use juggler::lora::{
    Bandwidth, CodingRate, LoraRadio, RxConfig, RxQuality, RxWindow, SpreadingFactor, TxConfig,
    RX_WINDOW_DURATION_MS, RX_WINDOW_OFFSET_MS,
};
#[allow(deprecated)]
pub use juggler::lora::{
    Downlink, LorawanDevice, LorawanError, LorawanResponse, LorawanSessionData, LorawanState,
};
pub use juggler::lora::{HeltecV3Pins, LoraConfig, Region};

mod driver;
pub use driver::{EspHalLoraRadio, LoraError};

// Tier-parity guard (ADR 022): every item `rustyfarian_esp_idf_network::lora`
// re-exports from `juggler::lora` must resolve here too. A dropped re-export fails
// to compile (E0432). Runs on the host via `just test-lora-hal`
// (`cargo test --no-default-features`, which sets `cfg(test)`). A unit test, not a
// doctest: rustdoc does not set `cfg(test)`, so the module would be invisible to it.
#[cfg(test)]
#[allow(deprecated)]
mod reexport_parity_guard {
    use super::{
        classify_irq, classify_response, commands, config, ldro_required, lorawan, map_rf_config,
        requested_deadline, sx1262, Bandwidth, CodingRate, Downlink, EspHalLoraRadio, HeltecV3Pins,
        IrqOutcome, LoraConfig, LoraError, LoraRadio, LorawanDevice, LorawanError, LorawanPhy,
        LorawanResponse, LorawanSessionData, LorawanState, LorawanTimeline, Outcome,
        RadioErrorKind, Region, RxConfig, RxQuality, RxWindow, SpreadingFactor, TickEvent,
        TxConfig, DOWNLINK_CRC_ON, DOWNLINK_IQ_INVERTED, IRQ_CRC_ERR, IRQ_RX_DONE, IRQ_TIMEOUT,
        IRQ_TX_DONE, LORAWAN_PREAMBLE_LEN, LORAWAN_PUBLIC_SYNC_WORD, RX_PAYLOAD_MAX,
        RX_WINDOW_DURATION_MS, RX_WINDOW_OFFSET_MS, UPLINK_CRC_ON, UPLINK_IQ_INVERTED,
    };

    #[test]
    fn lora_public_surface_matches_the_idf_tier() {
        fn assert_exported<T>() {}
        assert_exported::<Bandwidth>();
        assert_exported::<CodingRate>();
        assert_exported::<SpreadingFactor>();
        assert_exported::<TxConfig>();
        assert_exported::<RxConfig>();
        assert_exported::<RxWindow>();
        assert_exported::<RxQuality>();
        assert_exported::<Downlink>();
        assert_exported::<HeltecV3Pins>();
        assert_exported::<LoraConfig>();
        assert_exported::<Region>();
        assert_exported::<LorawanError<LoraError>>();
        assert_exported::<LorawanResponse>();
        assert_exported::<LorawanSessionData>();
        assert_exported::<LorawanState>();
        assert_exported::<LorawanTimeline>();
        let _ = (classify_response, requested_deadline);
        assert_exported::<TickEvent>();
        assert_exported::<Outcome>();
        assert_exported::<IrqOutcome>();
        assert_exported::<sx1262::InitError>();
        assert_exported::<sx1262::InitStep>();
        assert_exported::<config::Region>();
        assert_exported::<lorawan::LorawanState>();

        fn accepts_radio<R: LoraRadio>() {}
        accepts_radio::<EspHalLoraRadio<pennant::NoLed>>();
        fn accepts_device<R: LoraRadio>(_: &LorawanDevice<R>) {}
        let _ = accepts_device::<EspHalLoraRadio<pennant::NoLed>>;
        // The shared bridge accepts this tier's radio, as it does the IDF radio.
        assert_exported::<LorawanPhy<EspHalLoraRadio<pennant::NoLed>>>();
        assert!(matches!(
            <LoraError as RadioErrorKind>::invalid_rf_config(),
            LoraError::InvalidRfConfig
        ));
        assert!(LoraError::Timeout.is_rx_timeout());

        let _ = (classify_irq, ldro_required, map_rf_config);
        let _ = (
            DOWNLINK_CRC_ON,
            DOWNLINK_IQ_INVERTED,
            IRQ_CRC_ERR,
            IRQ_RX_DONE,
            IRQ_TIMEOUT,
            IRQ_TX_DONE,
            LORAWAN_PREAMBLE_LEN,
            LORAWAN_PUBLIC_SYNC_WORD,
            RX_PAYLOAD_MAX,
            RX_WINDOW_DURATION_MS,
            RX_WINDOW_OFFSET_MS,
            UPLINK_CRC_ON,
            UPLINK_IQ_INVERTED,
        );
        let _ = commands::OTA_COMMAND_PORT;
    }
}
