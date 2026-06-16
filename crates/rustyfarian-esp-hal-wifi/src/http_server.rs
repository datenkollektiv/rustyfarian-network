//! Minimal HTTP/1.1 server — Phase 2 spike code (ADR 015 §3 fallback).
//!
//! **Stability notice:** this module is internal spike code gated behind the
//! `provisioning-spike` Cargo feature.  It is expected to migrate to
//! `rustyfarian-esp-hal-provisioning::http` when Phase 2 proper begins.
//! Do not depend on the API stability of this module or the
//! `provisioning-spike` feature flag across releases.
//!
//! ## Why it exists
//!
//! The `edge-net` family (`edge-http`, `edge-dhcp`, `edge-captive`) was the
//! preferred Phase 2 substrate (ADR 015 §3), but its `embassy-sync 0.7`
//! dependency conflicts with the workspace-pinned `embassy-sync 0.8`.
//! ADR 015 §3 explicitly permits the hand-rolled fallback to proceed without a
//! new ADR — the architectural commitment is the private-substrate boundary,
//! not the crate family.
//!
//! ## What it covers
//!
//! A minimal RFC 7230 HTTP/1.1 server handling a single connection at a time,
//! suitable for a SoftAP captive-portal scenario where only one phone
//! configures one device.  Out of scope: keep-alive, pipelining, chunked
//! transfer, TLS, concurrent connections.
//!
//! ## Protocol coverage
//!
//! Request parsing (subset of RFC 7230 §3):
//! - Request line: `METHOD SP TARGET SP HTTP/1.1 CRLF`
//! - Headers: `Name: value CRLF` repeated until blank `CRLF`
//! - Methods understood: `GET`, `POST`
//! - Headers read: `Host`, `Content-Length`, `Content-Type`
//! - Unknown headers: skipped (RFC 7230 §3.2.1)
//! - Security: duplicate `Content-Length` rejected (request-smuggling defence,
//!   RFC 7230 §3.3.2), whitespace before colon rejected (RFC 7230 §3.2.4)
//!
//! Response generation:
//! - Always `Connection: close` — no keep-alive support
//! - `Content-Length` always explicit — no chunked transfer
//!
//! ## Routes
//!
//! - `GET <any path>` → 200 OK with the SoftAP spike HTML page
//! - Non-`GET` method (e.g. `POST`) → **405 Method Not Allowed** (plain text)
//!
//! Returning the portal page for **every** GET is the key captive-portal
//! trigger: the phone OS probes well-known sentinel URLs (`/generate_204`,
//! `/hotspot-detect.html`, `/ncsi.txt`, etc.) and compares the response body
//! to an expected value.  When the body does not match — our HTML portal page
//! — the OS concludes it is behind a captive portal and pops the browser
//! automatically.  Non-`GET` requests are rejected at the router (rather than
//! parsed half-way) because the spike's [`parse_request`] does not extract
//! request bodies — see the [`ParsedRequest`] docs for the rationale.  A real
//! form-submission route lands in Phase 2 proper.

// When building without the embassy + chip features the async `run` function
// and its TcpSocket usage are compiled away.  Allow dead-code on the types
// that remain so clippy -D warnings does not fail on stub/host builds.
#![cfg_attr(
    not(all(feature = "embassy", any(feature = "esp32c3", feature = "esp32c6"))),
    allow(dead_code)
)]

// ── HTTP wire constants ───────────────────────────────────────────────────────

/// Maximum length of the request-target (path) in bytes.
const MAX_TARGET_LEN: usize = 256;
/// Maximum total request size in bytes (request line + headers + body).
const DEFAULT_REQUEST_SIZE_CAP: usize = 2048;
/// Default port the HTTP server listens on.
const DEFAULT_PORT: u16 = 80;
/// Default socket receive buffer size.
const DEFAULT_RX_BUF: usize = 1024;
/// Default socket transmit buffer size.
const DEFAULT_TX_BUF: usize = 2048;

// ── SoftAP spike HTML page ────────────────────────────────────────────────────

/// HTML body served at `GET /`.
///
/// A `const &str` so the content compiles into flash and incurs zero heap.
const INDEX_HTML: &str = "\
<!DOCTYPE html>\
<html>\
<head><title>Rustyfarian SoftAP</title></head>\
<body>\
<h1>Rustyfarian SoftAP spike</h1>\
<p>Hand-rolled HTTP server running on bare-metal (ADR 015 \u{a7} 3 fallback).</p>\
<p>AP IP: 192.168.4.1</p>\
</body>\
</html>";

/// Plain-text body for 405 responses to non-`GET` methods.
const METHOD_NOT_ALLOWED_BODY: &str = "Method Not Allowed";

// ── HTTP method ───────────────────────────────────────────────────────────────

/// HTTP request method — the small subset the server handles.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Method {
    /// `GET` — used for the captive portal page.
    Get,
    /// `POST` — forward-compatible placeholder for `/save`. The spike's
    /// router returns 405 for every POST today; the parser already accepts
    /// POST bodies so Phase 2's `/save` route has somewhere to dispatch.
    Post,
}

// ── Parse error ───────────────────────────────────────────────────────────────

/// Errors produced by the HTTP request parser.
///
/// Each variant maps to an HTTP status code returned to the client.
#[derive(Debug, PartialEq, Eq)]
pub enum ParseError {
    /// The request line is missing, empty, or structurally invalid.
    ///
    /// Causes a 400-equivalent close (we return 400 only; the spec does not
    /// require a body, but being descriptive costs nothing here).
    BadRequestLine,
    /// The HTTP version in the request line is not `HTTP/1.1`.
    ///
    /// Causes a 505 HTTP Version Not Supported response.
    VersionNotSupported,
    /// The request target is longer than [`MAX_TARGET_LEN`].
    ///
    /// Causes a 414 URI Too Long response.
    UriTooLong,
    /// A header line has no `:` separator, or the header name contains
    /// whitespace immediately before the `:` (RFC 7230 §3.2.4 smuggling
    /// defence).
    BadHeader,
    /// `Content-Length` appeared more than once (request-smuggling defence,
    /// RFC 7230 §3.3.2).
    DuplicateContentLength,
    /// `Content-Length` value is not a valid decimal integer.
    InvalidContentLength,
    /// The total request size (headers + body) exceeds the cap.
    ///
    /// Causes a 413 Payload Too Large response.
    RequestTooLarge,
    /// The connection was closed before a complete request was received, or
    /// the internal buffer was exhausted trying to read headers.
    IncompleteRequest,
}

