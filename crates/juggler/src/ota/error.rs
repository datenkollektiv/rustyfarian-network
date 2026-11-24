//! OTA error types.

use core::fmt;

/// Experimental: API may change before 1.0.
///
/// Errors that can occur during OTA updates.
///
/// The enum is `#[non_exhaustive]`: a later release may add a variant, so a
/// `match` outside this crate needs a wildcard arm.
/// [`code`](Self::code) gives every variant, present and future, a stable string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
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
    /// The download was cancelled before the new boot slot was selected (`DownloadMachine::cancel`).
    ///
    /// The boot slot is unchanged.
    Cancelled,
    /// The download link failed the DDI origin check: its scheme, host or port differs from the configured DDI server, or it is not of the accepted URL shape; no request was made.
    ForeignOrigin,
    /// The download request was answered with a 3xx status; redirects are never followed.
    Redirect {
        /// The 3xx HTTP status code returned by the server, or `0` when parsed from a wire code.
        status: u16,
    },
    /// The declared `Content-Length` differs from the size the DDI deployment document gives for the artifact; nothing was written to flash.
    SizeMismatch,
    /// The image header names a different chip than the one expected (`DownloadMachine::with_expected_chip`); nothing was written to flash.
    ///
    /// Its code is `target_mismatch`, the same string as the fixed `FailReason::TargetMismatch`.
    TargetMismatch,
    /// Another download holds the flash: this one erased and wrote nothing, and the boot slot is unchanged.
    ///
    /// It goes away when the other download ends, so the DDI retry policy treats it as transient.
    FlashBusy,
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
            OtaError::Cancelled => "cancelled",
            OtaError::ForeignOrigin => "foreign_origin",
            OtaError::Redirect { .. } => "redirect",
            OtaError::SizeMismatch => "size_mismatch",
            OtaError::TargetMismatch => "target_mismatch",
            OtaError::FlashBusy => "flash_busy",
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
            OtaError::Cancelled => write!(f, "Download cancelled"),
            OtaError::ForeignOrigin => write!(f, "Download link refused by the origin check"),
            OtaError::Redirect { status } => write!(f, "Download redirected with status {status}"),
            OtaError::SizeMismatch => write!(f, "Download length differs from the artifact size"),
            OtaError::TargetMismatch => write!(f, "Image header targets a different chip"),
            OtaError::FlashBusy => write!(f, "Flash held by another download"),
        }
    }
}

/// Experimental: API may change before 1.0.
///
/// Classifies a failed body read.
///
/// `is_timeout` is `true` when the read gave up because its wait elapsed
/// (a stalled server) and `false` for any other transport failure such as a
/// connection reset.
/// A timeout is [`OtaError::DownloadTimeout`]; everything else is
/// [`OtaError::ServerUnreachable`].
///
/// ```
/// use juggler::ota::{classify_read_error, OtaError};
///
/// assert_eq!(classify_read_error(true), OtaError::DownloadTimeout);
/// assert_eq!(classify_read_error(false), OtaError::ServerUnreachable);
/// ```
pub const fn classify_read_error(is_timeout: bool) -> OtaError {
    if is_timeout {
        OtaError::DownloadTimeout
    } else {
        OtaError::ServerUnreachable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The published wire contract as fixed strings, independent of `code()`.
    /// Changing a string here is a breaking change for every backend.
    /// `DownloadFailed` and `Redirect` appear twice: the status never changes the code.
    const GOLDEN: [(OtaError, &str); 16] = [
        (OtaError::ServerUnreachable, "server_unreachable"),
        (OtaError::DownloadFailed { status: 404 }, "download_failed"),
        (OtaError::DownloadFailed { status: 0 }, "download_failed"),
        (OtaError::DownloadTimeout, "download_timeout"),
        (OtaError::ChecksumMismatch, "checksum_mismatch"),
        (OtaError::VersionInvalid, "version_invalid"),
        (OtaError::FlashWriteFailed, "flash_write_failed"),
        (OtaError::PartitionNotFound, "partition_not_found"),
        (OtaError::InsufficientSpace, "insufficient_space"),
        (OtaError::Cancelled, "cancelled"),
        (OtaError::ForeignOrigin, "foreign_origin"),
        (OtaError::Redirect { status: 302 }, "redirect"),
        (OtaError::Redirect { status: 0 }, "redirect"),
        (OtaError::SizeMismatch, "size_mismatch"),
        (OtaError::TargetMismatch, "target_mismatch"),
        (OtaError::FlashBusy, "flash_busy"),
    ];

    /// Marks the variant of `err` as covered.
    ///
    /// Nothing here is counted by hand; each step of adding a variant fails
    /// loudly: the exhaustive match stops compiling until the variant gets an
    /// arm, the arm's new index overflows `covered` (rejected at build time by
    /// the deny-by-default `unconditional_panic` lint) until the array grows,
    /// and `golden_covers_every_variant` fails until `GOLDEN` gets a row.
    fn mark(err: &OtaError, covered: &mut [bool; 14]) {
        match err {
            OtaError::ServerUnreachable => covered[0] = true,
            OtaError::DownloadFailed { .. } => covered[1] = true,
            OtaError::DownloadTimeout => covered[2] = true,
            OtaError::ChecksumMismatch => covered[3] = true,
            OtaError::VersionInvalid => covered[4] = true,
            OtaError::FlashWriteFailed => covered[5] = true,
            OtaError::PartitionNotFound => covered[6] = true,
            OtaError::InsufficientSpace => covered[7] = true,
            OtaError::Cancelled => covered[8] = true,
            OtaError::ForeignOrigin => covered[9] = true,
            OtaError::Redirect { .. } => covered[10] = true,
            OtaError::SizeMismatch => covered[11] = true,
            OtaError::TargetMismatch => covered[12] = true,
            OtaError::FlashBusy => covered[13] = true,
        }
    }

    #[test]
    fn golden_covers_every_variant() {
        let mut covered = [false; 14];
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
                // Same variant (DownloadFailed or Redirect with different status) shares a code.
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

    #[test]
    fn classify_read_error_maps_timeout_and_other() {
        assert_eq!(classify_read_error(true), OtaError::DownloadTimeout);
        assert_eq!(classify_read_error(false), OtaError::ServerUnreachable);
        assert_eq!(classify_read_error(true).code(), "download_timeout");
        assert_eq!(classify_read_error(false).code(), "server_unreachable");
    }
}
