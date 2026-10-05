//! The `rolled_back { reason }` vocabulary of the OTA status topic.

use core::fmt;

use serde::de::{Deserializer, Error, Visitor};
use serde::{Deserialize, Serialize, Serializer};

/// Experimental: API may change before 1.0.
///
/// Why the device rolled back to the previous image.
///
/// The strings are a wire contract shared with existing consumers and never change.
/// Wire input is parsed strictly with [`parse`](RollbackReason::parse) (and `Deserialize`); a consumer that persists the label with its report record reads it back leniently with [`from_label`](RollbackReason::from_label).
///
/// # Examples
///
/// ```
/// use juggler::ota::RollbackReason;
///
/// assert_eq!(RollbackReason::HealthDeadline.as_str(), "health_deadline");
/// assert_eq!(RollbackReason::parse("health_deadline"), Some(RollbackReason::HealthDeadline));
/// assert_eq!(RollbackReason::parse("garbage"), None);
/// assert_eq!(RollbackReason::from_label("health_deadline"), RollbackReason::HealthDeadline);
/// assert_eq!(RollbackReason::from_label("garbage"), RollbackReason::Unknown);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RollbackReason {
    /// An operator asked for the rollback.
    Operator,
    /// The new image did not report healthy before its deadline.
    HealthDeadline,
    /// The app's health predicate reported unhealthy and the image was rolled back (e.g. by restarting into the bootloader's rollback) before the health deadline.
    Unhealthy,
    /// The booted image is not the promised version.
    VersionMismatch,
    /// The bootloader rolled back on its own.
    Bootloader,
    /// The cause is not known, or the stored label was not recognised.
    Unknown,
}

impl RollbackReason {
    /// The snake_case wire string.
    pub const fn as_str(&self) -> &'static str {
        match self {
            RollbackReason::Operator => "operator",
            RollbackReason::HealthDeadline => "health_deadline",
            RollbackReason::Unhealthy => "unhealthy",
            RollbackReason::VersionMismatch => "version_mismatch",
            RollbackReason::Bootloader => "bootloader",
            RollbackReason::Unknown => "unknown",
        }
    }

    /// Parses a wire label by exact match; anything else is `None`.
    ///
    /// Accepts exactly the six wire strings.
    pub fn parse(label: &str) -> Option<RollbackReason> {
        match label {
            "operator" => Some(RollbackReason::Operator),
            "health_deadline" => Some(RollbackReason::HealthDeadline),
            "unhealthy" => Some(RollbackReason::Unhealthy),
            "version_mismatch" => Some(RollbackReason::VersionMismatch),
            "bootloader" => Some(RollbackReason::Bootloader),
            "unknown" => Some(RollbackReason::Unknown),
            _ => None,
        }
    }

    /// Parses a label read back from storage; anything unrecognised maps to [`RollbackReason::Unknown`].
    ///
    /// This is lenient on purpose, for stored labels that may be corrupt or written by another firmware version.
    /// Wire input must use the strict [`parse`](RollbackReason::parse) or `Deserialize` instead.
    pub fn from_label(label: &str) -> RollbackReason {
        RollbackReason::parse(label).unwrap_or(RollbackReason::Unknown)
    }
}

impl Serialize for RollbackReason {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// Deserializes from the wire string with [`RollbackReason::parse`]; an unknown label is an error with fixed text that never quotes the input.
impl<'de> Deserialize<'de> for RollbackReason {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct LabelVisitor;
        impl Visitor<'_> for LabelVisitor {
            type Value = RollbackReason;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("an OTA rollback reason label")
            }
            fn visit_str<E: Error>(self, v: &str) -> Result<RollbackReason, E> {
                RollbackReason::parse(v).ok_or_else(|| E::custom("unknown OTA rollback label"))
            }
        }
        deserializer.deserialize_str(LabelVisitor)
    }
}

impl fmt::Display for RollbackReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [(RollbackReason, &str); 6] = [
        (RollbackReason::Operator, "operator"),
        (RollbackReason::HealthDeadline, "health_deadline"),
        (RollbackReason::Unhealthy, "unhealthy"),
        (RollbackReason::VersionMismatch, "version_mismatch"),
        (RollbackReason::Bootloader, "bootloader"),
        (RollbackReason::Unknown, "unknown"),
    ];

    #[test]
    fn as_str_matches_the_frozen_table() {
        for (reason, expected) in ALL {
            assert_eq!(reason.as_str(), expected);
            assert_eq!(alloc::format!("{reason}"), expected);
        }
    }

    #[test]
    fn from_label_round_trips_every_variant() {
        for (reason, label) in ALL {
            assert_eq!(RollbackReason::from_label(label), reason);
            assert_eq!(RollbackReason::from_label(reason.as_str()), reason);
        }
    }

    #[test]
    fn deserialize_matches_parse() {
        for (reason, label) in ALL {
            let json = alloc::format!("\"{label}\"");
            assert_eq!(
                serde_json::from_str::<RollbackReason>(&json).unwrap(),
                reason
            );
        }
        assert!(serde_json::from_str::<RollbackReason>("\"garbage\"").is_err());
        assert!(serde_json::from_str::<RollbackReason>("1").is_err());
    }

    #[test]
    fn unknown_labels_fall_back_to_unknown() {
        for label in [
            "",
            "Unhealthy_demo",
            "Operator",
            " operator",
            "operator ",
            "nope",
            "health-deadline",
        ] {
            assert_eq!(RollbackReason::from_label(label), RollbackReason::Unknown);
        }
    }

    #[test]
    fn the_retired_unhealthy_demo_label_is_rejected() {
        assert_eq!(RollbackReason::parse("unhealthy_demo"), None);
        assert_eq!(
            RollbackReason::from_label("unhealthy_demo"),
            RollbackReason::Unknown
        );
        assert!(serde_json::from_str::<RollbackReason>("\"unhealthy_demo\"").is_err());
    }

    #[test]
    fn parse_round_trips_every_variant() {
        for (reason, label) in ALL {
            assert_eq!(RollbackReason::parse(label), Some(reason));
        }
    }

    #[test]
    fn unknown_label_is_strict_for_parse_and_deserialize_with_fixed_text() {
        assert_eq!(RollbackReason::parse("secretlabel"), None);
        assert_eq!(
            RollbackReason::from_label("secretlabel"),
            RollbackReason::Unknown
        );
        let err = serde_json::from_str::<RollbackReason>("\"secretlabel\"").unwrap_err();
        let text = alloc::format!("{err} {err:?}");
        assert!(text.contains("unknown OTA rollback label"));
        assert!(!text.contains("secret"));
    }
}
