//! The Wi-Fi country (regulatory domain) both tiers apply to the radio.
//!
//! The country decides which 2.4 GHz channels the radio may transmit on.
//! [`Country::WORLD`] (`"01"`, the ESP-IDF world-safe mode) allows channels 1-11 and is the
//! default on both tiers; a device that must reach a network on channel 12 or 13 sets the
//! country of that network, e.g. `Country::parse("DE")`.
//!
//! - [`Country`] — a validated two-letter code plus the 802.11d policy
//! - [`ChannelSet`] — a set of 2.4 GHz channels 1..=14
//! - [`Regulatory`] — what the radio reports back after the country was applied
//! - [`CountryPort`] and [`apply_country`] — the shared read-before-write apply sequence;
//!   each tier implements the port over its own FFI
//!
//! # 802.11d
//!
//! With 802.11d on (the default), an associated station adopts the country its AP
//! announces and falls back to the configured one after a disconnect.
//! An unassociated radio (an ESP-NOW scout, a SoftAP) always uses the configured country.
//! [`Country::with_ieee80211d`]`(false)` pins the configured country even while associated.
//!
//! # Persistence
//!
//! ESP-IDF keeps the country in flash (NVS); it survives a reflash.
//! Both tiers re-apply the configured country on every boot, so the stored value never
//! decides; [`apply_country`] skips the write when the radio already reports the requested
//! code and policy and the station is not associated, so a boot with an unchanged country
//! does not wear the flash.
//! A constructor that applies one country followed by a change to another still writes twice,
//! so the owned ESP-NOW constructors take the country directly.

use core::fmt;

use super::lines::CountryApplied;

/// Every country code the ESP-IDF Wi-Fi driver accepts, `"01"` (world-safe mode) first.
///
/// The list is the one documented for `esp_wifi_set_country_code` in ESP-IDF v5.3
/// (`esp_wifi.h`).
pub const SUPPORTED_COUNTRY_CODES: [[u8; 2]; 45] = [
    *b"01", *b"AT", *b"AU", *b"BE", *b"BG", *b"BR", *b"CA", *b"CH", *b"CN", *b"CY", *b"CZ", *b"DE",
    *b"DK", *b"EE", *b"ES", *b"FI", *b"FR", *b"GB", *b"GR", *b"HK", *b"HR", *b"HU", *b"IE", *b"IN",
    *b"IS", *b"IT", *b"JP", *b"KR", *b"LI", *b"LT", *b"LU", *b"LV", *b"MT", *b"MX", *b"NL", *b"NO",
    *b"NZ", *b"PL", *b"PT", *b"RO", *b"SE", *b"SI", *b"SK", *b"TW", *b"US",
];

// ─── ChannelSet ─────────────────────────────────────────────────────────────

/// A set of 2.4 GHz channels, one bit per channel 1..=14.
///
/// `Display` renders runs, e.g. `1-11` or `1-6, 9`, and `none` for the empty set.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ChannelSet(u16);

impl ChannelSet {
    /// Bits of the valid channels 1..=14.
    const MASK: u16 = 0b0111_1111_1111_1110;

    /// Every 2.4 GHz channel, 1..=14.
    pub const ALL: Self = Self(Self::MASK);

    /// No channel.
    pub const EMPTY: Self = Self(0);

    /// The channels `schan..schan + nchan`, clamped to 1..=14.
    ///
    /// This is the shape of the radio's `wifi_country_t` (first channel and channel count).
    /// Channels outside 1..=14 are dropped, so `schan` 0 loses channel 0 and a range past 14
    /// stops at 14.
    pub const fn from_range(schan: u8, nchan: u8) -> Self {
        let start = schan as u16;
        let end = start + nchan as u16;
        let mut bits = 0u16;
        let mut channel = start;
        while channel < end && channel <= 14 {
            bits |= 1 << channel;
            channel += 1;
        }
        Self(bits & Self::MASK)
    }

    /// The set from its raw bits (bit `n` = channel `n`); bits outside 1..=14 are dropped.
    ///
    /// The inverse of [`bits`](Self::bits), for storing a set in an atomic.
    pub const fn from_bits(bits: u16) -> Self {
        Self(bits & Self::MASK)
    }

    /// The raw bits (bit `n` = channel `n`).
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// Whether `channel` is in the set; always `false` outside 1..=14.
    pub const fn contains(self, channel: u8) -> bool {
        channel >= 1 && channel <= 14 && self.0 & (1 << channel) != 0
    }

