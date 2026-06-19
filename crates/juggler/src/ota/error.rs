//! OTA error types.

use core::fmt;

/// Experimental: API may change before 1.0.
///
/// Errors that can occur during OTA updates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OtaError {
    /// The update server could not be reached.
    ServerUnreachable,
    /// The download request returned a non-200 HTTP status, or the server
    /// response failed a strict-protocol check.
    ///
    /// `status == 0` is a sentinel for "protocol-shape rejection" — used by
    /// the bare-metal HTTP client when a response is syntactically rejected
    /// before a status code is meaningful (e.g. `Transfer-Encoding: chunked`
    /// or another unsupported response shape). Any non-zero value is the
    /// HTTP status code returned by the server.
    DownloadFailed {
        /// HTTP status code returned by the server, or `0` for a
        /// protocol-shape rejection (see variant docs).
        status: u16,
    },
    /// The download did not complete within the allowed time.
    DownloadTimeout,
    /// The computed SHA-256 digest does not match the expected value.
    ChecksumMismatch,
    /// The version string could not be parsed.
    VersionInvalid,
    /// Writing to flash failed.
    FlashWriteFailed,
    /// The OTA partition could not be located.
    PartitionNotFound,
    /// There is not enough flash space for the new image.
    InsufficientSpace,
}

impl fmt::Display for OtaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OtaError::ServerUnreachable => write!(f, "Update server unreachable"),
            OtaError::DownloadFailed { status } => {
                write!(f, "Download failed with status {status}")
            }
            OtaError::DownloadTimeout => write!(f, "Download timeout"),
            OtaError::ChecksumMismatch => write!(f, "Firmware checksum mismatch"),
            OtaError::VersionInvalid => write!(f, "Firmware version invalid"),
            OtaError::FlashWriteFailed => write!(f, "Flash write failed"),
            OtaError::PartitionNotFound => write!(f, "OTA partition not found"),
            OtaError::InsufficientSpace => write!(f, "Insufficient flash space"),
        }
    }
}
