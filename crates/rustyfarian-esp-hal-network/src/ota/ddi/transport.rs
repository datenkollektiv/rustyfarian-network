//! The async esp-hal DDI transport: the ESP-IDF tier's `DdiTransport` methods as `async fn` over `embassy-net`.

use alloc::boxed::Box;
use alloc::vec;
use core::fmt;
use core::time::Duration;

use embassy_net::tcp::TcpSocket;
use embassy_net::{IpEndpoint, Stack};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_time::{with_timeout, Instant};

use juggler::ota::ddi::wire::{ActionId, ConfigData, Feedback, Href};
use juggler::ota::ddi::{
    is_redirect, DdiEndpoint, DdiResponse, DdiToken, DdiTransportConfig, DdiTransportError,
    RetryAfter,
};
use juggler::ota::download::ImageChip;
use juggler::ota::{OtaError, UrlForLog};

use super::plan::{self, DdiGate, Planned};
use super::resolve::{resolve, ResolveError};
use crate::ota::http::async_client::send_and_read_head;
use crate::ota::http::ddi::{request_head, BodyReader, ACCEPT_JSON};
use crate::ota::http::parse_url;
use crate::ota::manager::{to_core, to_embassy, DdiFetch};
use crate::ota::{EspHalOtaManager, OtaCancel, TARGET_CHIP};

/// Receive buffer of the request socket: holds a JSON response while the transport copies it out.
const REQUEST_RX_BYTES: usize = 2048;
/// Transmit buffer of the request socket: the head and a feedback body go out in pieces of this size.
const REQUEST_TX_BYTES: usize = 1024;
/// Receive buffer of the download socket, as in the firmware OTA example.
const DOWNLOAD_RX_BYTES: usize = 4096;
/// Transmit buffer of the download socket: the GET head, a link of up to 512 bytes and the token.
const DOWNLOAD_TX_BYTES: usize = 1024;
/// Bytes asked for per body read of a JSON response; the read buffer lives in the request future.
const READ_CHUNK: usize = 512;
/// How long a request waits for its FIN or RST to leave before it returns.
const CLOSE_FLUSH_WAIT: embassy_time::Duration = embassy_time::Duration::from_millis(500);

/// The receive and transmit buffers of one socket, allocated once.
struct SocketBuffers {
    rx: Box<[u8]>,
    tx: Box<[u8]>,
}

impl SocketBuffers {
    fn new(rx: usize, tx: usize) -> Self {
        Self {
            rx: vec![0; rx].into_boxed_slice(),
            tx: vec![0; tx].into_boxed_slice(),
        }
    }
}

/// Experimental: API may change before 1.0.
///
/// The async esp-hal DDI transport: one method per DDI endpoint, each one request with no decisions, with the names, arguments and results of the ESP-IDF tier's blocking `DdiTransport` (ADR 022, ADR 027).
///
/// [`new`](Self::new) takes the station [`Stack`] besides the configuration, and the transport owns its sockets: one buffer set for the JSON requests and one for the artifact download, allocated once there (2 KiB + 1 KiB for the requests, 4 KiB + 1 KiB for the download: 8 KiB of heap).
/// A request and a download may run at once (the DDI runtime polls while it downloads); two requests, or two downloads, wait for each other.
/// Each request needs two TCP sockets' worth of `StackResources` in total with the download, which the `ota-ddi` feature adds to `wifi::STA_SOCKET_COUNT`.
///
/// Per request the transport allocates the request head (under 1 KiB, holding the token; freed, not zeroed), the encoded feedback or configData body (at most 4 KiB) and the response body (at most 8 KiB).
/// The server name is resolved with one DNS `A` lookup per request; an IPv4 literal is used as is and an IPv6 literal is refused (`ServerUnreachable`).
///
/// No public method takes a URL string, a header or an HTTP method: every URL is built from the configured base, or rebuilt from a server link after the origin check (fleet DDI profile §2).
/// Redirects are never followed: a 3xx other than 304 is [`DdiTransportError::Redirect`].
/// An `http://` base URL needs the `ota-ddi-insecure-http` feature (WARN once per boot); `https://` is refused with [`DdiTransportError::TlsUnavailable`] until the TLS round lands, as on the ESP-IDF tier.
///
/// # Time limits
///
/// Each request has a deadline (`DdiTransportConfig::deadline`) on top of the wait of one network operation (`DdiTransportConfig::timeout`).
/// The DNS lookup, the connect, the request send with the response head, and each body read wait at most `min(timeout, time left)`.
///
/// `Debug` prints the token redacted.
///
/// ```ignore
/// let token = DdiToken::parse(token_hex)?;
/// let transport = DdiTransport::new(
///     DdiTransportConfig::new("http://192.168.1.10:8080/DEFAULT", "dev-1", token),
///     stack,
/// )?;
/// let response = transport.poll().await?;
/// log::info!("poll answered {}", response.status);
/// ```
pub struct DdiTransport {
    endpoint: DdiEndpoint,
    token: DdiToken,
    timeout: Duration,
    deadline: Duration,
    stack: Stack<'static>,
    requests: Mutex<CriticalSectionRawMutex, SocketBuffers>,
    downloads: Mutex<CriticalSectionRawMutex, SocketBuffers>,
}

