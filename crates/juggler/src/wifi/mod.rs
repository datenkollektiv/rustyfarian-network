//! Platform-independent Wi-Fi types, traits, and validation.
//!
//! # Architecture
//!
//! - [`WiFiConfig`] — connection configuration (SSID, password, connect mode)
//! - [`ConnectMode`] — blocking vs. non-blocking association strategy
//! - [`LinkMachine`] — pure reconnect policy shared by both tiers (see [`link`])
//! - [`Country`] — the regulatory domain both tiers apply (see [`country`])
//! - [`JoinFailure`] — the class of a failed join, for the provisioning fallback
//!   (see [`join_failure`])
//! - [`mock::MockLink`] — test double for host-side unit tests
//!   (requires the `mock` feature or `#[cfg(test)]`)
//!
//! # Feature flags
//!
//! | Feature | What it enables                                              |
//! |:--------|:-------------------------------------------------------------|
//! | `mock`  | `MockLink` for downstream host tests                         |

#[cfg(any(test, feature = "mock"))]
mod link_mock;
#[cfg(any(test, feature = "mock"))]
pub mod mock;

pub mod country;
pub mod handover;
pub mod join_failure;
pub mod lines;
pub mod link;
pub mod network_link;
#[cfg(feature = "std")]
pub mod supervisor;

