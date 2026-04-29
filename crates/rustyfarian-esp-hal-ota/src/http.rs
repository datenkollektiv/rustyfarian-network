//! Strict HTTP/1.1 GET client — internal transport for `rustyfarian-esp-hal-ota`.
//!
//! This HTTP client is an implementation detail of `rustyfarian-esp-hal-ota`
//! and not part of the crate's public API.
//! It may be removed if a workspace HTTP dependency arrives later.
//!
//! The client accepts **only** `HTTP/1.1 200 OK` responses with exactly one
//! valid `Content-Length` header.  Everything else is an explicit error before
//! any flash partition is touched.

// When building without the embassy + chip features, the parsing types and
// functions defined here are only used in the `#[cfg(test)]` block below.
// Suppress the resulting dead_code warnings so `cargo clippy -D warnings`
// does not fail on stub/host builds.
#![cfg_attr(
    not(all(
        feature = "embassy",
        any(feature = "esp32c3", feature = "esp32c6", feature = "esp32")
    )),
    allow(dead_code)
)]

use ota_pure::OtaError;

// ── Internal error type ─────────────────────────────────────────────────────

/// Internal HTTP parse/protocol errors mapped to [`OtaError`] at the crate boundary.
#[derive(Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum HttpError {
    /// Status line did not start with `HTTP/1.1`.
    BadStatusLine,
    /// Status code was present and valid but not 200.
    NonSuccess { status: u16 },
    /// `Content-Length` header was absent.
    MissingContentLength,
    /// `Content-Length` header appeared more than once.
    DuplicateContentLength,
    /// `Content-Length` value could not be parsed as a decimal `u64`.
    InvalidContentLength,
    /// `Transfer-Encoding` header was present (any value — we reject it all).
    TransferEncodingPresent,
    /// The declared `Content-Length` exceeds the caller-supplied `max_bytes` limit.
    BodyTooLarge,
    /// I/O error reading from the socket.
    Io,
    /// Connection closed before the full response header was received.
    EarlyEof,
    /// URL could not be parsed as `http://<host>/<path>`.
    BadUrl,
}

impl From<HttpError> for OtaError {
    fn from(e: HttpError) -> OtaError {
        // The public `OtaError` surface collapses several internal variants
        // (BadStatusLine, MissingContentLength, Io, BadUrl, …) into
        // `ServerUnreachable`. Log the original variant before collapsing so
        // root cause survives in `espflash monitor` output.
        log::warn!("OTA HTTP error: {:?}", e);
        match e {
            // Server-side HTTP errors that carry a status code bubble up so
            // callers can log the exact code.
            HttpError::NonSuccess { status } => OtaError::DownloadFailed { status },

            // `Transfer-Encoding` present means the server is not speaking the
            // strict subset we accept — treat as a generic download failure with
            // status 0 so callers know it was a protocol-shape rejection rather
            // than a network error.
            HttpError::TransferEncodingPresent => OtaError::DownloadFailed { status: 0 },

            // Structural parse failures: the TCP endpoint answered but did not
            // speak recognisable HTTP/1.1 — treat as server unreachable.
            HttpError::BadStatusLine
            | HttpError::MissingContentLength
            | HttpError::DuplicateContentLength
            | HttpError::InvalidContentLength
            | HttpError::EarlyEof
            | HttpError::Io
            | HttpError::BadUrl => OtaError::ServerUnreachable,

            // The image is bigger than the inactive OTA partition.
            HttpError::BodyTooLarge => OtaError::InsufficientSpace,
        }
    }
}

// ── Parsed HTTP response descriptor ─────────────────────────────────────────

/// The validated header data produced by the HTTP parser.
///
/// Internal — not part of the crate's public API.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct HttpResponse {
    /// Exact byte count declared in `Content-Length`.
    pub content_length: u64,
}

// ── URL parser ───────────────────────────────────────────────────────────────

/// Parsed `http://host[:port]/path` URL.
///
/// DNS is the caller's responsibility for the MVP; only IP-literal hosts are
/// expected.  If the caller passes a hostname, it is forwarded verbatim in the
/// `Host` header — connect/resolve is the application's concern.
pub(crate) struct ParsedUrl<'a> {
    pub host: &'a str,
    /// TCP port — used both to connect the socket and to populate the `Host`
    /// request header when not the HTTP/1.1 default of 80 (per RFC 7230 §5.4,
    /// the `Host` field-value must mirror the authority component).
    pub port: u16,
    pub path: &'a str,
}