// ── Parsed request headers ─────────────────────────────────────────────────────

/// The small set of headers the server extracts from incoming requests.
///
/// All other headers are skipped.
#[derive(Default, Debug, PartialEq, Eq)]
pub struct ParsedHeaders {
    /// `Host` header value (trimmed).  Not validated beyond presence.
    ///
    /// Stored as a fixed-size buffer because `no_std` forbids `String`.
    pub host: Option<([u8; 256], usize)>,
    /// `Content-Length` value, parsed as `u32`.
    ///
    /// `None` if the header was absent.  Capped by the config's
    /// `request_size_cap` during parsing so the value is always usable as
    /// a bounded read length.
    pub content_length: Option<u32>,
    /// `Content-Type` value (trimmed), stored similarly to `host`.
    pub content_type: Option<([u8; 128], usize)>,
}

// ── Parsed request ─────────────────────────────────────────────────────────────

/// A parsed HTTP/1.1 request (request line + headers).
///
/// **Body is not parsed in this spike.**  The parser stops at the end of the
/// header block — the `\r\n\r\n` separator.  Reading and parsing the body
/// belongs to whichever route consumes it; the spike's router rejects every
/// non-`GET` method with `405 Method Not Allowed` before any body could be
/// needed.
#[derive(Debug, PartialEq, Eq)]
pub struct ParsedRequest {
    /// Request method.
    pub method: Method,
    /// Request target (path), trimmed to [`MAX_TARGET_LEN`] bytes.
    ///
    /// Stored as a fixed-size buffer because `no_std` forbids `String`.
    pub target: ([u8; MAX_TARGET_LEN], usize),
    /// Parsed header fields.
    pub headers: ParsedHeaders,
}

impl ParsedRequest {
    /// Returns the request target as a `&str`.
    ///
    /// Returns an empty string if the target bytes are not valid UTF-8
    /// (which cannot happen with well-formed HTTP/1.1 but keeps the API
    /// panic-free in all cases).
    pub fn target_str(&self) -> &str {
        let (buf, len) = &self.target;
        core::str::from_utf8(&buf[..*len]).unwrap_or("")
    }

    /// Returns the `Host` header value as a `&str`, if present.
    pub fn host_str(&self) -> Option<&str> {
        self.headers
            .host
            .as_ref()
            .and_then(|(buf, len)| core::str::from_utf8(&buf[..*len]).ok())
    }

    /// Returns the `Content-Type` value as a `&str`, if present.
    pub fn content_type_str(&self) -> Option<&str> {
        self.headers
            .content_type
            .as_ref()
            .and_then(|(buf, len)| core::str::from_utf8(&buf[..*len]).ok())
    }
}

// ── HTTP server configuration ─────────────────────────────────────────────────

/// Configuration for the minimal HTTP server.
///
/// All fields carry sensible defaults for a captive-portal SoftAP scenario.
///
/// # Why socket buffer sizes are not in this struct
///
/// The receive and transmit buffer sizes are compile-time constants
/// ([`DEFAULT_RX_BUF`] and [`DEFAULT_TX_BUF`]) because they back
/// `StaticCell<[u8; N]>` allocations and Rust requires `N` to be `const`.
/// Exposing them as runtime fields would be misleading — the values would
/// have no effect on actual memory use.  When the substrate migrates to
/// `rustyfarian-esp-hal-provisioning`, the right answer is const generics on
/// the server type itself.  See the lore entry under "esp-hal April 2026
/// Stack" in `docs/project-lore.md`.
pub struct HttpServerConfig {
    /// TCP port the server listens on (`80` by default).
    pub bind_port: u16,
    /// Maximum total request size in bytes; requests larger than this are
    /// rejected with 413 Payload Too Large (`2048` by default).
    pub request_size_cap: usize,
}

impl Default for HttpServerConfig {
    fn default() -> Self {
        Self {
            bind_port: DEFAULT_PORT,
            request_size_cap: DEFAULT_REQUEST_SIZE_CAP,
        }
    }
}

// ── Pure codec helpers (always compile — host-testable) ───────────────────────

/// Trim ASCII optional-whitespace (SP, HT, CR, LF) from both ends of a byte slice.
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

/// ASCII-case-insensitive byte-slice comparison (RFC 7230 header name rules).
fn eq_ignore_case(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter()
        .zip(b.iter())
        .all(|(x, y)| x.eq_ignore_ascii_case(y))
}

/// Parse the HTTP request line `METHOD SP TARGET SP HTTP/1.1 CRLF`.
///
/// Returns `(method, target_bytes)` on success, or a [`ParseError`] on
/// structural violations.  The caller is responsible for checking the byte
/// length of the target against [`MAX_TARGET_LEN`] before constructing a
/// `ParsedRequest`.
///
/// Accepted versions: `HTTP/1.1` only.  `HTTP/1.0` and `HTTP/2` return
/// [`ParseError::VersionNotSupported`].
pub fn parse_request_line(line: &[u8]) -> Result<(Method, &[u8]), ParseError> {
    // Find the first space separating METHOD from TARGET.
    let first_sp = line
        .iter()
        .position(|&b| b == b' ')
        .ok_or(ParseError::BadRequestLine)?;

    let method_bytes = &line[..first_sp];
    let rest = &line[first_sp + 1..];

    let method = if method_bytes == b"GET" {
        Method::Get
    } else if method_bytes == b"POST" {
        Method::Post
    } else {
        // Unknown method — treat as bad request.  A full server would return
        // 405 Method Not Allowed, but for this spike we collapse to bad-request
        // so the routing layer never sees an unrecognised method.
        return Err(ParseError::BadRequestLine);
    };

    // Find the second space separating TARGET from HTTP-version.
    // Search from the right to handle targets that contain spaces (unusual
    // but RFC 7230 technically allows encoded spaces as `%20`; those do not
    // contain literal spaces, so this is fine in practice).
    let last_sp = rest
        .iter()
        .rposition(|&b| b == b' ')
        .ok_or(ParseError::BadRequestLine)?;

    let target_bytes = &rest[..last_sp];
    let version_bytes = trim_ows(&rest[last_sp + 1..]);

    if target_bytes.is_empty() {
        return Err(ParseError::BadRequestLine);
    }

    if target_bytes.len() > MAX_TARGET_LEN {
        return Err(ParseError::UriTooLong);
    }

    // Validate the HTTP version.
    if version_bytes == b"HTTP/1.1" {
        // Good.
    } else if version_bytes.starts_with(b"HTTP/") {
        // Recognised but unsupported version (1.0, 2, 3, …).
        return Err(ParseError::VersionNotSupported);
    } else {
        return Err(ParseError::BadRequestLine);
    }

    Ok((method, target_bytes))
}

