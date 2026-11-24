//! The tier-neutral parts of a DDI transport (ADR 027): its configuration, its error, the scheme gate and the stack marks of a DDI runtime.
//!
//! Both tiers' `DdiTransport` take a [`DdiTransportConfig`], fail with a [`DdiTransportError`] and map that error onto the [`RequestOutcome`] the DDI machine takes ([`DdiTransportError::into_outcome`]); the ESP-IDF tier re-exports these items from its `ota::ddi` module, and so does the esp-hal tier.
//! [`check_scheme`] is the rule both tiers apply to the base URL's scheme: plain HTTP only in a lab build, HTTPS refused until the TLS round lands.

use alloc::string::String;
use core::fmt;
use core::time::Duration;

use super::endpoint::{EndpointError, Scheme, UrlRefusal};
use super::response::{BodyFailure, RequestOutcome, RetryAfter};
use super::token::DdiToken;
use super::wire::FeedbackError;
use crate::ota::UrlForLog;

/// Experimental: API may change before 1.0.
///
/// Default wait of one network operation (connect, header read, body read) of a DDI transport request.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Experimental: API may change before 1.0.
///
/// Default total time of one DDI transport request, from the connect to the last body byte.
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(60);

/// Experimental: API may change before 1.0.
///
/// What a tier's `DdiTransport` is built from: the server, the device and its token.
///
/// `Debug` prints the base URL through [`UrlForLog`] (no userinfo, query or fragment) and the token as `DdiToken(Secret(<redacted>))`.
///
/// ```
/// use juggler::ota::ddi::{DdiToken, DdiTransportConfig, DEFAULT_TIMEOUT};
///
/// let token = DdiToken::parse("0123456789abcdef0123456789abcdef").unwrap();
/// let config = DdiTransportConfig::new("http://192.168.1.10:8080/DEFAULT", "dev-1", token);
/// assert_eq!(config.timeout, DEFAULT_TIMEOUT);
/// assert!(!format!("{config:?}").contains("0123456789abcdef"));
/// ```
pub struct DdiTransportConfig {
    /// The DDI base URL including the tenant path, for example `https://ddi.example/DEFAULT`; the controller resource is `<base_url>/controller/v1/<controller_id>`.
    pub base_url: String,
    /// The controller id, the device id rule `^[a-z0-9][a-z0-9-]{2,19}$`.
    pub controller_id: String,
    /// The per-device target token, sent as `Authorization: TargetToken <token>`.
    pub token: DdiToken,
    /// The wait of one network operation of a request; the transport's `download` uses the OTA session's (or manager's) timeout instead.
    pub timeout: Duration,
    /// The total time of one request; the transport's `download` uses the OTA session's (or manager's) deadline instead.
    pub deadline: Duration,
}

impl DdiTransportConfig {
    /// Experimental: API may change before 1.0.
    ///
    /// A configuration with the [`DEFAULT_TIMEOUT`] and the [`DEFAULT_DEADLINE`].
    pub fn new(
        base_url: impl Into<String>,
        controller_id: impl Into<String>,
        token: DdiToken,
    ) -> Self {
        Self {
            base_url: base_url.into(),
            controller_id: controller_id.into(),
            token,
            timeout: DEFAULT_TIMEOUT,
            deadline: DEFAULT_DEADLINE,
        }
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Overrides the wait of one network operation.
    #[must_use]
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Overrides the total time of one request.
    #[must_use]
    pub fn with_deadline(mut self, deadline: Duration) -> Self {
        self.deadline = deadline;
        self
    }
}

impl fmt::Debug for DdiTransportConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DdiTransportConfig")
            .field("base_url", &format_args!("{}", UrlForLog(&self.base_url)))
            .field("controller_id", &self.controller_id)
            .field("token", &self.token)
            .field("timeout", &self.timeout)
            .field("deadline", &self.deadline)
            .finish()
    }
}

/// Experimental: API may change before 1.0.
///
/// Why a tier's `DdiTransport` could not be built or a request failed.
/// Carries no URL, header or token text.
/// The enum is `#[non_exhaustive]`: the TLS round adds variants, so a `match` outside this crate needs a wildcard arm.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum DdiTransportError {
    /// The base URL failed the origin check of [`DdiEndpoint::check_base_url`](super::DdiEndpoint::check_base_url) for this case; the transport's `new` also logs it as a WARN, never the URL.
    InvalidBaseUrl(UrlRefusal),
    /// The controller id does not match `^[a-z0-9][a-z0-9-]{2,19}$`.
    InvalidControllerId,
    /// An `http://` base URL in a build without the `ota-ddi-insecure-http` feature.
    InsecureHttp,
    /// An `https://` base URL: TLS for the DDI transport has not landed yet.
    TlsUnavailable,
    /// A server link that failed the origin check (fleet DDI profile §2): outside the configured scheme, host and port, or not of the accepted URL shape; no request was made, and the case was logged as a WARN.
    ForeignOrigin,
    /// The server answered with this 3xx status; redirects are never followed.
    Redirect {
        /// The 3xx status.
        status: u16,
    },
    /// The feedback could not be encoded within the codec's limits.
    Feedback(FeedbackError),
    /// The request body exceeds [`MAX_REQUEST_BYTES`](super::MAX_REQUEST_BYTES).
    RequestTooLarge,
    /// The status and headers arrived, the whole body did not.
    Body {
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
    ServerUnreachable,
}