pub use country::{
    apply_country, ChannelSet, Country, CountryError, CountryPort, Regulatory,
    SUPPORTED_COUNTRY_CODES,
};
pub use handover::{IdleWait, LinkHandover};
pub use join_failure::{JoinAttempts, JoinFailure, AUTH_EXPIRE_REPEAT};
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
/// [`AP_CHANNEL_MIN`]`..=`[`AP_CHANNEL_MAX`] and against the channels of
/// [`ApConfig::country`] (ESP-IDF silently moves a SoftAP on a channel outside
/// the country to channel 1), and `max_connections >= 1`.
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
    if !config.country.channels().contains(config.channel) {
        return Err("AP channel is not allowed by the configured country");
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
    /// Use the tier's `WiFiManager::is_connected()` to check readiness.
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
/// `Display` gives the shared text, e.g. `bssid 02:00:00:ca:fe:01, ch 11, rssi -21 dBm`,
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

/// Station TX power both tiers apply when the firmware sets none: [`TxPowerLevel::Low`] (~8.5 dBm).
///
/// PCB-antenna boards such as the ESP32-C3 Super Mini corrupt their own frames at high power
/// (WPA2 auth deauths with reason 2 on the bare-metal blob, broken ESP-NOW frames); 8.5 dBm is the
/// level validated on both tiers.
/// Both tiers apply [`WiFiConfig::station_tx_power_quarter_dbm`] verbatim, so this is the only
/// place the default lives.
pub const STATION_TX_POWER_DEFAULT: TxPowerLevel = TxPowerLevel::Low;

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
///     .with_ap_selection(ApSelection::FirstFound) // optional: faster join, first AP found
///     .with_country(Country::parse("DE")?);    // optional: channels 1-13 instead of 1-11
/// ```
#[derive(Debug, Clone)]
pub struct WiFiConfig<'a> {
    pub ssid: &'a str,
    pub password: &'a str,
    /// How the ESP-IDF `WiFiManager` constructor waits for the link (see
    /// [`ConnectMode::effective_timeout_secs`]). The esp-hal tier ignores it: a
    /// firmware there waits through `WiFiManager::wait_connected`.
    pub connect_mode: ConnectMode,
    pub power_save: WifiPowerSave,
    pub tx_power: TxPowerLevel,
    /// How the station picks an AP when several broadcast the SSID.
    pub ap_selection: ApSelection,
    /// The country applied to the radio before it starts (default [`Country::WORLD`]).
    pub country: Country,
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
            tx_power: STATION_TX_POWER_DEFAULT,
            ap_selection: ApSelection::default(),
            country: Country::WORLD,
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
    /// Defaults to [`STATION_TX_POWER_DEFAULT`] ([`TxPowerLevel::Low`], ~8.5 dBm) on both tiers.
    ///
    /// Lower levels reduce power draw and heat; higher levels increase range.
    /// The exact dBm mapping depends on the chip — see [`TxPowerLevel`].
    pub fn with_tx_power(mut self, level: TxPowerLevel) -> Self {
        self.tx_power = level;
        self
    }

    /// TX power the station applies, in quarter dBm: the configured level, else
    /// [`STATION_TX_POWER_DEFAULT`]; both tiers set exactly this after `wifi.start()`.
    pub fn station_tx_power_quarter_dbm(&self) -> i8 {
        self.tx_power.to_quarter_dbm()
    }

    /// Sets how the station picks an AP when several broadcast the SSID.
    ///
    /// Defaults to [`ApSelection::Strongest`] on both tiers.
    pub fn with_ap_selection(mut self, selection: ApSelection) -> Self {
        self.ap_selection = selection;
        self
    }

    /// Sets the country applied to the radio before it starts.
    ///
    /// Defaults to [`Country::WORLD`] (channels 1-11) on both tiers.
    /// With 802.11d on (the default) the station finds an AP on channel 12 or 13 by its
    /// passive scan and adopts the AP's country once associated; the configured one decides
    /// while unassociated (see [`country`]).
    /// A country that allows 12 and 13 is needed only with `with_ieee80211d(false)`, or for
    /// a faster active scan of them.
    pub fn with_country(mut self, country: Country) -> Self {
        self.country = country;
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
///     .with_tx_power(TxPowerLevel::Low)  // optional: reduce transmit power
///     .with_country(Country::WORLD);     // optional: the default, channels 1-11
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
    /// The country applied before the AP starts; `channel` must be one it allows.
    pub country: Country,
}

impl<'a> Clone for ApConfig<'a> {
    fn clone(&self) -> Self {
        Self {
            ssid: self.ssid,
            password: self.password,
            channel: self.channel,
            max_connections: self.max_connections,
            tx_power: self.tx_power,
            country: self.country,
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
            .field("country", &self.country)
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
            country: Country::WORLD,
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
            country: Country::WORLD,
        }
    }

    /// Sets the 2.4 GHz channel (validated to 1..=13, and to the channels of the
    /// country, by [`validate_ap_config`]).
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

    /// Sets the country applied before the AP starts (default [`Country::WORLD`],
    /// channels 1-11); the channel must be one the country allows.
    pub fn with_country(mut self, country: Country) -> Self {
        self.country = country;
        self
    }
}

// ─── Disconnect reason vocabulary ───────────────────────────────────────────

/// Why the station lost (or never got) its association, as both tiers name it.
///
/// Kinds and their [`name`](Self::name) follow `wifi_err_reason_t`
/// (`esp_wifi_types_generic.h`), so a log line can be matched against the
/// ESP-IDF documentation.
/// The 802.11 management codes 1..=24 and the ESP-IDF band 200..=212 are named;
/// the rarer 802.11 codes (TDLS, QoS, fast transition, 25..=68) are not.
///
/// The ESP-IDF tier builds a kind from the numeric code with
/// [`from_code`](Self::from_code).
/// The esp-hal tier maps esp-radio's named reasons onto the same kinds, so a named
/// kind reports its ESP-IDF [`code`](Self::code) on both tiers.
///
/// [`Display`](core::fmt::Display) renders `reason 8 (ASSOC_LEAVE)`,
/// `reason 77 (unmapped)`, or `reason ? (UNSPECIFIED)` when the code is unknown;
/// the shared [`lines::Disconnected`] line adds [`CREDENTIALS_HINT`] where
/// [`suggests_credentials`](Self::suggests_credentials) holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum DisconnectReason {
    /// `UNSPECIFIED` (1).
    ///
    /// esp-radio folds code 1 and every code it does not know into one variant,
    /// so on esp-hal this is also the catch-all and `code` is `None` there;
    /// on ESP-IDF `code` is `Some(1)`.
    Unspecified {
        /// The ESP-IDF code, when the tier can tell.
        code: Option<u16>,
    },
    /// `AUTH_EXPIRE` (2).
    AuthExpire,
    /// `AUTH_LEAVE` (3): the AP deauthenticated the station.
    AuthLeave,
    /// `DISASSOC_DUE_TO_INACTIVITY` (4).
    DisassocDueToInactivity,
    /// `ASSOC_TOOMANY` (5).
    AssocTooMany,
    /// `CLASS2_FRAME_FROM_NONAUTH_STA` (6).
    Class2FrameFromNonauthSta,
    /// `CLASS3_FRAME_FROM_NONASSOC_STA` (7).
    Class3FrameFromNonassocSta,
    /// `ASSOC_LEAVE` (8): the station or the AP left the association.
    AssocLeave,
    /// `ASSOC_NOT_AUTHED` (9).
    AssocNotAuthed,
    /// `DISASSOC_PWRCAP_BAD` (10).
    DisassocPwrcapBad,
    /// `DISASSOC_SUPCHAN_BAD` (11).
    DisassocSupchanBad,
    /// `BSS_TRANSITION_DISASSOC` (12).
    BssTransitionDisassoc,
    /// `IE_INVALID` (13).
    IeInvalid,
    /// `MIC_FAILURE` (14).
    MicFailure,
    /// `4WAY_HANDSHAKE_TIMEOUT` (15): typically a wrong password.
    FourWayHandshakeTimeout,
    /// `GROUP_KEY_UPDATE_TIMEOUT` (16).
    GroupKeyUpdateTimeout,
    /// `IE_IN_4WAY_DIFFERS` (17).
    IeIn4wayDiffers,
    /// `GROUP_CIPHER_INVALID` (18).
    GroupCipherInvalid,
    /// `PAIRWISE_CIPHER_INVALID` (19).
    PairwiseCipherInvalid,
    /// `AKMP_INVALID` (20).
    AkmpInvalid,
    /// `UNSUPP_RSN_IE_VERSION` (21).
    UnsuppRsnIeVersion,
    /// `INVALID_RSN_IE_CAP` (22).
    InvalidRsnIeCap,
    /// `802_1X_AUTH_FAILED` (23).
    Ieee8021xAuthFailed,
    /// `CIPHER_SUITE_REJECTED` (24).
    CipherSuiteRejected,
    /// `BEACON_TIMEOUT` (200): the AP's beacons stopped arriving.
    BeaconTimeout,
    /// `NO_AP_FOUND` (201): the SSID is not in range, or misspelt.
    NoApFound,
    /// `AUTH_FAIL` (202).
    AuthFail,
    /// `ASSOC_FAIL` (203).
    AssocFail,
    /// `HANDSHAKE_TIMEOUT` (204).
    HandshakeTimeout,
    /// `CONNECTION_FAIL` (205).
    ConnectionFail,
    /// `AP_TSF_RESET` (206).
    ApTsfReset,
    /// `ROAMING` (207).
    Roaming,
    /// `ASSOC_COMEBACK_TIME_TOO_LONG` (208).
    AssocComebackTimeTooLong,
    /// `SA_QUERY_TIMEOUT` (209).
    SaQueryTimeout,
    /// `NO_AP_FOUND_W_COMPATIBLE_SECURITY` (210).
    NoApFoundWCompatibleSecurity,
    /// `NO_AP_FOUND_IN_AUTHMODE_THRESHOLD` (211).
    NoApFoundInAuthmodeThreshold,
    /// `NO_AP_FOUND_IN_RSSI_THRESHOLD` (212).
    NoApFoundInRssiThreshold,
    /// An ESP-IDF code without a name here (the rarer 802.11 codes, or a newer one);
    /// logged as `(unmapped)` instead of a guess.
    Unmapped(u16),
}

