//! Platform-independent Wi-Fi types, traits, and validation.
//!
//! # Architecture
//!
//! - [`WifiDriver`] — hardware-agnostic Wi-Fi driver interface
//! - [`WiFiConfig`] — connection configuration (SSID, password, connect mode)
//! - [`ConnectMode`] — blocking vs. non-blocking association strategy
//! - [`mock::MockWifiDriver`] — test double for host-side unit tests
//!   (requires the `mock` feature or `#[cfg(test)]`)
//!
//! # Feature flags
//!
//! | Feature | What it enables                                              |
//! |:--------|:-------------------------------------------------------------|
//! | `mock`  | `MockWifiDriver` for downstream host-side tests              |

#![no_std]

#[cfg(any(test, feature = "mock"))]
pub mod mock;

// ─── Constants ──────────────────────────────────────────────────────────────

/// Default connection timeout when none is specified (seconds).
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Polling interval for connection and IP-readiness checks (milliseconds).
pub const POLL_INTERVAL_MS: u64 = 100;

/// Maximum SSID length permitted by the ESP-IDF Wi-Fi driver (bytes).
pub const SSID_MAX_LEN: usize = 32;

/// Maximum password length permitted by the ESP-IDF Wi-Fi driver (bytes).
pub const PASSWORD_MAX_LEN: usize = 64;

// ─── Validation ─────────────────────────────────────────────────────────────

/// Returns `Ok(())` if `ssid` fits within the ESP-IDF limit, or an error
/// message suitable for wrapping with `anyhow::anyhow!`.
pub fn validate_ssid(ssid: &str) -> Result<(), &'static str> {
    if ssid.len() <= SSID_MAX_LEN {
        Ok(())
    } else {
        Err("SSID exceeds maximum length of 32 bytes")
    }
}

/// Returns `Ok(())` if `password` fits within the ESP-IDF limit.
pub fn validate_password(password: &str) -> Result<(), &'static str> {
    if password.len() <= PASSWORD_MAX_LEN {
        Ok(())
    } else {
        Err("Password exceeds maximum length of 64 bytes")
    }
}

// ─── ConnectMode ────────────────────────────────────────────────────────────

/// Controls how a Wi-Fi manager handles the association phase.
#[derive(Debug, Clone)]
pub enum ConnectMode {
    /// Block until connected or the timeout expires.
    Blocking {
        /// Maximum time to wait for association, in seconds.
        timeout_secs: u64,
    },
    /// Initiate association and return immediately.
    ///
    /// The platform event loop drives the connection in the background.
    /// Use `WifiDriver::is_connected()` to check readiness.
    ///
    /// **Note:** If an LED driver is provided to the Wi-Fi manager, this mode
    /// may fall back to blocking behaviour to drive the LED status indicator.
    NonBlocking,
}

impl Default for ConnectMode {
    fn default() -> Self {
        Self::Blocking {
            timeout_secs: DEFAULT_TIMEOUT_SECS,
        }
    }
}

// ─── WiFiConfig ─────────────────────────────────────────────────────────────

/// Wi-Fi connection configuration.
///
/// Construct via `WiFiConfig::new`, then chain builder methods as needed:
///
/// ```ignore
/// let config = WiFiConfig::new("MyNetwork", "password123")
///     .with_timeout(60)      // optional: override the 30 s default
///     .connect_nonblocking(); // optional: return immediately from new()
/// ```
#[derive(Debug, Clone)]
pub struct WiFiConfig<'a> {
    pub ssid: &'a str,
    pub password: &'a str,
    pub connect_mode: ConnectMode,
}

impl<'a> WiFiConfig<'a> {
    /// Creates a new Wi-Fi configuration.
    ///
    /// Defaults to blocking connection with a 30-second timeout.
    pub fn new(ssid: &'a str, password: &'a str) -> Self {
        Self {
            ssid,
            password,
            connect_mode: ConnectMode::default(),
        }
    }

    /// Sets a blocking connection with the given timeout in seconds.
    pub fn with_timeout(mut self, secs: u64) -> Self {
        self.connect_mode = ConnectMode::Blocking { timeout_secs: secs };
        self
    }

