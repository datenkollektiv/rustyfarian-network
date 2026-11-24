//! HTTP/1.1 client parts of the DDI client — internal transport for `rustyfarian-esp-hal-network`.
//!
//! This HTTP client is an implementation detail of `rustyfarian-esp-hal-network`
//! and not part of the crate's public API.
//! It may be removed if a workspace HTTP dependency arrives later.
//!
//! The URL parser and the status-line and header-line parsers are here; the DDI exchange built on them is [`ddi`].
//! An artifact download hands the declared `Content-Length` and any
//! `Transfer-Encoding` on as raw facts ([`ResponseHeaders`]); the admission rule (missing, chunked,
//! zero, too large) is `juggler::ota::check_content_length`, applied by
//! `juggler::ota::download::DownloadMachine` before any flash partition is touched.

// When building without the embassy + chip features, the parsing types and
// functions defined here are only used in the `#[cfg(test)]` blocks below.
// Suppress the resulting dead_code warnings so `cargo clippy -D warnings`
// does not fail on stub/host builds.
#![cfg_attr(
    not(all(
        feature = "embassy",
        any(
            feature = "esp32c3",
            feature = "esp32c6",
            feature = "esp32",
            feature = "esp32s3"
        )
    )),
    allow(dead_code)
)]

use juggler::ota::OtaError;

// ── Internal error type ─────────────────────────────────────────────────────

/// Internal HTTP parse/protocol errors mapped to [`OtaError`] at the crate boundary.
#[derive(Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum HttpError {
    /// Status line did not start with `HTTP/1.1`.
    BadStatusLine,
    /// Status code was present and valid but not 200.
    NonSuccess { status: u16 },
    /// URL could not be parsed as `http://<host>/<path>`.
    BadUrl,
}

impl From<HttpError> for OtaError {
    fn from(e: HttpError) -> OtaError {
        // The public `OtaError` surface collapses several internal variants
        // (BadStatusLine, BadUrl) into
        // `ServerUnreachable`. Log the original variant before collapsing so
        // root cause survives in `espflash monitor` output.
        log::warn!("OTA HTTP error: {:?}", e);
        match e {
            // Server-side HTTP errors that carry a status code bubble up so
            // callers can log the exact code.
            HttpError::NonSuccess { status } => OtaError::DownloadFailed { status },

            // Structural parse failures: the TCP endpoint answered but did not
            // speak recognisable HTTP/1.1 — treat as server unreachable.
            HttpError::BadStatusLine | HttpError::BadUrl => OtaError::ServerUnreachable,
        }
    }
}

// ── Parsed HTTP response descriptor ─────────────────────────────────────────

/// The header facts the download admission needs (see
/// `juggler::ota::check_content_length`).
///
/// Internal — not part of the crate's public API.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ResponseHeaders {
    /// Byte count declared in `Content-Length`, `None` when absent.
    pub content_length: Option<u64>,
    /// A `Transfer-Encoding` header was present (any value).
    pub chunked: bool,
}

// ── URL parser ───────────────────────────────────────────────────────────────

/// Parsed `http://host[:port]/path` URL.
///
/// The host is forwarded verbatim in the `Host` header; resolving it is the caller's concern
/// (the DDI client looks a server name up through the station stack's DNS).
pub(crate) struct ParsedUrl<'a> {
    pub host: &'a str,
    /// TCP port — used both to connect the socket and to populate the `Host`
    /// request header when not the HTTP/1.1 default of 80 (per RFC 7230 §5.4,
    /// the `Host` field-value must mirror the authority component).
    pub port: u16,
    pub path: &'a str,
    /// Query including the leading `?`, or empty.
    pub query: &'a str,
}

