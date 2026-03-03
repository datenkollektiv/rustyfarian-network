//! LoRa radio driver and LoRaWAN Class A protocol for ESP-IDF projects.
//!
//! This crate provides the ESP-IDF-specific [`sx1262_driver::EspIdfLoraRadio`] implementation.
//! Platform-independent types (traits, config, LoRaWAN protocol) are in the
//! [`lora_pure`] crate, re-exported here for convenience.

// Re-export all pure types for backward compatibility.
pub use lora_pure::commands;
pub use lora_pure::config;
pub use lora_pure::lorawan;
pub use lora_pure::{
    Bandwidth, CodingRate, LoraRadio, RxConfig, RxQuality, RxWindow, SpreadingFactor, TxConfig,
    RX_WINDOW_DURATION_MS, RX_WINDOW_OFFSET_MS,
};
pub use lora_pure::{
    Downlink, LorawanDevice, LorawanError, LorawanResponse, LorawanSessionData, LorawanState,
};
pub use lora_pure::{HeltecV3Pins, LoraConfig, Region};

pub mod sx1262_driver;