impl fmt::Debug for DdiTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DdiTransport")
            .field("endpoint", &self.endpoint)
            .field("token", &self.token)
            .field("timeout", &self.timeout)
            .field("deadline", &self.deadline)
            .finish_non_exhaustive()
    }
}

/// One request on its socket: where it goes and what it sends.
struct Exchange<'a> {
    url: &'a str,
    endpoint: IpEndpoint,
    head: &'a [u8],
    body: &'a [u8],
    start: Instant,
}

impl DdiTransport {
    /// Experimental: API may change before 1.0.
    ///
    /// Validates the base URL and the controller id, takes the token and allocates the socket buffers; nothing is sent.
    ///
    /// An `http://` base URL is refused with [`DdiTransportError::InsecureHttp`] unless the `ota-ddi-insecure-http` feature is enabled, which logs a WARN on the first such transport of the boot.
    /// An `https://` base URL is refused with [`DdiTransportError::TlsUnavailable`] until the DDI transport gains TLS.
    pub fn new(
        config: DdiTransportConfig,
        stack: Stack<'static>,
    ) -> Result<Self, DdiTransportError> {
        let accepted = plan::accept(config, cfg!(feature = "ota-ddi-insecure-http"))?;
        Ok(Self::from_accepted(accepted, stack))
    }

    /// The transport of a configuration [`plan::accept`] already accepted: the DDI runtime validates in its `new` and builds the transport in its `start`, once the station stack exists.
    pub(crate) fn from_accepted(accepted: plan::Accepted, stack: Stack<'static>) -> Self {
        Self {
            endpoint: accepted.endpoint,
            token: accepted.token,
            timeout: accepted.timeout,
            deadline: accepted.deadline,
            stack,
            requests: Mutex::new(SocketBuffers::new(REQUEST_RX_BYTES, REQUEST_TX_BYTES)),
            downloads: Mutex::new(SocketBuffers::new(DOWNLOAD_RX_BYTES, DOWNLOAD_TX_BYTES)),
        }
    }

    /// Experimental: API may change before 1.0.
    ///
    /// GET of the controller base resource, the URL built from the configured base and controller id.
    pub async fn poll(&self) -> Result<DdiResponse, DdiTransportError> {
        self.request(plan::poll(&self.endpoint)).await
    }

    /// Experimental: API may change before 1.0.
    ///
    /// GET of a `deploymentBase` link, after the origin check, at the URL rebuilt from the checked parts.
    pub async fn deployment(&self, href: &Href) -> Result<DdiResponse, DdiTransportError> {
        self.request(plan::link(&self.endpoint, href)?).await
    }

    /// Experimental: API may change before 1.0.
    ///
    /// GET of a `cancelAction` link, after the origin check, at the URL rebuilt from the checked parts.
    pub async fn cancel_action(&self, href: &Href) -> Result<DdiResponse, DdiTransportError> {
        self.request(plan::link(&self.endpoint, href)?).await
    }