/// Parse `http://host[:port][/path][?query][#fragment]`.
///
/// `https://` is rejected — plain HTTP only (ADR 011 §2).
/// The scheme is matched case-insensitively.
/// Userinfo (everything up to the last `@` in the authority) is dropped: this client does no authentication and never logs it.
/// The authority ends at the first `/`, `?` or `#`; the fragment is never sent.
/// The path defaults to `/` when absent; a path-less URL with a query yields path `/` plus that query.
/// Any ASCII whitespace or control character is rejected so a URL can never inject request headers.
pub(crate) fn parse_url(url: &str) -> Result<ParsedUrl<'_>, HttpError> {
    const SCHEME: &str = "http://";
    let prefix = url.get(..SCHEME.len()).ok_or(HttpError::BadUrl)?;
    if !prefix.eq_ignore_ascii_case(SCHEME) {
        return Err(HttpError::BadUrl);
    }
    let rest = &url[SCHEME.len()..];
    if rest
        .bytes()
        .any(|b| b.is_ascii_whitespace() || b.is_ascii_control())
    {
        return Err(HttpError::BadUrl);
    }

    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let (authority, tail) = rest.split_at(auth_end);
    let tail = tail.split('#').next().unwrap_or("");
    let (path, query) = match tail.find('?') {
        Some(q) => (&tail[..q], &tail[q..]),
        None => (tail, ""),
    };
    let path = if path.is_empty() { "/" } else { path };

    let host_port = authority.rsplit('@').next().unwrap_or("");
    let (host, port_str) = if host_port.starts_with('[') {
        let close = host_port.find(']').ok_or(HttpError::BadUrl)?;
        let after = &host_port[close + 1..];
        let port = match after.strip_prefix(':') {
            Some(p) => Some(p),
            None if after.is_empty() => None,
            None => return Err(HttpError::BadUrl),
        };
        (&host_port[..=close], port)
    } else {
        match host_port.split_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (host_port, None),
        }
    };
    let port: u16 = match port_str {
        Some(p) => {
            if !p.bytes().all(|b| b.is_ascii_digit()) {
                return Err(HttpError::BadUrl);
            }
            p.parse().map_err(|_| HttpError::BadUrl)?
        }
        None => 80,
    };

    if host.is_empty() || host == "[]" {
        return Err(HttpError::BadUrl);
    }

    Ok(ParsedUrl {
        host,
        port,
        path,
        query,
    })
}

// ── Pure parsing functions (always compile, no I/O) ─────────────────────────

