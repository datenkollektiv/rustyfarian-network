//! Wi-Fi driver for ESP-HAL projects (bare-metal, no_std).
//!
//! This crate provides [`EspHalWifiManager`], a stub implementation of the
//! [`wifi_pure::WifiDriver`] trait for bare-metal ESP-HAL targets.
//!
//! # Status
//!
//! This driver is a stub.
//! All [`WifiDriver`] methods return [`WifiError::NotSupported`].
//! Real `esp-wifi` integration is planned for Phase 5.

#![no_std]

pub use wifi_pure::{
    ConnectMode, WiFiConfig, WifiDriver, DEFAULT_TIMEOUT_SECS, PASSWORD_MAX_LEN, POLL_INTERVAL_MS,
    SSID_MAX_LEN,
};

/// Error type for [`EspHalWifiManager`] operations.
#[derive(Debug)]
pub enum WifiError {
    /// Wi-Fi configuration rejected.
    ConfigureFailed,
    /// Wi-Fi hardware failed to start.
    StartFailed,
    /// Association with the AP failed.
    ConnectFailed,
    /// Operation not yet implemented on this platform.
    NotSupported,
}

impl core::fmt::Display for WifiError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::ConfigureFailed => write!(f, "Wi-Fi configuration failed"),
            Self::StartFailed => write!(f, "Wi-Fi start failed"),
            Self::ConnectFailed => write!(f, "Wi-Fi connect failed"),
            Self::NotSupported => write!(f, "Wi-Fi not supported on this platform"),
        }
    }
}

/// Bare-metal Wi-Fi manager for ESP-HAL targets.
///
/// # Implementation status
///
/// All methods return [`WifiError::NotSupported`].
/// Real `esp-wifi` + `smoltcp` integration is planned for Phase 5.
pub struct EspHalWifiManager;

impl EspHalWifiManager {
    /// Create a new stub Wi-Fi manager.
    pub fn new() -> Self {
        Self
    }
}

impl Default for EspHalWifiManager {
    fn default() -> Self {
        Self::new()
    }
}

impl WifiDriver for EspHalWifiManager {
    type Error = WifiError;

    fn configure(&mut self, _ssid: &str, _password: &str) -> Result<(), Self::Error> {
        Err(WifiError::NotSupported)
    }

    fn start(&mut self) -> Result<(), Self::Error> {
        Err(WifiError::NotSupported)
    }

    fn connect(&mut self) -> Result<(), Self::Error> {
        Err(WifiError::NotSupported)
    }

    fn disconnect(&mut self) -> Result<(), Self::Error> {
        Err(WifiError::NotSupported)
    }

    fn is_connected(&self) -> Result<bool, Self::Error> {
        Err(WifiError::NotSupported)
    }
}
