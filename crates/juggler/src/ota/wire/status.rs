//! Status messages published on the OTA status topic.

use alloc::borrow::Cow;
use alloc::string::String;
use core::fmt;

use serde::de::{Deserializer, Error, MapAccess, Visitor};
use serde::{Deserialize, Serialize};

use super::reason::FailReason;
use super::rollback::RollbackReason;

/// Experimental: API may change before 1.0.
///
/// A status message.
/// The JSON is compact, tagged with `"status"` in snake_case, with fields in declaration order, and byte-compatible with the first consumer.
///
/// The reasons serialize as their `as_str()` strings, see [`FailReason`] and [`RollbackReason`].
///
/// # Examples
///
/// ```
/// use juggler::ota::{FailReason, OtaStatus};
///
/// assert_eq!(OtaStatus::Downloading.to_json().unwrap(), r#"{"status":"downloading"}"#);
/// assert_eq!(
///     OtaStatus::Failed { reason: FailReason::UpToDate }.to_json().unwrap(),
///     r#"{"status":"failed","reason":"up_to_date"}"#
/// );
/// ```
///
/// # Deserializing
///
/// Statuses can be read back with `serde_json::from_str` or `from_slice` (not from a reader, which cannot lend a borrowed `version`).
/// The `status` tag must be one of the six known values and each status accepts exactly its own fields: an unknown tag, an unknown or duplicate field, a missing field or a field of another status is an error.
/// This is hand-written rather than derived: `serde`'s `deny_unknown_fields` is silently ignored for the unit variants of an internally tagged enum, so a derive would accept `{"status":"downloading","x":1}`.
/// The error messages for an unknown tag, field, code or a wrong-typed or out-of-range field (including floats, booleans and oversized integers) are fixed text and never quote the input.
/// `Applied` borrows its `version` from the input, so a version string containing a JSON escape fails to deserialize; versions never need escaping.
/// An unknown `failed` reason code is an error, an unknown `rolled_back` label is an error as well, and `download_failed` comes back as `DownloadFailed { status: 0 }` because the status is not on the wire (the bytes still round-trip).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum OtaStatus<'a> {
    /// The download has started.
    Downloading,
    /// The new image is written and selected; a reboot follows.
    SwapPending,
    /// The new version is running and healthy.
    Applied {
        /// The running version.
        version: &'a str,
    },
    /// The command was rejected or the update failed.
    Failed {
        /// A fixed reason code, see [`FailReason`].
        reason: FailReason,
    },
    /// The device rolled back to the previous image.
    RolledBack {
        /// Why the device rolled back.
        reason: RollbackReason,
        /// Stable event id of the attempt; the backend deduplicates on it.
        attempt_id: u32,
        /// Random per-install id; the backend deduplicates on (epoch, attempt id).
        epoch: u32,
    },
    /// An operator repair command finished.
    Repaired {
        /// How many unreadable record groups were repaired; zero is never reported (that is `repair_not_needed`).
        records: u32,
    },
}

/// One field value of a status object: a string or an unsigned integer, with fixed-text errors for anything else.
enum FieldValue<'a> {
    Str(Cow<'a, str>),
    Num(u64),
}

impl<'de> Deserialize<'de> for FieldValue<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct ValueVisitor;
        impl<'de> Visitor<'de> for ValueVisitor {
            type Value = FieldValue<'de>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a string or an unsigned integer")
            }
            fn visit_borrowed_str<E: Error>(self, v: &'de str) -> Result<Self::Value, E> {
                Ok(FieldValue::Str(Cow::Borrowed(v)))
            }
            fn visit_str<E: Error>(self, v: &str) -> Result<Self::Value, E> {
                Ok(FieldValue::Str(Cow::Owned(String::from(v))))
            }
            fn visit_u64<E: Error>(self, v: u64) -> Result<Self::Value, E> {
                Ok(FieldValue::Num(v))
            }
            fn visit_i64<E: Error>(self, v: i64) -> Result<Self::Value, E> {
                u64::try_from(v)
                    .map(FieldValue::Num)
                    .map_err(|_| E::custom("OTA status field out of range"))
            }
            fn visit_u128<E: Error>(self, _: u128) -> Result<Self::Value, E> {
                Err(E::custom("OTA status field out of range"))
            }
            fn visit_i128<E: Error>(self, _: i128) -> Result<Self::Value, E> {
                Err(E::custom("OTA status field out of range"))
            }
            fn visit_f64<E: Error>(self, _: f64) -> Result<Self::Value, E> {
                Err(E::custom("OTA status field has the wrong type"))
            }
            fn visit_bool<E: Error>(self, _: bool) -> Result<Self::Value, E> {
                Err(E::custom("OTA status field has the wrong type"))
            }
        }
        deserializer.deserialize_any(ValueVisitor)
    }
}

