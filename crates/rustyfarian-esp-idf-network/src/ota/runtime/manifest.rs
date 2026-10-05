//! Manifest fetch over plain HTTP with a size cap and a total time limit.

use std::time::{Duration, Instant};

use juggler::ota::runtime::{Feed, FetchError, ManifestBody};
use juggler::ota::{classify_read_error, Manifest, MAX_MANIFEST_BYTES};

use crate::ota::downloader::{create_http_connection, is_read_timeout};

/// The two time limits of one fetch.
#[derive(Debug, Clone, Copy)]
pub(super) struct ManifestLimits {
    /// `esp_http_client` timeout of one network operation.
    pub per_op: Duration,
    /// Total time of the whole fetch.
    pub total: Duration,
}

/// Fetches and parses the manifest at `url`.
///
/// The total clock starts before the connection is created.
/// The limit is checked after the response headers and after every read, so the worst case is `total` plus one `per_op`.
/// The URL is only ever logged by the helpers through `url_for_log`; the body is never logged.
/// Redirects follow the HTTP client's default, as the firmware download does.
pub(super) fn fetch_manifest(url: &str, limits: ManifestLimits) -> Result<Manifest, FetchError> {
    let start = Instant::now();
    let mut body = ManifestBody::new(limits.total);
    let mut client = create_http_connection(url, limits.per_op, "application/json")
        .map_err(FetchError::Connect)?;
    body.check(start.elapsed())?;

    let mut buffer = [0u8; MAX_MANIFEST_BYTES + 1];
    let mut len = 0usize;
    loop {
        let read = client.read(&mut buffer[len..]).map_err(|e| {
            log::warn!("[ota] manifest read failed: {:?}", e);
            FetchError::Read(classify_read_error(is_read_timeout(e.code())))
        })?;
        match body.feed(read, start.elapsed()) {
            Feed::Continue => len += read,
            Feed::Done => break,
            Feed::TooLarge => return Err(FetchError::TooLarge),
            Feed::DeadlineExceeded => return Err(FetchError::DeadlineExceeded),
        }
    }
    Manifest::parse(&buffer[..len]).map_err(FetchError::Parse)
}
