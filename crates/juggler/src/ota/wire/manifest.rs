//! Manifest: the small JSON document that names a firmware image, its digest and its target chip.

use alloc::string::String;
use core::fmt;

use serde::Deserialize;

use super::first_token_position;
use super::reason::FailReason;
use super::url::is_plain_http_url;
use crate::ota::{ImageMetadata, OtaError};

/// Experimental: API may change before 1.0.
///
/// Largest accepted manifest body in bytes.
/// A larger body is rejected before any parsing.
pub const MAX_MANIFEST_BYTES: usize = 1024;

/// Experimental: API may change before 1.0.
///
/// An OTA manifest; all four fields are required and unknown fields are rejected.
/// [`parse`](Manifest::parse) validates only `url` (a plain `http://` URL with a non-empty host), so a parsed manifest always has a usable download URL;
/// use [`check_target`](Manifest::check_target) and [`image_metadata`](Manifest::image_metadata) for the other fields.
/// The fields are public, so a manifest built by hand is not covered by that guarantee.
///
/// `Debug` redacts `url` because it may carry `user:password@`.
///
/// # Examples
///
/// ```
/// use juggler::ota::Manifest;
///
/// let body = br#"{"version":"1.2.3","sha256":"2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824","url":"http://h/fw.bin","target":"esp32c3"}"#;
/// let manifest = Manifest::parse(body).unwrap();
/// assert!(manifest.check_target("esp32c3").is_ok());
/// assert!(manifest.check_target("esp32c6").is_err());
/// assert!(manifest.image_metadata().is_ok());
/// ```
#[derive(Clone, PartialEq, Eq)]
pub struct Manifest {
    /// Firmware version, `major.minor.patch`.
    pub version: String,
    /// Lower- or upper-case hex SHA-256 of the image, 64 characters.
    pub sha256: String,
    /// Where to download the image.
    pub url: String,
    /// The chip the image was built for, for example `esp32c3`.
    pub target: String,
}

/// Private deserialization target for [`Manifest::parse`]; keeps `Manifest` itself non-`Deserialize` so the size limit, array guard and URL check cannot be bypassed.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    version: String,
    sha256: String,
    url: String,
    target: String,
}

impl fmt::Debug for Manifest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Manifest")
            .field("version", &self.version)
            .field("sha256", &self.sha256)
            .field("url", &"<redacted>")
            .field("target", &self.target)
            .finish()
    }
}

/// Experimental: API may change before 1.0.
///
/// Why a manifest was rejected.
/// Carries no payload text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManifestError {
    /// The body exceeds [`MAX_MANIFEST_BYTES`].
    TooLarge,
    /// Not UTF-8 / JSON, a field missing, `null`, of the wrong type, duplicated or unknown.
    ///
    /// `line` and `column` (both 1-based) come from the JSON parser and locate the problem; they are numbers only.
    /// When the body is rejected early for not starting with `{`, they point at its first non-whitespace byte, or 1/1 if there is none.
    Malformed {
        /// 1-based line of the problem.
        line: usize,
        /// 1-based column of the problem.
        column: usize,
    },
    /// The manifest targets a different chip: an offer rejection, not a download failure.
    TargetMismatch,
    /// `version` does not parse (for example a prerelease version).
    InvalidVersion,
    /// `sha256` is not a 64-character hex digest.
    InvalidDigest,
    /// `url` is empty, whitespace-only, not `http://`, or has no host.
    ///
    /// Both tiers download over plain HTTP only (ADR 011), so such a URL is rejected when the manifest is parsed instead of failing later as `server_unreachable`.
    InvalidUrl,
}

impl ManifestError {
    /// The status reason: `target_mismatch` for a chip mismatch, otherwise `manifest_invalid`.
    pub const fn reason(&self) -> FailReason {
        match self {
            ManifestError::TargetMismatch => FailReason::TargetMismatch,
            ManifestError::TooLarge
            | ManifestError::Malformed { .. }
            | ManifestError::InvalidVersion
            | ManifestError::InvalidDigest
            | ManifestError::InvalidUrl => FailReason::ManifestInvalid,
        }
    }
}

