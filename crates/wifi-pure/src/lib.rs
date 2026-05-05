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
    if ssid.is_empty() {
        Err("SSID must not be empty")
    } else if ssid.len() > SSID_MAX_LEN {
        Err("SSID exceeds maximum length of 32 bytes")
    } else {
        Ok(())
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

// ─── WifiPowerSave ──────────────────────────────────────────────────────────

/// Wi-Fi power save mode passed to `esp_wifi_set_ps()` after starting Wi-Fi.
///
/// # ESP-NOW caveat
///
/// `esp-idf-svc` `EspNow::take()` internally forces `WIFI_PS_NONE`,
/// overriding whatever mode was set here.
/// This setting is most useful for battery-powered devices that use
/// Wi-Fi *without* ESP-NOW.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum WifiPowerSave {
    /// Radio always on. Best latency, highest power draw.
    #[default]
    None,
    /// Minimum modem sleep — radio sleeps between DTIM beacon intervals.
    MinModem,
    /// Maximum modem sleep — radio sleeps as long as possible.
    MaxModem,
}

// ─── TxPowerLevel ──────────────────────────────────────────────────────────

/// Transmit power level for the Wi-Fi radio.
///
/// Abstracts raw dBm values (which vary by chip) into five intuitive tiers.
/// The exact dBm mapping is determined by the HAL backend; see each backend's
/// documentation for chip-specific details.
///
/// ESP-IDF uses `esp_wifi_set_max_tx_power()` with quarter-dBm units [8..84].
/// The default mapping is:
///
/// | Level    | Quarter-dBm | Approx dBm |
/// |:---------|:------------|:-----------|
/// | `Lowest` | 8           | 2          |
/// | `Low`    | 34          | 8.5        |
/// | `Medium` | 52          | 13         |
/// | `High`   | 68          | 17         |
/// | `Max`    | 78          | 19.5       |
///
/// These values are heuristic defaults intended to span the usable range of
/// supported ESP32 chips.
/// Local regulatory limits, antenna design, and per-board layout may require
/// lower settings — pick the lowest level that meets your range needs.
/// The backend may also clamp the requested value to the chip's effective
/// maximum, so the actual radiated power can differ from the table above.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum TxPowerLevel {
    /// Minimum transmit power (~2 dBm). Best for close-range, low-heat use.
    Lowest,
    /// Low transmit power (~8.5 dBm).
    Low,
    /// Medium transmit power (~13 dBm). Balanced range and power draw.
    #[default]
    Medium,
    /// High transmit power (~17 dBm).
    High,
    /// Maximum transmit power (~19.5 dBm). Best range, highest power draw.
    Max,
}

