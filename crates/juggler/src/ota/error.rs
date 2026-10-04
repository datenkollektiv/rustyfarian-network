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

impl OtaError {
    /// Experimental: API may change before 1.0.
    ///
    /// Stable, machine-readable wire code for this error.
    ///
    /// These strings are a STABLE wire contract (e.g. for status reports to a
    /// backend): a code never changes meaning, and new variants get new codes.
    /// Codes are lowercase snake_case ASCII. `DownloadFailed` maps to one code
    /// regardless of its status.
    ///
    /// Report the HTTP status (`DownloadFailed.status`) and the operation
    /// context alongside the code when available.
    ///
    /// ```
    /// use juggler::ota::OtaError;
    ///
    /// assert_eq!(OtaError::DownloadFailed { status: 404 }.code(), "download_failed");
    /// assert_eq!(OtaError::ChecksumMismatch.code(), "checksum_mismatch");
    /// ```
    pub const fn code(&self) -> &'static str {
        match self {
            OtaError::ServerUnreachable => "server_unreachable",
            OtaError::DownloadFailed { .. } => "download_failed",
            OtaError::DownloadTimeout => "download_timeout",
            OtaError::ChecksumMismatch => "checksum_mismatch",
            OtaError::VersionInvalid => "version_invalid",
            OtaError::FlashWriteFailed => "flash_write_failed",
            OtaError::PartitionNotFound => "partition_not_found",
            OtaError::InsufficientSpace => "insufficient_space",
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The published wire contract as fixed strings, independent of `code()`.
    /// Changing a string here is a breaking change for every backend.
    /// `DownloadFailed` appears twice: the status never changes its code.
    const GOLDEN: [(OtaError, &str); 9] = [
        (OtaError::ServerUnreachable, "server_unreachable"),
        (OtaError::DownloadFailed { status: 404 }, "download_failed"),
        (OtaError::DownloadFailed { status: 0 }, "download_failed"),
        (OtaError::DownloadTimeout, "download_timeout"),
        (OtaError::ChecksumMismatch, "checksum_mismatch"),
        (OtaError::VersionInvalid, "version_invalid"),
        (OtaError::FlashWriteFailed, "flash_write_failed"),
        (OtaError::PartitionNotFound, "partition_not_found"),
        (OtaError::InsufficientSpace, "insufficient_space"),
    ];

    /// Marks the variant of `err` as covered.
    ///
    /// Nothing here is counted by hand; each step of adding a variant fails
    /// loudly: the exhaustive match stops compiling until the variant gets an
    /// arm, the arm's new index overflows `covered` (rejected at build time by
    /// the deny-by-default `unconditional_panic` lint) until the array grows,
    /// and `golden_covers_every_variant` fails until `GOLDEN` gets a row.
    fn mark(err: &OtaError, covered: &mut [bool; 8]) {
        match err {
            OtaError::ServerUnreachable => covered[0] = true,
            OtaError::DownloadFailed { .. } => covered[1] = true,
            OtaError::DownloadTimeout => covered[2] = true,
            OtaError::ChecksumMismatch => covered[3] = true,
            OtaError::VersionInvalid => covered[4] = true,
            OtaError::FlashWriteFailed => covered[5] = true,
            OtaError::PartitionNotFound => covered[6] = true,
            OtaError::InsufficientSpace => covered[7] = true,
        }
    }

    #[test]
    fn golden_covers_every_variant() {
        let mut covered = [false; 8];
        for (err, _) in &GOLDEN {
            mark(err, &mut covered);
        }
        assert!(covered.iter().all(|c| *c), "GOLDEN misses a variant");
    }

    #[test]
    fn codes_match_the_golden_table() {
        for (err, expected) in &GOLDEN {
            assert_eq!(err.code(), *expected, "{err:?}");
        }
    }

    #[test]
    fn codes_are_distinct_per_variant() {
        for (i, (a, _)) in GOLDEN.iter().enumerate() {
            for (b, _) in &GOLDEN[i + 1..] {
                // Same variant (DownloadFailed with different status) shares a code.
                let same_variant = core::mem::discriminant(a) == core::mem::discriminant(b);
                assert_eq!(a.code() == b.code(), same_variant, "{a:?} vs {b:?}");
            }
        }
    }

    #[test]
    fn codes_are_lowercase_snake_case_ascii() {
        for (err, _) in &GOLDEN {
            let code = err.code();
            assert!(!code.is_empty());
            assert!(
                code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{code}"
            );
        }
    }
}