/// Parse `http://host[:port]/path`.
///
/// `https://` is rejected — plain HTTP only (ADR 011 §2).
/// The path defaults to `/` when absent.
pub(crate) fn parse_url(url: &str) -> Result<ParsedUrl<'_>, HttpError> {
    let rest = url.strip_prefix("http://").ok_or(HttpError::BadUrl)?;

    // Split host[:port] from path at the first `/`.
    let (authority, path) = match rest.find('/') {
        Some(idx) => (&rest[..idx], &rest[idx..]),
        None => (rest, "/"),
    };

    let (host, port) = if let Some(colon) = authority.rfind(':') {
        let port_str = &authority[colon + 1..];
        let port: u16 = port_str.parse().map_err(|_| HttpError::BadUrl)?;
        (&authority[..colon], port)
    } else {
        (authority, 80u16)
    };

    if host.is_empty() {
        return Err(HttpError::BadUrl);
    }

    Ok(ParsedUrl { host, port, path })
}

// ── Pure parsing functions (always compile, no I/O) ─────────────────────────

/// Parse an HTTP/1.1 status line and return the three-digit status code.
///
/// Accepts:
/// - `HTTP/1.1 NNN reason\r\n`
/// - `HTTP/1.1 NNN\r\n`  (reason phrase is optional per RFC 7230 §3.1.2)
///
/// Rejects anything that does not start with exactly `HTTP/1.1 `.
pub(crate) fn parse_status_line(line: &[u8]) -> Result<u16, HttpError> {
    // Minimum: "HTTP/1.1 NNN\r\n" = 14 bytes
    if !line.starts_with(b"HTTP/1.1 ") {
        return Err(HttpError::BadStatusLine);
    }
    let after_prefix = &line[b"HTTP/1.1 ".len()..];

    // Status code is the next three ASCII decimal digits.
    if after_prefix.len() < 3 {
        return Err(HttpError::BadStatusLine);
    }
    let code_bytes = &after_prefix[..3];

    // Must be exactly three ASCII digits.
    if !code_bytes.iter().all(|b| b.is_ascii_digit()) {
        return Err(HttpError::BadStatusLine);
    }

    // After the three digits there must be SP, CRLF, or LF (reason is optional).
    let after_code = &after_prefix[3..];
    if !after_code.is_empty()
        && after_code[0] != b' '
        && after_code[0] != b'\r'
        && after_code[0] != b'\n'
    {
        return Err(HttpError::BadStatusLine);
    }

    let hundreds = (code_bytes[0] - b'0') as u16 * 100;
    let tens = (code_bytes[1] - b'0') as u16 * 10;
    let ones = (code_bytes[2] - b'0') as u16;
    Ok(hundreds + tens + ones)
}

/// Split a single header line `Name: value\r\n` into the raw name and trimmed value bytes.
///
/// The name slice retains its original casing (case-insensitive comparison is
/// the caller's responsibility).  OWS (optional whitespace) is trimmed from
/// both ends of the value.
///
/// Returns `Err` if no `:` separator is present, or if the byte immediately
/// preceding the `:` is whitespace (RFC 7230 §3.2.4 forbids whitespace
/// between header field-name and colon — accepting it would let a crafted
/// `Content-Length\t: 0` silently bypass the duplicate-CL check because the
/// resulting name does not match `content-length` case-insensitively).
pub(crate) fn parse_header(line: &[u8]) -> Result<(&[u8], &[u8]), HttpError> {
    let colon = line
        .iter()
        .position(|&b| b == b':')
        .ok_or(HttpError::BadStatusLine)?;
    if colon == 0 {
        return Err(HttpError::BadStatusLine);
    }
    if matches!(line[colon - 1], b' ' | b'\t') {
        return Err(HttpError::BadStatusLine);
    }
    let name = &line[..colon];
    let value_raw = &line[colon + 1..];

    // Trim leading and trailing OWS and CRLF from value.
    let value = trim_ows(value_raw);
    Ok((name, value))
}

