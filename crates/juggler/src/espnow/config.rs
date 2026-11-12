//! Start settings of an owned ESP-NOW radio, shared by both tiers (ADR 022).
//!
//! [`EspNowConfig`] holds the Wi-Fi country, TX power and primary channel an owned radio is
//! started with.
//! The ESP-IDF constructors `init_with_radio_config` / `init_with_radio_sta_config` and the
//! esp-hal constructors `EspHalEspNow::new_owned` / `from_controller` take the same value;
//! which constructor ignores which setting is documented on the constructor.
//! Validation is pure, so a bad value is refused before any radio work.

use core::fmt;

use crate::wifi::{Country, TxPowerLevel};

/// Lowest Wi-Fi channel accepted by [`EspNowConfig::with_channel`] and `set_channel`.
pub const MIN_CHANNEL: u8 = 1;
/// Highest Wi-Fi channel accepted by [`EspNowConfig::with_channel`] and `set_channel`.
pub const MAX_CHANNEL: u8 = 14;
/// Lowest TX power accepted by the driver, in quarter dBm (2 dBm).
pub const MIN_TX_POWER_QUARTER_DBM: i8 = 8;
/// Highest TX power accepted by the driver, in quarter dBm (21 dBm).
pub const MAX_TX_POWER_QUARTER_DBM: i8 = 84;
/// TX power both tiers' owned-radio constructors apply when the config sets none: 13 dBm.
///
/// The drivers' own defaults differ (ESP-IDF 20 dBm, esp-radio 5 dBm), and 20 dBm corrupts the
/// frames and ACKs of an ESP32-C3 Super Mini.
pub const DEFAULT_TX_POWER_QUARTER_DBM: i8 = 52;

/// Why an [`EspNowConfig`] was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum EspNowConfigError {
    /// TX power outside `[8, 84]` quarter dBm.
    InvalidTxPower(i8),
    /// Channel outside `1..=14`, or one the configured country does not allow
    /// ([`EspNowConfig::validate_owned`]).
    InvalidChannel(u8),
}

impl fmt::Display for EspNowConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidTxPower(p) => write!(f, "TX power {p} outside [8, 84] quarter dBm"),
            Self::InvalidChannel(c) => write!(f, "invalid Wi-Fi channel {c}"),
        }
    }
}

impl core::error::Error for EspNowConfigError {}

/// Settings an owned ESP-NOW radio starts with.
///
/// The default leaves country, TX power and channel unset; each constructor documents what it
/// does then (the country defaults to [`Country::WORLD`] on both tiers).
///
/// ```
/// use juggler::espnow::EspNowConfig;
/// use juggler::wifi::Country;
///
/// let config = EspNowConfig::default()
///     .with_country(Country::parse("DE").unwrap())
///     .with_channel(13);
/// assert!(config.validate_owned().is_ok());
/// ```
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct EspNowConfig {
    tx_power_quarter_dbm: Option<i8>,
    channel: Option<u8>,
    country: Option<Country>,
}

impl EspNowConfig {
    /// Sets the TX power from a named level.
    pub fn with_tx_power(mut self, level: TxPowerLevel) -> Self {
        self.tx_power_quarter_dbm = Some(level.to_quarter_dbm());
        self
    }

    /// Sets the TX power in quarter dBm (`[8, 84]`, validated by the constructor); `None` leaves
    /// it unset.
    pub fn with_tx_power_quarter_dbm(mut self, quarter_dbm: Option<i8>) -> Self {
        self.tx_power_quarter_dbm = quarter_dbm;
        self
    }

    /// Pins the primary channel after start (`1..=14`).
    ///
    /// Meaningful for an owned radio, which has no access point to follow.
    /// A caller-managed station keeps the channel of its access point: ignored there.
    pub fn with_channel(mut self, channel: u8) -> Self {
        self.channel = Some(channel);
        self
    }

    /// Sets the Wi-Fi country an owned radio applies instead of [`Country::WORLD`].
    ///
    /// Applied before the TX power and the channel, so a channel the country allows (e.g. 13
    /// under `DE`) can be pinned with [`with_channel`](Self::with_channel).
    /// A caller-managed radio keeps its own country: ignored there.
    pub fn with_country(mut self, country: Country) -> Self {
        self.country = Some(country);
        self
    }

    /// Configured country, if any.
    pub fn country(&self) -> Option<Country> {
        self.country
    }

    /// Configured TX power in quarter dBm, if any.
    pub fn tx_power_quarter_dbm(&self) -> Option<i8> {
        self.tx_power_quarter_dbm
    }

    /// TX power an owned radio applies: the configured one, else [`DEFAULT_TX_POWER_QUARTER_DBM`].
    ///
    /// Both tiers' owned-radio constructors call this, so the default lives in one place.
    pub fn owned_tx_power_quarter_dbm(&self) -> i8 {
        self.tx_power_quarter_dbm
            .unwrap_or(DEFAULT_TX_POWER_QUARTER_DBM)
    }

    /// Country an owned radio applies: the configured one, else [`Country::WORLD`].
    ///
    /// Both tiers' owned-radio constructors call this, so the default lives in one place.
    pub fn owned_country(&self) -> Country {
        self.country.unwrap_or(Country::WORLD)
    }

    /// Configured channel, if any.
    pub fn channel(&self) -> Option<u8> {
        self.channel
    }

