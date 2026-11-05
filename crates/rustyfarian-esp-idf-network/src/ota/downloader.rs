//! Firmware downloader over plain HTTP using `EspHttpConnection`.
//!
//! HTTPS is explicitly rejected at the ADR 011 MVP scope.
//! TLS support belongs to `ota-hardened`.

use std::time::Duration;

use embedded_svc::http::Headers;
use esp_idf_svc::http::client::{Configuration, EspHttpConnection};
use esp_idf_svc::http::Method;

use juggler::ota::{classify_read_error, OtaError};

/// Buffer size for downloading firmware chunks.
pub(super) const DOWNLOAD_BUFFER_SIZE: usize = 4096;

/// `true` when an `esp_http_client_read` failure code means "the read timed out".
///
/// `esp_http_client_read` returns `-ESP_ERR_HTTP_EAGAIN` when its wait elapses
/// before data arrives, and `EspHttpConnection::read` wraps any negative return
/// as-is, so `EspError::code()` is negative.
/// The positive constant is also accepted because the ESP-IDF v4 workaround in
/// `esp-idf-svc` builds the error from it.
pub(super) fn is_read_timeout(code: esp_idf_svc::sys::esp_err_t) -> bool {
    use esp_idf_svc::sys::ESP_ERR_HTTP_EAGAIN;
    code == -ESP_ERR_HTTP_EAGAIN || code == ESP_ERR_HTTP_EAGAIN
}

/// Strip RFC 3986 `userinfo` (`user:password@`) from a URL before logging it.
///
/// HTTPS is rejected by [`create_http_connection`], but plain HTTP URLs may
/// still legally embed credentials per RFC 3986 section 3.2.1.
/// Logging the URL verbatim would leak those credentials into the console and
/// `espflash monitor` output.
/// This helper splits at `://`, drops anything up to the last `@` in the
/// authority, and rejoins.
/// If the input has no scheme it is returned unchanged (the caller is logging
/// arbitrary text, not a URL).
///
/// ```
/// use rustyfarian_esp_idf_network::ota::url_for_log;
///
/// assert_eq!(
///     url_for_log("http://user:pass@192.168.1.1/fw.bin"),
///     "http://192.168.1.1/fw.bin"
/// );
/// assert_eq!(url_for_log("not a url"), "not a url");
/// ```
pub fn url_for_log(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_string();
    };
    let (authority, path) = rest.split_once('/').map_or((rest, ""), |(a, p)| (a, p));
    let authority_no_userinfo = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    if path.is_empty() {
        format!("{scheme}://{authority_no_userinfo}")
    } else {
        format!("{scheme}://{authority_no_userinfo}/{path}")
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
        let content_length = client.content_len();
        let chunked = client.header("Transfer-Encoding").is_some();
        Ok(FirmwareResponse {
            client,
            content_length,
            chunked,
        })
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
    /// The declared `Content-Length`, or `None` when the response carries none
    /// (including `Transfer-Encoding: chunked`).
    pub fn content_length(&self) -> Option<u64> {
        self.content_length
    }

    /// The response carries a `Transfer-Encoding` header (for example `chunked`).
    pub fn chunked(&self) -> bool {
        self.chunked
    }

    /// Read the next body bytes into `buf`; `Ok(0)` is the end of the stream.
    ///
    /// A read that fails because its wait elapsed (`ESP_ERR_HTTP_EAGAIN`) is
    /// `DownloadTimeout`; any other read failure is `ServerUnreachable`
    /// (see [`classify_read_error`]).
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize, OtaError> {
        self.client.read(buf).map_err(|e| {
            log::error!("Read error during firmware download: {:?}", e);
            classify_read_error(is_read_timeout(e.code()))
        })
    }
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

    log::warn!("Using insecure HTTP for OTA: {}", url_for_log(url));

    let config = Configuration {
        timeout: Some(timeout),
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
