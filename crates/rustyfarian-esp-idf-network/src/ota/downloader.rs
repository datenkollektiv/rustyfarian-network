//! Firmware downloader over plain HTTP using `EspHttpConnection`.
//!
//! HTTPS is explicitly rejected at the ADR 011 MVP scope.
//! TLS support belongs to `ota-hardened`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use embedded_svc::http::Headers;
use esp_idf_svc::handle::RawHandle;
use esp_idf_svc::http::client::{Configuration, EspHttpConnection};
use esp_idf_svc::http::Method;
use esp_idf_svc::sys::{esp, esp_http_client_set_timeout_ms, EspError};

use juggler::ota::{capped_read_len, classify_read_error, OtaError, UrlForLog};

/// Buffer size for downloading firmware chunks.
pub(super) const DOWNLOAD_BUFFER_SIZE: usize = 4096;

/// Receive buffer of the HTTP client, in bytes.
///
/// `esp_http_client_read` loops over `esp_transport_read`, each call asking for
/// at most this many bytes and waiting up to the client timeout.
/// TCP returns whatever has arrived, so a partial read waits again: one call can
/// make one wait per partial segment, up to this many waits against a server
/// that trickles bytes.
/// [`FirmwareResponse::read`] requests no more than this per call, which limits
/// that worst case but does not make a call one wait.
/// It also bounds one response header line.
pub(super) const HTTP_RX_BUFFER_SIZE: usize = 1024;

/// `true` when an `esp_http_client` failure code means "a wait timed out": a read, a header fetch or a connect.
///
/// `esp_http_client_read` returns `-ESP_ERR_HTTP_EAGAIN` when its wait elapses
/// before data arrives, and `EspHttpConnection::read` wraps any negative return
/// as-is, so `EspError::code()` is negative.
/// The positive constant is also accepted because the ESP-IDF v4 workaround in
/// `esp-idf-svc` builds the error from it.
/// `ESP_ERR_HTTP_CONNECTING` is what the ESP-IDF v6.1 API documents for
/// `esp_http_client_open` timing out before the connection is made, so it is a
/// timeout too, in either sign.
pub(super) fn is_read_timeout(code: esp_idf_svc::sys::esp_err_t) -> bool {
    use esp_idf_svc::sys::{ESP_ERR_HTTP_CONNECTING, ESP_ERR_HTTP_EAGAIN};
    [ESP_ERR_HTTP_EAGAIN, ESP_ERR_HTTP_CONNECTING]
        .iter()
        .any(|&timeout| code == timeout || code == -timeout)
}

/// Make a URL safe to log: strip the RFC 3986 `userinfo` (`user:password@`), the query and the fragment, and escape control characters.
///
/// HTTPS is rejected by [`create_http_connection`], but plain HTTP URLs may
/// still legally embed credentials per RFC 3986 section 3.2.1, and a query can
/// carry a token.
/// Logging the URL verbatim would leak those into the console and
/// `espflash monitor` output, and a control character could break or forge a
/// log line.
/// The authority ends at the first `/`, `?` or `#`; only the scheme, host,
/// port and path are kept.
/// If the input has no scheme it is returned whole with control characters
/// escaped (the caller is logging arbitrary text, not a URL).
/// This is [`juggler::ota::UrlForLog`] as a `String`; log any other
/// server-supplied string (a link, a file name) through
/// [`juggler::ota::TextForLog`].
///
/// ```
/// use rustyfarian_esp_idf_network::ota::url_for_log;
///
/// assert_eq!(
///     url_for_log("http://user:pass@192.168.1.1/fw.bin?c=secret#frag"),
///     "http://192.168.1.1/fw.bin"
/// );
/// assert_eq!(url_for_log("not a url"), "not a url");
/// ```
pub fn url_for_log(url: &str) -> String {
    UrlForLog(url).to_string()
}

/// Set once the insecure-HTTP WARN has been logged; statics reset on every boot.
static INSECURE_HTTP_WARNED: AtomicBool = AtomicBool::new(false);

/// Logs the insecure-HTTP WARN for the first plain-HTTP OTA connection of the boot only.
pub(crate) fn warn_insecure_http_once(url: &str) {
    if !INSECURE_HTTP_WARNED.swap(true, Ordering::Relaxed) {
        log::warn!(
            "Using insecure HTTP for OTA: {} (logged once per boot)",
            url_for_log(url)
        );
    }
}

/// Downloads firmware over plain HTTP.
///
/// [`connect`](Self::connect) opens the connection and reads the response
/// headers; [`FirmwareResponse::read`] then reads the body. The order of the
/// steps, and the Content-Length admission, belong to
/// `juggler::ota::download::DownloadMachine`.
pub struct FirmwareDownloader {
    url: String,
    timeout: Duration,
}

