//! Firmware version parsing and comparison.

use core::cmp::Ordering;
use core::fmt;

use crate::OtaError;

/// Experimental: API may change before 1.0.
///
/// A firmware version following the `MAJOR.MINOR.PATCH` semver convention.
/// Each component is a `u16` (0–65535).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Version {
    /// Major version component.
    pub major: u16,
    /// Minor version component.
    pub minor: u16,
    /// Patch version component.
    pub patch: u16,
}

impl Version {
    /// Experimental: API may change before 1.0.
    ///
    /// Create a `Version` from its three numeric components.
    pub const fn new(major: u16, minor: u16, patch: u16) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Parse a version string of the form `"MAJOR.MINOR.PATCH"`.
    /// Leading/trailing whitespace is trimmed before parsing.
    ///
    /// Returns `Err(OtaError::VersionInvalid)` for any malformed input.
    pub fn parse(s: &str) -> Result<Self, OtaError> {
        let s = s.trim();

        // Split into at most 4 parts so we can reject "1.2.3.4" cheaply.
        let mut parts = s.splitn(4, '.');
        let major_str = parts.next().unwrap_or("");
        let minor_str = parts.next().ok_or(OtaError::VersionInvalid)?;
        let patch_str = parts.next().ok_or(OtaError::VersionInvalid)?;

        // A fourth segment means too many dots.
        if parts.next().is_some() {
            return Err(OtaError::VersionInvalid);
        }

        // Empty string edge case: major_str is "" after trim.
        if major_str.is_empty() {
            return Err(OtaError::VersionInvalid);
        }

        let major = major_str
            .parse::<u16>()
            .map_err(|_| OtaError::VersionInvalid)?;
        let minor = minor_str
            .parse::<u16>()
            .map_err(|_| OtaError::VersionInvalid)?;
        let patch = patch_str
            .parse::<u16>()
            .map_err(|_| OtaError::VersionInvalid)?;

        Ok(Self {
            major,
            minor,
            patch,
        })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        match self.major.cmp(&other.major) {
            Ordering::Equal => match self.minor.cmp(&other.minor) {
                Ordering::Equal => self.patch.cmp(&other.patch),
                other => other,
            },
            other => other,
        }
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- Version parsing ---

    #[test]
    fn parse_valid_version() {
        let v = Version::parse("1.2.3").unwrap();
        assert_eq!(v, Version::new(1, 2, 3));
    }

    #[test]
    fn parse_version_with_whitespace() {
        let v = Version::parse("  0.1.8\n").unwrap();
        assert_eq!(v, Version::new(0, 1, 8));
    }

    #[test]
    fn parse_zero_version() {
        let v = Version::parse("0.0.0").unwrap();
        assert_eq!(v, Version::new(0, 0, 0));
    }

    #[test]
    fn parse_max_version() {
        let v = Version::parse("65535.65535.65535").unwrap();
        assert_eq!(v, Version::new(65535, 65535, 65535));
    }

    #[test]
    fn parse_three_digit_minor() {
        // u8 fields would have rejected this; u16 fields accept it.
        let v = Version::parse("1.300.0").unwrap();
        assert_eq!(v, Version::new(1, 300, 0));
    }

    #[test]
    fn parse_invalid_format_two_parts() {
        assert_eq!(Version::parse("1.2"), Err(OtaError::VersionInvalid));
    }

    #[test]
    fn parse_invalid_format_four_parts() {
        assert_eq!(Version::parse("1.2.3.4"), Err(OtaError::VersionInvalid));
    }

    #[test]
    fn parse_invalid_non_numeric() {
        assert_eq!(Version::parse("a.b.c"), Err(OtaError::VersionInvalid));
    }

    #[test]
    fn parse_invalid_overflow() {
        // 65536 > u16::MAX, must be rejected.
        assert_eq!(Version::parse("65536.0.0"), Err(OtaError::VersionInvalid));
    }

    #[test]
    fn parse_empty_string() {
        assert_eq!(Version::parse(""), Err(OtaError::VersionInvalid));
    }

    // --- Version comparison ---

    #[test]
    fn version_equal() {
        let a = Version::new(0, 1, 8);
        let b = Version::new(0, 1, 8);
        assert_eq!(a.cmp(&b), Ordering::Equal);
        assert!(a >= b);
    }

    #[test]
    fn version_major_greater() {
        let running = Version::new(1, 0, 0);
        let remote = Version::new(0, 9, 9);
        assert!(running > remote);
    }

    #[test]
    fn version_minor_greater() {
        let running = Version::new(0, 2, 0);
        let remote = Version::new(0, 1, 9);
        assert!(running > remote);
    }

    #[test]
    fn version_patch_greater() {
        let running = Version::new(0, 1, 9);
        let remote = Version::new(0, 1, 8);
        assert!(running > remote);
    }

    #[test]
    fn version_already_up_to_date() {
        let running = Version::new(0, 1, 8);
        let remote = Version::new(0, 1, 8);
        assert!(running >= remote, "same version = already up to date");

        let running = Version::new(0, 2, 0);
        let remote = Version::new(0, 1, 8);
        assert!(running >= remote, "newer running = already up to date");
    }

    #[test]
    fn version_update_available() {
        let running = Version::new(0, 1, 8);
        let remote = Version::new(0, 1, 9);
        assert!(running < remote);

        let running = Version::new(0, 1, 8);
        let remote = Version::new(0, 2, 0);
        assert!(running < remote);

        let running = Version::new(0, 1, 8);
        let remote = Version::new(1, 0, 0);
        assert!(running < remote);
    }

    #[test]
    fn version_display() {
        use core::fmt::Write as _;
        let mut buf = heapless::String::<16>::new();
        write!(buf, "{}", Version::new(0, 1, 8)).unwrap();
        assert_eq!(buf.as_str(), "0.1.8");

        buf.clear();
        write!(buf, "{}", Version::new(1, 0, 0)).unwrap();
        assert_eq!(buf.as_str(), "1.0.0");
    }
}
