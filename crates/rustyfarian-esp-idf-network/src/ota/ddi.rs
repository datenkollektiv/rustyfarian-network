//! The credential-bound DDI transport (ADR 027): one blocking method per DDI endpoint, and no way to send the token anywhere else.
//!
//! [`DdiTransport`] takes the base URL, the controller id and the [`DdiToken`] once and builds every request itself.
//! No public method takes a URL string, a header or an HTTP method.
//! A server link ([`Href`]) is requested only after [`DdiEndpoint::check_href`] found it on the configured origin (fleet DDI profile §2), and only at the [`CheckedUrl`] it rebuilt from the checked parts, never at the link text; a refused link fails before any request, logged by its case and never by its text.
//! The base URL passes the same check at every boot ([`DdiTransport::new`]).
//! Redirects are never followed: every request sets `FollowRedirectsPolicy::FollowNone`, and a 3xx other than 304 is an error, because esp-idf-svc keeps every header set on the handle when it follows one.
//! Each request uses its own HTTP client, so the copies of the token esp-idf-svc makes (a `CString` and the `esp_http_client` header list) are freed when it returns; they are not zeroed.
//!
//! The transport only performs I/O: it hands back the status, the raw `Retry-After` and the body ([`DdiResponse`]), and the DDI machine in `juggler::ota::ddi` decides what they mean.
//! A body that fails after the status arrived still hands back the status and the `Retry-After` ([`DdiTransportError::Body`]).
//!
//! # Time limits
//!
//! Each request has a deadline ([`DEFAULT_DEADLINE`]) on top of the wait of one network operation ([`DEFAULT_TIMEOUT`]).
//! Every wait is clipped to the time left, and every body read fits it, so a server that trickles the body cannot hold the caller past the deadline.
//! The connect, the request body send and the header fetch each make one or more transport waits of at most the time left when they start; a server that trickles its headers can still overrun the deadline, as on the firmware download.
//!
//! # Plain HTTP and TLS
//!
//! An `http://` base URL is refused unless the `ota-ddi-insecure-http` feature is enabled; that build logs a WARN once per boot, and release images leave the feature out.
//! An `https://` base URL is refused with [`DdiTransportError::TlsUnavailable`] until the TLS round (one compiled-in trust anchor and a build-time clock floor) lands.
//! ESP-IDF's `esp_http_client` prints the whole request header block, `Authorization` included, at DEBUG: keep `CONFIG_LOG_MAXIMUM_LEVEL` at INFO or below.
//!
//! # Runtime
//!
//! [`DdiRuntime`] drives the transport with the sans-IO machines of `juggler::ota::ddi` on two kinds of thread, and needs neither the `mqtt` nor the `wifi` feature.
//! The hook points, in the order the app calls them:
//!
//! 1. [`open_records`], early in `main` before Wi-Fi: opens the record store and records the boot slot, so the note survives a crash later in that boot.
//! 2. [`DdiRuntime::new`]: validates the [`DdiSettings`] and builds the transport; it takes the token and touches neither the store nor the hardware.
//! 3. [`DdiRuntime::start`], once Wi-Fi is up: reconciles the boot records on the calling thread, repairs unreadable records, reads the update partition size and starts the worker.
//! 4. [`DdiHandle::run_health_policy`] on the app's main thread, instead of parking.
//!
//! Capture `let boot = Instant::now();` as the first statement of `main` for hook 4.
//!
//! # Threads
//!
//! - `ota-worker` owns the poll machine, the cancel machine, the verdict sender and every feedback: it polls, retrieves and admits a deployment, answers a cancel and delivers the persisted verdict.
//! - `ota-download` is spawned per admitted attempt with its own OTA session and its own cancel switch; the worker keeps polling meanwhile, and the offer gate stays closed while the attempt exists.
//!
//! The thread stacks ([`DdiStacks`]) are provisional until they are measured under TLS; [`DdiHandle::stack_high_water`] reports what each thread has used.
//!
//! # Health policy
//!
//! The health policy persists the success verdict and signals `ota-worker`, which sends it; nothing is published.
//! [`DdiHandle::run_health_policy`] marks the running image valid and records the completed attempt, which writes the success verdict of the bound action into the store before it clears the attempt.
//! The worker is then asked for an early poll, and the next poll delivers the verdict.
//! If the attempt record cannot be cleared the worker keeps retrying with backoff, and the verdict goes out once it can.
//! Before the health check the booted image reports `proceeding` once, best effort.
//!
//! # Nudge
//!
//! The library does not subscribe to MQTT itself.
//! [`DdiHandle::nudge_topic`] (also [`DdiRuntime::nudge_topic`], before the start) is the exact topic `rustyfarian/ota/<controller-id>/nudge`; the app registers it with `MqttBuilder::subscribe`, which re-sends it on every connect, and calls [`DdiNudge::on_message`] from its `MqttBuilder::on_message` closure.
//! The handle only compares the topic, sets a flag and wakes the worker, so it is safe inside the esp-mqtt callback and needs neither the `mqtt` feature nor an `MqttHandle`.
//! The worker applies the request on its own thread: one early poll, no sooner than 10 s after the last poll started and never while a poll is in flight.
//!
//! # Reboots
//!
//! The runtime never restarts the device itself.
//! After a successful download the download thread waits `restart_grace`, then calls the app's [`RestartFn`].
//! The only other reboot is `OtaSession::rollback`, used for a rollback demanded at boot and at the health deadline.
//!
//! # Store lock
//!
//! The record store sits behind one mutex that is only taken through short closures that contain store calls and nothing else; never across HTTP, `OtaSession`, hardware reads, sleeps, joins or channel operations.
//!
//! # Wall clock
//!
//! ESP-IDF boots at 1970, so a clock below [`WALL_CLOCK_FLOOR_SECS`] counts as unset: a `Retry-After` date is then ignored and a feedback carries no timestamp, until SNTP sets the clock.
//!
//! The esp-hal tier has no DDI transport, no DDI runtime and no `DdiNudge` yet (ADR 022 gap); it will land the same names as `async fn`.