    /// The set with `channel` added; a channel outside 1..=14 is ignored.
    #[must_use]
    pub const fn with(self, channel: u8) -> Self {
        if channel >= 1 && channel <= 14 {
            Self(self.0 | (1 << channel))
        } else {
            self
        }
    }

    /// The channels in ascending order.
    pub fn iter(self) -> impl Iterator<Item = u8> {
        (1..=14).filter(move |&channel| self.contains(channel))
    }

    /// Number of channels in the set.
    pub const fn len(self) -> usize {
        self.0.count_ones() as usize
    }

    /// Whether the set has no channel.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl fmt::Display for ChannelSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return f.write_str("none");
        }
        let mut first = true;
        let mut channel = 1u8;
        while channel <= 14 {
            if !self.contains(channel) {
                channel += 1;
                continue;
            }
            let start = channel;
            while channel < 14 && self.contains(channel + 1) {
                channel += 1;
            }
            if !first {
                f.write_str(", ")?;
            }
            first = false;
            if start == channel {
                write!(f, "{start}")?;
            } else {
                write!(f, "{start}-{channel}")?;
            }
            channel += 1;
        }
        Ok(())
    }
}

impl fmt::Debug for ChannelSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ChannelSet({self})")
    }
}

// ─── Country ────────────────────────────────────────────────────────────────

/// Why a country code was rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CountryError {
    /// A well-formed code the ESP-IDF Wi-Fi driver does not support
    /// (see [`SUPPORTED_COUNTRY_CODES`]); upper-cased.
    Unsupported([u8; 2]),
    /// Not two ASCII letters or digits.
    Malformed,
}

impl fmt::Display for CountryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported([a, b]) => write!(
                f,
                "country code {}{} is not supported by the Wi-Fi driver",
                char::from(*a),
                char::from(*b)
            ),
            Self::Malformed => f.write_str("country code must be two letters, e.g. DE"),
        }
    }
}

impl core::error::Error for CountryError {}

/// A Wi-Fi country: a supported two-letter code and the 802.11d policy.
///
/// The default is [`Country::WORLD`] (`"01"`, channels 1-11, 802.11d on) on both tiers.
/// [`Country::parse`] is a `const fn`, so a build-time setting fails the build when it is
/// not a supported code:
///
/// ```
/// use juggler::wifi::Country;
///
/// const COUNTRY: Country = match option_env!("WIFI_COUNTRY") {
///     Some(code) => match Country::parse(code) {
///         Ok(country) => country,
///         Err(_) => panic!("WIFI_COUNTRY is not a supported country code"),
///     },
///     None => Country::WORLD,
/// };
/// assert!(COUNTRY.channels().contains(1));
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Country {
    code: [u8; 2],
    ieee80211d: bool,
}

impl Default for Country {
    fn default() -> Self {
        Self::WORLD
    }
}

impl Country {
    /// The ESP-IDF world-safe mode `"01"`: channels 1-11, 802.11d on.
    ///
    /// The ESP-IDF factory default and the default of both tiers.
    pub const WORLD: Self = Self {
        code: *b"01",
        ieee80211d: true,
    };

    /// The country for `code` (case-insensitive), with 802.11d on.
    ///
    /// # Errors
    ///
    /// [`CountryError::Malformed`] unless `code` is two ASCII letters or digits,
    /// [`CountryError::Unsupported`] for a code outside [`SUPPORTED_COUNTRY_CODES`].
    pub const fn new(code: [u8; 2]) -> Result<Self, CountryError> {
        let code = [code[0].to_ascii_uppercase(), code[1].to_ascii_uppercase()];
        if !code[0].is_ascii_alphanumeric() || !code[1].is_ascii_alphanumeric() {
            return Err(CountryError::Malformed);
        }
        let mut i = 0;
        while i < SUPPORTED_COUNTRY_CODES.len() {
            let known = SUPPORTED_COUNTRY_CODES[i];
            if known[0] == code[0] && known[1] == code[1] {
                return Ok(Self {
                    code,
                    ieee80211d: true,
                });
            }
            i += 1;
        }
        Err(CountryError::Unsupported(code))
    }