/// Parse a single header line `Name: value` into `(name_bytes, value_bytes)`.
///
/// - The name slice retains its original casing; callers compare
///   case-insensitively.
/// - OWS is trimmed from both ends of the value.
/// - Whitespace immediately before `:` is rejected (RFC 7230 §3.2.4
///   smuggling defence).
pub fn parse_header_line(line: &[u8]) -> Result<(&[u8], &[u8]), ParseError> {
    let colon = line
        .iter()
        .position(|&b| b == b':')
        .ok_or(ParseError::BadHeader)?;
    if colon == 0 {
        return Err(ParseError::BadHeader);
    }
    // Reject whitespace immediately before the colon.
    if matches!(line[colon - 1], b' ' | b'\t') {
        return Err(ParseError::BadHeader);
    }
    let name = &line[..colon];
    let value = trim_ows(&line[colon + 1..]);
    Ok((name, value))
}

/// Accumulator that reads header lines and enforces the constraints for an
/// HTTP/1.1 server request.
///
/// Feed each header line (not including the request line or the blank
/// separator) to [`HeaderAccumulator::feed`].  After the blank line, call
/// [`HeaderAccumulator::finish`] to retrieve the [`ParsedHeaders`].
pub struct HeaderAccumulator {
    headers: ParsedHeaders,
    content_length_seen: bool,
}

impl HeaderAccumulator {
    /// Construct an empty accumulator.
    pub fn new() -> Self {
        Self {
            headers: ParsedHeaders::default(),
            content_length_seen: false,
        }
    }

    /// Feed one header line.  Returns `Err` on the first violation.
    ///
    /// Unknown header names are silently skipped (RFC 7230 §3.2.1).
    pub fn feed(&mut self, line: &[u8], request_size_cap: usize) -> Result<(), ParseError> {
        let trimmed = trim_ows(line);
        if trimmed.is_empty() {
            // Blank line signals end-of-headers; callers should not pass it
            // here but we are defensive.
            return Ok(());
        }

        let (name, value) = parse_header_line(trimmed)?;

        if eq_ignore_case(name, b"content-length") {
            if self.content_length_seen {
                return Err(ParseError::DuplicateContentLength);
            }
            self.content_length_seen = true;
            // RFC 7230 §3.3.2: Content-Length is `1*DIGIT` — no sign, no leading `+`.
            if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
                return Err(ParseError::InvalidContentLength);
            }
            let s = core::str::from_utf8(value).map_err(|_| ParseError::InvalidContentLength)?;
            let n: u64 = s.parse().map_err(|_| ParseError::InvalidContentLength)?;
            // Cap the parsed value against the request size limit so downstream
            // reads are always bounded.
            if n as usize > request_size_cap {
                return Err(ParseError::RequestTooLarge);
            }
            self.headers.content_length = Some(n as u32);
        } else if eq_ignore_case(name, b"host") {
            let mut buf = [0u8; 256];
            let len = value.len().min(256);
            buf[..len].copy_from_slice(&value[..len]);
            self.headers.host = Some((buf, len));
        } else if eq_ignore_case(name, b"content-type") {
            let mut buf = [0u8; 128];
            let len = value.len().min(128);
            buf[..len].copy_from_slice(&value[..len]);
            self.headers.content_type = Some((buf, len));
        }
        // All other headers are silently ignored.

        Ok(())
    }

    /// Consume the accumulator and return the collected [`ParsedHeaders`].
    pub fn finish(self) -> ParsedHeaders {
        self.headers
    }
}

impl Default for HeaderAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse an HTTP/1.1 request line + headers from a byte buffer.
///
/// `buf` must contain at least the request line and header block terminated
/// by `\r\n\r\n`.  Any bytes following that terminator are ignored — body
/// parsing is intentionally out of scope for this spike (see the
/// [`ParsedRequest`] docs).
///
/// `request_size_cap` bounds `Content-Length` values: a request advertising a
/// body larger than the cap is rejected with [`ParseError::RequestTooLarge`].
pub fn parse_request(buf: &[u8], request_size_cap: usize) -> Result<ParsedRequest, ParseError> {
    let header_end = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or(ParseError::IncompleteRequest)?;

    let header_section = &buf[..header_end];

    let mut lines = header_section.split(|&b| b == b'\n').map(|l| {
        if l.last() == Some(&b'\r') {
            &l[..l.len() - 1]
        } else {
            l
        }
    });

    let request_line = lines.next().ok_or(ParseError::IncompleteRequest)?;
    let (method, target_bytes) = parse_request_line(request_line)?;

    let mut target_buf = [0u8; MAX_TARGET_LEN];
    let target_len = target_bytes.len();
    target_buf[..target_len].copy_from_slice(target_bytes);

    let mut accum = HeaderAccumulator::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        accum.feed(line, request_size_cap)?;
    }

    let headers = accum.finish();

    Ok(ParsedRequest {
        method,
        target: (target_buf, target_len),
        headers,
    })
}

// ── Response writer ────────────────────────────────────────────────────────────

/// Status code + reason phrase table.
///
/// Only the codes the server actually emits are listed.
fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        404 => "Not Found",
        405 => "Method Not Allowed",
        411 => "Length Required",
        413 => "Payload Too Large",
        414 => "URI Too Long",
        505 => "HTTP Version Not Supported",
        _ => "Unknown",
    }
}

/// Append `src` to `buf` at `*pos`, advancing `*pos`.
///
/// Returns `Err(())` if the buffer is too small.
fn write_bytes(buf: &mut [u8], pos: &mut usize, src: &[u8]) -> Result<(), ()> {
    let end = *pos + src.len();
    if end > buf.len() {
        return Err(());
    }
    buf[*pos..end].copy_from_slice(src);
    *pos = end;
    Ok(())
}

