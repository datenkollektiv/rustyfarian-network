//! The frozen `failed { reason }` vocabulary of the OTA status topic.

use core::fmt;

use serde::de::{Deserializer, Error, Visitor};
use serde::{Deserialize, Serialize, Serializer};

use crate::ota::{Admission, BlockedBy, OfferDecision, OtaError};

/// Experimental: API may change before 1.0.
///
/// Every fixed `failed { reason }` code on the OTA status topic.
///
/// The strings are a wire contract shared with existing consumers and never change.
/// The [`Ota`](FailReason::Ota) variant carries a download or flash error whose code is [`OtaError::code`], unchanged.
///
/// # Examples
///
/// ```
/// use juggler::ota::{FailReason, OtaError};
///
/// assert_eq!(FailReason::UpToDate.as_str(), "up_to_date");
/// assert_eq!(FailReason::from(OtaError::DownloadTimeout).as_str(), "download_timeout");
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailReason {
    /// Intake: the command was oversized, malformed or of an unsupported shape.
    CommandInvalid,
    /// Intake: a command is already being worked on.
    Busy,
    /// Intake: the worker that runs commands is not running.
    WorkerUnavailable,
    /// Admission: the running image is still pending verification.
    PendingVerify,
    /// Admission: the manifest could not be fetched.
    ManifestFetch,
    /// Admission: the manifest was oversized, malformed or carried invalid metadata.
    ManifestInvalid,
    /// Admission: the manifest targets a different chip.
    TargetMismatch,
    /// Admission: the running version does not parse.
    VersionInvalid,
    /// Admission: the device rolled back from exactly this version.
    PreviouslyRolledBack,
    /// Admission: the offered version equals the running one.
    UpToDate,
    /// Admission: the offered version is older than the running one.
    Downgrade,
    /// Admission: an update attempt is unresolved.
    AttemptUnresolved,
    /// Admission: a rollback report is undelivered.
    ReportPending,
    /// Apply or rollback: the target partition does not exist.
    PartitionNotFound,
    /// Apply: the attempt record could not be persisted before activation.
    AttemptNotPersisted,
    /// Rollback: the command's `from` does not match the running version.
    VersionMismatch,
    /// Rollback: there is nothing to roll back to.
    RollbackUnavailable,
    /// Repair: the records could not be repaired (a storage error, or the repair did not converge).
    RepairFailed,
    /// Repair: no unreadable record exists, so nothing was changed.
    RepairNotNeeded,
    /// Any download or flash error; the code is [`OtaError::code`], unchanged.
    Ota(OtaError),
}

