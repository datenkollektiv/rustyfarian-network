//! [`MockWifiDriver`] — a test double for host-side unit tests.
//!
//! Enable with `features = ["mock"]` in `dev-dependencies`, or it is automatically
//! available inside `#[cfg(test)]` blocks within this crate.
//!
//! # Usage in a downstream crate
//!
//! ```toml
//! [dev-dependencies]
//! wifi-pure = { workspace = true, features = ["mock"] }
//! ```
//!
//! ```rust,ignore
//! use wifi_pure::mock::MockWifiDriver;
//! use wifi_pure::WifiDriver;
//!
//! let mut driver = MockWifiDriver::new();
//! driver.configure("my-ssid", "my-psk").unwrap();
//! driver.start().unwrap();
//! driver.connect().unwrap();
//! assert!(driver.is_connected().unwrap());
//! assert_eq!(driver.connect_count, 1);
//! ```

use crate::WifiDriver;

/// Error type for [`MockWifiDriver`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MockWifiError {
    /// Returned when `fail_connect` is set to `true`.
    ConnectFailed,
}

impl core::fmt::Display for MockWifiError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "mock wifi connect failed")
    }
}

/// Mock implementation of [`WifiDriver`] for host-side unit tests.
///
/// All state is public for direct assertion in tests.
/// Set `fail_connect` to `true` before calling `connect()` to simulate failure.
pub struct MockWifiDriver {
    pub configured: bool,
    pub started: bool,
    pub connected: bool,
    pub netif_up: bool,
    pub connect_count: u32,
    pub fail_connect: bool,
}

impl MockWifiDriver {
    /// Create a new mock driver in the disconnected state.
    pub fn new() -> Self {
        Self {
            configured: false,
            started: false,
            connected: false,
            netif_up: false,
            connect_count: 0,
            fail_connect: false,
        }
    }
}

impl Default for MockWifiDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl WifiDriver for MockWifiDriver {
    type Error = MockWifiError;

    fn configure(&mut self, _ssid: &str, _password: &str) -> Result<(), Self::Error> {
        self.configured = true;
        Ok(())
    }

    fn start(&mut self) -> Result<(), Self::Error> {
        self.started = true;
        Ok(())
    }

    fn connect(&mut self) -> Result<(), Self::Error> {
        if self.fail_connect {
            return Err(MockWifiError::ConnectFailed);
        }
        self.connected = true;
        self.netif_up = true;
        self.connect_count += 1;
        Ok(())
    }

    fn disconnect(&mut self) -> Result<(), Self::Error> {
        self.connected = false;
        self.netif_up = false;
        Ok(())
    }

    fn is_connected(&self) -> Result<bool, Self::Error> {
        Ok(self.connected)
    }
}