/// Append a decimal `u32` to `buf` at `*pos`, advancing `*pos`.
///
/// At most 10 ASCII digits (`u32::MAX = 4_294_967_295`).
fn write_u32_decimal(buf: &mut [u8], pos: &mut usize, n: u32) -> Result<(), ()> {
    let mut tmp = [0u8; 10];
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

/// Build an HTTP/1.1 response into `buf`.
///
/// Writes:
/// - `HTTP/1.1 <status> <reason> CRLF`
/// - `Content-Type: <content_type> CRLF`
/// - `Content-Length: <body.len()> CRLF`
/// - `Connection: close CRLF`
/// - Blank `CRLF`
/// - `body`
///
/// Returns the number of bytes written, or `Err(())` if the buffer is too
/// small.
///
/// # Design note
///
/// The status code and content type are per-call parameters because they
/// genuinely vary per response.  The `Connection: close` policy is a server
/// invariant and is therefore baked in, not passed as a parameter.
// Returning `Result<_, ()>` is intentional: the only failure mode is "ran out
// of bytes in the response buffer", and the caller (`route`) has no recovery
// path beyond "log and skip the write". Phase 2 will replace this with a
// real error enum once the response set grows past 200/405.
#[allow(clippy::result_unit_err)]
pub fn build_response(
    buf: &mut [u8],
    status: u16,
    content_type: &str,
    body: &[u8],
) -> Result<usize, ()> {
    let mut pos = 0;

    write_bytes(buf, &mut pos, b"HTTP/1.1 ")?;
    write_u32_decimal(buf, &mut pos, status as u32)?;
    write_bytes(buf, &mut pos, b" ")?;
    write_bytes(buf, &mut pos, reason_phrase(status).as_bytes())?;
    write_bytes(buf, &mut pos, b"\r\n")?;

    write_bytes(buf, &mut pos, b"Content-Type: ")?;
    write_bytes(buf, &mut pos, content_type.as_bytes())?;
    write_bytes(buf, &mut pos, b"\r\n")?;

    write_bytes(buf, &mut pos, b"Content-Length: ")?;
    write_u32_decimal(buf, &mut pos, body.len() as u32)?;
    write_bytes(buf, &mut pos, b"\r\n")?;

    write_bytes(buf, &mut pos, b"Connection: close\r\n")?;

    // Blank line separating headers from body.
    write_bytes(buf, &mut pos, b"\r\n")?;

    // Body.
    write_bytes(buf, &mut pos, body)?;

    Ok(pos)
}

// ── Routing ───────────────────────────────────────────────────────────────────

/// Route a [`ParsedRequest`] and write the HTTP response into `resp_buf`.
///
/// Returns the number of bytes written, or `Err(())` if the response buffer
/// is too small.
///
/// ## Captive-portal-aware routing
///
/// **Every `GET` request returns 200 OK with the portal HTML page**, regardless
/// of path.  This is the key behaviour that triggers the phone OS's
/// captive-portal pop-up: the phone probes well-known detection URLs
/// (`/generate_204`, `/hotspot-detect.html`, `/ncsi.txt`, etc.) and compares
/// the response to the expected sentinel.  When the body is _not_ the sentinel
/// — our HTML portal page — the OS knows it is behind a captive portal and
/// opens the browser automatically.
///
/// Any non-`GET` method (e.g. `POST`) returns **405 Method Not Allowed**.
/// The spike intentionally does not read request bodies (see
/// [`parse_request`] / [`ParsedRequest`]), so any method that would normally
/// carry a body is rejected at the router rather than handled half-way.
/// Phase 2 proper will replace this with a real form-submission route.
#[allow(clippy::result_unit_err)] // see `build_response` for rationale
pub fn route(req: &ParsedRequest, resp_buf: &mut [u8]) -> Result<usize, ()> {
    match req.method {
        Method::Get => build_response(
            resp_buf,
            200,
            "text/html; charset=utf-8",
            INDEX_HTML.as_bytes(),
        ),
        _ => build_response(
            resp_buf,
            405,
            "text/plain; charset=utf-8",
            METHOD_NOT_ALLOWED_BODY.as_bytes(),
        ),
    }
}

/// Write an error response (4xx / 5xx) for a parse failure.
///
/// Returns the number of bytes written.
fn error_response(buf: &mut [u8], err: &ParseError) -> usize {
    let (status, body): (u16, &str) = match err {
        ParseError::VersionNotSupported => (505, "HTTP Version Not Supported"),
        ParseError::UriTooLong => (414, "URI Too Long"),
        ParseError::RequestTooLarge => (413, "Payload Too Large"),
        ParseError::BadRequestLine
        | ParseError::BadHeader
        | ParseError::DuplicateContentLength
        | ParseError::InvalidContentLength
        | ParseError::IncompleteRequest => {
            // Minimal 400 response; we do not send explicit 400 status to
            // avoid information leakage on malformed inputs, but it conveys
            // the client is at fault.
            (400, "Bad Request")
        }
    };
    build_response(buf, status, "text/plain; charset=utf-8", body.as_bytes()).unwrap_or(0)
}

// ── Minimal fallback responses ────────────────────────────────────────────────

/// Hard-coded minimal `500 Internal Server Error` response.
///
/// Used when [`route`] reports the response buffer is too small to hold the
/// real response (today this is structurally unreachable — the spike's HTML
/// page + headers fits in well under 2 KiB — but the branch must still
/// produce a deterministic non-empty reply rather than silently emitting
/// zero bytes and letting the client time out).
///
/// The body is empty (`Content-Length: 0`); the status line tells the client
/// the server failed.  Length is well under any `resp_buf` size the server
/// will ever be constructed with, so writing it never fails.
const MINIMAL_500: &[u8] =
    b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

/// Write [`MINIMAL_500`] into `buf` and return the byte count.
///
/// Panics only if `buf.len() < MINIMAL_500.len()`, which would be a build-time
/// misconfiguration; the live `resp_buf` is `DEFAULT_TX_BUF = 2048` bytes.
fn write_minimal_500(buf: &mut [u8]) -> usize {
    let len = MINIMAL_500.len();
    debug_assert!(
        buf.len() >= len,
        "MINIMAL_500 must fit in any resp_buf the server uses"
    );
    buf[..len].copy_from_slice(MINIMAL_500);
    len
}

// ── Async server loop (bare-metal only) ──────────────────────────────────────

/// Runs the HTTP server on the given `embassy-net` stack.
///
/// This function never returns under normal operation.  It binds a TCP socket
/// on `config.bind_port`, accepts one connection at a time, reads one complete
/// HTTP request, dispatches it to the router, writes one response, then
/// closes the connection and waits for the next one.
///
/// # Single-connection design
///
/// `embassy-net`'s `TcpSocket` is a single-socket primitive: one instance can
/// be in listen, established, or close-wait state.  For a captive-portal
/// scenario a single-connection accept-loop is exactly what is needed — one
/// phone, one device.  Concurrent multi-client serving would require multiple
/// socket instances and a select loop, which is out of scope for this spike.
///
/// # Spawn this as a dedicated embassy task
///
/// ```ignore
/// use rustyfarian_esp_hal_wifi::http_server::{self, HttpServerConfig};
///
/// #[embassy_executor::task]
/// async fn http_task(stack: embassy_net::Stack<'static>) -> ! {
///     http_server::run(stack, HttpServerConfig::default()).await
/// }
/// ```
///
/// # Panic-free
///
/// All I/O errors, parse errors, and routing failures are logged at `warn`
/// and do not abort the server loop.  The only panics in this module are
/// the `StaticCell::init` calls, which are one-shot by construction and
/// identical to the pattern used throughout the crate.
///
/// # Stack usage
///
/// The task running `run` peaks at roughly 4 KiB of stack: a `req_buf` of
/// `DEFAULT_REQUEST_SIZE_CAP` (2048 B) plus a `resp_buf` of `DEFAULT_TX_BUF`
/// (2048 B), both held as on-stack arrays for the lifetime of the loop.
/// The static socket buffers ([`DEFAULT_RX_BUF`] + [`DEFAULT_TX_BUF`]) live
/// in `.bss` and do not count.  Other modules in this crate that share the
/// same task budget: the DHCP server's `rx_pkt` + `tx_pkt` (~1.5 KiB on the
/// `dhcp_task` stack), the DNS server's `rx_pkt` + `tx_pkt` (1 KiB total on
/// the `dns_task` stack), and `ProvisioningStore::save` which transiently
/// peaks at ~4–5 KiB on whatever task drives it.  Integrators sizing the
/// spawned-task stacks should budget at least 6 KiB on top of their own
/// requirements until the Phase 2B promotion lands the targeted-read store
/// optimisation.
#[cfg(all(feature = "embassy", any(feature = "esp32c3", feature = "esp32c6")))]
pub async fn run(stack: embassy_net::Stack<'static>, config: HttpServerConfig) -> ! {
    use embassy_net::tcp::TcpSocket;
    use static_cell::StaticCell;

    // Static socket buffers — required because `TcpSocket::new` in
    // embassy-net 0.8 internally transmutes the buffer slices to `'static`
    // (see `embassy_net::tcp::TcpSocket::new` safety contract in tcp.rs).
    static RX_BUF: StaticCell<[u8; DEFAULT_RX_BUF]> = StaticCell::new();
    static TX_BUF: StaticCell<[u8; DEFAULT_TX_BUF]> = StaticCell::new();

    let rx_buf = RX_BUF.init([0u8; DEFAULT_RX_BUF]);
    let tx_buf = TX_BUF.init([0u8; DEFAULT_TX_BUF]);

    // One TcpSocket, reused across connections.
    let mut socket = TcpSocket::new(stack, rx_buf, tx_buf);

    // Request and response staging buffers on the stack — no heap needed.
    let mut req_buf = [0u8; DEFAULT_REQUEST_SIZE_CAP];
    let mut resp_buf = [0u8; DEFAULT_TX_BUF];

    let port = config.bind_port;
    let size_cap = config.request_size_cap;

    log::info!("HTTP server listening on port {}", port);

    loop {
        // Accept the next connection.
        match socket.accept(port).await {
            Ok(()) => {}
            Err(e) => {
                log::warn!("HTTP accept error: {:?}; retrying", e);
                // Brief yield before looping so we do not spin-hammer on a
                // persistent accept failure.
                embassy_time::Timer::after(embassy_time::Duration::from_millis(100)).await;
                continue;
            }
        }

        // Read the request into req_buf until we see "\r\n\r\n" (header end)
        // or fill the buffer.
        let filled = read_request(&mut socket, &mut req_buf, size_cap).await;

        // Track how much of the request body has already been received into
        // `req_buf` past the header terminator, so the post-response drain
        // (RST-avoidance) does not double-count.
        let mut body_drain_target: Option<(usize, usize)> = None;

        // SECURITY: never log request bodies — POST bodies will carry
        // `wifi_pass` / `mqtt_pass` credentials when the `/save` route
        // lands in Phase 2B promotion.  Log lines below are limited to
        // method, target, and parser-error variants; bytes from `req_buf`
        // past the header terminator MUST NOT appear in any log call.
        // Parse and dispatch.
        let resp_len = match filled {
            Ok(n) => match parse_request(&req_buf[..n], size_cap) {
                Ok(req) => {
                    log::info!(
                        "HTTP {} {}",
                        if req.method == Method::Get {
                            "GET"
                        } else {
                            "POST"
                        },
                        req.target_str()
                    );

                    // Compute how many body bytes were already pulled into
                    // `req_buf` alongside the headers (clients that pipeline
                    // headers + body in a single TCP send).  The remainder
                    // (Content-Length minus what we already have) is what
                    // needs draining from the socket before close.
                    if let Some(cl) = req.headers.content_length {
                        if let Some(hdr_end) =
                            req_buf[..n].windows(4).position(|w| w == b"\r\n\r\n")
                        {
                            let already = n.saturating_sub(hdr_end + 4);
                            body_drain_target = Some((cl as usize, already));
                        }
                    }

                    route(&req, &mut resp_buf).unwrap_or_else(|()| {
                        // `route` could not fit the real response into
                        // `resp_buf`.  Emit a deterministic minimal 500
                        // instead of zero bytes — never let the client
                        // time out on an empty reply.
                        log::warn!(
                            "HTTP: route response too large for resp_buf — emitting minimal 500"
                        );
                        write_minimal_500(&mut resp_buf)
                    })
                }
                Err(e) => {
                    log::warn!("HTTP parse error: {:?}", e);
                    error_response(&mut resp_buf, &e)
                }
            },
            Err(e) => {
                log::warn!("HTTP read error: {:?}", e);
                error_response(&mut resp_buf, &e)
            }
        };

        // Write the response (partial-write safe loop).
        if resp_len > 0 {
            let mut written = 0;
            while written < resp_len {
                match socket.write(&resp_buf[written..resp_len]).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => written += n,
                }
            }
        }

        // Flush ensures the FIN is enqueued before close() teardown.
        let _ = socket.flush().await;

        // Drain any remaining request body before close — without this, a
        // client still uploading a POST body (e.g. the 405 path) sees RST
        // instead of FIN once `socket.close()` fires.  Browsers can
        // surface RST as "connection reset" rather than the response we
        // just wrote, so the drain is what makes 405 (and the future
        // /save route) deliver visibly to the client.  The drain runs
        // with a 500 ms total deadline so a slow or stuck client cannot
        // pin the single TCP socket indefinitely.
        if let Some((content_length, already_in_buf)) = body_drain_target {
            if content_length > already_in_buf {
                let remaining = content_length - already_in_buf;
                drain_body_with_deadline(
                    &mut socket,
                    remaining,
                    embassy_time::Duration::from_millis(500),
                )
                .await;
            }
        }

        // Close the write half; the connection will fully teardown after
        // the remote ACKs our FIN.
        socket.close();

        // Poll flush once more to drive the close handshake without blocking
        // indefinitely (a timeout is not available in the base API, so we
        // yield once and move on — the TCP stack handles the cleanup).
        let _ = socket.flush().await;
    }
}