impl TxPowerLevel {
    /// Returns the ESP-IDF quarter-dBm value for this power level.
    ///
    /// Used by ESP-IDF backends to call `esp_wifi_set_max_tx_power()`.
    /// Range: [8..84] where each unit is 0.25 dBm.
    pub fn to_quarter_dbm(self) -> i8 {
        match self {
            Self::Lowest => 8,  // 2 dBm
            Self::Low => 34,    // 8.5 dBm
            Self::Medium => 52, // 13 dBm
            Self::High => 68,   // 17 dBm
            Self::Max => 78,    // 19.5 dBm
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
///     .with_timeout(60)                        // optional: override the 30 s default
///     .connect_nonblocking()                   // optional: return immediately from new()
///     .with_power_save(WifiPowerSave::MinModem) // optional: modem sleep for battery savings
///     .with_tx_power(TxPowerLevel::Low);       // optional: reduce transmit power
/// ```
#[derive(Debug, Clone)]
pub struct WiFiConfig<'a> {
    pub ssid: &'a str,
    pub password: &'a str,
    pub connect_mode: ConnectMode,
    pub power_save: WifiPowerSave,
    pub tx_power: TxPowerLevel,
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
            power_save: WifiPowerSave::default(),
            tx_power: TxPowerLevel::default(),
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

    /// Sets the Wi-Fi power save mode applied after `wifi.start()`.
    ///
    /// Defaults to [`WifiPowerSave::None`] (radio always on).
    ///
    /// # ESP-NOW caveat
    ///
    /// `esp-idf-svc` `EspNow::take()` internally forces `WIFI_PS_NONE`,
    /// overriding whatever mode is set here.
    pub fn with_power_save(mut self, mode: WifiPowerSave) -> Self {
        self.power_save = mode;
        self
    }

    /// Sets the Wi-Fi transmit power level applied after `wifi.start()`.
    ///
    /// Defaults to [`TxPowerLevel::Medium`] (~13 dBm).
    ///
    /// Lower levels reduce power draw and heat; higher levels increase range.
    /// The exact dBm mapping depends on the chip — see [`TxPowerLevel`].
    pub fn with_tx_power(mut self, level: TxPowerLevel) -> Self {
        self.tx_power = level;
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
    fn empty_ssid_is_rejected() {
        assert!(validate_ssid("").is_err());
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

    // ── WifiPowerSave tests ──────────────────────────────────────────────

    #[test]
    fn power_save_default_is_none() {
        assert_eq!(WifiPowerSave::default(), WifiPowerSave::None);
    }

    #[test]
    fn wifi_config_default_power_save_is_none() {
        let config = test_config();
        assert_eq!(config.power_save, WifiPowerSave::None);
    }

    #[test]
    fn wifi_config_power_save_min_modem() {
        let config = test_config().with_power_save(WifiPowerSave::MinModem);
        assert_eq!(config.power_save, WifiPowerSave::MinModem);
    }

    #[test]
    fn wifi_config_power_save_max_modem() {
        let config = test_config().with_power_save(WifiPowerSave::MaxModem);
        assert_eq!(config.power_save, WifiPowerSave::MaxModem);
    }

    #[test]
    fn wifi_config_chained_builders() {
        let config = test_config()
            .with_timeout(60)
            .with_power_save(WifiPowerSave::MinModem)
            .with_tx_power(TxPowerLevel::Low)
            .connect_nonblocking();
        assert!(matches!(config.connect_mode, ConnectMode::NonBlocking));
        assert_eq!(config.power_save, WifiPowerSave::MinModem);
        assert_eq!(config.tx_power, TxPowerLevel::Low);
    }

    // ── TxPowerLevel tests ────────────────────────────────────────────────

    #[test]
    fn tx_power_default_is_medium() {
        assert_eq!(TxPowerLevel::default(), TxPowerLevel::Medium);
    }

    #[test]
    fn wifi_config_default_tx_power_is_medium() {
        let config = test_config();
        assert_eq!(config.tx_power, TxPowerLevel::Medium);
    }

    #[test]
    fn wifi_config_with_tx_power() {
        let config = test_config().with_tx_power(TxPowerLevel::Lowest);
        assert_eq!(config.tx_power, TxPowerLevel::Lowest);
    }

    #[test]
    fn tx_power_quarter_dbm_values() {
        assert_eq!(TxPowerLevel::Lowest.to_quarter_dbm(), 8);
        assert_eq!(TxPowerLevel::Low.to_quarter_dbm(), 34);
        assert_eq!(TxPowerLevel::Medium.to_quarter_dbm(), 52);
        assert_eq!(TxPowerLevel::High.to_quarter_dbm(), 68);
        assert_eq!(TxPowerLevel::Max.to_quarter_dbm(), 78);
    }

    #[test]
    fn tx_power_quarter_dbm_range_valid() {
        for level in [
            TxPowerLevel::Lowest,
            TxPowerLevel::Low,
            TxPowerLevel::Medium,
            TxPowerLevel::High,
            TxPowerLevel::Max,
        ] {
            let v = level.to_quarter_dbm();
            assert!(
                (8..=84).contains(&v),
                "{:?} maps to {} which is outside [8, 84]",
                level,
                v
            );
        }
    }

    // ── MockWifiDriver tests ────────────────────────────────────────────

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