impl DisconnectReason {
    /// The kind for an ESP-IDF disconnect code (`wifi_event_sta_disconnected_t::reason`).
    pub const fn from_code(code: u16) -> Self {
        match code {
            1 => Self::Unspecified { code: Some(1) },
            2 => Self::AuthExpire,
            3 => Self::AuthLeave,
            4 => Self::DisassocDueToInactivity,
            5 => Self::AssocTooMany,
            6 => Self::Class2FrameFromNonauthSta,
            7 => Self::Class3FrameFromNonassocSta,
            8 => Self::AssocLeave,
            9 => Self::AssocNotAuthed,
            10 => Self::DisassocPwrcapBad,
            11 => Self::DisassocSupchanBad,
            12 => Self::BssTransitionDisassoc,
            13 => Self::IeInvalid,
            14 => Self::MicFailure,
            15 => Self::FourWayHandshakeTimeout,
            16 => Self::GroupKeyUpdateTimeout,
            17 => Self::IeIn4wayDiffers,
            18 => Self::GroupCipherInvalid,
            19 => Self::PairwiseCipherInvalid,
            20 => Self::AkmpInvalid,
            21 => Self::UnsuppRsnIeVersion,
            22 => Self::InvalidRsnIeCap,
            23 => Self::Ieee8021xAuthFailed,
            24 => Self::CipherSuiteRejected,
            200 => Self::BeaconTimeout,
            201 => Self::NoApFound,
            202 => Self::AuthFail,
            203 => Self::AssocFail,
            204 => Self::HandshakeTimeout,
            205 => Self::ConnectionFail,
            206 => Self::ApTsfReset,
            207 => Self::Roaming,
            208 => Self::AssocComebackTimeTooLong,
            209 => Self::SaQueryTimeout,
            210 => Self::NoApFoundWCompatibleSecurity,
            211 => Self::NoApFoundInAuthmodeThreshold,
            212 => Self::NoApFoundInRssiThreshold,
            other => Self::Unmapped(other),
        }
    }

