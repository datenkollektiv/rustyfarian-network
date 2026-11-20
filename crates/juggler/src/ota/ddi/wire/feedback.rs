//! The DDI feedback body a device posts after (or while) working on an action.

use alloc::string::String;
use core::fmt;

use serde::{Serialize, Serializer};

use super::code::ProfileCode;
use super::{MAX_DETAILS, MAX_DETAIL_BYTES, MAX_FEEDBACK_BYTES};

/// Experimental: API may change before 1.0.
///
/// The `status.execution` of a feedback; the wire strings are the lowercase variant names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Execution {
    /// `proceeding`.
    Proceeding,
    /// `download`.
    Download,
    /// `downloaded`.
    Downloaded,
    /// `closed`.
    Closed,
    /// `canceled`.
    Canceled,
    /// `rejected`.
    Rejected,
    /// `resumed`.
    Resumed,
    /// `scheduled`.
    Scheduled,
}

impl Execution {
    const fn as_str(self) -> &'static str {
        match self {
            Execution::Proceeding => "proceeding",
            Execution::Download => "download",
            Execution::Downloaded => "downloaded",
            Execution::Closed => "closed",
            Execution::Canceled => "canceled",
            Execution::Rejected => "rejected",
            Execution::Resumed => "resumed",
            Execution::Scheduled => "scheduled",
        }
    }
}

impl Serialize for Execution {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// Experimental: API may change before 1.0.
///
/// The `status.result.finished` of a feedback.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Finished {
    /// `success`.
    Success,
    /// `failure`.
    Failure,
    /// `none`: the action is not finished.
    None,
}

impl Finished {
    const fn as_str(self) -> &'static str {
        match self {
            Finished::Success => "success",
            Finished::Failure => "failure",
            Finished::None => "none",
        }
    }
}

impl Serialize for Finished {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

/// Experimental: API may change before 1.0.
///
/// A DDI feedback, encoded by [`encode`](Feedback::encode) as compact JSON in a fixed field order.
///
/// By convention `details[0]` is the fixed reason string, `FailReason::as_str()`, so a backend can match on it; this is documented, not enforced.
/// Details are free text only for the backend operator, so keep them to fixed strings and never put a link or credential in one.
///
/// # Examples
///
/// ```
/// use juggler::ota::ddi::wire::{Execution, Feedback, Finished, ProfileCode};
///
/// let feedback = Feedback {
///     execution: Execution::Closed,
///     finished: Finished::Failure,
///     code: Some(ProfileCode::Checksum),
///     details: &["checksum_mismatch"],
///     timestamp: Some(1_700_000_000),
/// };
/// assert_eq!(
///     feedback.encode().unwrap(),
///     r#"{"status":{"execution":"closed","result":{"finished":"failure"},"code":1002,"details":["checksum_mismatch"]},"timestamp":1700000000}"#
/// );
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Feedback<'a> {
    /// `status.execution`.
    pub execution: Execution,
    /// `status.result.finished`.
    pub finished: Finished,
    /// `status.code`; omitted when `None`.
    pub code: Option<ProfileCode>,
    /// `status.details`: at most [`MAX_DETAILS`] entries of at most [`MAX_DETAIL_BYTES`] bytes each.
    pub details: &'a [&'a str],
    /// The profile's `timestamp` number, supplied by the machine and passed through unchanged; omitted when `None`.
    pub timestamp: Option<u64>,
}

/// Experimental: API may change before 1.0.
///
/// Why a feedback was refused: it would exceed a limit, and it is never truncated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackError {
    /// More than [`MAX_DETAILS`] details.
    TooManyDetails,
    /// A detail longer than [`MAX_DETAIL_BYTES`] bytes.
    DetailTooLong,
    /// The encoded body exceeds [`MAX_FEEDBACK_BYTES`] bytes (JSON escaping can grow the details).
    TooLarge,
}

impl fmt::Display for FeedbackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FeedbackError::TooManyDetails => f.write_str("DDI feedback has too many details"),
            FeedbackError::DetailTooLong => f.write_str("DDI feedback detail too long"),
            FeedbackError::TooLarge => f.write_str("DDI feedback too large"),
        }
    }
}

#[derive(Serialize)]
struct WireFeedback<'a> {
    status: WireStatus<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    timestamp: Option<u64>,
}

