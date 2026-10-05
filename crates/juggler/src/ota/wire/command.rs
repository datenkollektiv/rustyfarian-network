//! Command intake: the flat JSON an operator or backend publishes to the OTA command topic.

use alloc::string::String;
use core::fmt;

use serde::Deserialize;

use super::first_token_position;
use super::reason::FailReason;
use super::url::is_plain_http_url;
use crate::ota::Version;

/// Experimental: API may change before 1.0.
///
/// Largest accepted command payload in bytes.
/// A larger payload is rejected before any parsing.
pub const MAX_COMMAND_BYTES: usize = 512;

/// The flat wire shape; kept private so dispatch never goes through an untagged enum
/// (which buffers the document and recurses on the caller's stack).
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCommand {
    manifest_url: Option<String>,
    sig: Option<String>,
    action: Option<String>,
    from: Option<String>,
}

/// Experimental: API may change before 1.0.
///
/// A validated OTA command.
///
/// `Debug` redacts the URL because it may carry `user:password@`.
///
/// # Examples
///
/// ```
/// use juggler::ota::OtaCommand;
///
/// let cmd = OtaCommand::parse(br#"{"manifest_url":"http://h/m.json"}"#).unwrap();
/// assert_eq!(
///     cmd,
///     OtaCommand::Update { manifest_url: "http://h/m.json".into(), sig_present: false }
/// );
///
/// let cmd = OtaCommand::parse(br#"{"action":"rollback","from":"1.0.0"}"#).unwrap();
/// assert_eq!(cmd, OtaCommand::Rollback { from: juggler::ota::Version::new(1, 0, 0) });
/// ```
#[derive(Clone, PartialEq, Eq)]
pub enum OtaCommand {
    /// Fetch the manifest at `manifest_url` and update if it is acceptable.
    Update {
        /// Where to fetch the manifest; a plain `http://` URL with a non-empty host (checked by [`OtaCommand::parse`]).
        manifest_url: String,
        /// Whether the command carried a non-null `sig`.
        /// The signature itself is never stored; a consumer can log "signature received, not verified" from this flag.
        sig_present: bool,
    },
    /// Roll back from the named running version.
    Rollback {
        /// The version the operator believes is running, parsed with [`Version::parse`].
        from: Version,
    },
    /// Repair unreadable OTA records so a corrupt record does not need a reflash.
    ///
    /// The wire shape is `{"action":"repair"}`; it carries no other field.
    Repair,
}

impl fmt::Debug for OtaCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OtaCommand::Update { sig_present, .. } => f
                .debug_struct("Update")
                .field("manifest_url", &"<redacted>")
                .field("sig_present", sig_present)
                .finish(),
            OtaCommand::Rollback { from } => f
                .debug_struct("Rollback")
                .field("from", &format_args!("{from}"))
                .finish(),
            OtaCommand::Repair => f.write_str("Repair"),
        }
    }
}

/// Experimental: API may change before 1.0.
///
/// Why a command was rejected.
/// Carries no payload text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandError {
    /// The payload exceeds [`MAX_COMMAND_BYTES`].
    TooLarge,
    /// Not UTF-8, not a JSON object, wrong field types, an unknown or duplicate field, or trailing garbage.
    ///
    /// `line` and `column` (both 1-based) come from the JSON parser and locate the problem; they are numbers only.
    /// When the payload is rejected early for not starting with `{`, they point at its first non-whitespace byte, or 1/1 if there is none.
    Malformed {
        /// 1-based line of the problem.
        line: usize,
        /// 1-based column of the problem.
        column: usize,
    },
    /// Well-formed, but not one of the three accepted shapes.
    UnsupportedShape,
    /// An update's `manifest_url` is empty, whitespace-only, not `http://`, or has no host.
    ///
    /// Both tiers download over plain HTTP only (ADR 011), so such a URL is rejected at intake instead of failing later as `server_unreachable` or `manifest_fetch`.
    InvalidUrl,
    /// A rollback's `from` is not a `MAJOR.MINOR.PATCH` version.
    InvalidVersion,
}

