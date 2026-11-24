//! The numeric profile codes a DDI feedback carries, and the map from the shared failure reasons.

use serde::{Serialize, Serializer};

use crate::ota::wire::FailReason;
use crate::ota::OtaError;

/// Experimental: API may change before 1.0.
///
/// The numeric `code` of a DDI feedback: why the device did not finish an update.
/// The codes serialize as plain numbers.
///
/// [`for_reason`](ProfileCode::for_reason) maps every [`FailReason`] to a code or to `None` (nothing is reported):
///
/// | Code | Reasons |
/// |:-----|:--------|
/// | `Transfer` 1001 | `ManifestFetch`; `Ota(ServerUnreachable)`, `Ota(DownloadFailed)` for any status including 0, `Ota(DownloadTimeout)`, `Ota(SizeMismatch)` (a wrong body length is a transfer failure, fleet DDI profile §4) |
/// | `Checksum` 1002 | `Ota(ChecksumMismatch)` |
/// | `Flash` 1003 | `Ota(FlashWriteFailed)`, `PartitionNotFound`, `Ota(PartitionNotFound)`, `Ota(InsufficientSpace)`, `Ota(FlashBusy)` (still busy when the download deadline passed) |
/// | `RolledBack` 1004 | none: the machine uses it directly for a rollback report |
/// | `Device` 1005 | `WorkerUnavailable`, `AttemptNotPersisted`, `RepairFailed`, `VersionInvalid`, `Ota(VersionInvalid)` (the RUNNING version does not parse) |
/// | `Deployment` 1006 | `ManifestInvalid`, `Ota(ForeignOrigin)`, `Ota(Redirect)` for any status (the profile allows only `manifest_invalid`, `foreign_origin` or `redirect` as its `details[0]`) |
/// | `Policy` 1007 | `UpToDate`, `Downgrade`, `BelowFloor`, `PreviouslyRolledBack`, `TargetMismatch`, `Ota(TargetMismatch)` (the image header names another chip) |
///
/// `Ota(Cancelled)` maps to `None`: an accepted cancel is acknowledged on the `cancelAction` feedback, not reported as a failure.
///
/// `Policy` (1007) is part of the fleet DDI profile 0.4.0, frozen on 2026-11-21.
///
/// # Examples
///
/// ```
/// use juggler::ota::ddi::wire::ProfileCode;
/// use juggler::ota::{FailReason, OtaError};
///
/// assert_eq!(ProfileCode::Checksum.as_u16(), 1002);
/// assert_eq!(
///     ProfileCode::for_reason(FailReason::Ota(OtaError::ChecksumMismatch)),
///     Some(ProfileCode::Checksum)
/// );
/// assert_eq!(ProfileCode::for_reason(FailReason::Busy), None);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProfileCode {
    /// 1001: the manifest or image could not be fetched.
    Transfer = 1001,
    /// 1002: the image digest did not match.
    Checksum = 1002,
    /// 1003: the image could not be written.
    Flash = 1003,
    /// 1004: the device rolled back.
    RolledBack = 1004,
    /// 1005: a device-side fault.
    Device = 1005,
    /// 1006: the deployment itself is unusable.
    Deployment = 1006,
    /// 1007: a policy refused the update.
    Policy = 1007,
}

impl ProfileCode {
    /// The number on the wire.
    pub const fn as_u16(self) -> u16 {
        self as u16
    }

    /// The code for a number on the wire, or `None` outside 1001 to 1007; the inverse of [`as_u16`](ProfileCode::as_u16).
    pub const fn from_u16(number: u16) -> Option<ProfileCode> {
        Some(match number {
            1001 => ProfileCode::Transfer,
            1002 => ProfileCode::Checksum,
            1003 => ProfileCode::Flash,
            1004 => ProfileCode::RolledBack,
            1005 => ProfileCode::Device,
            1006 => ProfileCode::Deployment,
            1007 => ProfileCode::Policy,
            _ => return None,
        })
    }

    /// The code that reports `reason`, or `None` when the reason is never reported over DDI.
    ///
    /// See the type documentation for the full table.
    /// The match is exhaustive, so a new [`FailReason`] variant fails to compile until it is mapped.
    pub const fn for_reason(reason: FailReason) -> Option<ProfileCode> {
        match reason {
            FailReason::ManifestFetch => Some(ProfileCode::Transfer),
            FailReason::WorkerUnavailable
            | FailReason::AttemptNotPersisted
            | FailReason::RepairFailed
            | FailReason::VersionInvalid => Some(ProfileCode::Device),
            FailReason::PartitionNotFound => Some(ProfileCode::Flash),
            FailReason::ManifestInvalid => Some(ProfileCode::Deployment),
            FailReason::UpToDate
            | FailReason::Downgrade
            | FailReason::BelowFloor
            | FailReason::PreviouslyRolledBack
            | FailReason::TargetMismatch => Some(ProfileCode::Policy),
            FailReason::CommandInvalid
            | FailReason::Busy
            | FailReason::PendingVerify
            | FailReason::AttemptUnresolved
            | FailReason::ReportPending
            | FailReason::VersionMismatch
            | FailReason::RollbackUnavailable
            | FailReason::RepairNotNeeded => None,
            FailReason::Ota(error) => match error {
                OtaError::ServerUnreachable
                | OtaError::DownloadFailed { .. }
                | OtaError::DownloadTimeout
                | OtaError::SizeMismatch => Some(ProfileCode::Transfer),
                OtaError::ChecksumMismatch => Some(ProfileCode::Checksum),
                OtaError::VersionInvalid => Some(ProfileCode::Device),
                OtaError::FlashWriteFailed
                | OtaError::PartitionNotFound
                | OtaError::InsufficientSpace
                | OtaError::FlashBusy => Some(ProfileCode::Flash),
                OtaError::ForeignOrigin | OtaError::Redirect { .. } => {
                    Some(ProfileCode::Deployment)
                }
                OtaError::TargetMismatch => Some(ProfileCode::Policy),
                OtaError::Cancelled => None,
            },
        }
    }
}