impl fmt::Display for ManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ManifestError::TooLarge => f.write_str("OTA manifest too large"),
            ManifestError::Malformed { line, column } => {
                write!(f, "OTA manifest malformed at line {line} column {column}")
            }
            ManifestError::TargetMismatch => f.write_str("OTA manifest targets a different chip"),
            ManifestError::InvalidVersion => f.write_str("OTA manifest version invalid"),
            ManifestError::InvalidDigest => f.write_str("OTA manifest digest invalid"),
            ManifestError::InvalidUrl => f.write_str("OTA manifest url invalid"),
        }
    }
}

impl Manifest {
    /// Parses a manifest body and checks that `url` is a plain `http://` URL with a non-empty host.
    ///
    /// The URL is checked here, not in a later step, so a `Manifest` returned by `parse` always has a usable URL.
    /// Both tiers download over plain HTTP only (ADR 011); an `https://` or empty URL would otherwise surface much later as `server_unreachable` or `manifest_fetch`.
    ///
    /// Because the URL is validated here, a manifest with a bad URL reports `manifest_invalid` even when its `target` is also wrong; the URL check runs before [`check_target`](Manifest::check_target).
    ///
    /// # Errors
    ///
    /// [`ManifestError::TooLarge`] for more than [`MAX_MANIFEST_BYTES`] bytes (checked before parsing),
    /// [`ManifestError::Malformed`] (with line and column) for anything that is not exactly the four required string fields,
    /// [`ManifestError::InvalidUrl`] when `url` fails the check (reason `manifest_invalid`).
    pub fn parse(body: &[u8]) -> Result<Self, ManifestError> {
        if body.len() > MAX_MANIFEST_BYTES {
            return Err(ManifestError::TooLarge);
        }
        // serde's derived struct deserializer also accepts the positional array form.
        if body.iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'{') {
            let (line, column) = first_token_position(body);
            return Err(ManifestError::Malformed { line, column });
        }
        let raw: RawManifest =
            serde_json::from_slice(body).map_err(|e| ManifestError::Malformed {
                line: e.line(),
                column: e.column().max(1),
            })?;
        let manifest = Manifest {
            version: raw.version,
            sha256: raw.sha256,
            url: raw.url,
            target: raw.target,
        };
        if !is_plain_http_url(&manifest.url) {
            return Err(ManifestError::InvalidUrl);
        }
        Ok(manifest)
    }

    /// Checks the manifest targets `running_chip` (exact, case-sensitive, for example `"esp32c3"`).
    ///
    /// # Errors
    ///
    /// [`ManifestError::TargetMismatch`] on any difference.
    pub fn check_target(&self, running_chip: &str) -> Result<(), ManifestError> {
        if self.target == running_chip {
            Ok(())
        } else {
            Err(ManifestError::TargetMismatch)
        }
    }

    /// Parses `version` and `sha256` with [`ImageMetadata::parse`].
    ///
    /// # Errors
    ///
    /// [`ManifestError::InvalidDigest`] when `sha256` does not parse, otherwise [`ManifestError::InvalidVersion`] when `version` does not (both reason `manifest_invalid`).
    /// The digest is checked first, so a manifest with both wrong reports the digest.
    pub fn image_metadata(&self) -> Result<ImageMetadata, ManifestError> {
        ImageMetadata::parse(&self.sha256, &self.version).map_err(|e| match e {
            OtaError::VersionInvalid => ManifestError::InvalidVersion,
            _ => ManifestError::InvalidDigest,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;
    use alloc::vec::Vec;

    const HASH: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    fn json(version: &str, sha: &str, url: &str, target: &str) -> alloc::string::String {
        format!(r#"{{"version":"{version}","sha256":"{sha}","url":"{url}","target":"{target}"}}"#)
    }

    fn valid() -> Manifest {
        Manifest::parse(json("1.2.3", HASH, "http://h/fw.bin", "esp32c3").as_bytes()).unwrap()
    }

    #[test]
    fn bad_url_wins_over_wrong_target() {
        let err = Manifest::parse(json("1.2.3", HASH, "https://h/fw.bin", "esp32c6").as_bytes())
            .unwrap_err();
        assert_eq!(err, ManifestError::InvalidUrl);
        assert_eq!(err.reason(), FailReason::ManifestInvalid);
    }

    #[test]
    fn valid_manifest_parses() {
        let m = valid();
        assert_eq!(m.version, "1.2.3");
        assert_eq!(m.sha256, HASH);
        assert_eq!(m.url, "http://h/fw.bin");
        assert_eq!(m.target, "esp32c3");
    }

    #[test]
    fn each_missing_field_is_malformed() {
        let bodies = [
            format!(r#"{{"sha256":"{HASH}","url":"u","target":"t"}}"#),
            alloc::string::String::from(r#"{"version":"1.2.3","url":"u","target":"t"}"#),
            format!(r#"{{"version":"1.2.3","sha256":"{HASH}","target":"t"}}"#),
            format!(r#"{{"version":"1.2.3","sha256":"{HASH}","url":"u"}}"#),
        ];
        for body in &bodies {
            assert!(matches!(
                Manifest::parse(body.as_bytes()),
                Err(ManifestError::Malformed { .. })
            ));
        }
    }

    #[test]
    fn extra_null_and_non_string_fields_are_malformed() {
        let bodies = [
            format!(r#"{{"version":"1.2.3","sha256":"{HASH}","url":"u","target":"t","x":1}}"#),
            format!(r#"{{"version":null,"sha256":"{HASH}","url":"u","target":"t"}}"#),
            format!(r#"{{"version":"1.2.3","sha256":"{HASH}","url":7,"target":"t"}}"#),
            format!(r#"{{"version":"1.2.3","sha256":"{HASH}","url":"u","target":["t"]}}"#),
            format!(r#"{{"version":"1","version":"2","sha256":"{HASH}","url":"u","target":"t"}}"#),
        ];
        for body in &bodies {
            assert!(matches!(
                Manifest::parse(body.as_bytes()),
                Err(ManifestError::Malformed { .. })
            ));
        }
    }

    #[test]
    fn invalid_utf8_and_garbage_are_malformed() {
        let cases: [&[u8]; 4] = [
            b"\xff\xfe",
            b"",
            b"[]",
            br#"["1.2.3","00","http://h/fw.bin","esp32c3"]"#,
        ];
        for case in cases {
            assert!(matches!(
                Manifest::parse(case),
                Err(ManifestError::Malformed { .. })
            ));
        }
    }

    fn padded(len: usize) -> Vec<u8> {
        let mut v = json("1.2.3", HASH, "http://h/fw.bin", "esp32c3").into_bytes();
        v.resize(len, b' ');
        v
    }

    #[test]
    fn exactly_the_limit_is_parsed_and_one_over_is_too_large() {
        assert!(Manifest::parse(&padded(MAX_MANIFEST_BYTES)).is_ok());
        assert_eq!(
            Manifest::parse(&padded(MAX_MANIFEST_BYTES + 1)),
            Err(ManifestError::TooLarge)
        );
        assert_eq!(
            Manifest::parse(&alloc::vec![b'x'; MAX_MANIFEST_BYTES + 1]),
            Err(ManifestError::TooLarge)
        );
    }

    #[test]
    fn check_target_is_exact_and_case_sensitive() {
        let m = valid();
        assert_eq!(m.check_target("esp32c3"), Ok(()));
        assert_eq!(
            m.check_target("esp32c6"),
            Err(ManifestError::TargetMismatch)
        );
        assert_eq!(
            m.check_target("ESP32C3"),
            Err(ManifestError::TargetMismatch)
        );
        assert_eq!(m.check_target(""), Err(ManifestError::TargetMismatch));
    }

    #[test]
    fn bad_urls_are_invalid_url() {
        for bad in [
            "",
            "   ",
            "https://h/fw.bin",
            "ftp://h/fw.bin",
            "h/fw.bin",
            "http://",
            "http:///fw.bin",
        ] {
            assert_eq!(
                Manifest::parse(json("1.2.3", HASH, bad, "esp32c3").as_bytes()),
                Err(ManifestError::InvalidUrl),
                "{bad:?}"
            );
        }
        assert!(Manifest::parse(json("1.2.3", HASH, "HTTP://h/x", "esp32c3").as_bytes()).is_ok());
    }

    #[test]
    fn url_is_checked_even_when_the_target_differs() {
        let body = json("1.2.3", HASH, "https://h/fw.bin", "esp32c6");
        assert_eq!(
            Manifest::parse(body.as_bytes()),
            Err(ManifestError::InvalidUrl)
        );
    }

    #[test]
    fn invalid_url_error_carries_no_url_text() {
        let body = json("1.2.3", HASH, "https://user:secretpass@h/fw.bin", "esp32c3");
        let err = Manifest::parse(body.as_bytes()).unwrap_err();
        assert!(!format!("{err} {err:?}").contains("secretpass"));
    }

    #[test]
    fn reasons() {
        assert_eq!(
            ManifestError::InvalidUrl.reason(),
            FailReason::ManifestInvalid
        );
        assert_eq!(
            ManifestError::TooLarge.reason(),
            FailReason::ManifestInvalid
        );
        assert_eq!(
            ManifestError::Malformed { line: 1, column: 1 }.reason(),
            FailReason::ManifestInvalid
        );
        assert_eq!(
            ManifestError::InvalidVersion.reason(),
            FailReason::ManifestInvalid
        );
        assert_eq!(
            ManifestError::InvalidDigest.reason(),
            FailReason::ManifestInvalid
        );
        assert_eq!(
            ManifestError::TargetMismatch.reason(),
            FailReason::TargetMismatch
        );
    }

    #[test]
    fn image_metadata_valid() {
        let meta = valid().image_metadata().unwrap();
        assert_eq!(meta.version, crate::ota::Version::new(1, 2, 3));
        assert_eq!(meta.sha256[0], 0x2c);
    }

    #[test]
    fn image_metadata_distinguishes_version_from_digest() {
        let bad_versions = [
            json("1.2.3-rc1", HASH, "http://h/fw.bin", "esp32c3"),
            json("1.2", HASH, "http://h/fw.bin", "esp32c3"),
        ];
        for body in &bad_versions {
            let m = Manifest::parse(body.as_bytes()).unwrap();
            assert_eq!(m.image_metadata(), Err(ManifestError::InvalidVersion));
        }
        let bad_digests = [
            json("1.2.3", "abcd", "http://h/fw.bin", "esp32c3"),
            json("1.2.3", &"zz".repeat(32), "http://h/fw.bin", "esp32c3"),
        ];
        for body in &bad_digests {
            let m = Manifest::parse(body.as_bytes()).unwrap();
            assert_eq!(m.image_metadata(), Err(ManifestError::InvalidDigest));
        }
    }

    #[test]
    fn leading_whitespace_is_accepted() {
        for prefix in ["  ", "\n", "\t", " \n\t "] {
            let body = format!(
                "{prefix}{}",
                json("1.2.3", HASH, "http://h/fw.bin", "esp32c3")
            );
            assert!(Manifest::parse(body.as_bytes()).is_ok(), "{prefix:?}");
        }
    }

    #[test]
    fn malformed_reports_line_and_column_on_a_multi_line_payload() {
        let err = Manifest::parse(b"{\n  \"version\": \"1.2.3\",\n  bogus\n}").unwrap_err();
        match err {
            ManifestError::Malformed { line, column } => {
                assert_eq!(line, 3);
                assert!(column >= 1);
            }
            other => panic!("unexpected {other:?}"),
        }
        let shown = format!("{err}");
        assert!(shown.contains("line 3"));
        assert!(shown.contains("column"));
        assert!(!shown.contains("bogus"));
        assert_eq!(
            Manifest::parse(b"\n  [1]"),
            Err(ManifestError::Malformed { line: 2, column: 3 })
        );
    }

    #[test]
    fn errors_and_debug_never_contain_payload_text() {
        let body = json(
            "1.2.3",
            HASH,
            "http://user:secretpass@host/fw.bin",
            "esp32c3",
        );
        let m = Manifest::parse(body.as_bytes()).unwrap();
        assert!(!format!("{m:?}").contains("secretpass"));

        let bad = br#"{"url":"http://user:secretpass@host/fw.bin","bogus":1}"#;
        let err = Manifest::parse(bad).unwrap_err();
        assert!(!format!("{err} {err:?}").contains("secretpass"));
    }

    #[test]
    fn malformed_corpus_never_panics() {
        let mut inputs: Vec<Vec<u8>> = alloc::vec![
            alloc::vec![b'['; 1000],
            alloc::vec![b'{'; 1000],
            b"{\"version\":\"a\0b\"}".to_vec(),
            br#"{"version":"\ud800"}"#.to_vec(),
        ];
        let mut seed = 7_u32;
        for _ in 0..64 {
            let mut buf = Vec::new();
            for _ in 0..200 {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                buf.push((seed >> 24) as u8);
            }
            inputs.push(buf);
        }
        for input in &inputs {
            assert!(Manifest::parse(input).is_err());
        }
    }
}
