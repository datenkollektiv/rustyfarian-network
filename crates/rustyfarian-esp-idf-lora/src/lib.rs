//! LoRa radio driver and LoRaWAN Class A protocol for ESP-IDF projects.
//!
//! This crate provides the ESP-IDF-specific [`sx1262_driver::EspIdfLoraRadio`] implementation.
//! Platform-independent types (traits, config, LoRaWAN protocol) are in the
//! [`juggler::lora`] module, re-exported here for convenience.

// Re-export all pure types for backward compatibility.
pub use juggler::lora::commands;
pub use juggler::lora::config;
pub use juggler::lora::lorawan;
pub use juggler::lora::{
    Bandwidth, CodingRate, LoraRadio, RxConfig, RxQuality, RxWindow, SpreadingFactor, TxConfig,
    RX_WINDOW_DURATION_MS, RX_WINDOW_OFFSET_MS,
};
pub use juggler::lora::{
    Downlink, LorawanDevice, LorawanError, LorawanResponse, LorawanSessionData, LorawanState,
};
pub use juggler::lora::{HeltecV3Pins, LoraConfig, Region};

pub mod sx1262_driver;
