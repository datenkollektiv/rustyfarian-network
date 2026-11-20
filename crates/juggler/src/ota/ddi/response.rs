//! What a DDI transport hands back: the status, the raw `Retry-After` value and the capped body, plus the pure size rules a transport applies while it reads.
//!
//! The transport exposes the status and never interprets it; the DDI machine decides what a status means (ADR 027).

use alloc::vec::Vec;
use core::fmt;

use super::wire::{MAX_FEEDBACK_BYTES, MAX_RESPONSE_BYTES};

/// Experimental: API may change before 1.0.
///
/// Largest request body a transport sends, in bytes: an encoded feedback or configData.
pub const MAX_REQUEST_BYTES: usize = MAX_FEEDBACK_BYTES;
/// Experimental: API may change before 1.0.
///
/// Longest `Retry-After` value a transport keeps, in bytes.
/// An HTTP date is 29 bytes; a delta in seconds is far shorter.
pub const MAX_RETRY_AFTER_BYTES: usize = 32;

/// Experimental: API may change before 1.0.
///
/// `true` for a 3xx status other than 304.
/// A transport never follows a redirect and reports one as its own error instead of a response.
/// 304 Not Modified carries no new location (esp-idf-svc does not follow it either), so it is a plain response the DDI machine judges.
///
/// ```
/// use juggler::ota::ddi::is_redirect;
///
/// assert!(is_redirect(302));
/// assert!(!is_redirect(304));
/// assert!(!is_redirect(200));
/// assert!(!is_redirect(404));
/// ```
pub const fn is_redirect(status: u16) -> bool {
    matches!(status, 300..=399) && status != 304
}

/// Experimental: API may change before 1.0.
///
/// The raw `Retry-After` header value, unparsed: 1 to [`MAX_RETRY_AFTER_BYTES`] bytes of printable ASCII (space included).
/// The DDI machine parses it; the transport only captures it.
///
/// ```
/// use juggler::ota::ddi::RetryAfter;
///
/// assert_eq!(RetryAfter::capture("120").unwrap().as_str(), "120");
/// assert!(RetryAfter::capture("").is_none());
/// ```
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetryAfter(heapless::String<MAX_RETRY_AFTER_BYTES>);

impl RetryAfter {
    /// The header value as received, or `None` when it is empty, longer than [`MAX_RETRY_AFTER_BYTES`] or holds a byte outside printable ASCII.
    ///
    /// A value that is refused here counts as no `Retry-After` at all, never as a truncated one.
    pub fn capture(raw: &str) -> Option<Self> {
        if raw.is_empty()
            || raw.len() > MAX_RETRY_AFTER_BYTES
            || !raw.bytes().all(|b| b == b' ' || b.is_ascii_graphic())
        {
            return None;
        }
        let mut value = heapless::String::new();
        value.push_str(raw).ok()?;
        Some(RetryAfter(value))
    }

    /// The header value.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Experimental: API may change before 1.0.
///
/// One DDI response: the status, the raw `Retry-After` and the whole body.
/// The body never exceeds [`MAX_RESPONSE_BYTES`](super::wire::MAX_RESPONSE_BYTES); a longer one is an error, never a truncated body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DdiResponse {
    /// The HTTP status, never a redirect (see [`is_redirect`]).
    pub status: u16,
    /// The `Retry-After` header value, when present and within its bounds.
    pub retry_after: Option<RetryAfter>,
    /// The response body.
    pub body: Vec<u8>,
}

/// Experimental: API may change before 1.0.
///
/// Why a response body was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyError {
    /// The declared or received body exceeds [`MAX_RESPONSE_BYTES`](super::wire::MAX_RESPONSE_BYTES).
    TooLarge,
    /// The received body is shorter or longer than its declared `Content-Length`.
    LengthMismatch,
}

impl fmt::Display for BodyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BodyError::TooLarge => f.write_str("DDI response body too large"),
            BodyError::LengthMismatch => {
                f.write_str("DDI response body length differs from Content-Length")
            }
        }
    }
}

