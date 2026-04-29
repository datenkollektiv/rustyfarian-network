//! Firmware downloader over plain HTTP using `EspHttpConnection`.
//!
//! HTTPS is explicitly rejected at the ADR 011 MVP scope.
//! TLS support belongs to `ota-hardened`.

use std::io::Write;
use std::time::Duration;

use embedded_svc::http::Headers;
use esp_idf_svc::http::client::{Configuration, EspHttpConnection};
use esp_idf_svc::http::Method;

use ota_pure::OtaError;

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

/// Downloads firmware over plain HTTP and writes it to an arbitrary `Write` sink.
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

    /// Download the firmware, writing each chunk to `writer` and calling
    /// `progress(bytes_downloaded, total_bytes)` after each chunk.
    ///
    /// Returns the total number of bytes downloaded.
    ///
    /// HTTPS URLs are rejected; only plain `http://` is accepted at MVP scope.
    pub fn download<W, F>(&self, writer: &mut W, mut progress: F) -> Result<usize, OtaError>
    where
        W: Write,
        F: FnMut(usize, Option<usize>),
    {
        log::info!(
            "Starting firmware download from: {}",
            url_for_log(&self.url)
        );

        let mut client = create_http_connection(&self.url, self.timeout)?;

        let total_size = client.content_len().map(|len| len as usize);
        if let Some(size) = total_size {
            log::info!("Firmware size: {} bytes", size);
        }

        let mut downloaded = 0usize;
        let mut buffer = [0u8; DOWNLOAD_BUFFER_SIZE];

        loop {
            // The embedded-svc `read()` error type collapses connection-reset,
            // DNS-mid-stream, and read-timeout into one `IOError`. Map all of
            // them to `ServerUnreachable` — most production failures are
            // connection-shaped, and a true read-timeout is also a server that
            // stopped answering. A future hardened build can differentiate.
            let bytes_read = client.read(&mut buffer).map_err(|e| {
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
            progress(downloaded, total_size);

            if downloaded % (64 * 1024) < DOWNLOAD_BUFFER_SIZE {
                if let Some(total) = total_size {
                    let percent = (downloaded * 100) / total;
                    log::debug!("Download progress: {}% ({}/{})", percent, downloaded, total);
                }
            }
        }

        log::info!("Download complete: {} bytes", downloaded);
        Ok(downloaded)
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
    use super::url_for_log;

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