/// Parse an HTTP/1.x status line and return the three-digit status code.
///
/// Accepts:
/// - `HTTP/1.1 NNN reason\r\n`
/// - `HTTP/1.1 NNN\r\n`  (reason phrase is optional per RFC 7230 §3.1.2)
/// - the same with `HTTP/1.0`: an HTTP/1.1 client processes a 1.0 response
///   (RFC 7230 §2.6), and Python's `http.server` answers with `HTTP/1.0`; a 1.0
///   artifact response without `Content-Length` still fails `check_content_length`.
///
/// Rejects anything that does not start with exactly `HTTP/1.1 ` or `HTTP/1.0 `.
pub(crate) fn parse_status_line(line: &[u8]) -> Result<u16, HttpError> {
    // Minimum: "HTTP/1.1 NNN\r\n" = 14 bytes
    if !line.starts_with(b"HTTP/1.1 ") && !line.starts_with(b"HTTP/1.0 ") {
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

// ── Async exchange (embassy + `ota-ddi`) ─────────────────────────────────────

#[cfg(all(feature = "embassy", feature = "ota-ddi"))]
pub(crate) mod async_client {
    use embassy_net::tcp::TcpSocket;

    /// Sends `parts` (a DDI request head, then its body) and reads the response head ([`super::ddi::HeadParser`]).
    ///
    /// The socket's read cursor ends right after the head, so the caller reads the body next.
    /// The caller bounds the whole exchange with one timeout.
    pub(crate) async fn send_and_read_head(
        socket: &mut TcpSocket<'_>,
        parts: &[&[u8]],
    ) -> Result<super::ddi::ResponseHead, super::ddi::HeadError> {
        use super::ddi::{HeadError, HeadParser};

        for part in parts {
            let mut rest = *part;
            while !rest.is_empty() {
                match socket.write(rest).await {
                    Ok(0) | Err(_) => return Err(HeadError::Io),
                    Ok(n) => rest = &rest[n..],
                }
            }
        }
        let mut parser = HeadParser::new();
        loop {
            match socket.read_with(|buf| parser.feed(buf)).await {
                Err(_) => return Err(HeadError::Io),
                Ok(Ok(Some(head))) => return Ok(head),
                Ok(Ok(None)) => {}
                Ok(Err(e)) => return Err(e),
            }
        }
    }
}

// ── DDI extension (feature `ota-ddi`) ───────────────────────────────────────

/// The DDI transport's HTTP/1.1 exchange, sans-IO: the request head writer, the response head parser and the body decoder.
///
/// A DDI request may be a `GET`, `POST` or `PUT` with an `Authorization` header, and the transport must see any status, the `Retry-After` value and a JSON body that may come chunked (ADR 027).
/// Redirects are never followed: the transport reports a 3xx other than 304 as its own error ([`juggler::ota::ddi::is_redirect`]).
#[cfg(any(feature = "ota-ddi", test))]
pub(crate) mod ddi {
    use alloc::vec::Vec;

    use juggler::ota::ddi::wire::MAX_RESPONSE_BYTES;
    use juggler::ota::ddi::{is_redirect, BodyFailure, ResponseBody, RetryAfter};
    use juggler::ota::OtaError;

    use super::{
        eq_ignore_case, parse_header, parse_status_line, trim_ows, ParsedUrl, ResponseHeaders,
    };

    /// `Accept` of a DDI JSON request.
    pub(crate) const ACCEPT_JSON: &str = "application/json";
    /// `Accept` of a DDI artifact download.
    pub(crate) const ACCEPT_FIRMWARE: &str = "application/octet-stream";

    /// Bytes of one response head line kept for parsing; the rest of a longer line is counted, not kept.
    pub(crate) const LINE_CAP: usize = 512;
    /// Largest response head (status line, headers and the blank line, informational heads included) the parser reads.
    pub(crate) const HEAD_CAP: usize = 8 * 1024;
    /// Largest chunk-size line (size and extensions) of a chunked body.
    const SIZE_LINE_CAP: usize = 256;
    /// Largest trailer section of a chunked body.
    const TRAILER_CAP: usize = 1024;

    /// The method of a DDI request.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Method {
        /// The poll, a `deploymentBase` or `cancelAction` link, the artifact download.
        Get,
        /// A deployment or cancel feedback.
        Post,
        /// The configData body.
        Put,
    }

    impl Method {
        const fn as_str(self) -> &'static str {
            match self {
                Method::Get => "GET",
                Method::Post => "POST",
                Method::Put => "PUT",
            }
        }
    }

    /// Writes `n` in decimal into `buf` and returns the digits.
    fn decimal(buf: &mut [u8; 20], mut n: u64) -> &[u8] {
        let mut at = buf.len();
        loop {
            at -= 1;
            buf[at] = b'0' + (n % 10) as u8;
            n /= 10;
            if n == 0 {
                break;
            }
        }
        &buf[at..]
    }

    /// The request head of a DDI request, in a heap buffer of exactly its size.
    ///
    /// `POST` and `PUT` always carry `Content-Type: application/json` and a `Content-Length` of `body_len` (0 included); a `GET` carries neither.
    /// `Host` names the port unless it is 80 (RFC 7230 §5.4: the field mirrors the URL authority, and a server that routes by `Host` needs a non-default port).
    /// Every request asks the server to close the connection after the response.
    /// The head holds the `Authorization` value, the token included; it is freed, not zeroed, when the caller drops it, like the copies the ESP-IDF tier's HTTP client makes.
    pub(crate) fn request_head(
        method: Method,
        url: &ParsedUrl<'_>,
        authorization: &str,
        accept: &str,
        body_len: usize,
    ) -> Vec<u8> {
        let mut port_buf = [0u8; 20];
        let port: &[u8] = if url.port == 80 {
            &[]
        } else {
            decimal(&mut port_buf, u64::from(url.port))
        };
        let mut length_buf = [0u8; 20];
        let length = decimal(&mut length_buf, body_len as u64);
        let with_body = method != Method::Get;
        let parts: [&[u8]; 19] = [
            method.as_str().as_bytes(),
            b" ",
            url.path.as_bytes(),
            url.query.as_bytes(),
            b" HTTP/1.1\r\nHost: ",
            url.host.as_bytes(),
            if port.is_empty() { b"" } else { b":" },
            port,
            b"\r\nAuthorization: ",
            authorization.as_bytes(),
            b"\r\nAccept: ",
            accept.as_bytes(),
            b"\r\n",
            if with_body {
                b"Content-Type: application/json\r\nContent-Length: "
            } else {
                b""
            },
            if with_body { length } else { b"" },
            if with_body { b"\r\n" } else { b"" },
            b"Connection: close\r\n",
            b"\r\n",
            b"",
        ];
        let len = parts.iter().map(|p| p.len()).sum();
        let mut head = Vec::with_capacity(len);
        for part in parts {
            head.extend_from_slice(part);
        }
        head
    }

    /// Why a response head was refused; the transport maps both to "server unreachable".
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum HeadError {
        /// Not an HTTP/1.x status line, a header line without a name, an obsolete line folding, a bad or repeated `Content-Length` or `Transfer-Encoding`, a coding other than `chunked`, or both headers at once.
        Malformed,
        /// The head exceeds [`HEAD_CAP`].
        TooLarge,
        /// The connection failed, or closed before the head ended.
        Io,
    }

    /// How the body of a response is delimited.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Framing {
        /// No body: a 204 or 304.
        Empty,
        /// `Content-Length` bytes.
        Length(u64),
        /// `Transfer-Encoding: chunked`.
        Chunked,
        /// Until the server closes the connection.
        Close,
    }

    /// A parsed response head: the status, the raw `Retry-After` and the body framing.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct ResponseHead {
        /// The final (non-informational) status.
        pub(crate) status: u16,
        /// The first `Retry-After` value, when within [`RetryAfter::capture`]'s bounds.
        pub(crate) retry_after: Option<RetryAfter>,
        /// How the body is delimited.
        pub(crate) framing: Framing,
    }

    /// Reads a response head line by line, from reads split anywhere.
    ///
    /// Lines end at LF; a CR before it is dropped.
    /// The first [`LINE_CAP`] bytes of each line are kept: a longer header line is ignored unless it names `Content-Length` or `Transfer-Encoding`, which makes the head malformed, so a header block of any shape up to [`HEAD_CAP`] is read.
    /// A 1xx head is skipped and the next head read.
    pub(crate) struct HeadParser {
        line: [u8; LINE_CAP],
        len: usize,
        overflow: bool,
        total: usize,
        status: Option<u16>,
        retry_after: Option<RetryAfter>,
        retry_seen: bool,
        content_length: Option<u64>,
        chunked: Option<bool>,
    }

    impl HeadParser {
        pub(crate) const fn new() -> Self {
            Self {
                line: [0; LINE_CAP],
                len: 0,
                overflow: false,
                total: 0,
                status: None,
                retry_after: None,
                retry_seen: false,
                content_length: None,
                chunked: None,
            }
        }

        /// Feeds the bytes of one read: how many it consumed, and the head once the blank line after it arrived.
        ///
        /// It consumes nothing past the head, so the rest of `bytes` is the start of the body.
        pub(crate) fn feed(
            &mut self,
            bytes: &[u8],
        ) -> (usize, Result<Option<ResponseHead>, HeadError>) {
            for (i, &b) in bytes.iter().enumerate() {
                self.total += 1;
                if self.total > HEAD_CAP {
                    return (i + 1, Err(HeadError::TooLarge));
                }
                if b != b'\n' {
                    if self.len < LINE_CAP {
                        self.line[self.len] = b;
                        self.len += 1;
                    } else {
                        self.overflow = true;
                    }
                    continue;
                }
                match self.end_line() {
                    Ok(None) => {}
                    Ok(Some(head)) => return (i + 1, Ok(Some(head))),
                    Err(e) => return (i + 1, Err(e)),
                }
            }
            (bytes.len(), Ok(None))
        }

        /// Handles one complete line.
        fn end_line(&mut self) -> Result<Option<ResponseHead>, HeadError> {
            let mut len = self.len;
            if !self.overflow && len > 0 && self.line[len - 1] == b'\r' {
                len -= 1;
            }
            let overflow = self.overflow;
            self.len = 0;
            self.overflow = false;
            let line = &self.line[..len];

            if self.status.is_none() {
                // RFC 9112 §2.2: an empty line before the status line is ignored.
                if line.is_empty() {
                    return Ok(None);
                }
                // The status code sits in the kept prefix even when the reason phrase overflowed.
                let status = parse_status_line(line).map_err(|_| HeadError::Malformed)?;
                self.status = Some(status);
                return Ok(None);
            }
            if line.is_empty() {
                return self.end_head();
            }
            if matches!(line[0], b' ' | b'\t') {
                return Err(HeadError::Malformed);
            }
            let (name, value) = parse_header(line).map_err(|_| HeadError::Malformed)?;
            let framing_header = eq_ignore_case(name, b"content-length")
                || eq_ignore_case(name, b"transfer-encoding");
            if overflow {
                return if framing_header {
                    Err(HeadError::Malformed)
                } else {
                    Ok(None)
                };
            }
            if eq_ignore_case(name, b"content-length") {
                if self.content_length.is_some()
                    || value.is_empty()
                    || !value.iter().all(u8::is_ascii_digit)
                {
                    return Err(HeadError::Malformed);
                }
                let text = core::str::from_utf8(value).map_err(|_| HeadError::Malformed)?;
                self.content_length = Some(text.parse().map_err(|_| HeadError::Malformed)?);
            } else if eq_ignore_case(name, b"transfer-encoding") {
                if self.chunked.is_some() || !eq_ignore_case(trim_ows(value), b"chunked") {
                    return Err(HeadError::Malformed);
                }
                self.chunked = Some(true);
            } else if eq_ignore_case(name, b"retry-after") && !self.retry_seen {
                self.retry_seen = true;
                self.retry_after = core::str::from_utf8(value)
                    .ok()
                    .and_then(RetryAfter::capture);
            }
            Ok(None)
        }

        /// The blank line after the headers.
        fn end_head(&mut self) -> Result<Option<ResponseHead>, HeadError> {
            let status = self.status.unwrap_or(0);
            if (100..200).contains(&status) {
                let total = self.total;
                *self = Self::new();
                self.total = total;
                return Ok(None);
            }
            let chunked = self.chunked == Some(true);
            if chunked && self.content_length.is_some() {
                return Err(HeadError::Malformed);
            }
            let framing = if status == 204 || status == 304 {
                Framing::Empty
            } else if chunked {
                Framing::Chunked
            } else if let Some(len) = self.content_length {
                Framing::Length(len)
            } else {
                Framing::Close
            };
            Ok(Some(ResponseHead {
                status,
                retry_after: self.retry_after.take(),
                framing,
            }))
        }
    }

    /// The chunked-body decoder's position.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Chunk {
        /// In a chunk-size line: the size so far, its digit count, whether the digits ended, the line length.
        Size {
            size: u64,
            digits: u8,
            ended: bool,
            line: usize,
        },
        /// In chunk data, this many bytes left.
        Data(u64),
        /// After chunk data: the CRLF, `cr` once the CR arrived.
        DataEnd { cr: bool },
        /// In the trailer section: the bytes of the current line and of the whole section.
        Trailer { line: usize, total: usize },
    }

    impl Chunk {
        const START: Chunk = Chunk::Size {
            size: 0,
            digits: 0,
            ended: false,
            line: 0,
        };
    }

    #[derive(Debug)]
    enum Mode {
        Length(u64),
        Close,
        Chunked(Chunk),
    }

    /// Collects a response body of any [`Framing`] into a [`ResponseBody`], at most [`MAX_RESPONSE_BYTES`] bytes.
    ///
    /// A declared length above the cap fails before anything is read; a chunked body fails as soon as a chunk would pass it.
    /// Bytes after the end of the body are ignored (the server closes the connection anyway).
    #[derive(Debug)]
    pub(crate) struct BodyReader {
        body: ResponseBody,
        mode: Mode,
        done: bool,
        len: usize,
    }

    impl BodyReader {
        /// A reader for the body `framing` announces.
        pub(crate) fn new(framing: Framing) -> Result<Self, BodyFailure> {
            let (declared, mode, done) = match framing {
                Framing::Empty => (Some(0), Mode::Length(0), true),
                Framing::Length(len) => (Some(len), Mode::Length(len), len == 0),
                Framing::Chunked => (None, Mode::Chunked(Chunk::START), false),
                Framing::Close => (None, Mode::Close, false),
            };
            Ok(Self {
                body: ResponseBody::new(declared)?,
                mode,
                done,
                len: 0,
            })
        }

        /// Whether the whole body arrived (never for a body delimited by the close).
        pub(crate) fn is_done(&self) -> bool {
            self.done
        }

        fn push(&mut self, bytes: &[u8]) -> Result<(), BodyFailure> {
            self.body.push(bytes)?;
            self.len += bytes.len();
            Ok(())
        }

        /// Feeds the bytes of one read; `Ok(true)` once the body is complete.
        pub(crate) fn feed(&mut self, mut bytes: &[u8]) -> Result<bool, BodyFailure> {
            while !self.done && !bytes.is_empty() {
                match self.mode {
                    Mode::Close => {
                        self.push(bytes)?;
                        bytes = &[];
                    }
                    Mode::Length(left) => {
                        let take = (left.min(bytes.len() as u64)) as usize;
                        self.push(&bytes[..take])?;
                        bytes = &bytes[take..];
                        let left = left - take as u64;
                        self.mode = Mode::Length(left);
                        self.done = left == 0;
                    }
                    Mode::Chunked(state) => {
                        let (used, next) = self.chunked(state, bytes)?;
                        bytes = &bytes[used..];
                        self.mode = Mode::Chunked(next);
                    }
                }
            }
            Ok(self.done)
        }

        /// One step of the chunked decoder: the bytes it used and the next state.
        fn chunked(&mut self, state: Chunk, bytes: &[u8]) -> Result<(usize, Chunk), BodyFailure> {
            const MALFORMED: BodyFailure = BodyFailure::ServerUnreachable;
            let b = bytes[0];
            let next = match state {
                Chunk::Size {
                    size,
                    digits,
                    ended,
                    line,
                } => {
                    if line + 1 > SIZE_LINE_CAP {
                        return Err(MALFORMED);
                    }
                    if b == b'\n' {
                        if digits == 0 {
                            return Err(MALFORMED);
                        }
                        if size == 0 {
                            Chunk::Trailer { line: 0, total: 0 }
                        } else if self.len as u64 + size > MAX_RESPONSE_BYTES as u64 {
                            return Err(BodyFailure::TooLarge);
                        } else {
                            Chunk::Data(size)
                        }
                    } else if let (false, Some(v)) = (ended, (b as char).to_digit(16)) {
                        if digits >= 15 {
                            return Err(MALFORMED);
                        }
                        Chunk::Size {
                            size: size * 16 + u64::from(v),
                            digits: digits + 1,
                            ended,
                            line: line + 1,
                        }
                    } else if digits > 0 && (b == b'\t' || b == b'\r' || !b.is_ascii_control()) {
                        // Whitespace, a chunk extension (`;name=value`) or the CR: skipped.
                        Chunk::Size {
                            size,
                            digits,
                            ended: true,
                            line: line + 1,
                        }
                    } else {
                        return Err(MALFORMED);
                    }
                }
                Chunk::Data(left) => {
                    let take = (left.min(bytes.len() as u64)) as usize;
                    self.push(&bytes[..take])?;
                    let left = left - take as u64;
                    return Ok((
                        take,
                        if left == 0 {
                            Chunk::DataEnd { cr: false }
                        } else {
                            Chunk::Data(left)
                        },
                    ));
                }
                Chunk::DataEnd { cr } => match b {
                    b'\r' if !cr => Chunk::DataEnd { cr: true },
                    b'\n' => Chunk::START,
                    _ => return Err(MALFORMED),
                },
                Chunk::Trailer { line, total } => {
                    if total + 1 > TRAILER_CAP {
                        return Err(MALFORMED);
                    }
                    if b == b'\n' {
                        if line == 0 {
                            self.done = true;
                        }
                        Chunk::Trailer {
                            line: 0,
                            total: total + 1,
                        }
                    } else if b == b'\r' {
                        Chunk::Trailer {
                            line,
                            total: total + 1,
                        }
                    } else {
                        Chunk::Trailer {
                            line: line + 1,
                            total: total + 1,
                        }
                    }
                }
            };
            Ok((1, next))
        }

        /// The whole body, at the end of the stream (`eof`) or once [`feed`](Self::feed) reported it complete.
        ///
        /// A stream that ended early is [`BodyFailure::LengthMismatch`] against a `Content-Length` and [`BodyFailure::ServerUnreachable`] inside a chunked body.
        pub(crate) fn finish(self) -> Result<Vec<u8>, BodyFailure> {
            if matches!(self.mode, Mode::Chunked(_)) && !self.done {
                return Err(BodyFailure::ServerUnreachable);
            }
            Ok(self.body.finish()?)
        }
    }

    /// The header facts of an artifact download's head, for the download machine.
    ///
    /// A redirect is [`OtaError::Redirect`]; any other status but 200 is `DownloadFailed { status }` and hands its `Retry-After` back in `retry_after` for the retry policy (left unchanged on a 200 or a redirect).
    /// A chunked body is reported as such, and the machine refuses it like a missing `Content-Length`.
    pub(crate) fn download_headers(
        head: ResponseHead,
        retry_after: &mut Option<RetryAfter>,
    ) -> Result<ResponseHeaders, OtaError> {
        if is_redirect(head.status) {
            log::error!("DDI download answered {}: redirect refused", head.status);
            return Err(OtaError::Redirect {
                status: head.status,
            });
        }
        if head.status != 200 {
            log::error!("DDI download answered {}", head.status);
            *retry_after = head.retry_after;
            return Err(OtaError::DownloadFailed {
                status: head.status,
            });
        }
        Ok(ResponseHeaders {
            content_length: match head.framing {
                Framing::Length(len) => Some(len),
                _ => None,
            },
            chunked: head.framing == Framing::Chunked,
        })
    }

    #[cfg(test)]
    mod tests;
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
    fn status_line_http10_is_accepted() {
        // Python's `http.server` answers `HTTP/1.0`.
        assert_eq!(parse_status_line(b"HTTP/1.0 200 OK"), Ok(200));
        assert_eq!(parse_status_line(b"HTTP/1.0 404 Not Found\r\n"), Ok(404));
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

    #[test]
    fn url_scheme_is_case_insensitive() {
        let p = parse_url("HTTP://h/fw.bin").unwrap();
        assert_eq!((p.host, p.port, p.path), ("h", 80, "/fw.bin"));
    }

    #[test]
    fn url_userinfo_is_stripped() {
        let p = parse_url("http://u:pw@h/x").unwrap();
        assert_eq!((p.host, p.port, p.path), ("h", 80, "/x"));
        let p = parse_url("http://u:pw@h:81/x").unwrap();
        assert_eq!((p.host, p.port), ("h", 81));
    }

    #[test]
    fn url_query_without_path_gets_root_path() {
        let p = parse_url("http://h?x=1").unwrap();
        assert_eq!((p.host, p.path, p.query), ("h", "/", "?x=1"));
        let p = parse_url("http://h/a?x=1#frag").unwrap();
        assert_eq!((p.path, p.query), ("/a", "?x=1"));
        let p = parse_url("http://h#frag").unwrap();
        assert_eq!((p.host, p.path, p.query), ("h", "/", ""));
    }

    #[test]
    fn url_with_control_or_whitespace_is_rejected() {
        for url in [
            "http://h/x\r\nX-Evil: 1",
            "http://h/x\n",
            "http://h/x\r",
            "http://h/x y",
            "http://h\0/x",
            "http://   /x",
        ] {
            assert!(parse_url(url).is_err(), "{url:?}");
        }
    }

    #[test]
    fn url_bad_port_or_host_is_rejected() {
        for url in [
            "http://h:abc/x",
            "http://h:99999/x",
            "http://h:/x",
            "http://:80/x",
            "http://[",
            "http://[]",
            "http://[::1",
            "http://",
        ] {
            assert!(parse_url(url).is_err(), "{url:?}");
        }
        let p = parse_url("http://[::1]:8080/x").unwrap();
        assert_eq!((p.host, p.port), ("[::1]", 8080));
    }
}