impl fmt::Display for DdiTransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DdiTransportError::InvalidBaseUrl(case) => write!(f, "DDI base URL refused: {case}"),
            DdiTransportError::InvalidControllerId => f.write_str("DDI controller id invalid"),
            DdiTransportError::InsecureHttp => {
                f.write_str("DDI over plain HTTP needs the ota-ddi-insecure-http feature")
            }
            DdiTransportError::TlsUnavailable => f.write_str("DDI over HTTPS is not available yet"),
            DdiTransportError::ForeignOrigin => f.write_str("DDI link refused by the origin check"),
            DdiTransportError::Redirect { status } => {
                write!(f, "DDI request redirected with status {status}")
            }
            DdiTransportError::Feedback(e) => fmt::Display::fmt(e, f),
            DdiTransportError::RequestTooLarge => f.write_str("DDI request body too large"),
            DdiTransportError::Body {
                status, failure, ..
            } => write!(f, "DDI response {status}: {failure}"),
            DdiTransportError::Timeout => f.write_str("DDI request timed out"),
            DdiTransportError::ServerUnreachable => f.write_str("DDI server unreachable"),
        }
    }
}

impl core::error::Error for DdiTransportError {}

impl DdiTransportError {
    /// Experimental: API may change before 1.0.
    ///
    /// The tier-neutral form of a failed request that the DDI machine takes ([`RequestOutcome`]), the one place a tier maps its transport errors onto it.
    ///
    /// `None` for an error raised before any request was sent: a construction error (the transport's `new`), a feedback that could not be encoded or a request body over [`MAX_REQUEST_BYTES`](super::MAX_REQUEST_BYTES).
    /// A successful request is `RequestOutcome::Response`.
    ///
    /// ```
    /// use juggler::ota::ddi::{DdiTransportError, RequestOutcome};
    ///
    /// assert_eq!(DdiTransportError::Timeout.into_outcome(), Some(RequestOutcome::Timeout));
    /// assert_eq!(DdiTransportError::RequestTooLarge.into_outcome(), None);
    /// ```
    pub fn into_outcome(self) -> Option<RequestOutcome> {
        match self {
            DdiTransportError::Body {
                status,
                retry_after,
                failure,
            } => Some(RequestOutcome::BodyFailed {
                status,
                retry_after,
                failure,
            }),
            DdiTransportError::Timeout => Some(RequestOutcome::Timeout),
            DdiTransportError::ServerUnreachable => Some(RequestOutcome::Unreachable),
            DdiTransportError::Redirect { status } => Some(RequestOutcome::Redirect { status }),
            DdiTransportError::ForeignOrigin => Some(RequestOutcome::ForeignOrigin),
            DdiTransportError::InvalidBaseUrl(_)
            | DdiTransportError::InvalidControllerId
            | DdiTransportError::InsecureHttp
            | DdiTransportError::TlsUnavailable
            | DdiTransportError::Feedback(_)
            | DdiTransportError::RequestTooLarge => None,
        }
    }
}

impl From<EndpointError> for DdiTransportError {
    fn from(e: EndpointError) -> Self {
        match e {
            EndpointError::InvalidBaseUrl(case) => DdiTransportError::InvalidBaseUrl(case),
            EndpointError::InvalidControllerId => DdiTransportError::InvalidControllerId,
        }
    }
}

/// Experimental: API may change before 1.0.
///
/// The scheme rule of a DDI transport: `https` is refused with [`DdiTransportError::TlsUnavailable`] until the TLS round lands, and `http` is accepted only when `plain_http_allowed` (the tier's `ota-ddi-insecure-http` lab feature), else [`DdiTransportError::InsecureHttp`].
///
/// ```
/// use juggler::ota::ddi::{check_scheme, DdiTransportError, Scheme};
///
/// assert_eq!(check_scheme(Scheme::Https, true), Err(DdiTransportError::TlsUnavailable));
/// assert_eq!(check_scheme(Scheme::Http, false), Err(DdiTransportError::InsecureHttp));
/// assert_eq!(check_scheme(Scheme::Http, true), Ok(()));
/// ```
pub const fn check_scheme(
    scheme: Scheme,
    plain_http_allowed: bool,
) -> Result<(), DdiTransportError> {
    match scheme {
        Scheme::Https => Err(DdiTransportError::TlsUnavailable),
        Scheme::Http if plain_http_allowed => Ok(()),
        Scheme::Http => Err(DdiTransportError::InsecureHttp),
    }
}