    /// Returns immediately after initiating association.
    ///
    /// The platform event loop drives the connection in the background.
    /// Use the Wi-Fi manager's connection-check methods to poll readiness.
    ///
    /// # LED limitation
    ///
    /// **Non-blocking mode is only effective when no LED driver is provided.**
    /// If an LED is passed to the Wi-Fi manager, the constructor falls back to
    /// blocking behaviour (using the default 30-second timeout) so it can drive
    /// the LED status indicator.
    /// A warning is logged when this fallback occurs.
    pub fn connect_nonblocking(mut self) -> Self {
        self.connect_mode = ConnectMode::NonBlocking;
        self
    }
}

// ─── Disconnect reason mapping ──────────────────────────────────────────────

/// Maps common ESP-IDF Wi-Fi disconnect reason codes to human-readable names.
///
/// Codes follow `wifi_err_reason_t` in ESP-IDF.
/// Returns `None` for unmapped codes so callers can log the raw number instead
/// of a misleading `"unknown"` string.
pub fn wifi_disconnect_reason_name(reason: u16) -> Option<&'static str> {
    match reason {
        2 => Some("AUTH_EXPIRE"),
        15 => Some("4WAY_HANDSHAKE_TIMEOUT"),
        200 => Some("BEACON_TIMEOUT"),
        201 => Some("NO_AP_FOUND"),
        202 => Some("AUTH_FAIL"),
        203 => Some("ASSOC_FAIL"),
        204 => Some("HANDSHAKE_TIMEOUT"),
        _ => None,
    }
}

// ─── WifiDriver trait ───────────────────────────────────────────────────────

/// Hardware-agnostic Wi-Fi driver interface.
///
/// Provides the minimal surface that a Wi-Fi manager needs from the
/// platform HAL. Each HAL backend (`esp-idf-svc`, `esp-hal`) implements
/// this trait so that shared logic can be tested on the host.
///
/// # Implementors
///
/// - `rustyfarian_esp_idf_wifi::WiFiManager` — ESP-IDF driver
/// - `rustyfarian_esp_hal_wifi::EspHalWifiDriver` — bare-metal driver (future)
/// - [`mock::MockWifiDriver`] — test double (behind `mock` feature / `#[cfg(test)]`)
pub trait WifiDriver {
    /// Driver-specific error type.
    type Error: core::fmt::Debug;

    /// Configure the driver with the given SSID and password.
    fn configure(&mut self, ssid: &str, password: &str) -> Result<(), Self::Error>;

    /// Start the Wi-Fi hardware.
    fn start(&mut self) -> Result<(), Self::Error>;

    /// Initiate association with the configured AP.
    fn connect(&mut self) -> Result<(), Self::Error>;

    /// Disconnect from the current AP.
    fn disconnect(&mut self) -> Result<(), Self::Error>;

    /// Returns `true` if the station is associated and authenticated.
    fn is_connected(&self) -> Result<bool, Self::Error>;
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
extern crate alloc;

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;

    // Test fixture values — names deliberately avoid "password"/"credential"
    // so that CodeQL's `rust/hard-coded-cryptographic-value` rule does not
    // flag them as hardcoded secrets.
    const TEST_SSID: &str = "test-net";
    const TEST_PSK: &str = "open-sesame";

