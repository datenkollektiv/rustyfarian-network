//! Firmware downloader over plain HTTP using `EspHttpConnection`.
//!
//! HTTPS is explicitly rejected at the ADR 011 MVP scope.
//! TLS support belongs to `ota-hardened`.

use std::io::Write;
use std::time::Duration;

use embedded_svc::http::Headers;
use esp_idf_svc::http::client::{Configuration, EspHttpConnection};
use esp_idf_svc::http::Method;

use juggler::ota::OtaError;

/// Buffer size for downloading firmware chunks.
const DOWNLOAD_BUFFER_SIZE: usize = 4096;

/// Strip RFC 3986 `userinfo` (`user:password@`) from a URL before logging it.
///
/// HTTPS is rejected by [`create_http_connection`], but plain HTTP URLs may
/// still legally embed credentials per RFC 3986 §3.2.1. Logging the URL
/// verbatim would leak those credentials into the console / `espflash monitor`
/// output. This helper splits at `://`, drops anything up to the last `@` in
/// the authority, and rejoins. If the URL has no scheme, it is returned
/// unchanged (the caller is logging arbitrary text, not a URL).
fn url_for_log(url: &str) -> String {
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

        let client = create_http_connection(&self.url, self.timeout)?;
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
    /// `expected_len` is the validated `Content-Length` (see
    /// [`check_content_length`]). A connection that closes before all of it
    /// arrives is a transport failure (`DownloadFailed { status: 0 }`, matching
    /// the esp-hal tier), not a successful download.
    ///
    /// Returns the total number of bytes downloaded.
    pub fn stream<W, F>(
        mut self,
        expected_len: usize,
        writer: &mut W,
        mut progress: F,
    ) -> Result<usize, OtaError>
    where
        W: Write,
        F: FnMut(usize, usize),
    {
        log::info!("Firmware size: {} bytes", expected_len);

        let mut downloaded = 0usize;
        let mut buffer = [0u8; DOWNLOAD_BUFFER_SIZE];

        while downloaded < expected_len {
            let want = (expected_len - downloaded).min(DOWNLOAD_BUFFER_SIZE);
            // The embedded-svc `read()` error type collapses connection-reset,
            // DNS-mid-stream, and read-timeout into one `IOError`. Map all of
            // them to `ServerUnreachable` — most production failures are
            // connection-shaped, and a true read-timeout is also a server that
            // stopped answering. A future hardened build can differentiate.
            let bytes_read = self.client.read(&mut buffer[..want]).map_err(|e| {
                log::error!("Read error during firmware download: {:?}", e);
                OtaError::ServerUnreachable
            })?;

            if bytes_read == 0 {
                break;
            }

            writer.write_all(&buffer[..bytes_read]).map_err(|e| {
                log::error!("Write error during firmware flash: {:?}", e);
                OtaError::FlashWriteFailed
            })?;

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
pub fn create_http_connection(url: &str, timeout: Duration) -> Result<EspHttpConnection, OtaError> {
    if url.starts_with("https://") {
        log::error!(
            "HTTPS firmware download is not supported in this build (ota-hardened scope). \
             URL: {}",
            url_for_log(url)
        );
        return Err(OtaError::ServerUnreachable);
    }

    log::warn!(
        "Using insecure HTTP for firmware download: {}",
        url_for_log(url)
    );

    let config = Configuration {
        timeout: Some(timeout),
        ..Default::default()
    };

    let mut client = EspHttpConnection::new(&config).map_err(|e| {
        log::error!("Failed to create HTTP client: {:?}", e);
        OtaError::ServerUnreachable
    })?;

    let headers = [("Accept", "application/octet-stream")];
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
    use super::{check_body_complete, check_content_length, url_for_log};
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
}