    /// Parses a two-letter code such as `"DE"` or `"de"`, with 802.11d on.
    ///
    /// # Errors
    ///
    /// As [`new`](Self::new); [`CountryError::Malformed`] for a string that is not two bytes.
    pub const fn parse(code: &str) -> Result<Self, CountryError> {
        match code.as_bytes() {
            [a, b] => Self::new([*a, *b]),
            _ => Err(CountryError::Malformed),
        }
    }

    /// Sets the 802.11d policy.
    ///
    /// `true` (the default): an associated station adopts its AP's country.
    /// `false`: the configured country holds even while associated.
    #[must_use]
    pub const fn with_ieee80211d(mut self, enabled: bool) -> Self {
        self.ieee80211d = enabled;
        self
    }

    /// The upper-case two-letter code.
    pub const fn code(&self) -> [u8; 2] {
        self.code
    }

    /// The code as a string, e.g. `"DE"`.
    pub fn as_str(&self) -> &str {
        // Validated ASCII on construction.
        core::str::from_utf8(&self.code).unwrap_or("??")
    }

    /// Whether 802.11d is on.
    pub const fn ieee80211d(&self) -> bool {
        self.ieee80211d
    }

    /// The channels the country allows, from the built-in table.
    ///
    /// `01`, `US`, `CA` and `TW` allow 1-11, `JP` 1-14, every other supported code 1-13.
    /// The radio's read-back ([`Regulatory::channels`]) is authoritative;
    /// [`apply_country`] warns when the two differ.
    pub const fn channels(&self) -> ChannelSet {
        match self.code {
            [b'0', b'1'] | [b'U', b'S'] | [b'C', b'A'] | [b'T', b'W'] => {
                ChannelSet::from_range(1, 11)
            }
            [b'J', b'P'] => ChannelSet::from_range(1, 14),
            _ => ChannelSet::from_range(1, 13),
        }
    }

    /// The code as a NUL-terminated C string, as `esp_wifi_set_country_code` takes it.
    pub const fn c_code(&self) -> [u8; 3] {
        [self.code[0], self.code[1], 0]
    }
}

impl fmt::Debug for Country {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Country")
            .field("code", &self.as_str())
            .field("ieee80211d", &self.ieee80211d)
            .finish()
    }
}

impl fmt::Display for Country {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

// ─── Regulatory ─────────────────────────────────────────────────────────────

/// The country state the radio reports (`esp_wifi_get_country` on both tiers).
///
/// `Display` gives the shared text, e.g. `01, channels 1-11, 802.11d on`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Regulatory {
    /// The two-letter code the radio reports.
    pub code: [u8; 2],
    /// The channels the radio allows.
    pub channels: ChannelSet,
    /// Whether the radio's policy is automatic (802.11d on).
    pub ieee80211d: bool,
}

impl Regulatory {
    /// Builds the read-back from the fields of the radio's `wifi_country_t`: the code
    /// (`cc`, third byte ignored), first channel, channel count and whether the policy is
    /// `WIFI_COUNTRY_POLICY_AUTO`.
    pub const fn from_raw(cc: [u8; 3], schan: u8, nchan: u8, policy_auto: bool) -> Self {
        Self {
            code: [cc[0], cc[1]],
            channels: ChannelSet::from_range(schan, nchan),
            ieee80211d: policy_auto,
        }
    }

    /// Whether the radio reports `country`'s code and 802.11d policy.
    pub const fn matches(&self, country: &Country) -> bool {
        self.code[0] == country.code[0]
            && self.code[1] == country.code[1]
            && self.ieee80211d == country.ieee80211d
    }
}

impl fmt::Display for Regulatory {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.code {
            let c = if byte.is_ascii_graphic() {
                char::from(byte)
            } else {
                '?'
            };
            write!(f, "{c}")?;
        }
        write!(
            f,
            ", channels {}, 802.11d {}",
            self.channels,
            if self.ieee80211d { "on" } else { "off" }
        )
    }
}

// ─── Apply ──────────────────────────────────────────────────────────────────

/// The radio calls [`apply_country`] needs; each tier implements it over its FFI.
pub trait CountryPort {
    /// The driver error.
    type Error;

    /// Sets the country: `esp_wifi_set_country_code(cc, ieee80211d)`.
    ///
    /// `cc` is NUL-terminated ([`Country::c_code`]).
    fn set_country_code(&mut self, cc: &[u8; 3], ieee80211d: bool) -> Result<(), Self::Error>;