    /// The ESP-IDF code; `None` only for esp-hal's catch-all, whose code esp-radio
    /// does not report.
    pub const fn code(&self) -> Option<u16> {
        Some(match self {
            Self::Unspecified { code } => return *code,
            Self::AuthExpire => 2,
            Self::AuthLeave => 3,
            Self::DisassocDueToInactivity => 4,
            Self::AssocTooMany => 5,
            Self::Class2FrameFromNonauthSta => 6,
            Self::Class3FrameFromNonassocSta => 7,
            Self::AssocLeave => 8,
            Self::AssocNotAuthed => 9,
            Self::DisassocPwrcapBad => 10,
            Self::DisassocSupchanBad => 11,
            Self::BssTransitionDisassoc => 12,
            Self::IeInvalid => 13,
            Self::MicFailure => 14,
            Self::FourWayHandshakeTimeout => 15,
            Self::GroupKeyUpdateTimeout => 16,
            Self::IeIn4wayDiffers => 17,
            Self::GroupCipherInvalid => 18,
            Self::PairwiseCipherInvalid => 19,
            Self::AkmpInvalid => 20,
            Self::UnsuppRsnIeVersion => 21,
            Self::InvalidRsnIeCap => 22,
            Self::Ieee8021xAuthFailed => 23,
            Self::CipherSuiteRejected => 24,
            Self::BeaconTimeout => 200,
            Self::NoApFound => 201,
            Self::AuthFail => 202,
            Self::AssocFail => 203,
            Self::HandshakeTimeout => 204,
            Self::ConnectionFail => 205,
            Self::ApTsfReset => 206,
            Self::Roaming => 207,
            Self::AssocComebackTimeTooLong => 208,
            Self::SaQueryTimeout => 209,
            Self::NoApFoundWCompatibleSecurity => 210,
            Self::NoApFoundInAuthmodeThreshold => 211,
            Self::NoApFoundInRssiThreshold => 212,
            Self::Unmapped(code) => *code,
        })
    }

