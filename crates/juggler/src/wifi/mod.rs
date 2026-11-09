//! Platform-independent Wi-Fi types, traits, and validation.
//!
//! # Architecture
//!
//! - [`WifiDriver`] — hardware-agnostic Wi-Fi driver interface
//! - [`WiFiConfig`] — connection configuration (SSID, password, connect mode)
//! - [`ConnectMode`] — blocking vs. non-blocking association strategy
//! - [`LinkMachine`] — pure reconnect policy shared by both tiers (see [`link`])
//! - [`mock::MockWifiDriver`] — test double for host-side unit tests
//!   (requires the `mock` feature or `#[cfg(test)]`)
//!
//! # Feature flags
//!
//! | Feature | What it enables                                              |
//! |:--------|:-------------------------------------------------------------|
//! | `mock`  | `MockWifiDriver` and `MockLink` for downstream host tests    |

#[cfg(any(test, feature = "mock"))]
mod link_mock;
#[cfg(any(test, feature = "mock"))]
pub mod mock;

pub mod handover;
pub mod lines;
pub mod link;
pub mod network_link;
#[cfg(feature = "std")]
pub mod supervisor;

pub use handover::{IdleWait, LinkHandover};
pub use link::{
    LinkAction, LinkInput, LinkMachine, LinkState, RECONNECT_BACKOFF_BASE_MS,
    RECONNECT_BACKOFF_MAX_MS,
};
pub use network_link::NetworkLink;
#[cfg(feature = "std")]
pub use supervisor::{LinkEvent, LinkEvents, LinkRadio, LinkSupervisor, ReadyWait};

// ─── Constants ──────────────────────────────────────────────────────────────

/// Default connection timeout when none is specified (seconds).
pub const DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Polling interval for connection and IP-readiness checks (milliseconds).
pub const POLL_INTERVAL_MS: u64 = 100;

/// Maximum SSID length permitted by the ESP-IDF Wi-Fi driver (bytes).
pub const SSID_MAX_LEN: usize = 32;

/// Maximum password length permitted by the ESP-IDF Wi-Fi driver (bytes).
pub const PASSWORD_MAX_LEN: usize = 64;

/// Minimum password length for a WPA2-protected SoftAP (bytes).
///
/// WPA2-Personal requires a pre-shared key of at least 8 characters.
pub const AP_PASSWORD_MIN_LEN: usize = 8;

/// Lowest 2.4 GHz channel a SoftAP may be configured on.
pub const AP_CHANNEL_MIN: u8 = 1;

/// Highest 2.4 GHz channel a SoftAP may be configured on.
///
/// Channels above 13 are not permitted in most regulatory domains, so the
/// validator caps the range at 13 to stay portable across regions.
pub const AP_CHANNEL_MAX: u8 = 13;

/// Default maximum number of stations that may associate with a SoftAP.
pub const AP_MAX_CONNECTIONS_DEFAULT: u8 = 4;

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