    /// Reads the radio's country: `esp_wifi_get_country`.
    fn country(&mut self) -> Result<Regulatory, Self::Error>;

    /// Whether the station is associated, so that with 802.11d the radio may report the access
    /// point's country instead of the configured one.
    ///
    /// [`apply_country`] never skips the write while this is `true`.
    /// A port that cannot tell returns `true`.
    fn station_associated(&mut self) -> bool;
}

/// Applies `country` to the radio behind `port` and returns what the radio reports.
///
/// Reads first and skips the write when the radio already reports the requested code and
/// 802.11d policy and the station is not associated, since ESP-IDF stores the country in flash
/// on every write.
/// While associated the radio may report the country adopted from the access point through
/// 802.11d, not the configured one, so the write is never skipped then.
/// A failed first read does not stop the write.
/// After a write the radio is read back; a code or channel set that differs from the
/// request is logged as a warning and returned as the radio reports it, since the radio's
/// view decides which channels work.
/// Logs one shared line, e.g. `[wifi] country 01, channels 1-11, 802.11d on`.
///
/// # Errors
///
/// The error of the write or of the read-back.
pub fn apply_country<P: CountryPort>(
    port: &mut P,
    country: Country,
) -> Result<Regulatory, P::Error> {
    let applied = match port.country() {
        Ok(current) if current.matches(&country) && !port.station_associated() => current,
        current => {
            if current.is_err() {
                log::debug!("[wifi] country read failed before the write; writing {country}");
            }
            port.set_country_code(&country.c_code(), country.ieee80211d())?;
            port.country()?
        }
    };
    if applied.code != country.code() {
        log::warn!("[wifi] country {country} requested, but the radio reports {applied}");
    } else if applied.channels != country.channels() {
        log::warn!(
            "[wifi] country {country}: the radio allows channels {}, the table expects {}",
            applied.channels,
            country.channels()
        );
    }
    log::info!("{}", CountryApplied { applied });
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;
    use alloc::vec::Vec;

    #[test]
    fn every_supported_code_parses() {
        for code in SUPPORTED_COUNTRY_CODES {
            let s = core::str::from_utf8(&code).unwrap();
            let country = Country::parse(s).unwrap();
            assert_eq!(country.code(), code);
            assert_eq!(country.as_str(), s);
            assert!(country.ieee80211d());
        }
        assert_eq!(SUPPORTED_COUNTRY_CODES.len(), 45);
    }

    #[test]
    fn lowercase_is_accepted() {
        assert_eq!(Country::parse("de").unwrap().code(), *b"DE");
        assert_eq!(Country::parse("jP").unwrap().as_str(), "JP");
        assert_eq!(Country::new(*b"us").unwrap().code(), *b"US");
    }

    #[test]
    fn unsupported_and_malformed_are_rejected() {
        assert_eq!(Country::parse("ZZ"), Err(CountryError::Unsupported(*b"ZZ")));
        assert_eq!(Country::parse("zz"), Err(CountryError::Unsupported(*b"ZZ")));
        assert_eq!(Country::parse(""), Err(CountryError::Malformed));
        assert_eq!(Country::parse("D"), Err(CountryError::Malformed));
        assert_eq!(Country::parse("DEU"), Err(CountryError::Malformed));
        assert_eq!(Country::parse("D-"), Err(CountryError::Malformed));
        assert_eq!(Country::parse("ü"), Err(CountryError::Malformed));
        assert_eq!(Country::new([0, 0]), Err(CountryError::Malformed));
    }

    #[test]
    fn world_is_the_default() {
        assert_eq!(Country::default(), Country::WORLD);
        assert_eq!(Country::WORLD.code(), *b"01");
        assert!(Country::WORLD.ieee80211d());
        assert_eq!(Country::parse("01"), Ok(Country::WORLD));
    }

    #[test]
    fn ieee80211d_can_be_turned_off() {
        let country = Country::parse("DE").unwrap().with_ieee80211d(false);
        assert!(!country.ieee80211d());
        assert_eq!(country.code(), *b"DE");
    }

    #[test]
    fn c_code_is_nul_terminated() {
        assert_eq!(Country::WORLD.c_code(), *b"01\0");
        assert_eq!(Country::parse("de").unwrap().c_code(), *b"DE\0");
    }

    #[test]
    fn parse_evaluates_in_const_context() {
        const DE: Country = match Country::parse("DE") {
            Ok(country) => country,
            Err(_) => panic!("DE is supported"),
        };
        const BAD: Result<Country, CountryError> = Country::parse("XX");
        const DE_CHANNELS: ChannelSet = DE.channels();
        assert_eq!(DE.code(), *b"DE");
        assert_eq!(BAD, Err(CountryError::Unsupported(*b"XX")));
        assert_eq!(DE_CHANNELS, ChannelSet::from_range(1, 13));
    }

    #[test]
    fn channel_table() {
        let channels = |code: &str| Country::parse(code).unwrap().channels();
        let eleven = ChannelSet::from_range(1, 11);
        assert_eq!(channels("01"), eleven);
        assert_eq!(channels("US"), eleven);
        assert_eq!(channels("CA"), eleven);
        assert_eq!(channels("TW"), eleven);
        assert_eq!(channels("DE"), ChannelSet::from_range(1, 13));
        assert_eq!(channels("CN"), ChannelSet::from_range(1, 13));
        assert_eq!(channels("JP"), ChannelSet::ALL);
        assert!(!Country::WORLD.channels().contains(12));
        assert!(channels("DE").contains(13));
        assert!(!channels("DE").contains(14));
        assert_eq!(format!("{}", Country::WORLD.channels()), "1-11");
    }

    #[test]
    fn channel_set_boundaries() {
        assert_eq!(ChannelSet::from_range(0, 0), ChannelSet::EMPTY);
        assert_eq!(ChannelSet::from_range(1, 0), ChannelSet::EMPTY);
        assert_eq!(ChannelSet::from_range(0, 3), ChannelSet::from_range(1, 2));
        assert_eq!(ChannelSet::from_range(1, 14), ChannelSet::ALL);
        assert_eq!(ChannelSet::from_range(1, 255), ChannelSet::ALL);
        assert_eq!(ChannelSet::from_range(255, 255), ChannelSet::EMPTY);
        assert_eq!(ChannelSet::from_range(13, 5), ChannelSet::from_range(13, 2));
        assert_eq!(ChannelSet::from_range(15, 1), ChannelSet::EMPTY);
        assert!(!ChannelSet::ALL.contains(0));
        assert!(!ChannelSet::ALL.contains(15));
        assert!(ChannelSet::ALL.contains(1));
        assert!(ChannelSet::ALL.contains(14));
        assert_eq!(ChannelSet::ALL.len(), 14);
        assert!(ChannelSet::EMPTY.is_empty());
        assert_eq!(ChannelSet::from_bits(u16::MAX), ChannelSet::ALL);
        assert_eq!(
            ChannelSet::from_bits(ChannelSet::from_range(1, 11).bits()),
            ChannelSet::from_range(1, 11)
        );
        assert_eq!(ChannelSet::EMPTY.with(0).with(15), ChannelSet::EMPTY);
        let set = ChannelSet::EMPTY
            .with(1)
            .with(2)
            .with(3)
            .with(6)
            .with(9)
            .with(10);
        assert_eq!(set.iter().collect::<Vec<_>>(), [1, 2, 3, 6, 9, 10]);
        assert_eq!(format!("{set}"), "1-3, 6, 9-10");
        assert_eq!(format!("{}", ChannelSet::EMPTY), "none");
        assert_eq!(format!("{}", ChannelSet::ALL), "1-14");
        assert_eq!(format!("{}", ChannelSet::EMPTY.with(14)), "14");
    }

    #[test]
    fn regulatory_from_raw_and_display() {
        let r = Regulatory::from_raw(*b"DE ", 1, 13, true);
        assert_eq!(r.code, *b"DE");
        assert_eq!(r.channels, ChannelSet::from_range(1, 13));
        assert_eq!(format!("{r}"), "DE, channels 1-13, 802.11d on");
        let r = Regulatory::from_raw([b'0', b'1', 0], 1, 11, false);
        assert_eq!(format!("{r}"), "01, channels 1-11, 802.11d off");
        let junk = Regulatory::from_raw([0, 0xff, 0], 0, 0, false);
        assert_eq!(format!("{junk}"), "??, channels none, 802.11d off");
    }

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        Read,
        Write([u8; 3], bool),
    }

    /// Records every call; answers reads from `state`.
    struct MockPort {
        calls: Vec<Call>,
        state: Regulatory,
        fail_first_read: bool,
        fail_write: bool,
        associated: bool,
        /// What the radio reports after a write, `None` = what was written (by the table).
        after_write: Option<Regulatory>,
    }

    impl MockPort {
        fn with(state: Regulatory) -> Self {
            Self {
                calls: Vec::new(),
                state,
                fail_first_read: false,
                fail_write: false,
                associated: false,
                after_write: None,
            }
        }

        fn writes(&self) -> usize {
            self.calls
                .iter()
                .filter(|c| matches!(c, Call::Write(..)))
                .count()
        }
    }

    impl CountryPort for MockPort {
        type Error = &'static str;

        fn set_country_code(&mut self, cc: &[u8; 3], ieee80211d: bool) -> Result<(), Self::Error> {
            self.calls.push(Call::Write(*cc, ieee80211d));
            if self.fail_write {
                return Err("write");
            }
            self.state = self.after_write.unwrap_or_else(|| {
                let country = Country::new([cc[0], cc[1]]).unwrap();
                Regulatory {
                    code: country.code(),
                    channels: country.channels(),
                    ieee80211d,
                }
            });
            Ok(())
        }

        fn country(&mut self) -> Result<Regulatory, Self::Error> {
            let first = self.calls.is_empty();
            self.calls.push(Call::Read);
            if first && self.fail_first_read {
                return Err("read");
            }
            Ok(self.state)
        }

        fn station_associated(&mut self) -> bool {
            self.associated
        }
    }

    fn world() -> Regulatory {
        Regulatory::from_raw(*b"01\0", 1, 11, true)
    }

    #[test]
    fn apply_skips_the_write_when_code_and_policy_match() {
        let mut port = MockPort::with(world());
        assert_eq!(apply_country(&mut port, Country::WORLD), Ok(world()));
        assert_eq!(port.calls, [Call::Read]);
    }

    #[test]
    fn apply_writes_while_associated_although_the_adopted_code_matches() {
        let de = Country::parse("DE").unwrap();
        let mut port = MockPort::with(Regulatory::from_raw(*b"DE\0", 1, 13, true));
        port.associated = true;
        assert_eq!(apply_country(&mut port, de).unwrap().code, *b"DE");
        assert_eq!(
            port.calls,
            [Call::Read, Call::Write(*b"DE\0", true), Call::Read],
            "the radio may report the access point's country, so the write is never skipped"
        );
    }

    #[test]
    fn apply_writes_when_the_code_differs() {
        let mut port = MockPort::with(world());
        let de = Country::parse("DE").unwrap();
        let applied = apply_country(&mut port, de).unwrap();
        assert_eq!(
            port.calls,
            [Call::Read, Call::Write(*b"DE\0", true), Call::Read]
        );
        assert_eq!(applied.code, *b"DE");
        assert_eq!(applied.channels, ChannelSet::from_range(1, 13));
    }

    #[test]
    fn apply_writes_when_only_the_policy_differs() {
        let mut port = MockPort::with(world());
        apply_country(&mut port, Country::WORLD.with_ieee80211d(false)).unwrap();
        assert_eq!(port.writes(), 1);
        assert_eq!(port.calls[1], Call::Write(*b"01\0", false));
    }

    #[test]
    fn a_failed_first_read_still_writes() {
        let mut port = MockPort::with(world());
        port.fail_first_read = true;
        assert_eq!(apply_country(&mut port, Country::WORLD), Ok(world()));
        assert_eq!(
            port.calls,
            [Call::Read, Call::Write(*b"01\0", true), Call::Read]
        );
    }

    #[test]
    fn a_failed_write_is_returned() {
        let mut port = MockPort::with(world());
        port.fail_write = true;
        assert_eq!(
            apply_country(&mut port, Country::parse("DE").unwrap()),
            Err("write")
        );
        assert_eq!(port.calls, [Call::Read, Call::Write(*b"DE\0", true)]);
    }

    #[test]
    fn a_differing_read_back_is_returned() {
        let mut port = MockPort::with(world());
        let radio = Regulatory::from_raw(*b"DE\0", 1, 11, true);
        port.after_write = Some(radio);
        assert_eq!(
            apply_country(&mut port, Country::parse("DE").unwrap()),
            Ok(radio),
            "the radio's channels win over the table"
        );
    }

    #[test]
    fn country_applied_line_is_pinned() {
        assert_eq!(
            format!("{}", CountryApplied { applied: world() }),
            "[wifi] country 01, channels 1-11, 802.11d on"
        );
    }
}