/// Trim ASCII whitespace (SP, HT, CR, LF) from both ends of a byte slice.
fn trim_ows(s: &[u8]) -> &[u8] {
    let start = s
        .iter()
        .position(|&b| !matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
        .unwrap_or(s.len());
    let end = s
        .iter()
        .rposition(|&b| !matches!(b, b' ' | b'\t' | b'\r' | b'\n'))
        .map(|i| i + 1)
        .unwrap_or(0);
    if start >= end {
        &[]
    } else {
        &s[start..end]
    }
}

/// ASCII-case-insensitive byte-slice comparison (per RFC 7230 header name rules).
fn eq_ignore_case(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .all(|(x, y)| x.eq_ignore_ascii_case(y))
}

/// Accumulator that scans header lines and enforces ADR 011 §2 constraints.
///
/// Call [`HeaderState::feed`] for each `Name: value\r\n` line (excluding the
/// status line and the blank separator line).  Call [`HeaderState::finish`]
/// after the blank line to retrieve the validated [`HttpResponse`].
pub(crate) struct HeaderState {
    content_length: Option<u64>,
    has_transfer_encoding: bool,
}

impl HeaderState {
    pub(crate) fn new() -> Self {
        Self {
            content_length: None,
            has_transfer_encoding: false,
        }
    }

    /// Feed one header line.  Returns `Err` on the first violation.
    pub(crate) fn feed(&mut self, line: &[u8]) -> Result<(), HttpError> {
        // Skip empty / whitespace-only lines (blank line signals end of headers;
        // callers should not pass it here, but be defensive).
        if trim_ows(line).is_empty() {
            return Ok(());
        }

        let (name, value) = parse_header(line)?;

        if eq_ignore_case(name, b"content-length") {
            if self.content_length.is_some() {
                return Err(HttpError::DuplicateContentLength);
            }
            // RFC 7230 §3.3.2: Content-Length is `1*DIGIT` — no sign, no whitespace,
            // no leading `+` (which `u64::from_str` would otherwise accept). Enforce
            // strict ASCII-digit-only input before delegating to from_str for the
            // overflow check. Leading zeros are tolerated (RFC permits them).
            if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
                return Err(HttpError::InvalidContentLength);
            }
            let s = core::str::from_utf8(value).map_err(|_| HttpError::InvalidContentLength)?;
            let n: u64 = s.parse().map_err(|_| HttpError::InvalidContentLength)?;
            self.content_length = Some(n);
        } else if eq_ignore_case(name, b"transfer-encoding") {
            self.has_transfer_encoding = true;
        }

        Ok(())
    }

    /// Validate and produce an [`HttpResponse`] after all headers have been fed.
    pub(crate) fn finish(self, max_bytes: u64) -> Result<HttpResponse, HttpError> {
        if self.has_transfer_encoding {
            return Err(HttpError::TransferEncodingPresent);
        }
        let content_length = self.content_length.ok_or(HttpError::MissingContentLength)?;
        if content_length > max_bytes {
            return Err(HttpError::BodyTooLarge);
        }
        Ok(HttpResponse { content_length })
    }
}

// ── Async fetch helper (embassy feature only) ────────────────────────────────

#[cfg(feature = "embassy")]
pub(crate) mod async_client {
    use super::{parse_status_line, HeaderState, HttpError, HttpResponse, ParsedUrl};
    use embassy_net::tcp::TcpSocket;

    /// Read the HTTP response headers line by line into `header_buf`.
    ///
    /// Returns the number of bytes consumed (including the blank separator
    /// line) and the raw lines parsed so far.  The header section is expected
    /// to fit within `header_buf`; oversized headers are treated as a protocol
    /// error.
    ///
    /// Line endings accepted: `\r\n` only (strict HTTP/1.1).
    async fn read_headers(
        socket: &mut TcpSocket<'_>,
        header_buf: &mut [u8],
    ) -> Result<usize, HttpError> {
        let mut filled = 0usize;
        loop {
            if filled >= header_buf.len() {
                return Err(HttpError::Io); // header too large
            }
            let n = socket
                .read(&mut header_buf[filled..filled + 1])
                .await
                .map_err(|_| HttpError::Io)?;
            if n == 0 {
                return Err(HttpError::EarlyEof);
            }
            filled += 1;

            // Check if we just completed the blank separator line "\r\n\r\n".
            if filled >= 4 && &header_buf[filled - 4..filled] == b"\r\n\r\n" {
                return Ok(filled);
            }
        }
    }