#[cfg(feature = "ota-ddi-insecure-http")]
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use embedded_svc::http::Headers;
use esp_idf_svc::http::client::{Configuration, EspHttpConnection, FollowRedirectsPolicy};
use esp_idf_svc::http::Method;
use esp_idf_svc::sys::EspError;

use juggler::ota::ddi::{is_redirect, ControllerId, EndpointError, ResponseBody, Scheme};
use juggler::ota::{classify_read_error, read_bounded, UrlForLog};

pub use juggler::ota::ddi::wire::{
    ActionId, Board, Chip, ConfigData, Execution, Feedback, FeedbackError, Finished, Href,
    ProfileCode, MAX_RESPONSE_BYTES,
};
pub use juggler::ota::ddi::{
    BodyFailure, CheckedUrl, DdiEndpoint, DdiResponse, DdiToken, DdiTokenError, RequestOutcome,
    RetryAfter, UrlRefusal, MAX_REQUEST_BYTES,
};
pub use juggler::ota::ddi::{
    DdiTransportConfig, DdiTransportError, DEFAULT_DEADLINE, DEFAULT_TIMEOUT,
};

use super::downloader::{is_read_timeout, set_wait, FirmwareResponse, HTTP_RX_BUFFER_SIZE};
use super::{wait_for, OtaCancel, OtaError, OtaSession};

mod clock;
mod download;
mod handle;
mod nudge;
mod worker;

pub use handle::{DdiHandle, DdiRuntime, DdiRuntimeError, DdiStackMarks};
pub use juggler::ota::ddi::{
    clock_is_set, nudge_topic, DdiSettings, DdiSettingsError, DdiStacks, DdiTimings,
    DDI_MIN_STACK_BYTES, WALL_CLOCK_FLOOR_SECS,
};
pub use juggler::ota::health::HealthConfig;
pub use juggler::ota::{DeadlineAction, HealthVerdict};
pub use nudge::DdiNudge;

pub use crate::ota::flags::OtaFlags;
pub use crate::ota::records::{open_records, Records, RestartFn};

/// Transmit buffer of the HTTP client, in bytes.
///
/// `esp_http_client` writes the request line into it whole, so it must hold `GET`, a link of up to 512 bytes (the codec's `MAX_HREF_BYTES`) and `HTTP/1.1`.
const HTTP_TX_BUFFER_SIZE: usize = 1024;

/// Bytes asked for per body read of a JSON response; the read buffer lives on the stack.
const READ_CHUNK: usize = 512;