    /// Experimental: API may change before 1.0.
    ///
    /// POST of a deployment feedback to `<controller>/deploymentBase/<action>/feedback`, built from the configured base and `action`, never from a link.
    pub async fn deployment_feedback(
        &self,
        action: ActionId,
        feedback: &Feedback<'_>,
    ) -> Result<DdiResponse, DdiTransportError> {
        self.request(plan::deployment_feedback(
            &self.endpoint,
            &action,
            feedback,
        )?)
        .await
    }

    /// Experimental: API may change before 1.0.
    ///
    /// POST of a cancel feedback to `<controller>/cancelAction/<action>/feedback`, built from the configured base and `action`, never from a link.
    pub async fn cancel_feedback(
        &self,
        action: ActionId,
        feedback: &Feedback<'_>,
    ) -> Result<DdiResponse, DdiTransportError> {
        self.request(plan::cancel_feedback(&self.endpoint, &action, feedback)?)
            .await
    }

    /// Experimental: API may change before 1.0.
    ///
    /// PUT of the configData body to `<controller>/configData`, built from the configured base.
    pub async fn config_data(
        &self,
        data: &ConfigData<'_>,
    ) -> Result<DdiResponse, DdiTransportError> {
        self.request(plan::config_data(&self.endpoint, data)?).await
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Fetches the artifact at `href` into the slot `manager` writes, the same streaming download, verification and slot swap as [`EspHalOtaManager::fetch_and_apply_cancellable`], with the token sent, the server name resolved and redirects refused.
    ///
    /// A link that fails the origin check fails with [`OtaError::ForeignOrigin`] before any request, and an accepted one is fetched at the rebuilt URL, never at the link text; a 3xx fails with [`OtaError::Redirect`].
    /// The declared `Content-Length` must equal `size`, or the download fails with [`OtaError::SizeMismatch`] before any flash is touched.
    /// The image header must name this firmware's chip (`TARGET_CHIP`), always: this tier's activation does not read it, so the chip is checked here ([`OtaError::TargetMismatch`]), whatever `with_expected_chip` the manager has.
    /// The manager's timeout and deadline apply.
    pub async fn download(
        &self,
        manager: &mut EspHalOtaManager<'_>,
        href: &Href,
        sha256: &[u8; 32],
        size: u64,
        cancel: &OtaCancel,
    ) -> Result<(), OtaError> {
        self.download_capturing(manager, href, sha256, size, cancel, &mut None)
            .await
    }

    /// [`download`](Self::download) that also hands back the `Retry-After` of a non-200 answer in `retry_after` (left unchanged otherwise), for the retry policy.
    pub(crate) async fn download_capturing(
        &self,
        manager: &mut EspHalOtaManager<'_>,
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
        let Some(chip) = ImageChip::from_target(TARGET_CHIP) else {
            log::error!("DDI download: {} is not a known image chip", TARGET_CHIP);
            return Err(OtaError::TargetMismatch);
        };
        log::info!("DDI download from {}", UrlForLog(url.as_str()));
        let authorization = self.token.authorization();
        let mut buffers = self.downloads.lock().await;
        let SocketBuffers { rx, tx } = &mut *buffers;
        let mut socket = TcpSocket::new(self.stack, rx, tx);
        let fetch = DdiFetch {
            stack: self.stack,
            url: &url,
            authorization: authorization.expose(),
            gate: DdiGate { size, chip },
            retry_after,
        };
        manager.fetch_ddi(&mut socket, fetch, sha256, cancel).await
    }

    /// The wait of the next network operation of a request that began at `start`: the per-operation timeout clipped to the time left of the deadline; [`DdiTransportError::Timeout`] once the deadline passed.
    fn wait(&self, start: Instant) -> Result<Duration, DdiTransportError> {
        let wait = self
            .timeout
            .min(self.deadline.saturating_sub(to_core(start.elapsed())));
        if wait.is_zero() {
            Err(DdiTransportError::Timeout)
        } else {
            Ok(wait)
        }
    }

    /// One planned request on the request socket: resolve, connect, send, read the head, refuse a redirect, then read the capped body inside the deadline.
    async fn request(&self, planned: Planned) -> Result<DdiResponse, DdiTransportError> {
        let start = Instant::now();
        let Planned { method, url, body } = planned;
        let parsed = parse_url(&url).map_err(|e| {
            log::error!("DDI request to {} not sent: {:?}", UrlForLog(&url), e);
            DdiTransportError::ServerUnreachable
        })?;
        let body = body.unwrap_or_default();
        let authorization = self.token.authorization();
        let head = request_head(
            method,
            &parsed,
            authorization.expose(),
            ACCEPT_JSON,
            body.len(),
        );
        drop(authorization);

        let address = resolve(self.stack, parsed.host, self.wait(start)?)
            .await
            .map_err(|e| match e {
                ResolveError::Timeout => DdiTransportError::Timeout,
                ResolveError::Unreachable => DdiTransportError::ServerUnreachable,
            })?;
        let exchange = Exchange {
            url: &url,
            endpoint: IpEndpoint::new(address, parsed.port),
            head: &head,
            body: &body,
            start,
        };

        let mut buffers = self.requests.lock().await;
        let SocketBuffers { rx, tx } = &mut *buffers;
        let mut socket = TcpSocket::new(self.stack, rx, tx);
        let result = self.exchange(&mut socket, &exchange).await;
        if result.is_ok() {
            socket.close();
        } else {
            socket.abort();
        }
        let _ = with_timeout(CLOSE_FLUSH_WAIT, socket.flush()).await;
        result
    }

    /// The exchange of [`request`](Self::request) on a fresh socket.
    async fn exchange(
        &self,
        socket: &mut TcpSocket<'_>,
        x: &Exchange<'_>,
    ) -> Result<DdiResponse, DdiTransportError> {
        let url = UrlForLog(x.url);
        match with_timeout(to_embassy(self.wait(x.start)?), socket.connect(x.endpoint)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                log::error!("DDI request to {} failed: connect {:?}", url, e);
                return Err(DdiTransportError::ServerUnreachable);
            }
            Err(_) => {
                log::error!("DDI request to {} timed out connecting", url);
                return Err(DdiTransportError::Timeout);
            }
        }
        let head = match with_timeout(
            to_embassy(self.wait(x.start)?),
            send_and_read_head(socket, &[x.head, x.body]),
        )
        .await
        {
            Ok(Ok(head)) => head,
            Ok(Err(e)) => {
                log::error!("DDI response from {} failed: {:?}", url, e);
                return Err(DdiTransportError::ServerUnreachable);
            }
            Err(_) => {
                log::error!("DDI response from {} timed out", url);
                return Err(DdiTransportError::Timeout);
            }
        };

        let status = head.status;
        if is_redirect(status) {
            log::warn!("DDI {} answered {}: redirect refused", url, status);
            return Err(DdiTransportError::Redirect { status });
        }
        let retry_after = head.retry_after;
        let failed = |failure| {
            log::error!(
                "DDI response body from {} ({}) failed: {}",
                url,
                status,
                failure
            );
            DdiTransportError::Body {
                status,
                retry_after: retry_after.clone(),
                failure,
            }
        };
        let mut reader = BodyReader::new(head.framing).map_err(failed)?;
        let mut chunk = [0u8; READ_CHUNK];
        while !reader.is_done() {
            let Ok(wait) = self.wait(x.start) else {
                return Err(failed(juggler::ota::ddi::BodyFailure::Timeout));
            };
            match with_timeout(to_embassy(wait), socket.read(&mut chunk)).await {
                Err(_) => return Err(failed(juggler::ota::ddi::BodyFailure::Timeout)),
                Ok(Err(_)) => {
                    return Err(failed(juggler::ota::ddi::BodyFailure::ServerUnreachable))
                }
                Ok(Ok(0)) => break,
                Ok(Ok(n)) => {
                    reader.feed(&chunk[..n]).map_err(failed)?;
                }
            }
        }
        let body = reader.finish().map_err(failed)?;
        Ok(DdiResponse {
            status,
            retry_after,
            body,
        })
    }
}