/// Experimental: API may change before 1.0.
///
/// Collects a response body on the heap, at most [`MAX_RESPONSE_BYTES`](super::wire::MAX_RESPONSE_BYTES) bytes, and checks it against the declared `Content-Length`.
///
/// A declared length above the cap is refused before anything is read; the transport feeds every read to [`push`](Self::push) and ends with [`finish`](Self::finish).
///
/// ```
/// use juggler::ota::ddi::{BodyError, ResponseBody};
///
/// let mut body = ResponseBody::new(Some(4)).unwrap();
/// body.push(b"{}").unwrap();
/// assert_eq!(body.finish(), Err(BodyError::LengthMismatch));
/// assert_eq!(ResponseBody::new(Some(8193)).unwrap_err(), BodyError::TooLarge);
/// ```
#[derive(Debug)]
pub struct ResponseBody {
    bytes: Vec<u8>,
    declared: Option<u64>,
}

impl ResponseBody {
    /// A collector for a body of `content_length` bytes, `None` when the response declares none (a chunked body, say).
    pub fn new(content_length: Option<u64>) -> Result<Self, BodyError> {
        let reserve = match content_length {
            Some(len) if len > MAX_RESPONSE_BYTES as u64 => return Err(BodyError::TooLarge),
            Some(len) => len as usize,
            None => 0,
        };
        Ok(ResponseBody {
            bytes: Vec::with_capacity(reserve),
            declared: content_length,
        })
    }

    /// Appends the bytes of one read.
    ///
    /// Fails as soon as the body exceeds the cap or the declared length.
    pub fn push(&mut self, chunk: &[u8]) -> Result<(), BodyError> {
        let total = self.bytes.len() + chunk.len();
        if total > MAX_RESPONSE_BYTES {
            return Err(BodyError::TooLarge);
        }
        if self
            .declared
            .is_some_and(|declared| total as u64 > declared)
        {
            return Err(BodyError::LengthMismatch);
        }
        self.bytes.extend_from_slice(chunk);
        Ok(())
    }

    /// The whole body, once the transport read the end of the stream.
    ///
    /// Fails when fewer bytes arrived than declared.
    pub fn finish(self) -> Result<Vec<u8>, BodyError> {
        match self.declared {
            Some(declared) if self.bytes.len() as u64 != declared => Err(BodyError::LengthMismatch),
            _ => Ok(self.bytes),
        }
    }
}

/// Experimental: API may change before 1.0.
///
/// Why the body of a response with a known status was not read whole ([`RequestOutcome::BodyFailed`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyFailure {
    /// The body exceeds [`MAX_RESPONSE_BYTES`](super::wire::MAX_RESPONSE_BYTES).
    TooLarge,
    /// The body length differs from its `Content-Length`.
    LengthMismatch,
    /// A read timed out, or the request deadline passed.
    Timeout,
    /// The connection failed.
    ServerUnreachable,
}

impl fmt::Display for BodyFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BodyFailure::TooLarge => f.write_str("body too large"),
            BodyFailure::LengthMismatch => f.write_str("body length differs from Content-Length"),
            BodyFailure::Timeout => f.write_str("body read timed out"),
            BodyFailure::ServerUnreachable => f.write_str("body read failed"),
        }
    }
}

impl From<BodyError> for BodyFailure {
    fn from(e: BodyError) -> Self {
        match e {
            BodyError::TooLarge => BodyFailure::TooLarge,
            BodyError::LengthMismatch => BodyFailure::LengthMismatch,
        }
    }
}

