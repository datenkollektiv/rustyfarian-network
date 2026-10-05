//! Firmware downloader over plain HTTP using `EspHttpConnection`.
//!
//! HTTPS is explicitly rejected at the ADR 011 MVP scope.
//! TLS support belongs to `ota-hardened`.

use std::io::Write;
use std::time::Duration;

use embedded_svc::http::Headers;
use esp_idf_svc::http::client::{Configuration, EspHttpConnection};
use esp_idf_svc::http::Method;

use juggler::ota::{classify_read_error, OtaError};

/// Buffer size for downloading firmware chunks.
const DOWNLOAD_BUFFER_SIZE: usize = 4096;

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
/// Split into two phases so no flash is touched before the server has
/// answered: [`connect`](Self::connect) opens the connection and reads the
/// response headers, and [`FirmwareResponse::stream`] then writes the body to
/// an arbitrary `Write` sink.
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
        Ok(FirmwareResponse {
            client,
            content_length,
        })
    }
}

/// A `200` response whose body has not been read yet.
pub struct FirmwareResponse {
    client: EspHttpConnection,
    /// The declared `Content-Length`; `None` when absent or chunked.
    content_length: Option<u64>,
}

impl FirmwareResponse {
    /// The declared `Content-Length`, or `None` when the response carries none
    /// (including `Transfer-Encoding: chunked`).
    pub fn content_length(&self) -> Option<u64> {
        self.content_length
    }

    /// Stream exactly `expected_len` body bytes into `writer`, calling
    /// `progress(bytes_downloaded, expected_len)` after each chunk.
    ///
    /// `checkpoint` runs right after every successful, non-empty network read
    /// and right after every successful write to `writer`; an `Err` from it
    /// stops the download and is returned unchanged.
    /// It is never called after a failed read or write, nor after an
    /// end-of-stream read, so the operation's own error (including the short
    /// body `DownloadFailed { status: 0 }`) always takes precedence over a
    /// deadline.
    ///
    /// A read that fails because its wait elapsed (`ESP_ERR_HTTP_EAGAIN`) is
    /// `DownloadTimeout`; any other read failure is `ServerUnreachable`
    /// (see [`classify_read_error`]).
    ///
    /// `expected_len` is the validated `Content-Length` (see
    /// [`check_content_length`]). A connection that closes before all of it
    /// arrives is a transport failure (`DownloadFailed { status: 0 }`, matching
    /// the esp-hal tier), not a successful download.
    ///
    /// Returns the total number of bytes downloaded.
    pub fn stream<W, F, K>(
        mut self,
        expected_len: usize,
        writer: &mut W,
        mut progress: F,
        mut checkpoint: K,
    ) -> Result<usize, OtaError>
    where
        W: Write,
        F: FnMut(usize, usize),
        K: FnMut() -> Result<(), OtaError>,
    {
        log::info!("Firmware size: {} bytes", expected_len);

        let mut downloaded = 0usize;
        let mut buffer = [0u8; DOWNLOAD_BUFFER_SIZE];

        while downloaded < expected_len {
            let want = (expected_len - downloaded).min(DOWNLOAD_BUFFER_SIZE);
            let bytes_read = self.client.read(&mut buffer[..want]).map_err(|e| {
                log::error!("Read error during firmware download: {:?}", e);
                classify_read_error(is_read_timeout(e.code()))
            })?;

            if bytes_read == 0 {
                break;
            }
            checkpoint()?;

            writer.write_all(&buffer[..bytes_read]).map_err(|e| {
                log::error!("Write error during firmware flash: {:?}", e);
                OtaError::FlashWriteFailed
            })?;
            checkpoint()?;

            downloaded += bytes_read;
            progress(downloaded, expected_len);

            if downloaded % (64 * 1024) < DOWNLOAD_BUFFER_SIZE {
                let percent = (downloaded * 100) / expected_len;
                log::debug!(
                    "Download progress: {}% ({}/{})",
                    percent,
                    downloaded,
                    expected_len
                );
            }
        }

        check_body_complete(downloaded, expected_len)?;
        log::info!("Download complete: {} bytes", downloaded);
        Ok(downloaded)
    }
}

/// Validate the declared `Content-Length` against the update partition's
/// `capacity`, returning the image length to stream.
///
/// Mirrors the esp-hal tier's strict transport (ADR 011 §2): a missing
/// `Content-Length` (including chunked transfer) or a zero-length body is a
/// protocol-shape rejection (`DownloadFailed { status: 0 }`), and an image
/// larger than the partition is `InsufficientSpace`. Zero is rejected
/// explicitly because `esp_ota_begin` treats an image size of `0` as "erase
/// the whole partition".
pub fn check_content_length(
    content_length: Option<u64>,
    capacity: usize,
) -> Result<usize, OtaError> {
    match content_length {
        None => {
            log::error!("Firmware response has no Content-Length (missing or chunked); rejected");
            Err(OtaError::DownloadFailed { status: 0 })
        }
        Some(0) => {
            log::error!("Firmware response declares an empty body; rejected");
            Err(OtaError::DownloadFailed { status: 0 })
        }
        Some(len) if len > capacity as u64 => {
            log::error!(
                "Firmware image ({} bytes) exceeds the OTA partition ({} bytes)",
                len,
                capacity
            );
            Err(OtaError::InsufficientSpace)
        }
        Some(len) => Ok(len as usize),
    }
}

/// Reject a body that ended before `expected` bytes arrived.
fn check_body_complete(received: usize, expected: usize) -> Result<(), OtaError> {
    if received == expected {
        Ok(())
    } else {
        log::error!(
            "Firmware download ended early: {} of {} bytes received",
            received,
            expected
        );
        Err(OtaError::DownloadFailed { status: 0 })
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
    use super::{check_body_complete, check_content_length, is_read_timeout, url_for_log};
    use juggler::ota::OtaError;

    const SHAPE_REJECTED: OtaError = OtaError::DownloadFailed { status: 0 };

    #[test]
    fn content_length_within_capacity_accepted() {
        assert_eq!(check_content_length(Some(1024), 4096), Ok(1024));
        assert_eq!(check_content_length(Some(4096), 4096), Ok(4096));
    }

    #[test]
    fn content_length_over_capacity_is_insufficient_space() {
        assert_eq!(
            check_content_length(Some(4097), 4096),
            Err(OtaError::InsufficientSpace)
        );
        assert_eq!(
            check_content_length(Some(u64::MAX), 4096),
            Err(OtaError::InsufficientSpace)
        );
    }

    #[test]
    fn missing_or_zero_content_length_rejected() {
        assert_eq!(check_content_length(None, 4096), Err(SHAPE_REJECTED));
        assert_eq!(check_content_length(Some(0), 4096), Err(SHAPE_REJECTED));
    }

    #[test]
    fn short_body_rejected_complete_body_accepted() {
        assert_eq!(check_body_complete(100, 100), Ok(()));
        assert_eq!(check_body_complete(99, 100), Err(SHAPE_REJECTED));
        assert_eq!(check_body_complete(0, 100), Err(SHAPE_REJECTED));
    }

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
