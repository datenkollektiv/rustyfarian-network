//! The DDI configData body a device sends once per boot: the profile's `chip`, `board` and `version` as a merge.

use alloc::format;
use alloc::string::String;
use core::fmt;

use super::{MAX_BOARD_BYTES, MAX_CHIP_BYTES};
use crate::ota::Version;

/// `text` is 1 to `max` bytes of `[a-z0-9]`, plus `.`, `_` and `-` when `punctuation` is set.
const fn is_token(text: &str, max: usize, punctuation: bool) -> bool {
    let bytes = text.as_bytes();
    if bytes.is_empty() || bytes.len() > max {
        return false;
    }
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let allowed = b.is_ascii_lowercase()
            || b.is_ascii_digit()
            || (punctuation && matches!(b, b'.' | b'_' | b'-'));
        if !allowed {
            return false;
        }
        i += 1;
    }
    true
}

/// Experimental: API may change before 1.0.
///
/// A chip name in the profile grammar `[a-z0-9]{1,32}`: the configData `chip` key and the value of the `chip` metadata entry.
///
/// # Examples
///
/// ```
/// use juggler::ota::ddi::wire::{Chip, ConfigDataError};
///
/// assert_eq!(Chip::parse("esp32c3").unwrap().as_str(), "esp32c3");
/// assert_eq!(Chip::parse("ESP32C3"), Err(ConfigDataError::InvalidChip));
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chip(String);

impl Chip {
    /// Whether `text` matches `[a-z0-9]{1,32}`.
    pub const fn is_valid(text: &str) -> bool {
        is_token(text, MAX_CHIP_BYTES, false)
    }

    /// Validates `text` as a chip name; it is refused, never truncated.
    ///
    /// # Errors
    ///
    /// [`ConfigDataError::InvalidChip`] unless `text` matches `[a-z0-9]{1,32}`.
    pub fn parse(text: &str) -> Result<Self, ConfigDataError> {
        if !Self::is_valid(text) {
            return Err(ConfigDataError::InvalidChip);
        }
        Ok(Chip(String::from(text)))
    }

    /// The chip name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Experimental: API may change before 1.0.
///
/// A board name in the profile grammar `[a-z0-9._-]{1,32}`, fixed when the image is built and never provisioned.
///
/// A firmware checks its build-time constant at start-up with [`parse`](Board::parse), or at compile time with the `const` [`is_valid`](Board::is_valid).
///
/// # Examples
///
/// ```
/// use juggler::ota::ddi::wire::Board;
///
/// const BOARD: &str = "c3-supermini";
/// const _: () = assert!(Board::is_valid(BOARD));
///
/// let board = Board::parse(BOARD).expect("BOARD matches [a-z0-9._-]{1,32}");
/// assert_eq!(board.as_str(), "c3-supermini");
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Board(String);

impl Board {
    /// Whether `text` matches `[a-z0-9._-]{1,32}`.
    pub const fn is_valid(text: &str) -> bool {
        is_token(text, MAX_BOARD_BYTES, true)
    }

    /// Validates `text` as a board name; it is refused, never truncated.
    ///
    /// # Errors
    ///
    /// [`ConfigDataError::InvalidBoard`] unless `text` matches `[a-z0-9._-]{1,32}`.
    pub fn parse(text: &str) -> Result<Self, ConfigDataError> {
        if !Self::is_valid(text) {
            return Err(ConfigDataError::InvalidBoard);
        }
        Ok(Board(String::from(text)))
    }

    /// The board name.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Experimental: API may change before 1.0.
///
/// Why a configData input was refused.
/// Carries no input text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigDataError {
    /// The chip does not match `[a-z0-9]{1,32}`.
    InvalidChip,
    /// The board does not match `[a-z0-9._-]{1,32}`.
    InvalidBoard,
}

impl fmt::Display for ConfigDataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigDataError::InvalidChip => f.write_str("DDI configData chip invalid"),
            ConfigDataError::InvalidBoard => f.write_str("DDI configData board invalid"),
        }
    }
}

/// Experimental: API may change before 1.0.
///
/// The configData PUT body (profile §3b): `mode` `merge` and exactly the keys `chip`, `board` and `version`.
/// Every input is checked when it is made, so encoding cannot fail.
///
/// # Examples
///
/// ```
/// use juggler::ota::ddi::wire::{Board, Chip, ConfigData};
/// use juggler::ota::Version;
///
/// let chip = Chip::parse("esp32c3").unwrap();
/// let board = Board::parse("c3-supermini").unwrap();
/// let body = ConfigData { chip: &chip, board: &board, version: Version::new(1, 2, 3) }.encode();
/// assert_eq!(
///     body,
///     r#"{"mode":"merge","data":{"chip":"esp32c3","board":"c3-supermini","version":"1.2.3"}}"#
/// );
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigData<'a> {
    /// `data.chip`.
    pub chip: &'a Chip,
    /// `data.board`.
    pub board: &'a Board,
    /// `data.version`, written in the strict `MAJOR.MINOR.PATCH` form.
    pub version: Version,
}