    /// Send a strict `GET` request and parse the response headers.
    ///
    /// On success, the socket's read cursor is positioned immediately after
    /// the blank header separator — the caller can stream the body directly.
    ///
    /// `max_bytes` is the inactive OTA partition size; the parser rejects any
    /// `Content-Length` that exceeds it with `OtaError::InsufficientSpace`.
    pub(crate) async fn fetch_get(
        socket: &mut TcpSocket<'_>,
        parsed: &ParsedUrl<'_>,
        max_bytes: u64,
    ) -> Result<HttpResponse, super::super::OtaError> {
        // 1. Send the request.
        let mut req_buf = [0u8; 256];
        // Format: GET /path HTTP/1.1\r\nHost: host[:port]\r\nConnection: close\r\n\r\n
        let req_len =
            super::format_get_request(&mut req_buf, parsed.host, parsed.port, parsed.path)
                .map_err(|_| super::super::OtaError::ServerUnreachable)?;

        // Write the full request, handling partial writes (embassy-net TcpSocket::write
        // does not impl embedded_io_async::Write, so we use its native write() directly).
        let mut written = 0;
        while written < req_len {
            let n = socket
                .write(&req_buf[written..req_len])
                .await
                .map_err(|_| super::super::OtaError::ServerUnreachable)?;
            if n == 0 {
                return Err(super::super::OtaError::ServerUnreachable);
            }
            written += n;
        }

        // 2. Read headers byte-by-byte until "\r\n\r\n".
        let mut header_buf = [0u8; 1024];
        let header_len = read_headers(socket, &mut header_buf)
            .await
            .map_err(super::super::OtaError::from)?;
        let _ = header_len; // consumed by read_headers

        // 3. Parse status line + headers from header_buf.
        let header_data = &header_buf[..header_len];
        let (status, headers_block) = split_status_line(header_data)?;

        // Validate status: must be exactly 200.
        if status != 200 {
            return Err(super::super::OtaError::DownloadFailed { status });
        }

        // 4. Parse each header line.
        let mut state = HeaderState::new();
        for line in HeaderLineIter::new(headers_block) {
            state.feed(line).map_err(super::super::OtaError::from)?;
        }

        // 5. Finalise — enforces Content-Length exactly-once, no Transfer-Encoding,
        //    and body-size <= max_bytes.
        state
            .finish(max_bytes)
            .map_err(super::super::OtaError::from)
    }

    /// Split the raw header block into the first line and the remainder.
    fn split_status_line(data: &[u8]) -> Result<(u16, &[u8]), super::super::OtaError> {
        let crlf = data
            .windows(2)
            .position(|w| w == b"\r\n")
            .ok_or(super::super::OtaError::ServerUnreachable)?;
        let first_line = &data[..crlf];
        let rest = &data[crlf + 2..];
        let status = parse_status_line(first_line).map_err(super::super::OtaError::from)?;
        Ok((status, rest))
    }

    /// Iterator over header lines separated by `\r\n` in a byte slice.
    struct HeaderLineIter<'a> {
        data: &'a [u8],
    }

    impl<'a> HeaderLineIter<'a> {
        fn new(data: &'a [u8]) -> Self {
            Self { data }
        }
    }

    impl<'a> Iterator for HeaderLineIter<'a> {
        type Item = &'a [u8];

        fn next(&mut self) -> Option<Self::Item> {
            if self.data.is_empty() {
                return None;
            }
            match self.data.windows(2).position(|w| w == b"\r\n") {
                Some(pos) if pos == 0 => {
                    // Blank line — end of headers.
                    self.data = &[];
                    None
                }
                Some(pos) => {
                    let line = &self.data[..pos];
                    self.data = &self.data[pos + 2..];
                    Some(line)
                }
                None => {
                    // Last fragment without trailing CRLF — shouldn't happen with
                    // well-formed HTTP, but yield it anyway.
                    let line = self.data;
                    self.data = &[];
                    if line.is_empty() {
                        None
                    } else {
                        Some(line)
                    }
                }
            }
        }
    }
}

// ── Request formatting (always compiles — host-testable) ─────────────────────