/// Returns `Ok(())` if `config` is a valid SoftAP configuration.
///
/// Validates the SSID via [`validate_ssid`], the channel against
/// [`AP_CHANNEL_MIN`]`..=`[`AP_CHANNEL_MAX`], and `max_connections >= 1`.
/// When a password is present, it must be at least [`AP_PASSWORD_MIN_LEN`]
/// bytes (the WPA2-Personal minimum) and at most [`PASSWORD_MAX_LEN`] bytes —
/// the upper bound is delegated to [`validate_password`].
pub fn validate_ap_config(config: &ApConfig<'_>) -> Result<(), &'static str> {
    validate_ssid(config.ssid)?;
    if let Some(password) = config.password {
        if password.len() < AP_PASSWORD_MIN_LEN {
            return Err("AP password must be at least 8 bytes for WPA2");
        }
        validate_password(password)?;
    }
    if config.channel < AP_CHANNEL_MIN || config.channel > AP_CHANNEL_MAX {
        return Err("AP channel must be in the range 1..=13");
    }
    if config.max_connections < 1 {
        return Err("AP max_connections must be at least 1");
    }
    Ok(())
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

impl ConnectMode {
    /// How long the manager's constructor waits for the link, or `None` to
    /// return at once.
    ///
    /// A status indicator needs the blocking wait to drive it, so
    /// [`NonBlocking`](Self::NonBlocking) with `has_indicator` falls back to
    /// [`DEFAULT_TIMEOUT_SECS`].
    pub fn effective_timeout_secs(self, has_indicator: bool) -> Option<u64> {
        match self {
            Self::Blocking { timeout_secs } => Some(timeout_secs),
            Self::NonBlocking if has_indicator => Some(DEFAULT_TIMEOUT_SECS),
            Self::NonBlocking => None,
        }
    }
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

// ─── ApSelection ────────────────────────────────────────────────────────────

/// How the station picks an access point when several broadcast the SSID.
///
/// Both tiers map this value explicitly, so neither falls back to its driver
/// library's default: esp-radio defaults to a fast scan, `embedded-svc`'s
/// `ClientConfiguration` (re-exported by `esp-idf-svc`) to a complete scan, and on a mesh network the fast scan joined a -89 dBm node
/// next to a -20 dBm one (bench 2026-11-07).
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub enum ApSelection {
    /// Scan all channels and join the strongest AP of the SSID.
    #[default]
    Strongest,
    /// Join the first AP of the SSID found, sweeping channels upwards.
    ///
    /// Connects faster on a single-AP network; on a mesh it may join a far node.
    FirstFound,
}

// ─── JoinedAp ───────────────────────────────────────────────────────────────

/// The AP a station associated with, as both tiers log it on every link up.
///
/// `Display` gives the shared text, e.g. `bssid 72:d7:9a:ca:b9:44, ch 11, rssi -21 dBm`,
/// so a weak or far AP shows in the log rather than only in the router.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JoinedAp {
    /// BSSID (the AP radio's MAC address).
    pub bssid: [u8; 6],
    /// Primary channel.
    pub channel: u8,
    /// Signal strength in dBm at association.
    pub rssi: i8,
}

impl core::fmt::Display for JoinedAp {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let [a, b, c, d, e, g] = self.bssid;
        write!(
            f,
            "bssid {a:02x}:{b:02x}:{c:02x}:{d:02x}:{e:02x}:{g:02x}, ch {}, rssi {} dBm",
            self.channel, self.rssi
        )
    }
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
///     .with_tx_power(TxPowerLevel::Low)        // optional: reduce transmit power
///     .with_ap_selection(ApSelection::FirstFound); // optional: faster join, first AP found
/// ```
#[derive(Debug, Clone)]
pub struct WiFiConfig<'a> {
    pub ssid: &'a str,
    pub password: &'a str,
    /// How the ESP-IDF `WiFiManager` constructor waits for the link (see
    /// [`ConnectMode::effective_timeout_secs`]). The esp-hal tier ignores it: a
    /// firmware there waits through `WifiLink::wait_connected`.
    pub connect_mode: ConnectMode,
    pub power_save: WifiPowerSave,
    pub tx_power: TxPowerLevel,
    /// How the station picks an AP when several broadcast the SSID.
    pub ap_selection: ApSelection,
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
            ap_selection: ApSelection::default(),
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

    /// Sets how the station picks an AP when several broadcast the SSID.
    ///
    /// Defaults to [`ApSelection::Strongest`] on both tiers.
    pub fn with_ap_selection(mut self, selection: ApSelection) -> Self {
        self.ap_selection = selection;
        self
    }
}

// ─── ApConfig ───────────────────────────────────────────────────────────────

/// SoftAP (access point) configuration.
///
/// Used by the ESP-IDF SoftAP lifecycle to bring up a captive-portal access
/// point. Construct via [`ApConfig::open`] or [`ApConfig::wpa2`], then chain
/// builder methods as needed:
///
/// ```ignore
/// let config = ApConfig::wpa2("Rustyfarian-AB12", "provision-me")
///     .with_channel(6)                   // optional: override the channel-1 default
///     .with_max_connections(2)           // optional: cap associated stations
///     .with_tx_power(TxPowerLevel::Low); // optional: reduce transmit power
/// ```
pub struct ApConfig<'a> {
    /// Access-point SSID broadcast in beacons.
    pub ssid: &'a str,
    /// WPA2 pre-shared key, or `None` for an open network.
    pub password: Option<&'a str>,
    /// 2.4 GHz channel (1..=13).
    pub channel: u8,
    /// Maximum number of stations that may associate.
    pub max_connections: u8,
    /// Transmit power level applied after the AP starts.
    pub tx_power: TxPowerLevel,
}