impl ConfigData<'_> {
    /// Encodes the body as compact JSON in the fixed order `mode`, then `data` with `chip`, `board` and `version`; no trailing newline.
    pub fn encode(&self) -> String {
        // The chip and board grammars and the version digits contain nothing JSON escapes.
        format!(
            r#"{{"mode":"merge","data":{{"chip":"{}","board":"{}","version":"{}"}}}}"#,
            self.chip.as_str(),
            self.board.as_str(),
            self.version
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn body(chip: &str, board: &str, version: Version) -> String {
        ConfigData {
            chip: &Chip::parse(chip).unwrap(),
            board: &Board::parse(board).unwrap(),
            version,
        }
        .encode()
    }

    #[test]
    fn golden_body() {
        assert_eq!(
            body("esp32c3", "c3-supermini", Version::new(1, 2, 3)),
            r#"{"mode":"merge","data":{"chip":"esp32c3","board":"c3-supermini","version":"1.2.3"}}"#
        );
    }

    #[test]
    fn body_is_json_with_exactly_the_three_keys() {
        let text = body("esp32s3", "heltec.v3_lora-32", Version::new(65535, 0, 10));
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        let root = v.as_object().unwrap();
        assert_eq!(root.len(), 2);
        assert_eq!(root["mode"], "merge");
        let data = root["data"].as_object().unwrap();
        assert_eq!(data.len(), 3);
        assert_eq!(data["chip"], "esp32s3");
        assert_eq!(data["board"], "heltec.v3_lora-32");
        assert_eq!(data["version"], "65535.0.10");
        assert_eq!(
            Version::parse(data["version"].as_str().unwrap()),
            Ok(Version::new(65535, 0, 10))
        );
    }

    #[test]
    fn largest_body_is_small() {
        let chip = "a".repeat(MAX_CHIP_BYTES);
        let board = "b".repeat(MAX_BOARD_BYTES);
        let text = body(&chip, &board, Version::new(65535, 65535, 65535));
        assert!(text.len() < 160, "{}", text.len());
    }

    #[test]
    fn chip_grammar() {
        for ok in ["esp32", "esp32c3", "0", "a", &"z".repeat(MAX_CHIP_BYTES)] {
            assert!(Chip::is_valid(ok), "{ok:?}");
            assert_eq!(Chip::parse(ok).unwrap().as_str(), ok);
        }
        for bad in [
            "",
            &"z".repeat(MAX_CHIP_BYTES + 1),
            "ESP32",
            "esp32C3",
            "esp32-c3",
            "esp32.c3",
            "esp32_c3",
            " esp32",
            "esp32 ",
            "esp\"32",
            "esp\\32",
            "esp32\n",
            "\u{e9}sp32",
        ] {
            assert!(!Chip::is_valid(bad), "{bad:?}");
            assert_eq!(
                Chip::parse(bad),
                Err(ConfigDataError::InvalidChip),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn board_grammar() {
        for ok in [
            "c3-supermini",
            "heltec.v3",
            "a_b",
            ".",
            "-",
            "_",
            &"z".repeat(MAX_BOARD_BYTES),
        ] {
            assert!(Board::is_valid(ok), "{ok:?}");
            assert_eq!(Board::parse(ok).unwrap().as_str(), ok);
        }
        for bad in [
            "",
            &"z".repeat(MAX_BOARD_BYTES + 1),
            "C3-SuperMini",
            "c3 supermini",
            "c3/supermini",
            "c3+mini",
            "c3\"mini",
            "c3\\mini",
            "c3\u{0}",
            "c3\u{e9}",
        ] {
            assert!(!Board::is_valid(bad), "{bad:?}");
            assert_eq!(
                Board::parse(bad),
                Err(ConfigDataError::InvalidBoard),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn grammar_counts_bytes_not_chars() {
        assert!(!Chip::is_valid(&"\u{e9}".repeat(MAX_CHIP_BYTES / 2)));
        assert!(!Board::is_valid(&"\u{e9}".repeat(MAX_BOARD_BYTES / 2)));
    }

    #[test]
    fn errors_are_fixed_text_without_input() {
        assert_eq!(
            format!("{}", ConfigDataError::InvalidChip),
            "DDI configData chip invalid"
        );
        assert_eq!(
            format!("{}", ConfigDataError::InvalidBoard),
            "DDI configData board invalid"
        );
    }
}