/// Experimental: API may change before 1.0.
///
/// The least free stack bytes the DDI runtime threads have seen, in bytes; `None` until the thread has sampled once.
///
/// On the ESP-IDF tier the health policy runs on the app's own thread and is not tracked here, and the `ota-download` thread is sampled when a download ends and exists only while an attempt does.
/// The esp-hal tier runs the runtime as futures on the firmware's executor and reports `None` for both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DdiStackMarks {
    /// Least free bytes of the `ota-worker` thread.
    pub worker_min_free_bytes: Option<u32>,
    /// Least free bytes of the `ota-download` threads.
    pub download_min_free_bytes: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;
    use alloc::vec;
    use alloc::vec::Vec;

    fn retry() -> Option<RetryAfter> {
        RetryAfter::capture("120")
    }

    /// Every variant and its outcome; a new variant must be added here (the match below is exhaustive in this crate).
    fn table() -> Vec<(DdiTransportError, Option<RequestOutcome>)> {
        let all = vec![
            DdiTransportError::InvalidBaseUrl(UrlRefusal::Host),
            DdiTransportError::InvalidControllerId,
            DdiTransportError::InsecureHttp,
            DdiTransportError::TlsUnavailable,
            DdiTransportError::ForeignOrigin,
            DdiTransportError::Redirect { status: 302 },
            DdiTransportError::Feedback(FeedbackError::TooLarge),
            DdiTransportError::RequestTooLarge,
            DdiTransportError::Body {
                status: 200,
                retry_after: retry(),
                failure: BodyFailure::TooLarge,
            },
            DdiTransportError::Timeout,
            DdiTransportError::ServerUnreachable,
        ];
        all.into_iter()
            .map(|e| {
                let expected = match &e {
                    DdiTransportError::Body {
                        status,
                        retry_after,
                        failure,
                    } => Some(RequestOutcome::BodyFailed {
                        status: *status,
                        retry_after: retry_after.clone(),
                        failure: *failure,
                    }),
                    DdiTransportError::Timeout => Some(RequestOutcome::Timeout),
                    DdiTransportError::ServerUnreachable => Some(RequestOutcome::Unreachable),
                    DdiTransportError::Redirect { status } => {
                        Some(RequestOutcome::Redirect { status: *status })
                    }
                    DdiTransportError::ForeignOrigin => Some(RequestOutcome::ForeignOrigin),
                    DdiTransportError::InvalidBaseUrl(_)
                    | DdiTransportError::InvalidControllerId
                    | DdiTransportError::InsecureHttp
                    | DdiTransportError::TlsUnavailable
                    | DdiTransportError::Feedback(_)
                    | DdiTransportError::RequestTooLarge => None,
                };
                (e, expected)
            })
            .collect()
    }

    #[test]
    fn into_outcome_maps_every_variant() {
        let rows = table();
        assert_eq!(rows.len(), 11);
        for (error, expected) in rows {
            assert_eq!(error.clone().into_outcome(), expected, "{error:?}");
        }
    }

    #[test]
    fn body_failure_keeps_status_and_retry_after() {
        let outcome = DdiTransportError::Body {
            status: 503,
            retry_after: retry(),
            failure: BodyFailure::Timeout,
        }
        .into_outcome();
        assert_eq!(
            outcome,
            Some(RequestOutcome::BodyFailed {
                status: 503,
                retry_after: retry(),
                failure: BodyFailure::Timeout,
            })
        );
    }

    #[test]
    fn endpoint_errors_convert() {
        assert_eq!(
            DdiTransportError::from(EndpointError::InvalidBaseUrl(UrlRefusal::Port)),
            DdiTransportError::InvalidBaseUrl(UrlRefusal::Port)
        );
        assert_eq!(
            DdiTransportError::from(EndpointError::InvalidControllerId),
            DdiTransportError::InvalidControllerId
        );
    }

    #[test]
    fn scheme_gate() {
        assert_eq!(
            check_scheme(Scheme::Https, false),
            Err(DdiTransportError::TlsUnavailable)
        );
        assert_eq!(
            check_scheme(Scheme::Https, true),
            Err(DdiTransportError::TlsUnavailable)
        );
        assert_eq!(
            check_scheme(Scheme::Http, false),
            Err(DdiTransportError::InsecureHttp)
        );
        assert_eq!(check_scheme(Scheme::Http, true), Ok(()));
    }

    #[test]
    fn config_debug_redacts_the_token_and_url_secrets() {
        let token = DdiToken::parse("0123456789abcdef0123456789abcdef").unwrap();
        let config = DdiTransportConfig::new("http://u:pw@h:8080/DEFAULT?k=v", "dev-1", token)
            .with_timeout(Duration::from_secs(5))
            .with_deadline(Duration::from_secs(9));
        let text = format!("{config:?}");
        assert!(!text.contains("0123456789abcdef"), "{text}");
        assert!(!text.contains("pw"), "{text}");
        assert!(!text.contains("k=v"), "{text}");
        assert_eq!(config.timeout, Duration::from_secs(5));
        assert_eq!(config.deadline, Duration::from_secs(9));
    }

    #[test]
    fn display_carries_no_url_or_token() {
        for (error, _) in table() {
            let text = format!("{error}");
            assert!(!text.is_empty());
            assert!(!text.contains("http://"));
        }
    }
}