/// Read and discard up to `remaining` bytes from `socket`, honouring `deadline`.
///
/// Used between the response write and `socket.close()` so an in-flight POST
/// body does not race the FIN we send and trigger an RST.  The discard buffer
/// is small (256 B); body content is intentionally never inspected so a
/// `// SECURITY` rule about not handling credential bytes here is implicit.
///
/// Returns silently — partial drains, deadlines, and EOFs are all benign:
/// the close() that follows is best-effort either way.
#[cfg(all(feature = "embassy", any(feature = "esp32c3", feature = "esp32c6")))]
async fn drain_body_with_deadline(
    socket: &mut embassy_net::tcp::TcpSocket<'_>,
    mut remaining: usize,
    deadline: embassy_time::Duration,
) {
    let mut scratch = [0u8; 256];
    let drain = async {
        while remaining > 0 {
            let take = remaining.min(scratch.len());
            match socket.read(&mut scratch[..take]).await {
                Ok(0) | Err(_) => return,
                Ok(n) => remaining -= n,
            }
        }
    };
    let _ = embassy_time::with_timeout(deadline, drain).await;
}

/// Read from `socket` until the request headers are complete (`\r\n\r\n`)
/// or the buffer fills.
///
/// Returns the number of valid bytes in `buf`, or a [`ParseError`] on I/O
/// failure or a request that exceeds the cap before headers complete.
#[cfg(all(feature = "embassy", any(feature = "esp32c3", feature = "esp32c6")))]
async fn read_request(
    socket: &mut embassy_net::tcp::TcpSocket<'_>,
    buf: &mut [u8],
    size_cap: usize,
) -> Result<usize, ParseError> {
    let mut filled = 0usize;

    loop {
        if filled >= buf.len().min(size_cap) {
            // Buffer or cap exhausted — treat as oversized request.
            return Err(ParseError::RequestTooLarge);
        }

        let n = socket
            .read(&mut buf[filled..])
            .await
            .map_err(|_| ParseError::IncompleteRequest)?;

        if n == 0 {
            // Connection closed by remote before we got a complete request.
            return Err(ParseError::IncompleteRequest);
        }
        filled += n;

        // Check whether we have a complete header section.
        if buf[..filled].windows(4).any(|w| w == b"\r\n\r\n") {
            return Ok(filled);
        }
    }
}