    /// The `wifi_err_reason_t` spelling without the `WIFI_REASON_` prefix
    /// (`ASSOC_LEAVE`, `4WAY_HANDSHAKE_TIMEOUT`); `unmapped` for [`Unmapped`](Self::Unmapped).
    pub const fn name(&self) -> &'static str {
        match self {
            Self::Unspecified { .. } => "UNSPECIFIED",
            Self::AuthExpire => "AUTH_EXPIRE",
            Self::AuthLeave => "AUTH_LEAVE",
            Self::DisassocDueToInactivity => "DISASSOC_DUE_TO_INACTIVITY",
            Self::AssocTooMany => "ASSOC_TOOMANY",
            Self::Class2FrameFromNonauthSta => "CLASS2_FRAME_FROM_NONAUTH_STA",
            Self::Class3FrameFromNonassocSta => "CLASS3_FRAME_FROM_NONASSOC_STA",
            Self::AssocLeave => "ASSOC_LEAVE",
            Self::AssocNotAuthed => "ASSOC_NOT_AUTHED",
            Self::DisassocPwrcapBad => "DISASSOC_PWRCAP_BAD",
            Self::DisassocSupchanBad => "DISASSOC_SUPCHAN_BAD",
            Self::BssTransitionDisassoc => "BSS_TRANSITION_DISASSOC",
            Self::IeInvalid => "IE_INVALID",
            Self::MicFailure => "MIC_FAILURE",
            Self::FourWayHandshakeTimeout => "4WAY_HANDSHAKE_TIMEOUT",
            Self::GroupKeyUpdateTimeout => "GROUP_KEY_UPDATE_TIMEOUT",
            Self::IeIn4wayDiffers => "IE_IN_4WAY_DIFFERS",
            Self::GroupCipherInvalid => "GROUP_CIPHER_INVALID",
            Self::PairwiseCipherInvalid => "PAIRWISE_CIPHER_INVALID",
            Self::AkmpInvalid => "AKMP_INVALID",
            Self::UnsuppRsnIeVersion => "UNSUPP_RSN_IE_VERSION",
            Self::InvalidRsnIeCap => "INVALID_RSN_IE_CAP",
            Self::Ieee8021xAuthFailed => "802_1X_AUTH_FAILED",
            Self::CipherSuiteRejected => "CIPHER_SUITE_REJECTED",
            Self::BeaconTimeout => "BEACON_TIMEOUT",
            Self::NoApFound => "NO_AP_FOUND",
            Self::AuthFail => "AUTH_FAIL",
            Self::AssocFail => "ASSOC_FAIL",
            Self::HandshakeTimeout => "HANDSHAKE_TIMEOUT",
            Self::ConnectionFail => "CONNECTION_FAIL",
            Self::ApTsfReset => "AP_TSF_RESET",
            Self::Roaming => "ROAMING",
            Self::AssocComebackTimeTooLong => "ASSOC_COMEBACK_TIME_TOO_LONG",
            Self::SaQueryTimeout => "SA_QUERY_TIMEOUT",
            Self::NoApFoundWCompatibleSecurity => "NO_AP_FOUND_W_COMPATIBLE_SECURITY",
            Self::NoApFoundInAuthmodeThreshold => "NO_AP_FOUND_IN_AUTHMODE_THRESHOLD",
            Self::NoApFoundInRssiThreshold => "NO_AP_FOUND_IN_RSSI_THRESHOLD",
            Self::Unmapped(_) => "unmapped",
        }
    }

    /// `true` for the reasons that point at the credentials or at a network that is
    /// not there, so a log line may suggest checking them.
    ///
    /// The four are those a wrong SSID, a wrong password or an access point that is
    /// out of range produce: `4WAY_HANDSHAKE_TIMEOUT` (15), `NO_AP_FOUND` (201),
    /// `AUTH_FAIL` (202) and `HANDSHAKE_TIMEOUT` (204).
    /// A router-initiated deauthentication (`AUTH_LEAVE`, `ASSOC_LEAVE`), a beacon
    /// loss or the library's own suspend are deliberately not in the set: they say
    /// nothing about the credentials, and the old unconditional hint sent users
    /// hunting for a typo after every suspend.
    pub const fn suggests_credentials(&self) -> bool {
        matches!(
            self,
            Self::FourWayHandshakeTimeout
                | Self::NoApFound
                | Self::AuthFail
                | Self::HandshakeTimeout
        )
    }
}

impl core::fmt::Display for DisconnectReason {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.code() {
            Some(code) => write!(f, "reason {} ({})", code, self.name()),
            None => write!(f, "reason ? ({})", self.name()),
        }
    }
}