#[derive(Serialize)]
struct WireStatus<'a> {
    execution: Execution,
    result: WireResult,
    #[serde(skip_serializing_if = "Option::is_none")]
    code: Option<ProfileCode>,
    details: &'a [&'a str],
}

#[derive(Serialize)]
struct WireResult {
    finished: Finished,
}

impl Feedback<'_> {
    /// Encodes the feedback as compact JSON: `status` (`execution`, `result.finished`, `code`, `details`) then `timestamp`, no trailing newline.
    ///
    /// `code` and `timestamp` are omitted when `None`; `details` is always present.
    /// Control characters and quotes in details are JSON-escaped.
    ///
    /// # Errors
    ///
    /// [`FeedbackError::TooManyDetails`] and [`FeedbackError::DetailTooLong`] are checked first, before encoding;
    /// [`FeedbackError::TooLarge`] when the escaped body exceeds [`MAX_FEEDBACK_BYTES`] bytes.
    pub fn encode(&self) -> Result<String, FeedbackError> {
        if self.details.len() > MAX_DETAILS {
            return Err(FeedbackError::TooManyDetails);
        }
        if self.details.iter().any(|d| d.len() > MAX_DETAIL_BYTES) {
            return Err(FeedbackError::DetailTooLong);
        }
        let wire = WireFeedback {
            status: WireStatus {
                execution: self.execution,
                result: WireResult {
                    finished: self.finished,
                },
                code: self.code,
                details: self.details,
            },
            timestamp: self.timestamp,
        };
        // Serializing these plain types cannot fail; the only variant that fits a refusal is `TooLarge`.
        let body = serde_json::to_string(&wire).map_err(|_| FeedbackError::TooLarge)?;
        if body.len() > MAX_FEEDBACK_BYTES {
            return Err(FeedbackError::TooLarge);
        }
        Ok(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ota::wire::FailReason;
    use alloc::format;
    use alloc::vec::Vec;

    fn fb<'a>(details: &'a [&'a str]) -> Feedback<'a> {
        Feedback {
            execution: Execution::Closed,
            finished: Finished::Failure,
            code: Some(ProfileCode::Checksum),
            details,
            timestamp: Some(1_700_000_000),
        }
    }

    #[test]
    fn golden_full_feedback() {
        assert_eq!(
            fb(&["checksum_mismatch"]).encode().unwrap(),
            r#"{"status":{"execution":"closed","result":{"finished":"failure"},"code":1002,"details":["checksum_mismatch"]},"timestamp":1700000000}"#
        );
    }

    #[test]
    fn every_execution_has_its_wire_string() {
        let all = [
            (Execution::Proceeding, "proceeding"),
            (Execution::Download, "download"),
            (Execution::Downloaded, "downloaded"),
            (Execution::Closed, "closed"),
            (Execution::Canceled, "canceled"),
            (Execution::Rejected, "rejected"),
            (Execution::Resumed, "resumed"),
            (Execution::Scheduled, "scheduled"),
        ];
        for (execution, text) in all {
            let f = Feedback {
                execution,
                finished: Finished::None,
                code: None,
                details: &[],
                timestamp: None,
            };
            assert_eq!(
                f.encode().unwrap(),
                format!(
                    r#"{{"status":{{"execution":"{text}","result":{{"finished":"none"}},"details":[]}}}}"#
                )
            );
        }
    }

    #[test]
    fn every_finished_has_its_wire_string() {
        for (finished, text) in [
            (Finished::Success, "success"),
            (Finished::Failure, "failure"),
            (Finished::None, "none"),
        ] {
            let f = Feedback {
                execution: Execution::Proceeding,
                finished,
                code: None,
                details: &[],
                timestamp: None,
            };
            assert_eq!(
                f.encode().unwrap(),
                format!(
                    r#"{{"status":{{"execution":"proceeding","result":{{"finished":"{text}"}},"details":[]}}}}"#
                )
            );
        }
    }

    #[test]
    fn code_and_timestamp_are_omitted_when_none() {
        let mut f = fb(&["x"]);
        f.code = None;
        assert_eq!(
            f.encode().unwrap(),
            r#"{"status":{"execution":"closed","result":{"finished":"failure"},"details":["x"]},"timestamp":1700000000}"#
        );
        let mut f = fb(&["x"]);
        f.timestamp = None;
        assert_eq!(
            f.encode().unwrap(),
            r#"{"status":{"execution":"closed","result":{"finished":"failure"},"code":1002,"details":["x"]}}"#
        );
        f.code = None;
        assert_eq!(
            f.encode().unwrap(),
            r#"{"status":{"execution":"closed","result":{"finished":"failure"},"details":["x"]}}"#
        );
    }

    #[test]
    fn empty_details_encode_as_an_empty_array() {
        assert_eq!(
            fb(&[]).encode().unwrap(),
            r#"{"status":{"execution":"closed","result":{"finished":"failure"},"code":1002,"details":[]},"timestamp":1700000000}"#
        );
    }

    #[test]
    fn fail_reason_string_round_trips_as_the_first_detail() {
        let reason = FailReason::ManifestInvalid;
        let body = fb(&[reason.as_str()]).encode().unwrap();
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(v["status"]["details"][0], "manifest_invalid");
        assert_eq!(
            FailReason::from_code(v["status"]["details"][0].as_str().unwrap()),
            Some(reason)
        );
    }

    #[test]
    fn detail_count_is_bounded_exactly() {
        let ten = ["d"; MAX_DETAILS];
        assert!(fb(&ten).encode().is_ok());
        let eleven = ["d"; MAX_DETAILS + 1];
        assert_eq!(fb(&eleven).encode(), Err(FeedbackError::TooManyDetails));
    }

    #[test]
    fn detail_length_counts_bytes_not_chars() {
        let at = "a".repeat(MAX_DETAIL_BYTES);
        assert!(fb(&[&at]).encode().is_ok());
        let over = "a".repeat(MAX_DETAIL_BYTES + 1);
        assert_eq!(fb(&[&over]).encode(), Err(FeedbackError::DetailTooLong));
        let multi_at = format!("{}{}", "a".repeat(MAX_DETAIL_BYTES - 2), '\u{e9}');
        assert_eq!(multi_at.len(), MAX_DETAIL_BYTES);
        assert!(fb(&[&multi_at]).encode().is_ok());
        let multi_over = "\u{e9}".repeat(MAX_DETAIL_BYTES / 2 + 1);
        assert_eq!(multi_over.chars().count(), MAX_DETAIL_BYTES / 2 + 1);
        assert_eq!(
            fb(&[&multi_over]).encode(),
            Err(FeedbackError::DetailTooLong)
        );
    }

    #[test]
    fn count_is_checked_before_length() {
        let long = "a".repeat(MAX_DETAIL_BYTES + 1);
        let many: Vec<&str> = (0..=MAX_DETAILS).map(|_| long.as_str()).collect();
        assert_eq!(fb(&many).encode(), Err(FeedbackError::TooManyDetails));
    }

    #[test]
    fn escaping_can_push_the_body_over_the_limit() {
        let quotes = "\"".repeat(MAX_DETAIL_BYTES);
        let details: Vec<&str> = (0..MAX_DETAILS).map(|_| quotes.as_str()).collect();
        assert_eq!(fb(&details).encode(), Err(FeedbackError::TooLarge));
    }

    #[test]
    fn largest_unescaped_feedback_fits() {
        let text = "a".repeat(MAX_DETAIL_BYTES);
        let details: Vec<&str> = (0..MAX_DETAILS).map(|_| text.as_str()).collect();
        let body = fb(&details).encode().unwrap();
        assert!(body.len() <= MAX_FEEDBACK_BYTES);
    }

    #[test]
    fn control_characters_and_quotes_are_escaped() {
        let body = fb(&["a\nb", "c\u{1}d", "e\"f", "g\\h"]).encode().unwrap();
        assert!(body.contains(r#""a\nb""#));
        assert!(body.contains(r#""c\u0001d""#));
        assert!(body.contains(r#""e\"f""#));
        assert!(body.contains(r#""g\\h""#));
        assert!(!body.contains('\n'));
    }

    #[test]
    fn there_is_no_trailing_newline() {
        let body = fb(&["x"]).encode().unwrap();
        assert!(body.ends_with('}'));
    }

    #[test]
    fn error_display_is_fixed_text() {
        assert_eq!(
            format!("{}", FeedbackError::TooManyDetails),
            "DDI feedback has too many details"
        );
        assert_eq!(
            format!("{}", FeedbackError::DetailTooLong),
            "DDI feedback detail too long"
        );
        assert_eq!(
            format!("{}", FeedbackError::TooLarge),
            "DDI feedback too large"
        );
    }
}