/// Format `GET /path HTTP/1.1\r\nHost: host[:port]\r\nConnection: close\r\n\r\n`
/// into `buf`.  Returns the number of bytes written, or `Err(())` if the
/// buffer is too small.
///
/// Per RFC 7230 §5.4, the `Host` field-value must mirror the URL authority
/// component.  The port is included whenever it is not the HTTP/1.1
/// default of 80; servers behind reverse proxies / virtual-host routers
/// will otherwise route the request incorrectly or reject it outright.
pub(crate) fn format_get_request(
    buf: &mut [u8],
    host: &str,
    port: u16,
    path: &str,
) -> Result<usize, ()> {
    let mut pos = 0;
    write_bytes(buf, &mut pos, b"GET ")?;
    write_bytes(buf, &mut pos, path.as_bytes())?;
    write_bytes(buf, &mut pos, b" HTTP/1.1\r\nHost: ")?;
    write_bytes(buf, &mut pos, host.as_bytes())?;
    if port != 80 {
        write_bytes(buf, &mut pos, b":")?;
        write_u16_decimal(buf, &mut pos, port)?;
    }
    write_bytes(buf, &mut pos, b"\r\nConnection: close\r\n\r\n")?;
    Ok(pos)
}

/// Append `src` at `*pos` in `buf`, advancing `*pos`. Errors if `buf` is too small.
fn write_bytes(buf: &mut [u8], pos: &mut usize, src: &[u8]) -> Result<(), ()> {
    let end = *pos + src.len();
    if end > buf.len() {
        return Err(());
    }
    buf[*pos..end].copy_from_slice(src);
    *pos = end;
    Ok(())
}