impl FailReason {
    /// The snake_case wire string.
    pub const fn as_str(&self) -> &'static str {
        match self {
            FailReason::CommandInvalid => "command_invalid",
            FailReason::Busy => "busy",
            FailReason::WorkerUnavailable => "worker_unavailable",
            FailReason::PendingVerify => "pending_verify",
            FailReason::ManifestFetch => "manifest_fetch",
            FailReason::ManifestInvalid => "manifest_invalid",
            FailReason::TargetMismatch => "target_mismatch",
            FailReason::VersionInvalid => "version_invalid",
            FailReason::PreviouslyRolledBack => "previously_rolled_back",
            FailReason::UpToDate => "up_to_date",
            FailReason::Downgrade => "downgrade",
            FailReason::AttemptUnresolved => "attempt_unresolved",
            FailReason::ReportPending => "report_pending",
            FailReason::PartitionNotFound => "partition_not_found",
            FailReason::AttemptNotPersisted => "attempt_not_persisted",
            FailReason::VersionMismatch => "version_mismatch",
            FailReason::RollbackUnavailable => "rollback_unavailable",
            FailReason::RepairFailed => "repair_failed",
            FailReason::RepairNotNeeded => "repair_not_needed",
            FailReason::Ota(e) => e.code(),
        }
    }

    /// Parses a wire code by exact match; the inverse of [`as_str`](FailReason::as_str).
    ///
    /// Covers the 19 fixed codes and every [`OtaError::code`].
    /// `partition_not_found` and `version_invalid` are both fixed codes and `OtaError` codes; they map to [`FailReason::PartitionNotFound`] and [`FailReason::VersionInvalid`], which serialize back to the same code.
    /// `download_failed` maps to `Ota(DownloadFailed { status: 0 })`, because the HTTP status is not on the wire; the code still round-trips.
    /// Anything else, including a different case or surrounding whitespace, is `None`.
    ///
    /// # Examples
    ///
    /// ```
    /// use juggler::ota::{FailReason, OtaError};
    ///
    /// assert_eq!(FailReason::from_code("up_to_date"), Some(FailReason::UpToDate));
    /// assert_eq!(
    ///     FailReason::from_code("download_failed"),
    ///     Some(FailReason::Ota(OtaError::DownloadFailed { status: 0 }))
    /// );
    /// assert_eq!(FailReason::from_code("nope"), None);
    /// ```
    pub fn from_code(code: &str) -> Option<FailReason> {
        Some(match code {
            "command_invalid" => FailReason::CommandInvalid,
            "busy" => FailReason::Busy,
            "worker_unavailable" => FailReason::WorkerUnavailable,
            "pending_verify" => FailReason::PendingVerify,
            "manifest_fetch" => FailReason::ManifestFetch,
            "manifest_invalid" => FailReason::ManifestInvalid,
            "target_mismatch" => FailReason::TargetMismatch,
            "version_invalid" => FailReason::VersionInvalid,
            "previously_rolled_back" => FailReason::PreviouslyRolledBack,
            "up_to_date" => FailReason::UpToDate,
            "downgrade" => FailReason::Downgrade,
            "attempt_unresolved" => FailReason::AttemptUnresolved,
            "report_pending" => FailReason::ReportPending,
            "partition_not_found" => FailReason::PartitionNotFound,
            "attempt_not_persisted" => FailReason::AttemptNotPersisted,
            "version_mismatch" => FailReason::VersionMismatch,
            "rollback_unavailable" => FailReason::RollbackUnavailable,
            "repair_failed" => FailReason::RepairFailed,
            "repair_not_needed" => FailReason::RepairNotNeeded,
            "server_unreachable" => FailReason::Ota(OtaError::ServerUnreachable),
            "download_failed" => FailReason::Ota(OtaError::DownloadFailed { status: 0 }),
            "download_timeout" => FailReason::Ota(OtaError::DownloadTimeout),
            "checksum_mismatch" => FailReason::Ota(OtaError::ChecksumMismatch),
            "flash_write_failed" => FailReason::Ota(OtaError::FlashWriteFailed),
            "insufficient_space" => FailReason::Ota(OtaError::InsufficientSpace),
            _ => return None,
        })
    }

    /// Maps an offer decision to the reason it is reported with.
    ///
    /// `Apply` is not a failure and maps to `None`.
    /// `admission` supplies the cause of `Blocked`: [`BlockedBy::AttemptUnresolved`] maps to
    /// `attempt_unresolved` and [`BlockedBy::ReportPending`] to `report_pending`.
    /// A `Blocked` decision with an `Open` admission cannot come from `decide_offer`
    /// (a caller error); it is reported as `attempt_unresolved` rather than panicking.
    pub const fn from_offer(decision: OfferDecision, admission: Admission) -> Option<FailReason> {
        match decision {
            OfferDecision::Apply => None,
            OfferDecision::Skip => Some(FailReason::UpToDate),
            OfferDecision::Reject => Some(FailReason::Downgrade),
            OfferDecision::Refused => Some(FailReason::PreviouslyRolledBack),
            OfferDecision::Blocked => Some(match admission {
                Admission::Blocked(BlockedBy::ReportPending) => FailReason::ReportPending,
                Admission::Blocked(BlockedBy::AttemptUnresolved) | Admission::Open => {
                    FailReason::AttemptUnresolved
                }
            }),
        }
    }
}

impl From<OtaError> for FailReason {
    fn from(e: OtaError) -> Self {
        FailReason::Ota(e)
    }
}

impl Serialize for FailReason {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// Deserializes from the wire string with [`FailReason::from_code`]; an unknown code is an error that does not echo it.
impl<'de> Deserialize<'de> for FailReason {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct CodeVisitor;
        impl Visitor<'_> for CodeVisitor {
            type Value = FailReason;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a known OTA failure code")
            }
            fn visit_str<E: Error>(self, v: &str) -> Result<FailReason, E> {
                FailReason::from_code(v).ok_or_else(|| E::custom("unknown OTA failure code"))
            }
        }
        deserializer.deserialize_str(CodeVisitor)
    }
}

