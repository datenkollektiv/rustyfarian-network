#![no_std]
//! LoRa radio driver for ESP-HAL projects (bare-metal, no_std).
//!
//! This crate provides [`EspHalLoraRadio`], an implementation of the
//! [`lora_pure::LoraRadio`] trait using `esp-hal` SPI and GPIO peripherals.
//!
//! # Status
//!
//! This driver is a stub.
//! Each method returns the most appropriate error variant for the operation:
//! TX methods return [`driver::LoraError::TransmitFailed`],
//! RX methods return [`driver::LoraError::ReceiveFailed`], and
//! configuration methods return [`driver::LoraError::RadioInitFailed`].
//! Hardware integration is planned as part of the Phase 5 TTN validation.
//!
//! # LED status
//!
//! [`EspHalLoraRadio`] accepts a generic `S: StatusLed` at construction time,
//! enabling WS2812 RGB LED feedback (join, uplink, downlink states) or
//! [`led_effects::NoLed`] for headless configurations.

pub use lora_pure::{
    Bandwidth, CodingRate, LoraRadio, RxConfig, RxQuality, RxWindow, SpreadingFactor, TxConfig,
    RX_WINDOW_DURATION_MS, RX_WINDOW_OFFSET_MS,
};

mod driver;
pub use driver::{EspHalLoraRadio, LoraError};