// ── Unit tests (host-testable codec + routing) ────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Request line parsing ──────────────────────────────────────────────────

    /// Well-formed GET / HTTP/1.1 parses correctly.
    #[test]
    fn request_line_get_root() {
        let (method, target) = parse_request_line(b"GET / HTTP/1.1").unwrap();
        assert_eq!(method, Method::Get);
        assert_eq!(target, b"/");
    }

    /// POST with a longer path parses correctly.
    #[test]
    fn request_line_post_save() {
        let (method, target) = parse_request_line(b"POST /save HTTP/1.1").unwrap();
        assert_eq!(method, Method::Post);
        assert_eq!(target, b"/save");
    }

    /// HTTP/1.0 is rejected with VersionNotSupported.
    #[test]
    fn request_line_http10_rejected() {
        assert_eq!(
            parse_request_line(b"GET / HTTP/1.0"),
            Err(ParseError::VersionNotSupported)
        );
    }

    /// HTTP/2.0 is rejected with VersionNotSupported.
    #[test]
    fn request_line_http2_rejected() {
        assert_eq!(
            parse_request_line(b"GET / HTTP/2.0"),
            Err(ParseError::VersionNotSupported)
        );
    }

    /// A request line with no spaces at all is a bad request.
    #[test]
    fn request_line_no_spaces() {
        assert_eq!(
            parse_request_line(b"GETHTTP/1.1"),
            Err(ParseError::BadRequestLine)
        );
    }

    /// A target exactly at MAX_TARGET_LEN is accepted.
    #[test]
    fn request_line_target_at_max_len() {
        let mut line = b"GET /".to_vec();
        // Fill target with 'a' to exactly reach MAX_TARGET_LEN including the '/'.
        line.extend(core::iter::repeat_n(b'a', MAX_TARGET_LEN - 1));
        line.extend_from_slice(b" HTTP/1.1");
        let (method, target) = parse_request_line(&line).unwrap();
        assert_eq!(method, Method::Get);
        assert_eq!(target.len(), MAX_TARGET_LEN);
    }

    /// A target one byte over MAX_TARGET_LEN is rejected with UriTooLong.
    #[test]
    fn request_line_target_too_long() {
        let mut line = b"GET /".to_vec();
        line.extend(core::iter::repeat_n(b'a', MAX_TARGET_LEN));
        line.extend_from_slice(b" HTTP/1.1");
        assert_eq!(parse_request_line(&line), Err(ParseError::UriTooLong));
    }

    // ── Full request round-trips ──────────────────────────────────────────────

    /// GET / with Host header parses method, path, and Host.
    #[test]
    fn parse_get_root_with_host() {
        let raw = b"GET / HTTP/1.1\r\nHost: 192.168.4.1\r\n\r\n";
        let req = parse_request(raw, DEFAULT_REQUEST_SIZE_CAP).unwrap();
        assert_eq!(req.method, Method::Get);
        assert_eq!(req.target_str(), "/");
        assert_eq!(req.host_str(), Some("192.168.4.1"));
    }

    /// POST /save parses the request line and headers; the body is intentionally
    /// not extracted (see [`ParsedRequest`] — body is out of scope for the spike).
    #[test]
    fn parse_post_save_request_line_and_headers() {
        let raw = b"POST /save HTTP/1.1\r\n\
            Host: 192.168.4.1\r\n\
            Content-Length: 11\r\n\
            Content-Type: application/x-www-form-urlencoded\r\n\
            \r\n\
            foo=1&bar=2";

        let req = parse_request(raw, DEFAULT_REQUEST_SIZE_CAP).unwrap();
        assert_eq!(req.method, Method::Post);
        assert_eq!(req.target_str(), "/save");
        assert_eq!(req.host_str(), Some("192.168.4.1"));
        assert_eq!(
            req.content_type_str(),
            Some("application/x-www-form-urlencoded")
        );
        assert_eq!(req.headers.content_length, Some(11));
    }

    /// Duplicate Content-Length is rejected.
    #[test]
    fn duplicate_content_length_rejected() {
        let raw = b"POST /save HTTP/1.1\r\n\
            Host: 192.168.4.1\r\n\
            Content-Length: 5\r\n\
            Content-Length: 5\r\n\
            \r\n\
            hello";
        assert_eq!(
            parse_request(raw, DEFAULT_REQUEST_SIZE_CAP),
            Err(ParseError::DuplicateContentLength)
        );
    }

    /// Request without CRLF terminator is IncompleteRequest.
    #[test]
    fn incomplete_request_no_crlf() {
        let raw = b"GET / HTTP/1.1\r\nHost: x";
        assert_eq!(
            parse_request(raw, DEFAULT_REQUEST_SIZE_CAP),
            Err(ParseError::IncompleteRequest)
        );
    }

    /// Request exceeding the size cap is RequestTooLarge.
    #[test]
    fn request_too_large() {
        // Build a request with Content-Length larger than the cap.
        let raw = b"POST /save HTTP/1.1\r\n\
            Content-Length: 9999\r\n\
            \r\n\
            x";
        assert_eq!(
            parse_request(raw, 64), // tiny cap of 64 bytes
            Err(ParseError::RequestTooLarge)
        );
    }

    // ── Header case insensitivity ─────────────────────────────────────────────

    /// `HOST: x` (upper-case) parses to the same host as `Host: x`.
    #[test]
    fn host_header_case_insensitive_upper() {
        let raw = b"GET / HTTP/1.1\r\nHOST: 192.168.4.1\r\n\r\n";
        let req = parse_request(raw, DEFAULT_REQUEST_SIZE_CAP).unwrap();
        assert_eq!(req.host_str(), Some("192.168.4.1"));
    }

    /// `host: x` (lower-case) parses to the same host as `Host: x`.
    #[test]
    fn host_header_case_insensitive_lower() {
        let raw = b"GET / HTTP/1.1\r\nhost: 192.168.4.1\r\n\r\n";
        let req = parse_request(raw, DEFAULT_REQUEST_SIZE_CAP).unwrap();
        assert_eq!(req.host_str(), Some("192.168.4.1"));
    }

    /// `content-length: 5` (lower-case) is honoured.
    #[test]
    fn content_length_case_insensitive() {
        let raw = b"POST /save HTTP/1.1\r\n\
            content-length: 5\r\n\
            \r\n\
            hello";
        let req = parse_request(raw, DEFAULT_REQUEST_SIZE_CAP).unwrap();
        assert_eq!(req.headers.content_length, Some(5));
    }

    /// Unknown headers are skipped without failing the parser.
    #[test]
    fn unknown_headers_are_skipped() {
        let raw = b"GET / HTTP/1.1\r\n\
            Host: 192.168.4.1\r\n\
            X-Custom-Header: some-value\r\n\
            Accept: */*\r\n\
            \r\n";
        let req = parse_request(raw, DEFAULT_REQUEST_SIZE_CAP).unwrap();
        assert_eq!(req.target_str(), "/");
        assert_eq!(req.host_str(), Some("192.168.4.1"));
    }

    // ── Response builder ──────────────────────────────────────────────────────

    /// A 200 response round-trips: status line, all four headers, body.
    #[test]
    fn response_200_structure() {
        let body = b"hello";
        let mut buf = [0u8; 256];
        let n = build_response(&mut buf, 200, "text/plain", body).unwrap();
        let resp = core::str::from_utf8(&buf[..n]).unwrap();

        // Status line.
        assert!(
            resp.starts_with("HTTP/1.1 200 OK\r\n"),
            "status line: {}",
            resp
        );

        // Required response headers.
        assert!(resp.contains("Content-Type: text/plain\r\n"));
        assert!(resp.contains("Content-Length: 5\r\n"));
        assert!(resp.contains("Connection: close\r\n"));

        // Blank separator line.
        assert!(resp.contains("\r\n\r\n"));

        // Body at the end.
        assert!(resp.ends_with("hello"));
    }

    /// A 404 response has the correct status line.
    #[test]
    fn response_404_status_line() {
        let mut buf = [0u8; 256];
        let n = build_response(&mut buf, 404, "text/plain", b"Not Found").unwrap();
        let resp = core::str::from_utf8(&buf[..n]).unwrap();
        assert!(resp.starts_with("HTTP/1.1 404 Not Found\r\n"));
    }

    /// A 505 response has the correct status line.
    #[test]
    fn response_505_status_line() {
        let mut buf = [0u8; 256];
        let n = build_response(&mut buf, 505, "text/plain", b"HTTP Version Not Supported").unwrap();
        let resp = core::str::from_utf8(&buf[..n]).unwrap();
        assert!(resp.starts_with("HTTP/1.1 505 HTTP Version Not Supported\r\n"));
    }

    /// A 413 response has the correct status line.
    #[test]
    fn response_413_status_line() {
        let mut buf = [0u8; 256];
        let n = build_response(&mut buf, 413, "text/plain", b"Payload Too Large").unwrap();
        let resp = core::str::from_utf8(&buf[..n]).unwrap();
        assert!(resp.starts_with("HTTP/1.1 413 Payload Too Large\r\n"));
    }

    // ── Routing ───────────────────────────────────────────────────────────────

    /// GET / routes to the HTML page (status 200 in the response bytes).
    #[test]
    fn route_get_root_returns_200() {
        let raw = b"GET / HTTP/1.1\r\nHost: 192.168.4.1\r\n\r\n";
        let req = parse_request(raw, DEFAULT_REQUEST_SIZE_CAP).unwrap();
        let mut resp_buf = [0u8; 4096];
        let n = route(&req, &mut resp_buf).unwrap();
        let resp = core::str::from_utf8(&resp_buf[..n]).unwrap();
        assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"));
        assert!(resp.contains("text/html; charset=utf-8"));
        assert!(resp.contains("Rustyfarian SoftAP"));
    }

    /// GET /generate_204 (Android captive-portal probe) returns 200 + portal page.
    ///
    /// This is the key captive-portal trigger: the OS expects a 204 response
    /// body but receives our HTML page, so it knows to pop the captive browser.
    #[test]
    fn route_captive_portal_probe_generate_204_returns_200() {
        let raw = b"GET /generate_204 HTTP/1.1\r\nHost: 192.168.4.1\r\n\r\n";
        let req = parse_request(raw, DEFAULT_REQUEST_SIZE_CAP).unwrap();
        let mut resp_buf = [0u8; 4096];
        let n = route(&req, &mut resp_buf).unwrap();
        let resp = core::str::from_utf8(&resp_buf[..n]).unwrap();
        assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"), "resp: {}", resp);
        assert!(resp.contains("Rustyfarian SoftAP"));
    }

    /// GET /hotspot-detect.html (iOS captive-portal probe) returns 200 + portal page.
    #[test]
    fn route_captive_portal_probe_hotspot_detect_returns_200() {
        let raw = b"GET /hotspot-detect.html HTTP/1.1\r\nHost: captive.apple.com\r\n\r\n";
        let req = parse_request(raw, DEFAULT_REQUEST_SIZE_CAP).unwrap();
        let mut resp_buf = [0u8; 4096];
        let n = route(&req, &mut resp_buf).unwrap();
        let resp = core::str::from_utf8(&resp_buf[..n]).unwrap();
        assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"), "resp: {}", resp);
        assert!(resp.contains("Rustyfarian SoftAP"));
    }

    /// GET /ncsi.txt (Windows captive-portal probe) returns 200 + portal page.
    #[test]
    fn route_captive_portal_probe_ncsi_returns_200() {
        let raw = b"GET /ncsi.txt HTTP/1.1\r\nHost: 192.168.4.1\r\n\r\n";
        let req = parse_request(raw, DEFAULT_REQUEST_SIZE_CAP).unwrap();
        let mut resp_buf = [0u8; 4096];
        let n = route(&req, &mut resp_buf).unwrap();
        let resp = core::str::from_utf8(&resp_buf[..n]).unwrap();
        assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"), "resp: {}", resp);
        assert!(resp.contains("Rustyfarian SoftAP"));
    }

    /// GET /favicon.ico returns 200 + portal HTML (browser ignores invalid favicon).
    #[test]
    fn route_get_favicon_returns_200() {
        let raw = b"GET /favicon.ico HTTP/1.1\r\nHost: 192.168.4.1\r\n\r\n";
        let req = parse_request(raw, DEFAULT_REQUEST_SIZE_CAP).unwrap();
        let mut resp_buf = [0u8; 4096];
        let n = route(&req, &mut resp_buf).unwrap();
        let resp = core::str::from_utf8(&resp_buf[..n]).unwrap();
        assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"), "resp: {}", resp);
        assert!(resp.contains("Rustyfarian SoftAP"));
    }

    /// GET /any/arbitrary/path returns 200 — catch-all GET behaviour.
    #[test]
    fn route_any_get_path_returns_200() {
        let raw = b"GET /some/deep/path?q=1 HTTP/1.1\r\nHost: 192.168.4.1\r\n\r\n";
        let req = parse_request(raw, DEFAULT_REQUEST_SIZE_CAP).unwrap();
        let mut resp_buf = [0u8; 4096];
        let n = route(&req, &mut resp_buf).unwrap();
        let resp = core::str::from_utf8(&resp_buf[..n]).unwrap();
        assert!(resp.starts_with("HTTP/1.1 200 OK\r\n"), "resp: {}", resp);
    }

    // ── Minimal 500 fallback ──────────────────────────────────────────────────

    /// `write_minimal_500` emits a syntactically complete `Content-Length: 0`
    /// response that any client can parse — never zero bytes.
    #[test]
    fn minimal_500_is_a_valid_complete_response() {
        let mut buf = [0u8; 2048];
        let n = write_minimal_500(&mut buf);
        assert!(n > 0, "minimal 500 must never be empty");
        let resp = core::str::from_utf8(&buf[..n]).unwrap();
        assert!(
            resp.starts_with("HTTP/1.1 500 Internal Server Error\r\n"),
            "status line: {}",
            resp
        );
        assert!(resp.contains("Content-Length: 0\r\n"));
        assert!(resp.contains("Connection: close\r\n"));
        assert!(resp.ends_with("\r\n\r\n"));
    }

    /// POST /save returns 405 Method Not Allowed — the spike is GET-only.
    ///
    /// Will become a real form-submission route when provisioning lands in
    /// Phase 2 proper.
    #[test]
    fn route_post_save_returns_405() {
        let raw = b"POST /save HTTP/1.1\r\nHost: 192.168.4.1\r\nContent-Length: 0\r\n\r\n";
        let req = parse_request(raw, DEFAULT_REQUEST_SIZE_CAP).unwrap();
        let mut resp_buf = [0u8; 256];
        let n = route(&req, &mut resp_buf).unwrap();
        let resp = core::str::from_utf8(&resp_buf[..n]).unwrap();
        assert!(
            resp.starts_with("HTTP/1.1 405 Method Not Allowed\r\n"),
            "resp: {}",
            resp
        );
        assert!(resp.contains("Method Not Allowed"));
    }
}