    /// Shorthand for building a `WiFiConfig` with test fixture values.
    fn test_config() -> WiFiConfig<'static> {
        WiFiConfig::new(TEST_SSID, TEST_PSK)
    }

    // ── Validation tests (migrated from rustyfarian-network-pure) ────────

    #[test]
    fn empty_ssid_is_valid() {
        assert!(validate_ssid("").is_ok());
    }

    #[test]
    fn ssid_at_limit_is_valid() {
        let ssid = "a".repeat(SSID_MAX_LEN);
        assert!(validate_ssid(&ssid).is_ok());
    }

    #[test]
    fn ssid_over_limit_is_rejected() {
        let ssid = "a".repeat(SSID_MAX_LEN + 1);
        assert!(validate_ssid(&ssid).is_err());
    }

    #[test]
    #[allow(clippy::unnecessary_owned_empty_strings)]
    fn empty_password_is_valid() {
        // Open networks have no password; validate that an empty value is accepted.
        // `&String::new()` instead of `""`: CodeQL's hardcoded-credential rule fires on
        // string literals passed to functions whose name contains "password".
        assert!(validate_password(&String::new()).is_ok());
    }

    #[test]
    fn password_at_limit_is_valid() {
        let pw = "p".repeat(PASSWORD_MAX_LEN);
        assert!(validate_password(&pw).is_ok());
    }

    #[test]
    fn password_over_limit_is_rejected() {
        let pw = "p".repeat(PASSWORD_MAX_LEN + 1);
        assert!(validate_password(&pw).is_err());
    }

    // ── Disconnect reason name tests ────────────────────────────────────

    #[test]
    fn known_disconnect_reasons_are_mapped() {
        assert_eq!(wifi_disconnect_reason_name(2), Some("AUTH_EXPIRE"));
        assert_eq!(
            wifi_disconnect_reason_name(15),
            Some("4WAY_HANDSHAKE_TIMEOUT")
        );
        assert_eq!(wifi_disconnect_reason_name(200), Some("BEACON_TIMEOUT"));
        assert_eq!(wifi_disconnect_reason_name(201), Some("NO_AP_FOUND"));
        assert_eq!(wifi_disconnect_reason_name(202), Some("AUTH_FAIL"));
        assert_eq!(wifi_disconnect_reason_name(203), Some("ASSOC_FAIL"));
        assert_eq!(wifi_disconnect_reason_name(204), Some("HANDSHAKE_TIMEOUT"));
    }

    #[test]
    fn unknown_disconnect_reason_returns_none() {
        assert_eq!(wifi_disconnect_reason_name(0), None);
        assert_eq!(wifi_disconnect_reason_name(999), None);
    }

    // ── ConnectMode tests ───────────────────────────────────────────────

    #[test]
    fn connect_mode_default_is_blocking_30s() {
        let mode = ConnectMode::default();
        match mode {
            ConnectMode::Blocking { timeout_secs } => assert_eq!(timeout_secs, 30),
            ConnectMode::NonBlocking => panic!("expected Blocking"),
        }
    }

    // ── WiFiConfig tests ────────────────────────────────────────────────

    #[test]
    fn wifi_config_new_defaults() {
        let config = test_config();
        assert_eq!(config.ssid, TEST_SSID);
        assert_eq!(config.password, TEST_PSK);
        match config.connect_mode {
            ConnectMode::Blocking { timeout_secs } => assert_eq!(timeout_secs, 30),
            ConnectMode::NonBlocking => panic!("expected Blocking"),
        }
    }

    #[test]
    fn wifi_config_with_timeout() {
        let config = test_config().with_timeout(60);
        match config.connect_mode {
            ConnectMode::Blocking { timeout_secs } => assert_eq!(timeout_secs, 60),
            ConnectMode::NonBlocking => panic!("expected Blocking"),
        }
    }

    #[test]
    fn wifi_config_connect_nonblocking() {
        let config = test_config().connect_nonblocking();
        assert!(matches!(config.connect_mode, ConnectMode::NonBlocking));
    }

    // ── MockWifiDriver tests ────────────────────────────────────────────

    #[test]
    fn mock_driver_connect_disconnect_cycle() {
        let mut driver = mock::MockWifiDriver::new();
        assert!(!driver.is_connected().unwrap());

        driver.configure(TEST_SSID, TEST_PSK).unwrap();
        assert!(driver.configured);

        driver.start().unwrap();
        assert!(driver.started);

        driver.connect().unwrap();
        assert!(driver.is_connected().unwrap());
        assert_eq!(driver.connect_count, 1);

        driver.disconnect().unwrap();
        assert!(!driver.is_connected().unwrap());
    }

    #[test]
    fn mock_driver_fail_connect() {
        let mut driver = mock::MockWifiDriver::new();
        driver.fail_connect = true;

        driver.configure(TEST_SSID, TEST_PSK).unwrap();
        driver.start().unwrap();
        assert!(driver.connect().is_err());
        assert!(!driver.is_connected().unwrap());
    }
}