/// Bytes per request-body write, below lwIP's `TCP_SNDLOWAT` (at least `2 * TCP_MSS + 1`), so one write is one wait (see `DdiTransport::write_body`).
const WRITE_CHUNK: usize = 1024;

/// Set once the plain-HTTP WARN has been logged; statics reset on every boot.
#[cfg(feature = "ota-ddi-insecure-http")]
static INSECURE_HTTP_WARNED: AtomicBool = AtomicBool::new(false);

/// Experimental: API may change before 1.0.
///
/// The blocking ESP-IDF DDI transport: one method per DDI endpoint, each one request with no decisions.
///
/// Run it on the thread that owns OTA work (the library's `ota-worker` in the runtime); every method blocks until its request ends.
/// `Debug` prints the token redacted.
///
/// ```rust,no_run
/// use rustyfarian_esp_idf_network::ota::ddi::{DdiToken, DdiTransport, DdiTransportConfig};
///
/// let token = DdiToken::parse("0123456789abcdef0123456789abcdef").unwrap();
/// let transport = DdiTransport::new(DdiTransportConfig::new(
///     "http://192.168.1.10:8080/DEFAULT",
///     "dev-1",
///     token,
/// ))
/// .unwrap();
/// let response = transport.poll().unwrap();
/// log::info!("poll answered {}", response.status);
/// ```
#[derive(Debug)]
pub struct DdiTransport {
    endpoint: DdiEndpoint,
    token: DdiToken,
    timeout: Duration,
    deadline: Duration,
}

impl DdiTransport {
    /// Experimental: API may change before 1.0.
    ///
    /// Validates the base URL and the controller id and takes the token.
    ///
    /// An `http://` base URL is refused with [`DdiTransportError::InsecureHttp`] unless the `ota-ddi-insecure-http` feature is enabled, which logs a WARN on the first such transport of the boot.
    /// An `https://` base URL is refused with [`DdiTransportError::TlsUnavailable`] until the DDI transport gains TLS.
    pub fn new(config: DdiTransportConfig) -> Result<Self, DdiTransportError> {
        let controller_id = ControllerId::parse(&config.controller_id)?;
        let endpoint = DdiEndpoint::new(&config.base_url, &controller_id).inspect_err(|e| {
            if let EndpointError::InvalidBaseUrl(case) = e {
                log::warn!("DDI base URL refused: {case}");
            }
        })?;
        match endpoint.scheme() {
            Scheme::Https => return Err(DdiTransportError::TlsUnavailable),
            Scheme::Http => allow_plain_http(&config.base_url)?,
        }
        Ok(Self {
            endpoint,
            token: config.token,
            timeout: config.timeout,
            deadline: config.deadline,
        })
    }

    /// Experimental: API may change before 1.0.
    ///
    /// GET of the controller base resource, the URL built from the configured base and controller id.
    pub fn poll(&self) -> Result<DdiResponse, DdiTransportError> {
        self.request(Method::Get, self.endpoint.poll_url(), None)
    }

    /// Experimental: API may change before 1.0.
    ///
    /// GET of a `deploymentBase` link, after the origin check, at the URL rebuilt from the checked parts.
    pub fn deployment(&self, href: &Href) -> Result<DdiResponse, DdiTransportError> {
        let url = self.link(href)?;
        self.request(Method::Get, url.as_str(), None)
    }

    /// Experimental: API may change before 1.0.
    ///
    /// GET of a `cancelAction` link, after the origin check, at the URL rebuilt from the checked parts.
    pub fn cancel_action(&self, href: &Href) -> Result<DdiResponse, DdiTransportError> {
        let url = self.link(href)?;
        self.request(Method::Get, url.as_str(), None)
    }

    /// Experimental: API may change before 1.0.
    ///
    /// POST of a deployment feedback to `<controller>/deploymentBase/<action>/feedback`, built from the configured base and `action`, never from a link.
    pub fn deployment_feedback(
        &self,
        action: ActionId,
        feedback: &Feedback,
    ) -> Result<DdiResponse, DdiTransportError> {
        let body = feedback.encode().map_err(DdiTransportError::Feedback)?;
        let url = self.endpoint.deployment_feedback_url(&action);
        self.request(Method::Post, &url, Some(body.as_bytes()))
    }

