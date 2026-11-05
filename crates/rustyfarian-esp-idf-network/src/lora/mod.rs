//! LoRa radio driver and LoRaWAN Class A protocol for ESP-IDF projects.
//!
//! This crate provides the ESP-IDF-specific [`sx1262_driver::EspIdfLoraRadio`] implementation.
//! Platform-independent types (traits, config, LoRaWAN protocol) are in the
//! [`juggler::lora`] module, re-exported here for convenience.
//!
//! `rustyfarian_esp_hal_network::lora` re-exports the same shared surface (ADR 022).

// Re-export all pure types for backward compatibility.
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

pub mod sx1262_driver;