impl<'a> Clone for ApConfig<'a> {
    fn clone(&self) -> Self {
        Self {
            ssid: self.ssid,
            password: self.password,
            channel: self.channel,
            max_connections: self.max_connections,
            tx_power: self.tx_power,
        }
    }
}

impl core::fmt::Debug for ApConfig<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("ApConfig")
            .field("ssid", &self.ssid)
            .field("password", &self.password.map(|_| "<redacted>"))
            .field("channel", &self.channel)
            .field("max_connections", &self.max_connections)
            .field("tx_power", &self.tx_power)
            .finish()
    }
}

impl<'a> ApConfig<'a> {
    /// Creates an **open** (passwordless) SoftAP configuration.
    ///
    /// Defaults to channel 1, [`AP_MAX_CONNECTIONS_DEFAULT`] connections, and
    /// [`TxPowerLevel::Medium`].
    ///
    /// # Warning
    ///
    /// An open AP exposes the captive portal's `/save` and `/factory-reset`
    /// endpoints to anyone within radio range — no authentication gates the
    /// provisioning or destructive-reset routes. Prefer [`ApConfig::wpa2`] for
    /// any deployment where bystanders could be on the network.
    pub fn open(ssid: &'a str) -> Self {
        Self {
            ssid,
            password: None,
            channel: AP_CHANNEL_MIN,
            max_connections: AP_MAX_CONNECTIONS_DEFAULT,
            tx_power: TxPowerLevel::default(),
        }
    }

    /// Creates a WPA2-protected SoftAP configuration.
    ///
    /// Defaults to channel 1, [`AP_MAX_CONNECTIONS_DEFAULT`] connections, and
    /// [`TxPowerLevel::Medium`]. The password must satisfy
    /// [`AP_PASSWORD_MIN_LEN`] when later passed to [`validate_ap_config`].
    pub fn wpa2(ssid: &'a str, password: &'a str) -> Self {
        Self {
            ssid,
            password: Some(password),
            channel: AP_CHANNEL_MIN,
            max_connections: AP_MAX_CONNECTIONS_DEFAULT,
            tx_power: TxPowerLevel::default(),
        }
    }

    /// Sets the 2.4 GHz channel (validated to 1..=13 by [`validate_ap_config`]).
    pub fn with_channel(mut self, channel: u8) -> Self {
        self.channel = channel;
        self
    }

    /// Sets the maximum number of associated stations.
    pub fn with_max_connections(mut self, max: u8) -> Self {
        self.max_connections = max;
        self
    }

    /// Sets the transmit power level applied after the AP starts.
    pub fn with_tx_power(mut self, level: TxPowerLevel) -> Self {
        self.tx_power = level;
        self
    }
}

// ─── Disconnect reason mapping ──────────────────────────────────────────────

/// Maps common ESP-IDF Wi-Fi disconnect reason codes to human-readable names.
///
/// Codes and spellings follow `wifi_err_reason_t` (`esp_wifi_types_generic.h`)
/// so a log line can be matched against the ESP-IDF documentation.
/// The 802.11 management codes 1..=24 and the ESP-IDF band 200..=212 are
/// covered; rarer 802.11 codes (TDLS, QoS, fast transition, 25..=68) stay
/// unmapped and log as `(unmapped)` instead of a guess.
pub fn wifi_disconnect_reason_name(reason: u16) -> Option<&'static str> {
    match reason {
        1 => Some("UNSPECIFIED"),
        2 => Some("AUTH_EXPIRE"),
        3 => Some("AUTH_LEAVE"),
        4 => Some("ASSOC_EXPIRE"),
        5 => Some("ASSOC_TOOMANY"),
        6 => Some("NOT_AUTHED"),
        7 => Some("NOT_ASSOCED"),
        8 => Some("ASSOC_LEAVE"),
        9 => Some("ASSOC_NOT_AUTHED"),
        10 => Some("DISASSOC_PWRCAP_BAD"),
        11 => Some("DISASSOC_SUPCHAN_BAD"),
        12 => Some("BSS_TRANSITION_DISASSOC"),
        13 => Some("IE_INVALID"),
        14 => Some("MIC_FAILURE"),
        15 => Some("4WAY_HANDSHAKE_TIMEOUT"),
        16 => Some("GROUP_KEY_UPDATE_TIMEOUT"),
        17 => Some("IE_IN_4WAY_DIFFERS"),
        18 => Some("GROUP_CIPHER_INVALID"),
        19 => Some("PAIRWISE_CIPHER_INVALID"),
        20 => Some("AKMP_INVALID"),
        21 => Some("UNSUPP_RSN_IE_VERSION"),
        22 => Some("INVALID_RSN_IE_CAP"),
        23 => Some("802_1X_AUTH_FAILED"),
        24 => Some("CIPHER_SUITE_REJECTED"),
        200 => Some("BEACON_TIMEOUT"),
        201 => Some("NO_AP_FOUND"),
        202 => Some("AUTH_FAIL"),
        203 => Some("ASSOC_FAIL"),
        204 => Some("HANDSHAKE_TIMEOUT"),
        205 => Some("CONNECTION_FAIL"),
        206 => Some("AP_TSF_RESET"),
        207 => Some("ROAMING"),
        208 => Some("ASSOC_COMEBACK_TIME_TOO_LONG"),
        209 => Some("SA_QUERY_TIMEOUT"),
        210 => Some("NO_AP_FOUND_W_COMPATIBLE_SECURITY"),
        211 => Some("NO_AP_FOUND_IN_AUTHMODE_THRESHOLD"),
        212 => Some("NO_AP_FOUND_IN_RSSI_THRESHOLD"),
        _ => None,
    }
}

