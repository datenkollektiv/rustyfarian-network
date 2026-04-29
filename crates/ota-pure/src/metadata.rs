//! Firmware image sidecar metadata parser.

use crate::error::OtaError;
use crate::verifier::hex_to_bytes;
use crate::version::Version;

/// Experimental: API may change before 1.0.
///
/// Sidecar metadata for a firmware image.
///
/// Parsed from the `.bin.sha256` (64-char hex digest) and `.bin.version`
/// (semver string) sidecar files that accompany each firmware image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageMetadata {
    /// Expected SHA-256 digest of the firmware image.
    pub sha256: [u8; 32],
    /// Declared firmware version.
    pub version: Version,
}

impl ImageMetadata {
    /// Experimental: API may change before 1.0.
    ///
    /// Parse metadata from raw sidecar strings.
    ///
    /// `sha256_hex` must be a 64-character lowercase (or uppercase) hex string.
    /// `version_str` must be a `"MAJOR.MINOR.PATCH"` semver string.
    /// Both inputs are trimmed of leading/trailing whitespace before parsing.
    ///
    /// Returns `Err(OtaError::ChecksumMismatch)` for a malformed digest, or
    /// `Err(OtaError::VersionInvalid)` for a malformed version string.
    pub fn parse(sha256_hex: &str, version_str: &str) -> Result<Self, OtaError> {
        let sha256 = hex_to_bytes(sha256_hex.trim())?;
        let version = Version::parse(version_str.trim())?;
        Ok(Self { sha256, version })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HELLO_HASH: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    #[test]
    fn parse_valid_metadata() {
        let m = ImageMetadata::parse(HELLO_HASH, "1.2.3").unwrap();
        assert_eq!(m.version, Version::new(1, 2, 3));
        assert_eq!(m.sha256[0], 0x2c);
    }

    #[test]
    fn parse_trims_whitespace() {
        // Build padded hex without heap allocation.
        use core::fmt::Write as _;
        let mut padded = heapless::String::<68>::new();
        write!(padded, "  {HELLO_HASH}  ").unwrap();
        let m = ImageMetadata::parse(padded.as_str(), "  0.1.8\n").unwrap();
        assert_eq!(m.version, Version::new(0, 1, 8));
    }

    #[test]
    fn parse_invalid_hash_returns_checksum_error() {
        let result = ImageMetadata::parse("not-a-hash", "1.0.0");
        assert_eq!(result, Err(OtaError::ChecksumMismatch));
    }

    #[test]
    fn parse_invalid_version_returns_version_error() {
        let result = ImageMetadata::parse(HELLO_HASH, "bad-version");
        assert_eq!(result, Err(OtaError::VersionInvalid));
    }
}