impl fmt::Display for FailReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_code_inverts_as_str_for_every_fixed_code() {
        for (reason, code) in TABLE {
            let parsed = FailReason::from_code(code).expect(code);
            assert_eq!(parsed.as_str(), code);
            if !matches!(reason, FailReason::Ota(_)) {
                assert_eq!(parsed, reason);
            }
        }
    }

    #[test]
    fn from_code_covers_every_ota_error_code() {
        let errors = [
            OtaError::ServerUnreachable,
            OtaError::DownloadFailed { status: 503 },
            OtaError::DownloadTimeout,
            OtaError::ChecksumMismatch,
            OtaError::VersionInvalid,
            OtaError::FlashWriteFailed,
            OtaError::PartitionNotFound,
            OtaError::InsufficientSpace,
        ];
        for e in errors {
            let parsed = FailReason::from_code(e.code()).unwrap();
            assert_eq!(parsed.as_str(), e.code());
        }
    }

    #[test]
    fn download_failed_loses_its_status() {
        assert_eq!(
            FailReason::from_code("download_failed"),
            Some(FailReason::Ota(OtaError::DownloadFailed { status: 0 }))
        );
    }

    #[test]
    fn from_code_rejects_everything_else() {
        for code in [
            "",
            "Busy",
            " busy",
            "busy ",
            "nope",
            "up-to-date",
            "unknown",
        ] {
            assert_eq!(FailReason::from_code(code), None, "{code:?}");
        }
    }

    #[test]
    fn deserialize_uses_from_code_and_rejects_unknown_without_echo() {
        let ok: FailReason = serde_json::from_str("\"busy\"").unwrap();
        assert_eq!(ok, FailReason::Busy);
        let err = serde_json::from_str::<FailReason>("\"secretcode\"").unwrap_err();
        assert!(!alloc::format!("{err}").contains("secretcode"));
        assert!(serde_json::from_str::<FailReason>("5").is_err());
    }

    /// The frozen wire vocabulary.
    /// Changing a string here is a breaking change for every consumer.
    const TABLE: [(FailReason, &str); 19] = [
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
    ];

    #[test]
    fn as_str_matches_the_frozen_table() {
        for (reason, expected) in &TABLE {
            assert_eq!(reason.as_str(), *expected, "{reason:?}");
        }
    }

    #[test]
    fn ota_variant_is_exactly_ota_error_code() {
        let all = [
            OtaError::ServerUnreachable,
            OtaError::DownloadFailed { status: 404 },
            OtaError::DownloadTimeout,
            OtaError::ChecksumMismatch,
            OtaError::VersionInvalid,
            OtaError::FlashWriteFailed,
            OtaError::PartitionNotFound,
            OtaError::InsufficientSpace,
        ];
        for err in all {
            let code = err.code();
            assert_eq!(FailReason::Ota(err).as_str(), code);
            assert_eq!(FailReason::from(err).as_str(), code);
        }
    }

    #[test]
    fn display_writes_the_code() {
        assert_eq!(alloc::format!("{}", FailReason::UpToDate), "up_to_date");
        assert_eq!(
            alloc::format!("{}", FailReason::Ota(OtaError::DownloadTimeout)),
            "download_timeout"
        );
    }

    #[test]
    fn from_offer_maps_every_decision() {
        let open = Admission::Open;
        let attempt = Admission::Blocked(BlockedBy::AttemptUnresolved);
        let report = Admission::Blocked(BlockedBy::ReportPending);
        assert_eq!(FailReason::from_offer(OfferDecision::Apply, open), None);
        assert_eq!(FailReason::from_offer(OfferDecision::Apply, attempt), None);
        assert_eq!(
            FailReason::from_offer(OfferDecision::Skip, open),
            Some(FailReason::UpToDate)
        );
        assert_eq!(
            FailReason::from_offer(OfferDecision::Reject, open),
            Some(FailReason::Downgrade)
        );
        assert_eq!(
            FailReason::from_offer(OfferDecision::Refused, report),
            Some(FailReason::PreviouslyRolledBack)
        );
        assert_eq!(
            FailReason::from_offer(OfferDecision::Blocked, attempt),
            Some(FailReason::AttemptUnresolved)
        );
        assert_eq!(
            FailReason::from_offer(OfferDecision::Blocked, report),
            Some(FailReason::ReportPending)
        );
    }

    #[test]
    fn from_offer_blocked_with_open_admission_is_attempt_unresolved() {
        assert_eq!(
            FailReason::from_offer(OfferDecision::Blocked, Admission::Open),
            Some(FailReason::AttemptUnresolved)
        );
    }
}