/// `true` for the disconnect reasons that point at the credentials or at a
/// network that is not there, so a log line may suggest checking them.
///
/// The four are those a wrong SSID, a wrong password or an access point that is
/// out of range produce: `4WAY_HANDSHAKE_TIMEOUT` (15), `NO_AP_FOUND` (201),
/// `AUTH_FAIL` (202) and `HANDSHAKE_TIMEOUT` (204).
/// A router-initiated deauthentication (`AUTH_LEAVE`, `ASSOC_LEAVE`), a beacon
/// loss or the library's own suspend are deliberately not in the set: they say
/// nothing about the credentials, and the old unconditional hint sent users
/// hunting for a typo after every suspend (bug 011).
pub fn reason_suggests_credentials(reason: u16) -> bool {
    matches!(reason, 15 | 201 | 202 | 204)
}

/// The sentence appended to a disconnect log line when the reason points at the
/// credentials; empty otherwise.
pub const CREDENTIALS_HINT: &str = " — check SSID/password and network availability";

/// The hint to append for a reason, `""` when it says nothing about credentials.
pub fn credentials_hint(reason: u16) -> &'static str {
    if reason_suggests_credentials(reason) {
        CREDENTIALS_HINT
    } else {
        ""
    }
}

/// One-line form of an ESP-IDF disconnect reason:
/// `reason 8 (ASSOC_LEAVE)`, plus [`CREDENTIALS_HINT`] only where it applies.
///
/// The ESP-IDF tier logs this directly; the esp-hal tier shares the hint but not
/// the code formatting, because esp-radio reports a named reason instead of the
/// numeric code and `WifiLink` logs that name followed by [`credentials_hint`]'s
/// text (bug 011).
///
/// ```ignore
/// log::warn!("[wifi] disconnected — {}", DisconnectReason(info.reason()));
/// ```
pub struct DisconnectReason(pub u16);

impl core::fmt::Display for DisconnectReason {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match wifi_disconnect_reason_name(self.0) {
            Some(name) => write!(f, "reason {} ({})", self.0, name)?,
            None => write!(f, "reason {} (unmapped)", self.0)?,
        }
        f.write_str(credentials_hint(self.0))
    }
}

// ─── WifiDriver trait ───────────────────────────────────────────────────────

/// Hardware-agnostic Wi-Fi driver interface.
///
/// Deprecated: no tier implements it; ADR 022 replaces it with shared link
/// traits that both tiers implement.
///
/// # Implementors
///
/// - [`mock::MockWifiDriver`] — test double (behind `mock` feature / `#[cfg(test)]`)
#[deprecated(
    since = "0.7.0",
    note = "no tier implements WifiDriver; ADR 022 replaces it with shared link traits"
)]
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
mod tests {
    extern crate std;

    use super::*;
    use alloc::string::String;

    const TEST_SSID: &str = "test-net";

    use crate::test_support::test_psk;