/// The sentence the shared disconnect line appends when the reason points at the
/// credentials ([`DisconnectReason::suggests_credentials`]).
pub const CREDENTIALS_HINT: &str = " — check SSID/password and network availability";

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

    // ── Disconnect reason vocabulary tests ──────────────────────────────

    /// Every named kind with its ESP-IDF code and `wifi_err_reason_t` spelling.
    const NAMED_REASONS: [(u16, &str); 37] = [
        (1, "UNSPECIFIED"),
        (2, "AUTH_EXPIRE"),
        (3, "AUTH_LEAVE"),
        (4, "DISASSOC_DUE_TO_INACTIVITY"),
        (5, "ASSOC_TOOMANY"),
        (6, "CLASS2_FRAME_FROM_NONAUTH_STA"),
        (7, "CLASS3_FRAME_FROM_NONASSOC_STA"),
        (8, "ASSOC_LEAVE"),
        (9, "ASSOC_NOT_AUTHED"),
        (10, "DISASSOC_PWRCAP_BAD"),
        (11, "DISASSOC_SUPCHAN_BAD"),
        (12, "BSS_TRANSITION_DISASSOC"),
        (13, "IE_INVALID"),
        (14, "MIC_FAILURE"),
        (15, "4WAY_HANDSHAKE_TIMEOUT"),
        (16, "GROUP_KEY_UPDATE_TIMEOUT"),
        (17, "IE_IN_4WAY_DIFFERS"),
        (18, "GROUP_CIPHER_INVALID"),
        (19, "PAIRWISE_CIPHER_INVALID"),
        (20, "AKMP_INVALID"),
        (21, "UNSUPP_RSN_IE_VERSION"),
        (22, "INVALID_RSN_IE_CAP"),
        (23, "802_1X_AUTH_FAILED"),
        (24, "CIPHER_SUITE_REJECTED"),
        (200, "BEACON_TIMEOUT"),
        (201, "NO_AP_FOUND"),
        (202, "AUTH_FAIL"),
        (203, "ASSOC_FAIL"),
        (204, "HANDSHAKE_TIMEOUT"),
        (205, "CONNECTION_FAIL"),
        (206, "AP_TSF_RESET"),
        (207, "ROAMING"),
        (208, "ASSOC_COMEBACK_TIME_TOO_LONG"),
        (209, "SA_QUERY_TIMEOUT"),
        (210, "NO_AP_FOUND_W_COMPATIBLE_SECURITY"),
        (211, "NO_AP_FOUND_IN_AUTHMODE_THRESHOLD"),
        (212, "NO_AP_FOUND_IN_RSSI_THRESHOLD"),
    ];

    #[test]
    fn every_named_code_maps_to_its_kind_and_back() {
        for (code, name) in NAMED_REASONS {
            let reason = DisconnectReason::from_code(code);
            assert_eq!(reason.name(), name, "code {code}");
            assert_eq!(reason.code(), Some(code), "code {code}");
            assert!(
                !matches!(reason, DisconnectReason::Unmapped(_)),
                "code {code}"
            );
        }
    }

    #[test]
    fn the_codes_map_to_the_expected_kinds() {
        assert_eq!(
            DisconnectReason::from_code(1),
            DisconnectReason::Unspecified { code: Some(1) }
        );
        assert_eq!(DisconnectReason::from_code(2), DisconnectReason::AuthExpire);
        assert_eq!(DisconnectReason::from_code(8), DisconnectReason::AssocLeave);
        assert_eq!(
            DisconnectReason::from_code(15),
            DisconnectReason::FourWayHandshakeTimeout
        );
        assert_eq!(
            DisconnectReason::from_code(23),
            DisconnectReason::Ieee8021xAuthFailed
        );
        assert_eq!(
            DisconnectReason::from_code(200),
            DisconnectReason::BeaconTimeout
        );
        assert_eq!(
            DisconnectReason::from_code(201),
            DisconnectReason::NoApFound
        );
        assert_eq!(DisconnectReason::from_code(202), DisconnectReason::AuthFail);
        assert_eq!(
            DisconnectReason::from_code(212),
            DisconnectReason::NoApFoundInRssiThreshold
        );
    }

    #[test]
    fn unnamed_codes_stay_unmapped_with_their_code() {
        for code in [0, 25, 68, 199, 213, 999, u16::MAX] {
            let reason = DisconnectReason::from_code(code);
            assert_eq!(reason, DisconnectReason::Unmapped(code), "code {code}");
            assert_eq!(reason.code(), Some(code), "code {code}");
            assert_eq!(reason.name(), "unmapped", "code {code}");
            assert!(!reason.suggests_credentials(), "code {code}");
        }
    }

    #[test]
    fn the_esp_hal_catch_all_has_no_code() {
        let catch_all = DisconnectReason::Unspecified { code: None };
        assert_eq!(catch_all.code(), None);
        assert_eq!(catch_all.name(), "UNSPECIFIED");
        assert!(!catch_all.suggests_credentials());
    }

    #[test]
    fn only_reasons_that_point_at_the_credentials_suggest_them() {
        for code in [15, 201, 202, 204] {
            assert!(
                DisconnectReason::from_code(code).suggests_credentials(),
                "code {code}"
            );
        }
        for (code, _) in NAMED_REASONS {
            if ![15, 201, 202, 204].contains(&code) {
                assert!(
                    !DisconnectReason::from_code(code).suggests_credentials(),
                    "code {code}"
                );
            }
        }
    }

    #[test]
    fn a_reason_renders_its_code_and_name() {
        let text = |reason: DisconnectReason| alloc::format!("{reason}");
        assert_eq!(
            text(DisconnectReason::from_code(8)),
            "reason 8 (ASSOC_LEAVE)"
        );
        assert_eq!(
            text(DisconnectReason::from_code(3)),
            "reason 3 (AUTH_LEAVE)"
        );
        assert_eq!(
            text(DisconnectReason::from_code(202)),
            "reason 202 (AUTH_FAIL)",
            "the hint belongs to the line, not the reason"
        );
        assert_eq!(
            text(DisconnectReason::from_code(77)),
            "reason 77 (unmapped)"
        );
        assert_eq!(
            text(DisconnectReason::Unspecified { code: None }),
            "reason ? (UNSPECIFIED)"
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
    fn wifi_config_default_tx_power_is_low() {
        let config = test_config();
        assert_eq!(config.tx_power, TxPowerLevel::Low);
        assert_eq!(STATION_TX_POWER_DEFAULT, TxPowerLevel::Low);
        assert_eq!(config.station_tx_power_quarter_dbm(), 34);
    }

    #[test]
    fn station_tx_power_follows_an_explicit_level() {
        let config = test_config().with_tx_power(TxPowerLevel::Medium);
        assert_eq!(config.station_tx_power_quarter_dbm(), 52);
    }

    #[test]
    fn wifi_config_with_tx_power() {
        let config = test_config().with_tx_power(TxPowerLevel::Lowest);
        assert_eq!(config.tx_power, TxPowerLevel::Lowest);
    }

    #[test]
    fn joined_ap_display_is_the_shared_log_text() {
        let ap = JoinedAp {
            bssid: [0x02, 0x00, 0x00, 0x0c, 0xfe, 0x04],
            channel: 11,
            rssi: -21,
        };
        assert_eq!(
            alloc::format!("{ap}"),
            "bssid 02:00:00:0c:fe:04, ch 11, rssi -21 dBm"
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
        assert!(validate_ap_config(&ApConfig::open(TEST_SSID).with_channel(11)).is_ok());
        assert!(validate_ap_config(&ApConfig::open(TEST_SSID).with_channel(14)).is_err());
        let de = Country::parse("DE").unwrap();
        let de_ap = |channel| {
            ApConfig::open(TEST_SSID)
                .with_country(de)
                .with_channel(channel)
        };
        assert!(validate_ap_config(&de_ap(13)).is_ok());
        assert!(validate_ap_config(&de_ap(14)).is_err());
    }

    #[test]
    fn ap_config_channel_must_be_allowed_by_the_country() {
        assert_eq!(
            validate_ap_config(&ApConfig::open(TEST_SSID).with_channel(12)),
            Err("AP channel is not allowed by the configured country")
        );
        assert!(validate_ap_config(&ApConfig::open(TEST_SSID).with_channel(13)).is_err());
        let de = Country::parse("DE").unwrap();
        assert!(
            validate_ap_config(&ApConfig::open(TEST_SSID).with_channel(12).with_country(de))
                .is_ok()
        );
        let jp = Country::parse("JP").unwrap();
        assert!(
            validate_ap_config(&ApConfig::open(TEST_SSID).with_channel(14).with_country(jp))
                .is_err(),
            "the portable 1..=13 cap still holds"
        );
    }

    #[test]
    fn country_defaults_to_world_and_is_carried() {
        assert_eq!(
            WiFiConfig::new(TEST_SSID, test_psk()).country,
            Country::WORLD
        );
        let de = Country::parse("DE").unwrap();
        assert_eq!(
            WiFiConfig::new(TEST_SSID, test_psk())
                .with_country(de)
                .country,
            de
        );
        let ap = ApConfig::open(TEST_SSID).with_country(de);
        assert_eq!(ap.clone().country, de);
        assert!(alloc::format!("{ap:?}").contains("DE"));
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