impl FirmwareDownloader {
    /// Create a new downloader targeting `url`.
    ///
    /// Default timeout is 30 seconds.
    pub fn new(url: &str) -> Self {
        Self {
            url: url.to_string(),
            timeout: Duration::from_secs(30),
        }
    }

    /// Override the connection timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Open the connection and read the `200` response headers.
    ///
    /// HTTPS URLs are rejected; only plain `http://` is accepted at MVP scope.
    pub fn connect(&self) -> Result<FirmwareResponse, OtaError> {
        log::info!(
            "Starting firmware download from: {}",
            url_for_log(&self.url)
        );

        let client = create_http_connection(&self.url, self.timeout, "application/octet-stream")?;
        Ok(FirmwareResponse::from_connection(client))
    }
}

/// A `200` response whose body has not been read yet.
pub struct FirmwareResponse {
    client: EspHttpConnection,
    /// The declared `Content-Length`; `None` when absent or chunked.
    content_length: Option<u64>,
    /// A `Transfer-Encoding` header is present.
    chunked: bool,
}

impl FirmwareResponse {
    /// Wraps a connection whose response headers have been read.
    pub(super) fn from_connection(client: EspHttpConnection) -> Self {
        let content_length = client.content_len();
        let chunked = client.header("Transfer-Encoding").is_some();
        Self {
            client,
            content_length,
            chunked,
        }
    }

    /// The declared `Content-Length`, or `None` when the response carries none
    /// (including `Transfer-Encoding: chunked`).
    pub fn content_length(&self) -> Option<u64> {
        self.content_length
    }

    /// The response carries a `Transfer-Encoding` header (for example `chunked`).
    pub fn chunked(&self) -> bool {
        self.chunked
    }

    /// Read the next body bytes into `buf`, waiting at most `timeout`;
    /// `Ok(0)` is the end of the stream.
    ///
    /// `timeout` replaces the connection's timeout for this read, so the
    /// caller can clip it to the time left of a total deadline.
    /// The request is capped to the client's receive buffer, which does NOT
    /// make a call one transport wait: `esp_http_client_read` waits again
    /// (up to `timeout` each time) whenever a read returns fewer bytes than
    /// requested, so a server that trickles bytes can keep one call going for
    /// up to [`HTTP_RX_BUFFER_SIZE`] waits.
    /// Drive it through `juggler::ota::read_bounded` to keep a call
    /// inside the time left of a deadline.
    ///
    /// A read that fails because its wait elapsed (`ESP_ERR_HTTP_EAGAIN`) is
    /// `DownloadTimeout`; any other read failure is `ServerUnreachable`
    /// (see [`classify_read_error`]).
    pub fn read(&mut self, buf: &mut [u8], timeout: Duration) -> Result<usize, OtaError> {
        let want = capped_read_len(
            u32::try_from(buf.len()).unwrap_or(u32::MAX),
            buf.len(),
            HTTP_RX_BUFFER_SIZE,
        );
        let buf = &mut buf[..want];
        set_wait(&mut self.client, timeout).map_err(|e| {
            log::error!("Could not set the read timeout: {:?}", e);
            OtaError::ServerUnreachable
        })?;
        self.client.read(buf).map_err(|e| {
            log::error!("Read error during firmware download: {:?}", e);
            classify_read_error(is_read_timeout(e.code()))
        })
    }
}

/// Sets the wait of the next transport operations of `client` (a header fetch or a body read), clamped to the C `int` milliseconds.
pub(super) fn set_wait(client: &mut EspHttpConnection, wait: Duration) -> Result<(), EspError> {
    let wait_ms = i32::try_from(wait.as_millis()).unwrap_or(i32::MAX);
    // SAFETY: the handle is the live `esp_http_client` owned by `client`,
    // which outlives the call; `esp_http_client_set_timeout_ms` only stores
    // the value in it.
    esp!(unsafe { esp_http_client_set_timeout_ms(client.handle(), wait_ms) })
}