impl CommandError {
    /// The status reason: always [`FailReason::CommandInvalid`].
    pub const fn reason(&self) -> FailReason {
        FailReason::CommandInvalid
    }
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CommandError::TooLarge => f.write_str("OTA command too large"),
            CommandError::Malformed { line, column } => {
                write!(f, "OTA command malformed at line {line} column {column}")
            }
            CommandError::UnsupportedShape => f.write_str("OTA command has an unsupported shape"),
            CommandError::InvalidUrl => f.write_str("OTA command manifest URL invalid"),
            CommandError::InvalidVersion => f.write_str("OTA command rollback version invalid"),
        }
    }
}

impl OtaCommand {
    /// Parses and validates a command payload.
    ///
    /// Accepted: `{manifest_url, sig?}` (an update), `{action: "rollback", from}` with no `sig` (a rollback) and `{action: "repair"}` with nothing else (a repair).
    /// An explicit JSON `null` counts as an absent field.
    /// Nothing else is accepted.
    /// The `manifest_url` must be a plain `http://` URL (scheme matched ASCII case-insensitively) with a non-empty host, and `from` must be a `MAJOR.MINOR.PATCH` version.
    /// The URL is only shape-checked, never fetched or resolved.
    ///
    /// # Errors
    ///
    /// [`CommandError::TooLarge`] for more than [`MAX_COMMAND_BYTES`] bytes (checked before parsing),
    /// [`CommandError::Malformed`] (with line and column) for invalid JSON, [`CommandError::UnsupportedShape`] for any other field combination,
    /// [`CommandError::InvalidUrl`] for an update whose `manifest_url` is empty, whitespace-only, not `http://` or hostless, and [`CommandError::InvalidVersion`] for a rollback whose `from` does not parse.
    pub fn parse(payload: &[u8]) -> Result<Self, CommandError> {
        if payload.len() > MAX_COMMAND_BYTES {
            return Err(CommandError::TooLarge);
        }
        // serde's derived struct deserializer also accepts the positional array form.
        if payload.iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'{') {
            let (line, column) = first_token_position(payload);
            return Err(CommandError::Malformed { line, column });
        }
        let raw: RawCommand =
            serde_json::from_slice(payload).map_err(|e| CommandError::Malformed {
                line: e.line(),
                column: e.column().max(1),
            })?;
        match (raw.manifest_url, raw.action, raw.from) {
            // TODO(security-model): `sig` is reserved. It is accepted, never verified,
            // never stored and never logged until the ROADMAP security-model item
            // decides the trust boundary.
            (Some(manifest_url), None, None) => {
                if !is_plain_http_url(&manifest_url) {
                    return Err(CommandError::InvalidUrl);
                }
                Ok(OtaCommand::Update {
                    manifest_url,
                    sig_present: raw.sig.is_some(),
                })
            }
            (None, Some(action), Some(from)) if action == "rollback" && raw.sig.is_none() => {
                let from = Version::parse(&from).map_err(|_| CommandError::InvalidVersion)?;
                Ok(OtaCommand::Rollback { from })
            }
            (None, Some(action), None) if action == "repair" && raw.sig.is_none() => {
                Ok(OtaCommand::Repair)
            }
            _ => Err(CommandError::UnsupportedShape),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;
    use alloc::string::ToString;
    use alloc::vec::Vec;

    fn url(u: &str) -> OtaCommand {
        OtaCommand::Update {
            manifest_url: u.to_string(),
            sig_present: false,
        }
    }

    #[test]
    fn update_minimal() {
        assert_eq!(
            OtaCommand::parse(br#"{"manifest_url":"http://h/m.json"}"#),
            Ok(url("http://h/m.json"))
        );
    }

    #[test]
    fn sig_present_reflects_a_non_null_sig() {
        assert_eq!(
            OtaCommand::parse(br#"{"manifest_url":"http://h/m.json","sig":"abcd"}"#),
            Ok(OtaCommand::Update {
                manifest_url: "http://h/m.json".to_string(),
                sig_present: true
            })
        );
        assert_eq!(
            OtaCommand::parse(br#"{"manifest_url":"http://h/m.json","sig":null}"#),
            Ok(url("http://h/m.json"))
        );
        assert_eq!(
            OtaCommand::parse(br#"{"manifest_url":"http://h/m.json"}"#),
            Ok(url("http://h/m.json"))
        );
    }

    #[test]
    fn debug_shows_sig_present_but_not_the_signature() {
        let cmd =
            OtaCommand::parse(br#"{"manifest_url":"http://h","sig":"topsecretsig"}"#).unwrap();
        let shown = format!("{cmd:?}");
        assert!(shown.contains("sig_present: true"));
        assert!(!shown.contains("topsecretsig"));
    }

    #[test]
    fn leading_whitespace_is_accepted() {
        for prefix in ["  ", "\n", "\t", " \n\t "] {
            let payload = format!("{prefix}{{\"manifest_url\":\"http://h\"}}");
            assert_eq!(
                OtaCommand::parse(payload.as_bytes()),
                Ok(url("http://h")),
                "{prefix:?}"
            );
        }
    }

    #[test]
    fn malformed_reports_line_and_column_on_a_multi_line_payload() {
        let err = OtaCommand::parse(b"{\n  \"manifest_url\": \"u\",\n  bogus\n}").unwrap_err();
        match err {
            CommandError::Malformed { line, column } => {
                assert_eq!(line, 3);
                assert!(column >= 1);
            }
            other => panic!("unexpected {other:?}"),
        }
        let shown = format!("{err}");
        assert!(shown.contains("line 3"));
        assert!(shown.contains("column"));
        assert!(!shown.contains("bogus"));
    }

    #[test]
    fn early_rejection_points_at_the_first_token() {
        assert_eq!(
            OtaCommand::parse(b"\n  [1]"),
            Err(CommandError::Malformed { line: 2, column: 3 })
        );
        assert_eq!(
            OtaCommand::parse(b""),
            Err(CommandError::Malformed { line: 1, column: 1 })
        );
    }

    #[test]
    fn malformed_column_is_never_zero_at_end_of_line() {
        assert_eq!(
            OtaCommand::parse(b"{\n"),
            Err(CommandError::Malformed { line: 2, column: 1 })
        );
    }

    #[test]
    fn bad_manifest_urls_are_invalid_url() {
        for bad in [
            "",
            "   ",
            "https://h/m.json",
            "ftp://h/m.json",
            "h/m.json",
            "http://",
            "http:///m.json",
        ] {
            let payload = format!(r#"{{"manifest_url":"{bad}"}}"#);
            assert_eq!(
                OtaCommand::parse(payload.as_bytes()),
                Err(CommandError::InvalidUrl),
                "{bad:?}"
            );
            let with_sig = format!(r#"{{"manifest_url":"{bad}","sig":"x"}}"#);
            assert_eq!(
                OtaCommand::parse(with_sig.as_bytes()),
                Err(CommandError::InvalidUrl)
            );
        }
    }

    #[test]
    fn scheme_is_case_insensitive() {
        assert_eq!(
            OtaCommand::parse(br#"{"manifest_url":"HTTP://h/x"}"#),
            Ok(url("HTTP://h/x"))
        );
    }

    #[test]
    fn invalid_url_is_checked_after_shape() {
        assert_eq!(
            OtaCommand::parse(br#"{"manifest_url":"","from":"1.0.0"}"#),
            Err(CommandError::UnsupportedShape)
        );
    }

    #[test]
    fn null_counts_as_absent() {
        assert_eq!(
            OtaCommand::parse(br#"{"manifest_url":"http://h","action":null,"from":null}"#),
            Ok(url("http://h"))
        );
    }

    #[test]
    fn rollback() {
        assert_eq!(
            OtaCommand::parse(br#"{"action":"rollback","from":"1.0.0"}"#),
            Ok(OtaCommand::Rollback {
                from: Version::new(1, 0, 0)
            })
        );
    }

    #[test]
    fn rollback_from_must_be_a_version() {
        for bad in ["", "1.0", "1.0.0.0", "1.0.0-rc1", "abc", "1.0.70000"] {
            let payload = format!(r#"{{"action":"rollback","from":"{bad}"}}"#);
            assert_eq!(
                OtaCommand::parse(payload.as_bytes()),
                Err(CommandError::InvalidVersion),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn repair() {
        assert_eq!(
            OtaCommand::parse(br#"{"action":"repair"}"#),
            Ok(OtaCommand::Repair)
        );
        assert_eq!(
            OtaCommand::parse(br#"{"action":"repair","from":null,"sig":null}"#),
            Ok(OtaCommand::Repair)
        );
        assert_eq!(format!("{:?}", OtaCommand::Repair), "Repair");
    }

    #[test]
    fn repair_rejects_every_other_field() {
        let cases: [&[u8]; 6] = [
            br#"{"action":"repair","sig":"x"}"#,
            br#"{"action":"repair","from":"1.0.0"}"#,
            br#"{"action":"repair","manifest_url":"http://h/m.json"}"#,
            br#"{"action":"Repair"}"#,
            br#"{"action":"repair "}"#,
            br#"{"action":null}"#,
        ];
        for case in cases {
            assert_eq!(
                OtaCommand::parse(case),
                Err(CommandError::UnsupportedShape),
                "{}",
                core::str::from_utf8(case).unwrap_or("?")
            );
        }
    }

    #[test]
    fn rollback_debug_shows_the_version() {
        let cmd = OtaCommand::parse(br#"{"action":"rollback","from":"1.2.3"}"#).unwrap();
        assert!(format!("{cmd:?}").contains("1.2.3"));
    }

    #[test]
    fn unsupported_shapes() {
        let cases: [&[u8]; 8] = [
            br#"{"action":"rollback","from":"1.0.0","sig":"x"}"#,
            br#"{"action":"rollback"}"#,
            br#"{"from":"1.0.0"}"#,
            br#"{"action":"reboot","from":"1.0.0"}"#,
            br#"{"manifest_url":"u","action":"rollback","from":"1.0.0"}"#,
            br#"{"manifest_url":"u","action":"rollback"}"#,
            br#"{"manifest_url":"u","from":"1.0.0"}"#,
            b"{}",
        ];
        for case in cases {
            assert_eq!(
                OtaCommand::parse(case),
                Err(CommandError::UnsupportedShape),
                "{}",
                core::str::from_utf8(case).unwrap_or("?")
            );
        }
    }

    #[test]
    fn malformed_inputs() {
        let cases: [&[u8]; 11] = [
            br#"{"manifest_url":"u","bogus":1}"#,
            br#"{"manifest_url":"u","manifest_url":"v"}"#,
            br#"{"manifest_url":{"a":1}}"#,
            br#"{"manifest_url":["u"]}"#,
            br#"{"manifest_url":5}"#,
            b"\xff\xfe",
            b"",
            br#"{"manifest_url":"u""#,
            br#"["manifest_url"]"#,
            br#"{"manifest_url":"u"} trailing"#,
            b"null",
        ];
        for case in cases {
            assert!(
                matches!(OtaCommand::parse(case), Err(CommandError::Malformed { .. })),
                "{case:?}"
            );
        }
    }

    #[test]
    fn invalid_utf8_inside_a_string_is_malformed() {
        assert!(matches!(
            OtaCommand::parse(b"{\"manifest_url\":\"\xff\"}"),
            Err(CommandError::Malformed { .. })
        ));
    }

    #[test]
    fn trailing_whitespace_is_accepted() {
        assert_eq!(
            OtaCommand::parse(b"{\"manifest_url\":\"http://h\"}  \n"),
            Ok(url("http://h"))
        );
    }

    fn padded(len: usize) -> Vec<u8> {
        let mut v = br#"{"manifest_url":"http://h"}"#.to_vec();
        v.resize(len, b' ');
        v
    }

    #[test]
    fn exactly_the_limit_is_parsed() {
        assert_eq!(padded(MAX_COMMAND_BYTES).len(), 512);
        assert_eq!(
            OtaCommand::parse(&padded(MAX_COMMAND_BYTES)),
            Ok(url("http://h"))
        );
    }

    #[test]
    fn one_over_the_limit_is_too_large_even_for_garbage() {
        assert_eq!(
            OtaCommand::parse(&alloc::vec![b'x'; MAX_COMMAND_BYTES + 1]),
            Err(CommandError::TooLarge)
        );
        assert_eq!(
            OtaCommand::parse(&padded(MAX_COMMAND_BYTES + 1)),
            Err(CommandError::TooLarge)
        );
    }

    #[test]
    fn huge_payload_is_too_large() {
        assert_eq!(
            OtaCommand::parse(&alloc::vec![0u8; 1024 * 1024]),
            Err(CommandError::TooLarge)
        );
    }

    #[test]
    fn every_error_reports_command_invalid() {
        for e in [
            CommandError::TooLarge,
            CommandError::Malformed { line: 1, column: 1 },
            CommandError::UnsupportedShape,
            CommandError::InvalidUrl,
            CommandError::InvalidVersion,
        ] {
            assert_eq!(e.reason(), FailReason::CommandInvalid);
        }
    }

    #[test]
    fn errors_and_debug_never_contain_payload_text() {
        let secret = br#"{"manifest_url":"http://user:secretpass@host/m.json","bogus":1}"#;
        let err = OtaCommand::parse(secret).unwrap_err();
        let shown = format!("{err} {err:?}");
        assert!(!shown.contains("secretpass"));
        assert!(!shown.contains("bogus"));

        let bad = OtaCommand::parse(br#"{"manifest_url":"https://user:secretpass@host/m.json"}"#)
            .unwrap_err();
        assert!(!format!("{bad} {bad:?}").contains("secretpass"));

        let ok =
            OtaCommand::parse(br#"{"manifest_url":"http://user:secretpass@host/m.json"}"#).unwrap();
        assert!(!format!("{ok:?}").contains("secretpass"));
    }

    #[test]
    fn malformed_corpus_never_panics() {
        let mut corpus: Vec<Vec<u8>> = Vec::new();
        corpus.push(alloc::vec![b'['; 400]);
        corpus.push(alloc::vec![b'{'; 400]);
        let mut nested = Vec::new();
        for _ in 0..200 {
            nested.extend_from_slice(b"[{");
        }
        corpus.push(nested);
        corpus.push(br#"{"manifest_url":"\ud800"}"#.to_vec());
        corpus.push(br#"{"manifest_url":"\udc00\ud800"}"#.to_vec());
        corpus.push(b"{\"manifest_url\":\"a\0b\"}".to_vec());
        corpus.push(alloc::vec![0u8; 64]);
        corpus.push(br#"["http://h/m.json",null,null,null]"#.to_vec());
        corpus.push(br#"[null,null,"rollback","1.0.0"]"#.to_vec());
        let mut seed = 0x9E37_79B9_u32;
        for _ in 0..64 {
            let mut buf = Vec::new();
            for _ in 0..96 {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                buf.push((seed >> 24) as u8);
            }
            corpus.push(buf);
        }
        for input in &corpus {
            assert!(input.len() <= MAX_COMMAND_BYTES);
            assert!(OtaCommand::parse(input).is_err());
        }
    }
}