/// The object key, as one of the five known names; anything else is a fixed-text error.
enum FieldKey {
    Status,
    Version,
    Reason,
    AttemptId,
    Epoch,
    Records,
}

impl<'de> Deserialize<'de> for FieldKey {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct KeyVisitor;
        impl Visitor<'_> for KeyVisitor {
            type Value = FieldKey;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an OTA status field name")
            }
            fn visit_str<E: Error>(self, v: &str) -> Result<FieldKey, E> {
                match v {
                    "status" => Ok(FieldKey::Status),
                    "version" => Ok(FieldKey::Version),
                    "reason" => Ok(FieldKey::Reason),
                    "attempt_id" => Ok(FieldKey::AttemptId),
                    "epoch" => Ok(FieldKey::Epoch),
                    "records" => Ok(FieldKey::Records),
                    _ => Err(E::custom("unknown OTA status field")),
                }
            }
        }
        deserializer.deserialize_identifier(KeyVisitor)
    }
}

impl<'de> Deserialize<'de> for OtaStatus<'de> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct StatusVisitor;
        impl<'de> Visitor<'de> for StatusVisitor {
            type Value = OtaStatus<'de>;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an OTA status object")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
                let mut status = None;
                let mut version = None;
                let mut reason = None;
                let mut attempt_id = None;
                let mut epoch = None;
                let mut records = None;
                while let Some(key) = map.next_key::<FieldKey>()? {
                    let value = map.next_value::<FieldValue<'de>>()?;
                    let slot = match key {
                        FieldKey::Status => &mut status,
                        FieldKey::Version => &mut version,
                        FieldKey::Reason => &mut reason,
                        FieldKey::AttemptId => &mut attempt_id,
                        FieldKey::Epoch => &mut epoch,
                        FieldKey::Records => &mut records,
                    };
                    if slot.replace(value).is_some() {
                        return Err(A::Error::custom("duplicate OTA status field"));
                    }
                }
                let bad_shape = || A::Error::custom("OTA status has missing or unexpected fields");
                let text = |v: Option<FieldValue<'de>>| match v {
                    Some(FieldValue::Str(s)) => Ok(Some(s)),
                    Some(FieldValue::Num(_)) => Err(bad_shape()),
                    None => Ok(None),
                };
                let number = |v: Option<FieldValue<'de>>| match v {
                    Some(FieldValue::Num(n)) => {
                        Ok(Some(u32::try_from(n).map_err(|_| {
                            A::Error::custom("OTA status field out of range")
                        })?))
                    }
                    Some(FieldValue::Str(_)) => Err(bad_shape()),
                    None => Ok(None),
                };
                let status = text(status)?.ok_or_else(bad_shape)?;
                let version = text(version)?;
                let reason = text(reason)?;
                let attempt_id = number(attempt_id)?;
                let epoch = number(epoch)?;
                let records = number(records)?;
                match (status.as_ref(), version, reason, attempt_id, epoch, records) {
                    ("downloading", None, None, None, None, None) => Ok(OtaStatus::Downloading),
                    ("swap_pending", None, None, None, None, None) => Ok(OtaStatus::SwapPending),
                    ("applied", Some(Cow::Borrowed(version)), None, None, None, None) => {
                        Ok(OtaStatus::Applied { version })
                    }
                    ("applied", Some(Cow::Owned(_)), None, None, None, None) => {
                        Err(A::Error::custom(
                            "OTA status version needs a JSON escape and cannot be borrowed",
                        ))
                    }
                    ("repaired", None, None, None, None, Some(records)) => {
                        Ok(OtaStatus::Repaired { records })
                    }
                    ("failed", None, Some(code), None, None, None) => FailReason::from_code(&code)
                        .map(|reason| OtaStatus::Failed { reason })
                        .ok_or_else(|| A::Error::custom("unknown OTA failure code")),
                    ("rolled_back", None, Some(label), Some(attempt_id), Some(epoch), None) => {
                        RollbackReason::parse(&label)
                            .map(|reason| OtaStatus::RolledBack {
                                reason,
                                attempt_id,
                                epoch,
                            })
                            .ok_or_else(|| A::Error::custom("unknown OTA rollback label"))
                    }
                    (
                        "downloading" | "swap_pending" | "applied" | "failed" | "rolled_back"
                        | "repaired",
                        ..,
                    ) => Err(bad_shape()),
                    _ => Err(A::Error::custom("unknown OTA status")),
                }
            }
        }
        deserializer.deserialize_map(StatusVisitor)
    }
}