/// Append `n` as ASCII decimal at `*pos` in `buf`, advancing `*pos`.
/// At most 5 bytes (`u16::MAX = 65535`).
fn write_u16_decimal(buf: &mut [u8], pos: &mut usize, n: u16) -> Result<(), ()> {
    // Build the digits LSB-first into a fixed scratch buffer, then copy
    // them out in MSB-first order.
    let mut tmp = [0u8; 5];
    let mut len = 0;
    let mut n = n;
    if n == 0 {
        tmp[0] = b'0';
        len = 1;
    } else {
        while n > 0 {
            tmp[len] = b'0' + (n % 10) as u8;
            n /= 10;
            len += 1;
        }
    }
    if *pos + len > buf.len() {
        return Err(());
    }
    for i in 0..len {
        buf[*pos + i] = tmp[len - 1 - i];
    }
    *pos += len;
    Ok(())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Status line ──────────────────────────────────────────────────────────

    #[test]
    fn status_line_ok() {
        assert_eq!(parse_status_line(b"HTTP/1.1 200 OK"), Ok(200));
    }

    #[test]
    fn status_line_with_crlf() {
        assert_eq!(parse_status_line(b"HTTP/1.1 200 OK\r\n"), Ok(200));
    }

    #[test]
    fn status_line_no_reason_phrase_with_space() {
        // "HTTP/1.1 200 \r\n" — trailing space before CRLF
        assert_eq!(parse_status_line(b"HTTP/1.1 200 \r\n"), Ok(200));
    }

    #[test]
    fn status_line_no_reason_phrase_bare() {
        // "HTTP/1.1 200\r\n" — no reason phrase at all
        assert_eq!(parse_status_line(b"HTTP/1.1 200\r\n"), Ok(200));
    }

    #[test]
    fn status_line_wrong_version_http10() {
        assert_eq!(
            parse_status_line(b"HTTP/1.0 200 OK"),
            Err(HttpError::BadStatusLine)
        );
    }

    #[test]
    fn status_line_wrong_version_http2() {
        assert_eq!(
            parse_status_line(b"HTTP/2 200 OK"),
            Err(HttpError::BadStatusLine)
        );
    }

    #[test]
    fn status_line_redirect_301() {
        // parse_status_line itself succeeds; the *caller* maps non-200 to DownloadFailed.
        let status = parse_status_line(b"HTTP/1.1 301 Moved Permanently").unwrap();
        assert_eq!(status, 301);
        // Caller-level check:
        let err: OtaError = HttpError::NonSuccess { status }.into();
        assert_eq!(err, OtaError::DownloadFailed { status: 301 });
    }

    #[test]
    fn status_line_redirect_307() {
        let status = parse_status_line(b"HTTP/1.1 307 Temporary Redirect").unwrap();
        assert_eq!(status, 307);
        let err: OtaError = HttpError::NonSuccess { status }.into();
        assert_eq!(err, OtaError::DownloadFailed { status: 307 });
    }

    #[test]
    fn status_line_404() {
        let status = parse_status_line(b"HTTP/1.1 404 Not Found").unwrap();
        assert_eq!(status, 404);
        let err: OtaError = HttpError::NonSuccess { status }.into();
        assert_eq!(err, OtaError::DownloadFailed { status: 404 });
    }

    #[test]
    fn status_line_garbage() {
        assert_eq!(
            parse_status_line(b"not http"),
            Err(HttpError::BadStatusLine)
        );
        assert_eq!(parse_status_line(b""), Err(HttpError::BadStatusLine));
        assert_eq!(
            parse_status_line(b"GET / HTTP/1.1"),
            Err(HttpError::BadStatusLine)
        );
    }

    // ── Header parsing ───────────────────────────────────────────────────────

    #[test]
    fn header_content_length_present() {
        let mut state = HeaderState::new();
        state.feed(b"Content-Length: 1024").unwrap();
        let resp = state.finish(u64::MAX).unwrap();
        assert_eq!(resp.content_length, 1024);
    }

    #[test]
    fn header_content_length_missing() {
        let state = HeaderState::new();
        assert_eq!(state.finish(u64::MAX), Err(HttpError::MissingContentLength));
    }

    #[test]
    fn header_content_length_duplicate() {
        let mut state = HeaderState::new();
        state.feed(b"Content-Length: 100").unwrap();
        assert_eq!(
            state.feed(b"Content-Length: 200"),
            Err(HttpError::DuplicateContentLength)
        );
    }

    #[test]
    fn header_content_length_zero() {
        let mut state = HeaderState::new();
        state.feed(b"Content-Length: 0").unwrap();
        let resp = state.finish(u64::MAX).unwrap();
        assert_eq!(resp.content_length, 0);
    }

    #[test]
    fn header_content_length_overflow() {
        // 2^64 = 18446744073709551616 — exceeds u64::MAX
        let mut state = HeaderState::new();
        assert_eq!(
            state.feed(b"Content-Length: 18446744073709551616"),
            Err(HttpError::InvalidContentLength)
        );
    }

    #[test]
    fn header_content_length_non_numeric() {
        let mut state = HeaderState::new();
        assert_eq!(
            state.feed(b"Content-Length: abc"),
            Err(HttpError::InvalidContentLength)
        );
    }

    #[test]
    fn header_content_length_case_insensitive() {
        // RFC 7230: header field names are case-insensitive.
        let mut state = HeaderState::new();
        state.feed(b"content-length: 1024").unwrap();
        let resp = state.finish(u64::MAX).unwrap();
        assert_eq!(resp.content_length, 1024);
    }

    #[test]
    fn header_content_length_mixed_case() {
        let mut state = HeaderState::new();
        state.feed(b"CONTENT-LENGTH: 512").unwrap();
        let resp = state.finish(u64::MAX).unwrap();
        assert_eq!(resp.content_length, 512);
    }

    #[test]
    fn header_transfer_encoding_chunked() {
        // Transfer-Encoding: chunked must cause rejection even when CL is present.
        let mut state = HeaderState::new();
        state.feed(b"Transfer-Encoding: chunked").unwrap();
        state.feed(b"Content-Length: 1024").unwrap();
        assert_eq!(
            state.finish(u64::MAX),
            Err(HttpError::TransferEncodingPresent)
        );
    }

    #[test]
    fn header_transfer_encoding_identity() {
        // Any Transfer-Encoding header is rejected — identity included.
        let mut state = HeaderState::new();
        state.feed(b"Transfer-Encoding: identity").unwrap();
        state.feed(b"Content-Length: 100").unwrap();
        assert_eq!(
            state.finish(u64::MAX),
            Err(HttpError::TransferEncodingPresent)
        );
    }

    #[test]
    fn header_content_length_rejects_leading_plus() {
        // RFC 7230 §3.3.2 requires Content-Length to be `1*DIGIT` — no sign.
        // `u64::from_str` would otherwise accept `+1024`, opening a request-
        // smuggling primitive when paired with a strict upstream proxy.
        let mut state = HeaderState::new();
        assert_eq!(
            state.feed(b"Content-Length: +1024"),
            Err(HttpError::InvalidContentLength)
        );
    }

    #[test]
    fn header_content_length_rejects_leading_minus() {
        let mut state = HeaderState::new();
        assert_eq!(
            state.feed(b"Content-Length: -1"),
            Err(HttpError::InvalidContentLength)
        );
    }

    #[test]
    fn header_content_length_rejects_empty_value() {
        let mut state = HeaderState::new();
        assert_eq!(
            state.feed(b"Content-Length: "),
            Err(HttpError::InvalidContentLength)
        );
    }

    #[test]
    fn header_rejects_whitespace_before_colon() {
        // RFC 7230 §3.2.4 forbids whitespace between header name and colon.
        // Accepting it would silently disable header recognition (the name
        // `Content-Length\t` does not match `content-length`), enabling a
        // duplicate-CL bypass and Transfer-Encoding smuggling.
        let mut state = HeaderState::new();
        assert_eq!(
            state.feed(b"Content-Length\t: 1024"),
            Err(HttpError::BadStatusLine)
        );

        let mut state = HeaderState::new();
        assert_eq!(
            state.feed(b"Transfer-Encoding : chunked"),
            Err(HttpError::BadStatusLine)
        );
    }

    #[test]
    fn oversized_body_rejected() {
        // max_bytes = 100 but Content-Length = 200 → InsufficientSpace via BodyTooLarge.
        let mut state = HeaderState::new();
        state.feed(b"Content-Length: 200").unwrap();
        assert_eq!(state.finish(100), Err(HttpError::BodyTooLarge));
        // Verify the mapping to OtaError.
        let ota_err: OtaError = HttpError::BodyTooLarge.into();
        assert_eq!(ota_err, OtaError::InsufficientSpace);
    }

    // ── URL parsing ──────────────────────────────────────────────────────────

    #[test]
    fn url_basic_parse() {
        let p = parse_url("http://192.168.1.1/firmware.bin").unwrap();
        assert_eq!(p.host, "192.168.1.1");
        assert_eq!(p.port, 80);
        assert_eq!(p.path, "/firmware.bin");
    }

    #[test]
    fn url_with_port() {
        let p = parse_url("http://192.168.1.1:8080/firmware.bin").unwrap();
        assert_eq!(p.host, "192.168.1.1");
        assert_eq!(p.port, 8080);
        assert_eq!(p.path, "/firmware.bin");
    }

    #[test]
    fn url_https_rejected() {
        assert!(parse_url("https://192.168.1.1/firmware.bin").is_err());
    }

    #[test]
    fn url_no_path_defaults_to_slash() {
        let p = parse_url("http://192.168.1.1").unwrap();
        assert_eq!(p.path, "/");
    }

    // ── GET request formatting (Host header port handling) ───────────────────

    #[test]
    fn host_header_omits_default_port() {
        // RFC 7230 §5.4: when the authority's port is the default (80 for HTTP),
        // the Host field-value MAY omit it. We omit it for compactness.
        let mut buf = [0u8; 256];
        let n = format_get_request(&mut buf, "192.168.1.1", 80, "/fw.bin").unwrap();
        let req = core::str::from_utf8(&buf[..n]).unwrap();
        assert_eq!(
            req,
            "GET /fw.bin HTTP/1.1\r\nHost: 192.168.1.1\r\nConnection: close\r\n\r\n"
        );
    }

    #[test]
    fn host_header_includes_non_default_port() {
        // Servers that route by Host (vhost / reverse proxy) require the port
        // when it is not the HTTP/1.1 default. Omitting it on :8080 caused the
        // P2 review finding.
        let mut buf = [0u8; 256];
        let n = format_get_request(&mut buf, "192.168.1.1", 8080, "/fw.bin").unwrap();
        let req = core::str::from_utf8(&buf[..n]).unwrap();
        assert_eq!(
            req,
            "GET /fw.bin HTTP/1.1\r\nHost: 192.168.1.1:8080\r\nConnection: close\r\n\r\n"
        );
    }

    #[test]
    fn host_header_includes_low_non_default_port() {
        // Boundary: any port other than 80 is "non-default" — including 81.
        let mut buf = [0u8; 256];
        let n = format_get_request(&mut buf, "host", 81, "/").unwrap();
        let req = core::str::from_utf8(&buf[..n]).unwrap();
        assert!(req.contains("Host: host:81\r\n"));
    }

    #[test]
    fn host_header_includes_max_port() {
        // u16::MAX boundary: 5 ASCII digits must fit.
        let mut buf = [0u8; 256];
        let n = format_get_request(&mut buf, "h", 65535, "/").unwrap();
        let req = core::str::from_utf8(&buf[..n]).unwrap();
        assert!(req.contains("Host: h:65535\r\n"));
    }
}