    /// Shorthand for building a `WiFiConfig` with test fixture values.
    fn test_config() -> WiFiConfig<'static> {
        WiFiConfig::new(TEST_SSID, test_psk())
    }

    // ── Validation tests (migrated from the former rustyfarian-network-pure) ────────

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
        assert_eq!(wifi_disconnect_reason_name(25), None, "TDLS stays unmapped");
    }

    #[test]
    fn the_common_802_11_codes_are_named() {
        for (code, name) in [
            (1, "UNSPECIFIED"),
            (3, "AUTH_LEAVE"),
            (4, "ASSOC_EXPIRE"),
            (8, "ASSOC_LEAVE"),
            (13, "IE_INVALID"),
            (23, "802_1X_AUTH_FAILED"),
            (24, "CIPHER_SUITE_REJECTED"),
            (205, "CONNECTION_FAIL"),
            (210, "NO_AP_FOUND_W_COMPATIBLE_SECURITY"),
            (212, "NO_AP_FOUND_IN_RSSI_THRESHOLD"),
        ] {
            assert_eq!(wifi_disconnect_reason_name(code), Some(name), "code {code}");
        }
    }

    #[test]
    fn only_reasons_that_point_at_the_credentials_carry_the_hint() {
        for reason in [15, 201, 202, 204] {
            assert!(reason_suggests_credentials(reason), "reason {reason}");
        }
        for reason in [1, 2, 3, 8, 200, 203, 205, 207] {
            assert!(!reason_suggests_credentials(reason), "reason {reason}");
            assert_eq!(credentials_hint(reason), "", "reason {reason}");
        }
        assert_eq!(credentials_hint(202), CREDENTIALS_HINT);
    }

    #[test]
    fn a_disconnect_line_names_the_reason_and_hints_only_when_useful() {
        assert_eq!(
            alloc::format!("{}", DisconnectReason(8)),
            "reason 8 (ASSOC_LEAVE)"
        );
        assert_eq!(
            alloc::format!("{}", DisconnectReason(3)),
            "reason 3 (AUTH_LEAVE)",
            "a router deauth must not blame the credentials"
        );
        assert_eq!(
            alloc::format!("{}", DisconnectReason(202)),
            "reason 202 (AUTH_FAIL) — check SSID/password and network availability"
        );
        assert_eq!(
            alloc::format!("{}", DisconnectReason(77)),
            "reason 77 (unmapped)"
        );
    }

    // ── ConnectMode tests ───────────────────────────────────────────────

    #[test]
    fn effective_timeout_covers_every_mode_and_indicator() {
        let blocking = ConnectMode::Blocking { timeout_secs: 7 };
        assert_eq!(blocking.clone().effective_timeout_secs(false), Some(7));
        assert_eq!(blocking.effective_timeout_secs(true), Some(7));
        assert_eq!(ConnectMode::NonBlocking.effective_timeout_secs(false), None);
        assert_eq!(
            ConnectMode::NonBlocking.effective_timeout_secs(true),
            Some(DEFAULT_TIMEOUT_SECS),
            "an indicator falls back to the blocking default"
        );
    }

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
        assert_eq!(config.password, test_psk());
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
    #[allow(deprecated)] // exercises the deprecated `WifiDriver` through its mock
    fn mock_driver_connect_disconnect_cycle() {
        let mut driver = mock::MockWifiDriver::new();
        assert!(!driver.is_connected().unwrap());

        driver.configure(TEST_SSID, test_psk()).unwrap();
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
    fn joined_ap_display_is_the_shared_log_text() {
        let ap = JoinedAp {
            bssid: [0x72, 0xd7, 0x9a, 0x0c, 0xb9, 0x04],
            channel: 11,
            rssi: -21,
        };
        assert_eq!(
            alloc::format!("{ap}"),
            "bssid 72:d7:9a:0c:b9:04, ch 11, rssi -21 dBm"
        );
    }

    #[test]
    fn wifi_config_default_ap_selection_is_strongest() {
        assert_eq!(test_config().ap_selection, ApSelection::Strongest);
    }

    #[test]
    fn wifi_config_with_ap_selection() {
        let config = test_config().with_ap_selection(ApSelection::FirstFound);
        assert_eq!(config.ap_selection, ApSelection::FirstFound);
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
    #[allow(deprecated)] // exercises the deprecated `WifiDriver` through its mock
    fn mock_driver_fail_connect() {
        let mut driver = mock::MockWifiDriver::new();
        driver.fail_connect = true;

        driver.configure(TEST_SSID, test_psk()).unwrap();
        driver.start().unwrap();
        assert!(driver.connect().is_err());
        assert!(!driver.is_connected().unwrap());
    }

    // ── ApConfig tests ────────────────────────────────────────────────────

    #[test]
    fn ap_config_open_defaults() {
        let config = ApConfig::open(TEST_SSID);
        assert_eq!(config.ssid, TEST_SSID);
        assert!(config.password.is_none());
        assert_eq!(config.channel, AP_CHANNEL_MIN);
        assert_eq!(config.max_connections, AP_MAX_CONNECTIONS_DEFAULT);
        assert_eq!(config.tx_power, TxPowerLevel::Medium);
    }

    #[test]
    fn ap_config_wpa2_defaults() {
        let config = ApConfig::wpa2(TEST_SSID, test_psk());
        assert_eq!(config.ssid, TEST_SSID);
        assert_eq!(config.password, Some(test_psk()));
        assert_eq!(config.channel, AP_CHANNEL_MIN);
        assert_eq!(config.max_connections, AP_MAX_CONNECTIONS_DEFAULT);
        assert_eq!(config.tx_power, TxPowerLevel::Medium);
    }

    #[test]
    fn ap_config_chained_builders() {
        let config = ApConfig::wpa2(TEST_SSID, test_psk())
            .with_channel(6)
            .with_max_connections(2)
            .with_tx_power(TxPowerLevel::Low);
        assert_eq!(config.channel, 6);
        assert_eq!(config.max_connections, 2);
        assert_eq!(config.tx_power, TxPowerLevel::Low);
    }

    #[test]
    fn ap_config_valid_passes() {
        assert!(validate_ap_config(&ApConfig::open(TEST_SSID)).is_ok());
        assert!(validate_ap_config(&ApConfig::wpa2(TEST_SSID, test_psk())).is_ok());
    }

    #[test]
    fn ap_config_ssid_boundary() {
        let ssid_ok = "a".repeat(SSID_MAX_LEN);
        assert!(validate_ap_config(&ApConfig::open(&ssid_ok)).is_ok());
        let ssid_too_long = "a".repeat(SSID_MAX_LEN + 1);
        assert!(validate_ap_config(&ApConfig::open(&ssid_too_long)).is_err());
    }

    #[test]
    fn ap_config_password_length_boundaries() {
        let too_short = "x".repeat(AP_PASSWORD_MIN_LEN - 1);
        assert!(validate_ap_config(&ApConfig::wpa2(TEST_SSID, &too_short)).is_err());
        let min_ok = "x".repeat(AP_PASSWORD_MIN_LEN);
        assert!(validate_ap_config(&ApConfig::wpa2(TEST_SSID, &min_ok)).is_ok());
        let max_ok = "x".repeat(PASSWORD_MAX_LEN);
        assert!(validate_ap_config(&ApConfig::wpa2(TEST_SSID, &max_ok)).is_ok());
        let too_long = "x".repeat(PASSWORD_MAX_LEN + 1);
        assert!(validate_ap_config(&ApConfig::wpa2(TEST_SSID, &too_long)).is_err());
    }

    #[test]
    fn ap_config_channel_boundaries() {
        assert!(validate_ap_config(&ApConfig::open(TEST_SSID).with_channel(0)).is_err());
        assert!(validate_ap_config(&ApConfig::open(TEST_SSID).with_channel(1)).is_ok());
        assert!(validate_ap_config(&ApConfig::open(TEST_SSID).with_channel(13)).is_ok());
        assert!(validate_ap_config(&ApConfig::open(TEST_SSID).with_channel(14)).is_err());
    }

    #[test]
    fn ap_config_max_connections_zero_rejected() {
        assert!(validate_ap_config(&ApConfig::open(TEST_SSID).with_max_connections(0)).is_err());
        assert!(validate_ap_config(&ApConfig::open(TEST_SSID).with_max_connections(1)).is_ok());
    }

    #[test]
    fn ap_config_debug_redacts_password() {
        let config = ApConfig::wpa2(TEST_SSID, test_psk());
        let rendered = alloc::format!("{:?}", config);
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains(test_psk()));
    }

    #[test]
    fn ap_config_debug_open_shows_no_password() {
        let config = ApConfig::open(TEST_SSID);
        let rendered = alloc::format!("{:?}", config);
        assert!(rendered.contains("None"));
        assert!(!rendered.contains("<redacted>"));
    }
}