/// Experimental: API may change before 1.0.
///
/// Serializing a status failed.
///
/// This is opaque on purpose: the cause is not carried and `serde_json::Error` is not part of the public API.
/// It cannot occur for [`OtaStatus`]: every field is a string, an integer or a unit variant, which `serde_json` writes to a `String` without any fallible step, so the error path is unreachable in practice.
/// Even if it did occur, the underlying error could echo content, so it is dropped rather than exposed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusError;

impl fmt::Display for StatusError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OTA status serialization failed")
    }
}

impl OtaStatus<'_> {
    /// Serializes to compact JSON without a trailing newline.
    ///
    /// # Errors
    ///
    /// [`StatusError`] if serialization fails (see its docs: not expected for these types); the underlying error is dropped.
    pub fn to_json(&self) -> Result<String, StatusError> {
        serde_json::to_string(self).map_err(|_| StatusError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ota::OtaError;

    fn json(s: OtaStatus<'_>) -> alloc::string::String {
        s.to_json().unwrap()
    }

    fn parse(text: &str) -> Result<OtaStatus<'_>, serde_json::Error> {
        serde_json::from_str(text)
    }

    fn parse_ok(text: &str) -> Option<OtaStatus<'_>> {
        parse(text).ok()
    }

    fn goldens() -> alloc::vec::Vec<alloc::string::String> {
        let mut all = alloc::vec![
            alloc::string::String::from(r#"{"status":"downloading"}"#),
            alloc::string::String::from(r#"{"status":"swap_pending"}"#),
            alloc::string::String::from(r#"{"status":"applied","version":"1.2.3"}"#),
            alloc::string::String::from(
                r#"{"status":"rolled_back","reason":"health_deadline","attempt_id":7,"epoch":4294967295}"#,
            ),
            alloc::string::String::from(r#"{"status":"repaired","records":3}"#),
        ];
        for label in [
            "operator",
            "health_deadline",
            "unhealthy",
            "version_mismatch",
            "bootloader",
            "unknown",
        ] {
            all.push(alloc::format!(
                r#"{{"status":"rolled_back","reason":"{label}","attempt_id":1,"epoch":2}}"#
            ));
        }
        for code in [
            "command_invalid",
            "busy",
            "worker_unavailable",
            "pending_verify",
            "manifest_fetch",
            "manifest_invalid",
            "target_mismatch",
            "version_invalid",
            "previously_rolled_back",
            "up_to_date",
            "downgrade",
            "attempt_unresolved",
            "report_pending",
            "partition_not_found",
            "attempt_not_persisted",
            "version_mismatch",
            "rollback_unavailable",
            "repair_failed",
            "repair_not_needed",
            "server_unreachable",
            "download_failed",
            "download_timeout",
            "checksum_mismatch",
            "flash_write_failed",
            "insufficient_space",
        ] {
            all.push(alloc::format!(r#"{{"status":"failed","reason":"{code}"}}"#));
        }
        all
    }

    #[test]
    fn every_golden_status_round_trips_to_identical_bytes() {
        for text in goldens() {
            let status = parse(&text).expect(&text);
            assert_eq!(status.to_json().unwrap(), text);
        }
    }

    #[test]
    fn parsed_values_are_the_expected_variants() {
        assert_eq!(
            parse_ok(r#"{"status":"applied","version":"1.2.3"}"#),
            Some(OtaStatus::Applied { version: "1.2.3" })
        );
        assert_eq!(
            parse_ok(r#"{"status":"rolled_back","reason":"operator","attempt_id":3,"epoch":9}"#),
            Some(OtaStatus::RolledBack {
                reason: RollbackReason::Operator,
                attempt_id: 3,
                epoch: 9
            })
        );
        assert_eq!(
            parse_ok(r#"{"status":"failed","reason":"up_to_date"}"#),
            Some(OtaStatus::Failed {
                reason: FailReason::UpToDate
            })
        );
    }

    #[test]
    fn download_failed_status_is_lost_but_bytes_round_trip() {
        let text = r#"{"status":"failed","reason":"download_failed"}"#;
        assert_eq!(
            parse_ok(text),
            Some(OtaStatus::Failed {
                reason: FailReason::Ota(OtaError::DownloadFailed { status: 0 })
            })
        );
    }

    #[test]
    fn every_serialized_fail_reason_parses_back_to_the_same_bytes() {
        for status in [404_u16, 0, 503] {
            let text = json(OtaStatus::Failed {
                reason: FailReason::Ota(OtaError::DownloadFailed { status }),
            });
            assert_eq!(parse(&text).unwrap().to_json().unwrap(), text);
        }
    }

    #[test]
    fn unknown_tag_is_rejected() {
        for text in [
            r#"{"status":"exploded"}"#,
            r#"{"status":""}"#,
            r#"{"status":"Downloading"}"#,
            r#"{"status":5}"#,
            r#"{}"#,
            r#"{"version":"1.0.0"}"#,
        ] {
            assert!(parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn unknown_fields_are_rejected_for_every_status() {
        for text in [
            r#"{"status":"downloading","x":1}"#,
            r#"{"status":"swap_pending","x":1}"#,
            r#"{"status":"applied","version":"1.0.0","x":1}"#,
            r#"{"status":"failed","reason":"busy","x":1}"#,
            r#"{"status":"rolled_back","reason":"operator","attempt_id":1,"epoch":2,"x":1}"#,
            r#"{"status":"repaired","records":1,"x":1}"#,
        ] {
            assert!(parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn missing_misplaced_duplicate_and_mistyped_fields_are_rejected() {
        for text in [
            r#"{"status":"applied"}"#,
            r#"{"status":"failed"}"#,
            r#"{"status":"rolled_back","reason":"operator","attempt_id":1}"#,
            r#"{"status":"rolled_back","attempt_id":1,"epoch":2}"#,
            r#"{"status":"downloading","version":"1.0.0"}"#,
            r#"{"status":"applied","version":"1.0.0","reason":"busy"}"#,
            r#"{"status":"failed","reason":"busy","epoch":1}"#,
            r#"{"status":"downloading","status":"downloading"}"#,
            r#"{"status":"applied","version":1}"#,
            r#"{"status":"failed","reason":1}"#,
            r#"{"status":"rolled_back","reason":"operator","attempt_id":"1","epoch":2}"#,
            r#"{"status":"rolled_back","reason":"operator","attempt_id":-1,"epoch":2}"#,
            r#"{"status":"rolled_back","reason":"operator","attempt_id":4294967296,"epoch":2}"#,
            r#"{"status":"rolled_back","reason":"operator","attempt_id":1.5,"epoch":2}"#,
            r#"{"status":"applied","version":null}"#,
            r#"{"status":"repaired"}"#,
            r#"{"status":"repaired","records":"1"}"#,
            r#"{"status":"repaired","records":-1}"#,
            r#"{"status":"repaired","records":4294967296}"#,
            r#"{"status":"repaired","records":1,"reason":"busy"}"#,
            r#"{"status":"failed","reason":"busy","records":1}"#,
            r#"["downloading"]"#,
            r#"null"#,
            r#""#,
        ] {
            assert!(parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn unknown_fail_code_and_unknown_rollback_label_are_errors() {
        assert!(parse(r#"{"status":"failed","reason":"nope"}"#).is_err());
        assert!(
            parse(r#"{"status":"rolled_back","reason":"nope","attempt_id":1,"epoch":2}"#).is_err()
        );
    }

    #[test]
    fn escaped_version_fails_to_deserialize() {
        assert!(parse(r#"{"status":"applied","version":"1\u002e0.0"}"#).is_err());
        let nasty = json(OtaStatus::Applied { version: "a\"b" });
        assert!(parse(&nasty).is_err());
    }

    #[test]
    fn deserialize_errors_never_quote_the_input() {
        for text in [
            r#"{"status":"secrettag"}"#,
            r#"{"status":"downloading","secretfield":1}"#,
            r#"{"status":"failed","reason":"secretcode"}"#,
            r#"{"status":"rolled_back","reason":"secretlabel","attempt_id":1,"epoch":2}"#,
            r#"{"status":"applied","version":"1.0.0","secretfield":"secretval"}"#,
            r#"{"status":"downloading","status":"secretdup"}"#,
        ] {
            let err = parse(text).unwrap_err();
            assert!(
                !alloc::format!("{err} {err:?}").contains("secret"),
                "{text}"
            );
        }
    }

    #[test]
    fn numeric_type_errors_are_fixed_text() {
        for value in [
            "1.5",
            "true",
            "1e30",
            "18446744073709551616",
            "-1",
            "4294967296",
            "-9223372036854775809",
        ] {
            for field in ["attempt_id", "epoch"] {
                let (a, e) = if field == "attempt_id" {
                    (value, "2")
                } else {
                    ("1", value)
                };
                let text = alloc::format!(
                    r#"{{"status":"rolled_back","reason":"operator","attempt_id":{a},"epoch":{e}}}"#
                );
                let err = parse(&text).unwrap_err();
                let shown = alloc::format!("{err} {err:?}");
                assert!(!shown.contains(value), "{text}: {shown}");
            }
        }
        let err = parse(r#"{"status":"applied","version":1.5}"#).unwrap_err();
        assert!(!alloc::format!("{err}").contains("1.5"));
    }

    #[test]
    fn deserialize_from_slice_works_and_garbage_never_panics() {
        let ok: OtaStatus<'_> = serde_json::from_slice(br#"{"status":"downloading"}"#).unwrap();
        assert_eq!(ok, OtaStatus::Downloading);
        let mut seed = 11_u32;
        for _ in 0..64 {
            let mut buf = alloc::vec::Vec::new();
            for _ in 0..96 {
                seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                buf.push((seed >> 24) as u8);
            }
            assert!(serde_json::from_slice::<OtaStatus<'_>>(&buf).is_err());
        }
    }

    #[test]
    fn unit_variants() {
        assert_eq!(json(OtaStatus::Downloading), r#"{"status":"downloading"}"#);
        assert_eq!(json(OtaStatus::SwapPending), r#"{"status":"swap_pending"}"#);
    }

    #[test]
    fn applied() {
        assert_eq!(
            json(OtaStatus::Applied { version: "1.2.3" }),
            r#"{"status":"applied","version":"1.2.3"}"#
        );
    }

    #[test]
    fn failed() {
        assert_eq!(
            json(OtaStatus::Failed {
                reason: FailReason::CommandInvalid
            }),
            r#"{"status":"failed","reason":"command_invalid"}"#
        );
    }

    #[test]
    fn rolled_back_field_order_and_u32_extremes() {
        assert_eq!(
            json(OtaStatus::RolledBack {
                reason: RollbackReason::HealthDeadline,
                attempt_id: 7,
                epoch: 4_294_967_295,
            }),
            r#"{"status":"rolled_back","reason":"health_deadline","attempt_id":7,"epoch":4294967295}"#
        );
        assert_eq!(
            json(OtaStatus::RolledBack {
                reason: RollbackReason::Operator,
                attempt_id: 0,
                epoch: 0,
            }),
            r#"{"status":"rolled_back","reason":"operator","attempt_id":0,"epoch":0}"#
        );
    }

    #[test]
    fn every_rolled_back_reason_has_a_golden_json() {
        let cases = [
            (RollbackReason::Operator, "operator"),
            (RollbackReason::HealthDeadline, "health_deadline"),
            (RollbackReason::Unhealthy, "unhealthy"),
            (RollbackReason::VersionMismatch, "version_mismatch"),
            (RollbackReason::Bootloader, "bootloader"),
            (RollbackReason::Unknown, "unknown"),
        ];
        for (reason, label) in cases {
            let expected = alloc::format!(
                r#"{{"status":"rolled_back","reason":"{label}","attempt_id":1,"epoch":2}}"#
            );
            assert_eq!(
                json(OtaStatus::RolledBack {
                    reason,
                    attempt_id: 1,
                    epoch: 2
                }),
                expected
            );
        }
    }

    #[test]
    fn every_fail_reason_has_a_golden_json() {
        let cases = [
            (FailReason::CommandInvalid, "command_invalid"),
            (FailReason::Busy, "busy"),
            (FailReason::WorkerUnavailable, "worker_unavailable"),
            (FailReason::PendingVerify, "pending_verify"),
            (FailReason::ManifestFetch, "manifest_fetch"),
            (FailReason::ManifestInvalid, "manifest_invalid"),
            (FailReason::TargetMismatch, "target_mismatch"),
            (FailReason::VersionInvalid, "version_invalid"),
            (FailReason::PreviouslyRolledBack, "previously_rolled_back"),
            (FailReason::UpToDate, "up_to_date"),
            (FailReason::Downgrade, "downgrade"),
            (FailReason::AttemptUnresolved, "attempt_unresolved"),
            (FailReason::ReportPending, "report_pending"),
            (FailReason::PartitionNotFound, "partition_not_found"),
            (FailReason::AttemptNotPersisted, "attempt_not_persisted"),
            (FailReason::VersionMismatch, "version_mismatch"),
            (FailReason::RollbackUnavailable, "rollback_unavailable"),
            (FailReason::RepairFailed, "repair_failed"),
            (FailReason::RepairNotNeeded, "repair_not_needed"),
            (
                FailReason::Ota(OtaError::DownloadFailed { status: 404 }),
                "download_failed",
            ),
            (
                FailReason::Ota(OtaError::DownloadTimeout),
                "download_timeout",
            ),
            (
                FailReason::Ota(OtaError::ChecksumMismatch),
                "checksum_mismatch",
            ),
        ];
        for (reason, label) in cases {
            let expected = alloc::format!(r#"{{"status":"failed","reason":"{label}"}}"#);
            assert_eq!(json(OtaStatus::Failed { reason }), expected, "{reason:?}");
        }
    }

    #[test]
    fn repaired_has_a_golden_json_and_u32_extremes() {
        assert_eq!(
            json(OtaStatus::Repaired { records: 3 }),
            r#"{"status":"repaired","records":3}"#
        );
        assert_eq!(
            json(OtaStatus::Repaired { records: u32::MAX }),
            r#"{"status":"repaired","records":4294967295}"#
        );
        assert_eq!(
            parse_ok(r#"{"status":"repaired","records":7}"#),
            Some(OtaStatus::Repaired { records: 7 })
        );
    }

    #[test]
    fn special_characters_in_applied_version_are_escaped() {
        let nasty = "a\"b\\c\nd\te";
        let text = json(OtaStatus::Applied { version: nasty });
        assert!(!text.contains('\n'));
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value.get("version").and_then(|v| v.as_str()), Some(nasty));
    }

    #[test]
    fn no_trailing_newline_or_spaces() {
        let text = json(OtaStatus::Downloading);
        assert_eq!(text.trim(), text);
        assert!(!text.contains(' '));
    }
}