    /// Experimental: API may change before 1.0.
    ///
    /// POST of a cancel feedback to `<controller>/cancelAction/<action>/feedback`, built from the configured base and `action`, never from a link.
    pub fn cancel_feedback(
        &self,
        action: ActionId,
        feedback: &Feedback,
    ) -> Result<DdiResponse, DdiTransportError> {
        let body = feedback.encode().map_err(DdiTransportError::Feedback)?;
        let url = self.endpoint.cancel_feedback_url(&action);
        self.request(Method::Post, &url, Some(body.as_bytes()))
    }

    /// Experimental: API may change before 1.0.
    ///
    /// PUT of the configData body to `<controller>/configData`, built from the configured base.
    pub fn config_data(&self, data: &ConfigData) -> Result<DdiResponse, DdiTransportError> {
        let body = data.encode();
        let url = self.endpoint.config_data_url();
        self.request(Method::Put, &url, Some(body.as_bytes()))
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Fetches the artifact at `href` into `session`, the same streaming download, verification and slot swap as [`OtaSession::fetch_and_apply_cancellable`], with the token sent and redirects refused.
    ///
    /// A link that fails the origin check ([`DdiEndpoint::check_href`]) fails with [`OtaError::ForeignOrigin`] before any request, and an accepted one is fetched at the rebuilt [`CheckedUrl`], never at the link text; a 3xx fails with [`OtaError::Redirect`].
    /// The declared `Content-Length` must equal `size`, or the download fails with [`OtaError::SizeMismatch`] before any flash is touched; the bytes written then equal it as well.
    /// The session's timeout, deadline and expected chip ([`OtaSession::with_expected_chip`]) apply.
    pub fn download(
        &self,
        session: &mut OtaSession,
        href: &Href,
        sha256: &[u8; 32],
        size: u64,
        cancel: &OtaCancel,
    ) -> Result<(), OtaError> {
        self.download_capturing(session, href, sha256, size, cancel, &mut None)
    }

    /// [`download`](Self::download) that also hands back the `Retry-After` of a non-200 answer in `retry_after` (left unchanged otherwise), for the retry policy.
    pub(crate) fn download_capturing(
        &self,
        session: &mut OtaSession,
        href: &Href,
        sha256: &[u8; 32],
        size: u64,
        cancel: &OtaCancel,
        retry_after: &mut Option<RetryAfter>,
    ) -> Result<(), OtaError> {
        let url = self.endpoint.check_href(href).map_err(|case| {
            log::warn!("DDI download link refused: {case}");
            OtaError::ForeignOrigin
        })?;
        log::info!("DDI download from {}", UrlForLog(url.as_str()));
        let authorization = self.token.authorization();
        session.fetch_and_apply_with(sha256, Some(size), cancel, |timeout| {
            open_download(url.as_str(), authorization.expose(), timeout, retry_after)
        })
    }

    /// The endpoint every request URL is built from, for the machines that classify server links.
    pub(crate) fn endpoint(&self) -> &DdiEndpoint {
        &self.endpoint
    }

    /// The URL to request for `href` when it is on the configured origin, rebuilt from the checked parts; a refusal is logged by its case, never by the link.
    fn link(&self, href: &Href) -> Result<CheckedUrl, DdiTransportError> {
        self.endpoint.check_href(href).map_err(|case| {
            log::warn!("DDI link refused: {case}");
            DdiTransportError::ForeignOrigin
        })
    }

    /// The wait of the next network operation of a request that began at `start`: the per-operation timeout clipped to the time left of the deadline.
    fn budget(&self, start: Instant) -> Duration {
        self.timeout
            .min(self.deadline.saturating_sub(start.elapsed()))
    }

    /// Writes a request body in [`WRITE_CHUNK`] pieces, each with the wait clipped to what is left of the deadline of a request that began at `start`.
    ///
    /// `esp_http_client_write` polls the socket for writability with the client's wait, then sends; lwIP reports a socket writable only with more than `TCP_SNDLOWAT` bytes free (at least `2 * TCP_MSS + 1`), so a piece of at most [`WRITE_CHUNK`] bytes is queued whole without blocking.
    /// Every piece is therefore one wait inside the deadline, and the body cannot outlast it.
    fn write_body(
        &self,
        conn: &mut EspHttpConnection,
        url: &str,
        body: &[u8],
        start: Instant,
    ) -> Result<(), DdiTransportError> {
        for piece in body.chunks(WRITE_CHUNK) {
            let wait = self.budget(start);
            if wait.is_zero() {
                log::error!("DDI request to {} passed its deadline", UrlForLog(url));
                return Err(DdiTransportError::Timeout);
            }
            set_wait(conn, wait_for(wait))
                .and_then(|()| conn.write_all(piece))
                .map_err(|e| {
                    log::error!("DDI request body to {} failed: {:?}", UrlForLog(url), e);
                    head_error(&e)
                })?;
        }
        Ok(())
    }

    /// One request on its own client: send, read the status and headers, refuse a redirect, then read the capped body inside the deadline.
    fn request(
        &self,
        method: Method,
        url: &str,
        body: Option<&[u8]>,
    ) -> Result<DdiResponse, DdiTransportError> {
        if body.is_some_and(|b| b.len() > MAX_REQUEST_BYTES) {
            return Err(DdiTransportError::RequestTooLarge);
        }
        let start = Instant::now();
        let authorization = self.token.authorization();
        let length = body.map(|b| b.len().to_string());
        let mut headers = vec![
            ("Authorization", authorization.expose()),
            ("Accept", "application/json"),
        ];
        if let Some(length) = &length {
            headers.push(("Content-Type", "application/json"));
            headers.push(("Content-Length", length));
        }

        let mut conn = open(method, url, &headers, wait_for(self.budget(start))).map_err(|e| {
            log::error!("DDI request to {} failed: {:?}", UrlForLog(url), e);
            head_error(&e)
        })?;
        drop(headers);
        drop(authorization);
        if let Some(body) = body {
            self.write_body(&mut conn, url, body, start)?;
        }
        let wait = self.budget(start);
        if wait.is_zero() {
            log::error!("DDI request to {} passed its deadline", UrlForLog(url));
            return Err(DdiTransportError::Timeout);
        }
        set_wait(&mut conn, wait_for(wait))
            .and_then(|()| conn.initiate_response())
            .map_err(|e| {
                log::error!("DDI response from {} failed: {:?}", UrlForLog(url), e);
                head_error(&e)
            })?;

        let status = conn.status();
        if is_redirect(status) {
            log::warn!(
                "DDI {} answered {}: redirect refused",
                UrlForLog(url),
                status
            );
            return Err(DdiTransportError::Redirect { status });
        }
        let retry_after = conn.header("Retry-After").and_then(RetryAfter::capture);
        match read_body(&mut conn, || self.budget(start)) {
            Ok(body) => Ok(DdiResponse {
                status,
                retry_after,
                body,
            }),
            Err(failure) => {
                log::error!(
                    "DDI response body from {} ({}) failed: {}",
                    UrlForLog(url),
                    status,
                    failure
                );
                Err(DdiTransportError::Body {
                    status,
                    retry_after,
                    failure,
                })
            }
        }
    }
}

/// Reads the whole capped body; `budget` is the time a read may take, zero once the request deadline has passed.
///
/// `per_op` is `Duration::MAX`, so `read_bounded` always takes its near-deadline split: every call ends inside its budget, even against a server that trickles bytes, and the deadline bounds the whole body.
fn read_body(
    conn: &mut EspHttpConnection,
    budget: impl Fn() -> Duration,
) -> Result<Vec<u8>, BodyFailure> {
    let mut collected = ResponseBody::new(conn.content_len())?;
    let mut chunk = [0u8; READ_CHUNK];
    loop {
        let budget = budget();
        if budget.is_zero() {
            return Err(BodyFailure::Timeout);
        }
        let began = Instant::now();
        let n = read_bounded(
            &mut chunk,
            u32::MAX,
            HTTP_RX_BUFFER_SIZE,
            budget,
            Duration::MAX,
            || began.elapsed(),
            |buf, wait| {
                set_wait(conn, wait_for(wait)).map_err(|_| OtaError::ServerUnreachable)?;
                conn.read(buf)
                    .map_err(|e| classify_read_error(is_read_timeout(e.code())))
            },
        )
        .map_err(|e| match e {
            OtaError::DownloadTimeout => BodyFailure::Timeout,
            _ => BodyFailure::ServerUnreachable,
        })?;
        if n == 0 {
            return Ok(collected.finish()?);
        }
        collected.push(&chunk[..n])?;
    }
}

/// A failure before the status arrived: a wait that elapsed is [`DdiTransportError::Timeout`], anything else [`DdiTransportError::ServerUnreachable`].
fn head_error(e: &EspError) -> DdiTransportError {
    if is_read_timeout(e.code()) {
        DdiTransportError::Timeout
    } else {
        DdiTransportError::ServerUnreachable
    }
}

/// Refuses plain HTTP unless this is a lab build, which warns once per boot.
#[cfg(feature = "ota-ddi-insecure-http")]
fn allow_plain_http(base_url: &str) -> Result<(), DdiTransportError> {
    if !INSECURE_HTTP_WARNED.swap(true, Ordering::Relaxed) {
        log::warn!(
            "DDI transport uses insecure HTTP: {} (ota-ddi-insecure-http lab build; logged once per boot)",
            UrlForLog(base_url)
        );
    }
    Ok(())
}

/// Refuses plain HTTP unless this is a lab build, which warns once per boot.
#[cfg(not(feature = "ota-ddi-insecure-http"))]
fn allow_plain_http(_base_url: &str) -> Result<(), DdiTransportError> {
    Err(DdiTransportError::InsecureHttp)
}

/// A new client that never follows a redirect, with the request sent.
fn open(
    method: Method,
    url: &str,
    headers: &[(&str, &str)],
    timeout: Duration,
) -> Result<EspHttpConnection, EspError> {
    // esp-idf-svc casts the milliseconds to the C `int` unchecked, so clamp them first.
    let timeout = timeout.min(Duration::from_millis(i32::MAX as u64));
    let config = Configuration {
        timeout: Some(timeout),
        buffer_size: Some(HTTP_RX_BUFFER_SIZE),
        buffer_size_tx: Some(HTTP_TX_BUFFER_SIZE),
        follow_redirects_policy: FollowRedirectsPolicy::FollowNone,
        ..Default::default()
    };
    let mut conn = EspHttpConnection::new(&config)?;
    conn.initiate_request(method, url, headers)?;
    Ok(conn)
}

/// Opens the artifact GET for [`OtaSession::fetch_and_apply_with`]: a `200` response, a redirect as [`OtaError::Redirect`], a timed-out wait as `DownloadTimeout`, any other status as `DownloadFailed`.
///
/// Opening the request and fetching the headers are classified alike, as in the poll path's `head_error`: `ESP_ERR_HTTP_EAGAIN` and `ESP_ERR_HTTP_CONNECTING`, the code the ESP-IDF v6.1 API documents for a connect that timed out, are `DownloadTimeout`.
/// The v6.1 source returns `ESP_ERR_HTTP_CONNECTING` only to a non-blocking client, though, and a blocking one like this gets `ESP_ERR_HTTP_CONNECT` for a refused and a timed-out connect alike, which stays `ServerUnreachable`.
fn open_download(
    url: &str,
    authorization: &str,
    timeout: Duration,
    retry_after: &mut Option<RetryAfter>,
) -> Result<FirmwareResponse, OtaError> {
    let headers = [
        ("Authorization", authorization),
        ("Accept", "application/octet-stream"),
    ];
    let mut conn = open(Method::Get, url, &headers, timeout).map_err(|e| {
        log::error!("DDI download request to {} failed: {:?}", UrlForLog(url), e);
        classify_read_error(is_read_timeout(e.code()))
    })?;
    conn.initiate_response().map_err(|e| {
        log::error!(
            "DDI download response from {} failed: {:?}",
            UrlForLog(url),
            e
        );
        classify_read_error(is_read_timeout(e.code()))
    })?;
    let status = conn.status();
    if is_redirect(status) {
        log::error!(
            "DDI download {} answered {}: redirect refused",
            UrlForLog(url),
            status
        );
        return Err(OtaError::Redirect { status });
    }
    if status != 200 {
        log::error!("DDI download {} answered {}", UrlForLog(url), status);
        *retry_after = conn.header("Retry-After").and_then(RetryAfter::capture);
        return Err(OtaError::DownloadFailed { status });
    }
    Ok(FirmwareResponse::from_connection(conn))
}