/// Experimental: API may change before 1.0.
///
/// How one DDI request ended, as a tier hands it to the DDI machine: the tier-neutral form of a transport's result.
///
/// A tier maps its transport error onto these variants and nothing else; the machine decides what each one means for the request it made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestOutcome {
    /// The whole response arrived.
    Response(DdiResponse),
    /// The status and headers arrived, the whole body did not.
    BodyFailed {
        /// The HTTP status, never a redirect.
        status: u16,
        /// The `Retry-After` header value, when present and within its bounds.
        retry_after: Option<RetryAfter>,
        /// Why the body was not read whole.
        failure: BodyFailure,
    },
    /// The connect, the request or the header fetch timed out, or the request deadline passed, before a status arrived.
    Timeout,
    /// The connection, the request or the response failed before a status arrived.
    Unreachable,
    /// The server answered with this 3xx status (see [`is_redirect`]); it was not followed.
    Redirect {
        /// The 3xx status.
        status: u16,
    },
    /// The link failed the origin check: outside the configured origin, or not of the accepted URL shape; no request was made.
    ForeignOrigin,
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;
    use alloc::vec;

    #[test]
    fn redirect_is_3xx_except_304() {
        for s in [300, 301, 302, 303, 305, 307, 308, 399] {
            assert!(is_redirect(s), "{s}");
        }
        for s in [0, 100, 200, 204, 299, 304, 400, 401, 404, 410, 429, 500] {
            assert!(!is_redirect(s), "{s}");
        }
    }

    #[test]
    fn retry_after_capture_bounds() {
        assert_eq!(RetryAfter::capture("0").unwrap().as_str(), "0");
        let date = "Wed, 21 Oct 2015 07:28:00 GMT";
        assert_eq!(RetryAfter::capture(date).unwrap().as_str(), date);
        let at_cap = "9".repeat(MAX_RETRY_AFTER_BYTES);
        assert_eq!(RetryAfter::capture(&at_cap).unwrap().as_str(), at_cap);
        assert!(RetryAfter::capture(&"9".repeat(MAX_RETRY_AFTER_BYTES + 1)).is_none());
        assert!(RetryAfter::capture("").is_none());
    }

    #[test]
    fn retry_after_refuses_control_and_non_ascii() {
        for bad in ["12\r\n", "1\t2", "\u{0}", "12\u{7f}", "z\u{e4}hn"] {
            assert!(RetryAfter::capture(bad).is_none(), "{bad:?}");
        }
        assert!(RetryAfter::capture(" 12 ").is_some());
    }

    #[test]
    fn declared_length_above_cap_is_refused_before_reading() {
        assert_eq!(
            ResponseBody::new(Some(MAX_RESPONSE_BYTES as u64 + 1)).unwrap_err(),
            BodyError::TooLarge
        );
        assert_eq!(
            ResponseBody::new(Some(u64::MAX)).unwrap_err(),
            BodyError::TooLarge
        );
        assert!(ResponseBody::new(Some(MAX_RESPONSE_BYTES as u64)).is_ok());
    }

    #[test]
    fn body_at_the_cap_passes_and_one_more_byte_fails() {
        let mut body = ResponseBody::new(None).unwrap();
        body.push(&vec![b'a'; MAX_RESPONSE_BYTES - 1]).unwrap();
        body.push(b"b").unwrap();
        assert_eq!(body.push(b"c"), Err(BodyError::TooLarge));
        assert_eq!(body.finish().unwrap().len(), MAX_RESPONSE_BYTES);
    }

    #[test]
    fn undeclared_body_over_cap_fails_without_growing() {
        let mut body = ResponseBody::new(None).unwrap();
        body.push(&vec![0; 1024]).unwrap();
        assert_eq!(
            body.push(&vec![0; MAX_RESPONSE_BYTES]),
            Err(BodyError::TooLarge)
        );
        assert_eq!(body.finish().unwrap().len(), 1024);
    }

    #[test]
    fn declared_length_is_enforced_both_ways() {
        let mut body = ResponseBody::new(Some(3)).unwrap();
        body.push(b"ab").unwrap();
        assert_eq!(body.push(b"cd"), Err(BodyError::LengthMismatch));
        body.push(b"c").unwrap();
        assert_eq!(body.finish().unwrap(), b"abc");

        let short = ResponseBody::new(Some(3)).unwrap();
        assert_eq!(short.finish(), Err(BodyError::LengthMismatch));

        let empty = ResponseBody::new(Some(0)).unwrap();
        assert_eq!(empty.finish().unwrap(), b"");
        let undeclared = ResponseBody::new(None).unwrap();
        assert_eq!(undeclared.finish().unwrap(), b"");
    }

    #[test]
    fn request_cap_is_the_feedback_cap() {
        assert_eq!(MAX_REQUEST_BYTES, 4096);
        assert_eq!(
            format!("{}", BodyError::TooLarge),
            "DDI response body too large"
        );
    }
}