    /// Checks the TX power and the channel against their driver ranges.
    ///
    /// # Errors
    ///
    /// [`EspNowConfigError::InvalidTxPower`] outside `[8, 84]`,
    /// [`EspNowConfigError::InvalidChannel`] outside `1..=14`.
    pub fn validate(&self) -> Result<(), EspNowConfigError> {
        if let Some(quarter_dbm) = self.tx_power_quarter_dbm {
            if !(MIN_TX_POWER_QUARTER_DBM..=MAX_TX_POWER_QUARTER_DBM).contains(&quarter_dbm) {
                return Err(EspNowConfigError::InvalidTxPower(quarter_dbm));
            }
        }
        if let Some(channel) = self.channel {
            if !(MIN_CHANNEL..=MAX_CHANNEL).contains(&channel) {
                return Err(EspNowConfigError::InvalidChannel(channel));
            }
        }
        Ok(())
    }

    /// [`validate`](Self::validate), plus: a configured channel must be allowed by the
    /// configured country ([`Country::WORLD`] when unset, channels 1-11).
    ///
    /// For constructors that apply the country and pin the channel.
    /// The radio's read-back stays authoritative; this only refuses, before any radio work, a
    /// channel the built-in table already forbids.
    ///
    /// # Errors
    ///
    /// As [`validate`](Self::validate), and [`EspNowConfigError::InvalidChannel`] for a channel
    /// outside the country's channels.
    pub fn validate_owned(&self) -> Result<(), EspNowConfigError> {
        self.validate()?;
        if let Some(channel) = self.channel {
            if !self
                .country
                .unwrap_or(Country::WORLD)
                .channels()
                .contains(channel)
            {
                return Err(EspNowConfigError::InvalidChannel(channel));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn owned_defaults_are_13_dbm_and_world() {
        let config = super::EspNowConfig::default();
        assert_eq!(
            config.owned_tx_power_quarter_dbm(),
            super::DEFAULT_TX_POWER_QUARTER_DBM
        );
        assert_eq!(super::DEFAULT_TX_POWER_QUARTER_DBM, 52);
        assert_eq!(config.owned_country(), crate::wifi::Country::WORLD);
    }

    #[test]
    fn owned_values_follow_the_config() {
        let de = crate::wifi::Country::parse("DE").unwrap();
        let config = super::EspNowConfig::default()
            .with_tx_power_quarter_dbm(Some(34))
            .with_country(de);
        assert_eq!(config.owned_tx_power_quarter_dbm(), 34);
        assert_eq!(config.owned_country(), de);
    }

    use super::*;

    #[test]
    fn default_config_is_unset_and_valid() {
        let c = EspNowConfig::default();
        assert_eq!(c.tx_power_quarter_dbm(), None);
        assert_eq!(c.channel(), None);
        assert_eq!(c.country(), None);
        assert_eq!(DEFAULT_TX_POWER_QUARTER_DBM, 52);
        assert_eq!(c.validate(), Ok(()));
        assert_eq!(c.validate_owned(), Ok(()));
    }

    #[test]
    fn tx_power_bounds() {
        let with = |q| EspNowConfig::default().with_tx_power_quarter_dbm(Some(q));
        assert_eq!(with(8).validate(), Ok(()));
        assert_eq!(with(84).validate(), Ok(()));
        assert_eq!(
            with(7).validate(),
            Err(EspNowConfigError::InvalidTxPower(7))
        );
        assert_eq!(
            with(85).validate(),
            Err(EspNowConfigError::InvalidTxPower(85))
        );
        assert_eq!(
            with(-1).validate(),
            Err(EspNowConfigError::InvalidTxPower(-1))
        );
    }

    #[test]
    fn levels_map_to_quarter_dbm() {
        let max = EspNowConfig::default().with_tx_power(TxPowerLevel::Max);
        assert_eq!(max.tx_power_quarter_dbm(), Some(78));
        assert_eq!(max.validate(), Ok(()));
    }

    #[test]
    fn channel_bounds() {
        let with = |c| EspNowConfig::default().with_channel(c);
        assert_eq!(with(1).validate(), Ok(()));
        assert_eq!(with(14).validate(), Ok(()));
        assert_eq!(
            with(0).validate(),
            Err(EspNowConfigError::InvalidChannel(0))
        );
        assert_eq!(
            with(15).validate(),
            Err(EspNowConfigError::InvalidChannel(15))
        );
    }

    #[test]
    fn owned_validation_checks_channel_against_country() {
        let de = Country::parse("DE").unwrap();
        let config = EspNowConfig::default().with_channel(13);
        assert_eq!(config.validate(), Ok(()));
        assert_eq!(
            config.validate_owned(),
            Err(EspNowConfigError::InvalidChannel(13))
        );
        assert_eq!(config.with_country(de).validate_owned(), Ok(()));
        assert_eq!(
            config.with_country(de).with_channel(14).validate_owned(),
            Err(EspNowConfigError::InvalidChannel(14))
        );
        assert_eq!(
            EspNowConfig::default().with_channel(11).validate_owned(),
            Ok(())
        );
    }

    #[test]
    fn range_error_wins_over_country_error() {
        assert_eq!(
            EspNowConfig::default()
                .with_tx_power_quarter_dbm(Some(1))
                .with_channel(13)
                .validate_owned(),
            Err(EspNowConfigError::InvalidTxPower(1))
        );
    }
}