/// Create an `EspHttpConnection`, initiate a GET request, read the response headers,
/// and return the connection ready for reading the response body.
///
/// Only plain `http://` is supported.
/// Returns `Err(OtaError::ServerUnreachable)` for `https://` URLs —
/// TLS is deferred to the `ota-hardened` scope (ADR 011).
///
/// `accept` is sent as the `Accept` header (`application/octet-stream` for firmware, `application/json` for a manifest).
pub fn create_http_connection(
    url: &str,
    timeout: Duration,
    accept: &str,
) -> Result<EspHttpConnection, OtaError> {
    if url.starts_with("https://") {
        log::error!(
            "HTTPS firmware download is not supported in this build (ota-hardened scope). \
             URL: {}",
            url_for_log(url)
        );
        return Err(OtaError::ServerUnreachable);
    }

    warn_insecure_http_once(url);

    let config = Configuration {
        timeout: Some(timeout),
        buffer_size: Some(HTTP_RX_BUFFER_SIZE),
        ..Default::default()
    };

    let mut client = EspHttpConnection::new(&config).map_err(|e| {
        log::error!("Failed to create HTTP client: {:?}", e);
        OtaError::ServerUnreachable
    })?;

    let headers = [("Accept", accept)];
    client
        .initiate_request(Method::Get, url, &headers)
        .map_err(|e| {
            log::error!(
                "Failed to initiate HTTP GET request to {}: {:?}",
                url_for_log(url),
                e
            );
            OtaError::ServerUnreachable
        })?;

    client.initiate_response().map_err(|e| {
        log::error!(
            "Failed to read HTTP response from {}: {:?}",
            url_for_log(url),
            e
        );
        OtaError::ServerUnreachable
    })?;

    let status = client.status();
    if status != 200 {
        log::error!("HTTP GET {} returned status {}", url_for_log(url), status);
        return Err(OtaError::DownloadFailed { status });
    }

    Ok(client)
}

#[cfg(test)]
mod tests {
    use super::{is_read_timeout, url_for_log};

    #[test]
    fn url_without_userinfo_unchanged() {
        assert_eq!(
            url_for_log("http://192.168.1.1/fw.bin"),
            "http://192.168.1.1/fw.bin"
        );
        assert_eq!(
            url_for_log("http://example.com:8080/x"),
            "http://example.com:8080/x"
        );
    }

    #[test]
    fn url_with_userinfo_strips_credentials() {
        assert_eq!(
            url_for_log("http://user:pass@192.168.1.1/fw.bin"),
            "http://192.168.1.1/fw.bin"
        );
        assert_eq!(
            url_for_log("http://alice:secret@example.com:8080/firmware"),
            "http://example.com:8080/firmware"
        );
    }

    #[test]
    fn url_with_userinfo_only_username() {
        assert_eq!(url_for_log("http://user@host/path"), "http://host/path");
    }

    #[test]
    fn url_without_path_or_userinfo() {
        assert_eq!(url_for_log("http://host"), "http://host");
        assert_eq!(url_for_log("http://user:pass@host"), "http://host");
    }

    #[test]
    fn url_query_and_fragment_are_stripped() {
        assert_eq!(url_for_log("http://h/fw.bin?c=0123abcd"), "http://h/fw.bin");
        assert_eq!(url_for_log("http://h/fw.bin#frag"), "http://h/fw.bin");
        assert_eq!(url_for_log("http://user@host?x"), "http://host");
    }

    #[test]
    fn url_control_characters_are_escaped() {
        assert_eq!(url_for_log("http://h/a\r\nb"), r"http://h/a\r\nb");
    }

    #[test]
    fn non_url_input_passes_through() {
        // No `://` separator → not a URL, return as-is.
        assert_eq!(url_for_log("not a url"), "not a url");
    }

    #[test]
    fn eagain_is_a_read_timeout_in_either_sign() {
        use esp_idf_svc::sys::ESP_ERR_HTTP_EAGAIN;
        assert!(is_read_timeout(-ESP_ERR_HTTP_EAGAIN));
        assert!(is_read_timeout(ESP_ERR_HTTP_EAGAIN));
    }

    #[test]
    fn a_connect_timeout_is_a_timeout_in_either_sign() {
        use esp_idf_svc::sys::ESP_ERR_HTTP_CONNECTING;
        assert!(is_read_timeout(ESP_ERR_HTTP_CONNECTING));
        assert!(is_read_timeout(-ESP_ERR_HTTP_CONNECTING));
        assert_eq!(
            juggler::ota::classify_read_error(is_read_timeout(ESP_ERR_HTTP_CONNECTING)),
            juggler::ota::OtaError::DownloadTimeout
        );
    }

    #[test]
    fn other_codes_are_not_read_timeouts() {
        use esp_idf_svc::sys::{ESP_ERR_HTTP_CONNECT, ESP_FAIL};
        assert!(!is_read_timeout(ESP_FAIL));
        assert!(!is_read_timeout(-ESP_FAIL));
        assert!(!is_read_timeout(ESP_ERR_HTTP_CONNECT));
        assert!(!is_read_timeout(-ESP_ERR_HTTP_CONNECT));
        assert!(!is_read_timeout(0));
    }

    #[test]
    fn read_timeout_classification_matches_wire_codes() {
        use esp_idf_svc::sys::ESP_ERR_HTTP_EAGAIN;
        assert_eq!(
            juggler::ota::classify_read_error(is_read_timeout(-ESP_ERR_HTTP_EAGAIN)).code(),
            "download_timeout"
        );
        assert_eq!(
            juggler::ota::classify_read_error(is_read_timeout(-1)).code(),
            "server_unreachable"
        );
    }
}