impl Serialize for ProfileCode {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u16(self.as_u16())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ProfileCode::{Checksum, Deployment, Device, Flash, Policy, Transfer};

    /// Every `FailReason` variant and every `OtaError` inside `Ota`, with the exact code.
    const TABLE: [(FailReason, Option<ProfileCode>); 37] = [
        (FailReason::CommandInvalid, None),
        (FailReason::Busy, None),
        (FailReason::WorkerUnavailable, Some(Device)),
        (FailReason::PendingVerify, None),
        (FailReason::ManifestFetch, Some(Transfer)),
        (FailReason::ManifestInvalid, Some(Deployment)),
        (FailReason::TargetMismatch, Some(Policy)),
        (FailReason::VersionInvalid, Some(Device)),
        (FailReason::PreviouslyRolledBack, Some(Policy)),
        (FailReason::UpToDate, Some(Policy)),
        (FailReason::Downgrade, Some(Policy)),
        (FailReason::BelowFloor, Some(Policy)),
        (FailReason::AttemptUnresolved, None),
        (FailReason::ReportPending, None),
        (FailReason::PartitionNotFound, Some(Flash)),
        (FailReason::AttemptNotPersisted, Some(Device)),
        (FailReason::VersionMismatch, None),
        (FailReason::RollbackUnavailable, None),
        (FailReason::RepairFailed, Some(Device)),
        (FailReason::RepairNotNeeded, None),
        (FailReason::Ota(OtaError::ServerUnreachable), Some(Transfer)),
        (
            FailReason::Ota(OtaError::DownloadFailed { status: 0 }),
            Some(Transfer),
        ),
        (
            FailReason::Ota(OtaError::DownloadFailed { status: 503 }),
            Some(Transfer),
        ),
        (
            FailReason::Ota(OtaError::DownloadFailed { status: 404 }),
            Some(Transfer),
        ),
        (FailReason::Ota(OtaError::DownloadTimeout), Some(Transfer)),
        (FailReason::Ota(OtaError::ChecksumMismatch), Some(Checksum)),
        (FailReason::Ota(OtaError::VersionInvalid), Some(Device)),
        (FailReason::Ota(OtaError::FlashWriteFailed), Some(Flash)),
        (FailReason::Ota(OtaError::PartitionNotFound), Some(Flash)),
        (FailReason::Ota(OtaError::InsufficientSpace), Some(Flash)),
        (FailReason::Ota(OtaError::Cancelled), None),
        (FailReason::Ota(OtaError::ForeignOrigin), Some(Deployment)),
        (
            FailReason::Ota(OtaError::Redirect { status: 302 }),
            Some(Deployment),
        ),
        (
            FailReason::Ota(OtaError::Redirect { status: 0 }),
            Some(Deployment),
        ),
        (FailReason::Ota(OtaError::SizeMismatch), Some(Transfer)),
        (FailReason::Ota(OtaError::TargetMismatch), Some(Policy)),
        (FailReason::Ota(OtaError::FlashBusy), Some(Flash)),
    ];

    #[test]
    fn every_reason_maps_to_its_documented_code() {
        for (reason, code) in TABLE {
            assert_eq!(ProfileCode::for_reason(reason), code, "{reason:?}");
        }
    }

    #[test]
    fn numbers_are_1001_to_1007() {
        let all = [
            (ProfileCode::Transfer, 1001),
            (ProfileCode::Checksum, 1002),
            (ProfileCode::Flash, 1003),
            (ProfileCode::RolledBack, 1004),
            (ProfileCode::Device, 1005),
            (ProfileCode::Deployment, 1006),
            (ProfileCode::Policy, 1007),
        ];
        for (code, n) in all {
            assert_eq!(code.as_u16(), n);
            assert_eq!(ProfileCode::from_u16(n), Some(code));
        }
        for n in [0, 1000, 1008, u16::MAX] {
            assert_eq!(ProfileCode::from_u16(n), None);
        }
    }

    #[test]
    fn rolled_back_is_never_a_reason_mapping() {
        for (_, code) in TABLE {
            assert_ne!(code, Some(ProfileCode::RolledBack));
        }
    }

    #[test]
    fn serializes_as_a_number() {
        assert_eq!(serde_json::to_string(&ProfileCode::Flash).unwrap(), "1003");
        assert_eq!(serde_json::to_string(&ProfileCode::Policy).unwrap(), "1007");
    }
}
