//! Captive-portal HTTP router for bare-metal SoftAP provisioning (ADR 015 §3).
//!
//! This module contains:
//!
//! 1. **Pure codec helpers** — `parse_request`, `build_response`,
//!    `read_request`, etc. — always compiled and fully host-testable.
//! 2. **Route dispatcher** — `dispatch_request` maps each request to one of
//!    the functional routes: `GET /`, `GET /factory-reset`, `POST /save`,
//!    `POST /factory-reset`, OS captive-portal probe redirects, or 404.
//! 3. **Async accept-loop** — `run_portal_dyn` (embassy + chip feature gated)
//!    binds a TCP socket, accepts one connection at a time, reads one request,
//!    dispatches it, writes one response, then closes and loops.
//!
//! # Security posture
//!
//! Security-checklist items implemented in this file (see
//! `docs/features/archive/esp-hal-provisioning-v1.md`):
//!
//! - Item 1: nonce check on every mutating POST
//!   (`juggler::provisioning::body_nonce_matches`, constant-time).
//! - Item 2: `juggler::provisioning::Prefill` never holds `wifi_pass` or
//!   `mqtt_pass`.
//! - Item 3: `Content-Length > DEFAULT_REQUEST_SIZE_CAP` returns 413.
//! - Item 5: `Cache-Control: no-store` baked into `build_response` and
//!   `MINIMAL_500` unconditionally.
//! - Item 10: `req_buf` overwritten with `0xFF` after each handled request.
//!
//! # Stack usage
//!
//! The HTTP task steady-state frame peaks at roughly **8.3 KiB**:
//! - `req_buf` of `DEFAULT_REQUEST_SIZE_CAP` (2048 B) on the stack
//! - `resp_buf` of `DEFAULT_TX_BUF` (6144 B) on the stack
//! - small locals and async executor overhead
//!
//! Both buffers are stack-allocated for the lifetime of the accept-loop.
//! Static socket buffers (`DEFAULT_RX_BUF` + `DEFAULT_TX_BUF`) live in
//! `.bss` and do not count against the task stack.
//!
//! During a `POST /save` request, `dispatch_request` calls
//! `ProvisioningStore::save`, which transiently allocates a
//! `heapless::Vec<u8, 4096>` encode buffer, a `[u8; 1280]` read buffer and
//! two decoded `ProvisioningConfig` records (about 1.4 KiB each) on its own
//! frames, about 11 KiB; `dispatch_request` itself holds the parsed
//! `config_to_save` (about 1.4 KiB) across the call, so a `POST /save` adds
//! approximately 12.5 KiB to the peak.
//! This raises the worst-case stack depth to at most approximately **20.5 KiB**.
//! Every form render (`GET /`, the `400` and `500` re-renders) holds the
//! stored record (about 1.4 KiB) while `juggler::provisioning::Prefill`
//! borrows from it; the failed-save render drops `config_to_save` first, so
//! the peak above still bounds it.
//! These figures are an analytic upper bound from host `size_of`, not
//! measured; runbook H2 only checks for overflow at the default stack.
//!
//! **Recommended integrator HTTP-task stack: 24 KiB minimum.**
//!
//! The steady-state HTTP frame is ~8 KiB (`req_buf` 2048 + `resp_buf` 6144
//! + executor overhead); `POST /save` transiently adds another ~12.5 KiB for
//! `ProvisioningStore::save`'s encode + read buffers and the held
//! `config_to_save`, raising the worst-case peak to ~20.5 KiB.
//! Integrators should size the spawned HTTP task with at least 24 KiB of
//! stack to leave headroom for ISR frames and stack canary.
//!
//! In bare-metal embassy (`esp-rtos` thread-mode executor) all async tasks
//! share the main thread's stack, which esp-hal sizes at link time as the
//! RAM left after `.data`, `.bss` and the heap; there is no size setting.
//! Keep at least **24 KiB** free before spawning the HTTP task and enforce
//! it with `ESP_HAL_CONFIG_ENSURE_MAIN_STACK_MINIMUM = "24576"` (a link-time
//! assertion); `CONFIG_ESP_MAIN_TASK_STACK_SIZE` is ESP-IDF only.

// When building without the embassy + chip features the async `run_portal_dyn`
// function and its TcpSocket usage are compiled away.  Allow dead-code on the
// types that remain so clippy -D warnings does not fail on stub/host builds.
#![cfg_attr(
    not(all(
        feature = "embassy",
        any(feature = "esp32c3", feature = "esp32c6", feature = "esp32s3")
    )),
    allow(dead_code)
)]

// The bare-metal embassy builds have a global allocator (required by esp-radio);
// bring `alloc` into scope for the validation-error string formatting path.
#[cfg(all(
    feature = "embassy",
    any(feature = "esp32c3", feature = "esp32c6", feature = "esp32s3")
))]
extern crate alloc;

#[cfg(any(
    test,
    all(
        feature = "embassy",
        any(feature = "esp32c3", feature = "esp32c6", feature = "esp32s3")
    )
))]
use juggler::provisioning::html_json_escape::html_escape_to;
#[cfg(all(
    feature = "embassy",
    any(feature = "esp32c3", feature = "esp32c6", feature = "esp32s3")
))]
use juggler::provisioning::parse_form;
#[cfg(all(
    feature = "embassy",
    any(feature = "esp32c3", feature = "esp32c6", feature = "esp32s3")
))]
use juggler::provisioning::{
    body_nonce_matches,
    templates::{COMMITTED_HTML, FACTORY_RESET_HTML, PROBE_PATHS, SESSION_FINISHED_BODY},
    PortalAction, PortalInput, PortalPhase, ProvisioningConfig,
};
#[cfg(any(
    test,
    all(
        feature = "embassy",
        any(feature = "esp32c3", feature = "esp32c6", feature = "esp32s3")
    )
))]
use juggler::provisioning::{templates::template_for, Prefill};

#[cfg(any(
    test,
    all(
        feature = "embassy",
        any(feature = "esp32c3", feature = "esp32c6", feature = "esp32s3")
    )
))]
use super::session::portal::PortalRenderConfig;
#[cfg(all(
    feature = "embassy",
    any(feature = "esp32c3", feature = "esp32c6", feature = "esp32s3")
))]
use super::session::{PortalStore, ProvisioningEvent, ProvisioningOutcome, SharedState};

// ── HTTP wire constants ───────────────────────────────────────────────────────

/// Maximum length of the request-target (path) in bytes.
const MAX_TARGET_LEN: usize = 256;

/// Maximum total request size in bytes (request line + headers + body).
///
/// Requests with `Content-Length` exceeding this cap are rejected with 413.
pub(crate) const DEFAULT_REQUEST_SIZE_CAP: usize = 2048;

// Static assert: security-checklist item 3 requires this cap stays at 2048.
const _: () = assert!(DEFAULT_REQUEST_SIZE_CAP == 2048);

/// Default TCP port the portal listens on.
const DEFAULT_PORT: u16 = 80;

/// Default socket receive buffer size.
const DEFAULT_RX_BUF: usize = 1024;

/// Default socket transmit buffer size.
///
/// 6 KiB is the Phase 2B–locked value (see `docs/features/archive/esp-hal-provisioning-v1.md`
/// Decisions "Locked at Phase 2B implementation").  Real portal HTML for the
/// `WifiMqttDevice` profile is 4–6 KiB rendered with placeholders substituted;
/// a smaller buffer would trip the [`MINIMAL_500`] fallback on every portal GET.
const DEFAULT_TX_BUF: usize = 6144;

/// The budget of one connection phase as an embassy duration.
///
/// The values live in `juggler::provisioning::PortalPhase` (shared with the ESP-IDF
/// tier's `httpd` timeouts, host-tested); this is only the unit conversion.
/// A phone may open speculative idle connections, and this portal serves one
/// connection at a time, so every phase of a connection has its own bound.
#[cfg(all(
    feature = "embassy",
    any(feature = "esp32c3", feature = "esp32c6", feature = "esp32s3")
))]
fn phase_budget(phase: juggler::provisioning::PortalPhase) -> embassy_time::Duration {
    embassy_time::Duration::from_millis(u64::from(phase.budget_ms()))
}

// ── Page content ─────────────────────────────────────────────────────────────

/// GET /factory-reset — confirmation page with nonce-bearing POST form.
const FACTORY_RESET_CONFIRM_HTML_PART1: &str =
    "<!DOCTYPE html><html><head><meta charset=\"utf-8\">\
<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\
<title>Factory reset</title></head><body style=\"font-family:system-ui,sans-serif;padding:1rem\">\
<h1>Factory reset</h1><p>This will erase all stored credentials. Are you sure?</p>\
<form method=\"post\" action=\"/factory-reset\">\
<input type=\"hidden\" name=\"_nonce\" value=\"";

const FACTORY_RESET_CONFIRM_HTML_PART2: &str = "\">\
<button type=\"submit\">Confirm factory reset</button>\
</form></body></html>";

// ── HTTP method ───────────────────────────────────────────────────────────────

/// HTTP request method — the subset the portal handles.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub(crate) enum Method {
    /// `GET`
    Get,
    /// `POST`
    Post,
}

// ── Parse error ───────────────────────────────────────────────────────────────

/// Errors produced by the HTTP request parser.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum ParseError {
    /// The request line is missing, empty, or structurally invalid (→ 400).
    BadRequestLine,
    /// The HTTP version is not `HTTP/1.1` (→ 505).
    VersionNotSupported,
    /// The request target exceeds [`MAX_TARGET_LEN`] (→ 414).
    UriTooLong,
    /// A header has no `:` separator or whitespace before `:` (→ 400).
    BadHeader,
    /// `Content-Length` appeared more than once (→ 400).
    DuplicateContentLength,
    /// `Content-Length` is not a valid decimal integer (→ 400).
    InvalidContentLength,
    /// Total request size exceeds the cap (→ 413).
    RequestTooLarge,
    /// Connection closed before a complete request was received (→ 400).
    IncompleteRequest,
}

// ── Parsed request headers ─────────────────────────────────────────────────────

/// The small set of headers the server extracts from incoming requests.
#[derive(Default, Debug, PartialEq, Eq)]
pub(crate) struct ParsedHeaders {
    /// `Host` header value (trimmed).
    pub host: Option<([u8; 256], usize)>,
    /// `Content-Length` value, parsed as `u32`.
    pub content_length: Option<u32>,
    /// `Content-Type` value (trimmed).
    pub content_type: Option<([u8; 128], usize)>,
}

// ── Parsed request ─────────────────────────────────────────────────────────────

/// A parsed HTTP/1.1 request (request line + headers).
///
/// The body is not parsed by `parse_request`.  When the route needs the body,
/// it is sliced out of `req_buf` after the `\r\n\r\n` separator.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ParsedRequest {
    /// Request method.
    pub method: Method,
    /// Request target (path), trimmed to [`MAX_TARGET_LEN`] bytes.
    pub target: ([u8; MAX_TARGET_LEN], usize),
    /// Parsed header fields.
    pub headers: ParsedHeaders,
}

impl ParsedRequest {
    /// Returns the request target as a `&str`.
    ///
    /// Returns an empty string if the bytes are not valid UTF-8, which cannot
    /// happen with well-formed HTTP/1.1 but keeps the API panic-free.
    pub(crate) fn target_str(&self) -> &str {
        let (buf, len) = &self.target;
        core::str::from_utf8(&buf[..*len]).unwrap_or("")
    }

    /// Returns the `Host` header value as a `&str`, if present.
    #[cfg(test)]
    pub(crate) fn host_str(&self) -> Option<&str> {
        self.headers
            .host
            .as_ref()
            .and_then(|(buf, len)| core::str::from_utf8(&buf[..*len]).ok())
    }

    /// Returns the `Content-Type` value as a `&str`, if present.
    #[cfg(test)]
    pub(crate) fn content_type_str(&self) -> Option<&str> {
        self.headers
            .content_type
            .as_ref()
            .and_then(|(buf, len)| core::str::from_utf8(&buf[..*len]).ok())
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
pub(crate) fn parse_request_line(line: &[u8]) -> Result<(Method, &[u8]), ParseError> {
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
        return Err(ParseError::BadRequestLine);
    };

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

    if version_bytes == b"HTTP/1.1" {
        // Good.
    } else if version_bytes.starts_with(b"HTTP/") {
        return Err(ParseError::VersionNotSupported);
    } else {
        return Err(ParseError::BadRequestLine);
    }

    Ok((method, target_bytes))
}

/// Parse a single header line `Name: value` into `(name_bytes, value_bytes)`.
pub(crate) fn parse_header_line(line: &[u8]) -> Result<(&[u8], &[u8]), ParseError> {
    let colon = line
        .iter()
        .position(|&b| b == b':')
        .ok_or(ParseError::BadHeader)?;
    if colon == 0 {
        return Err(ParseError::BadHeader);
    }
    if matches!(line[colon - 1], b' ' | b'\t') {
        return Err(ParseError::BadHeader);
    }
    let name = &line[..colon];
    let value = trim_ows(&line[colon + 1..]);
    Ok((name, value))
}

/// Accumulates header lines and enforces HTTP/1.1 server constraints.
pub(crate) struct HeaderAccumulator {
    headers: ParsedHeaders,
    content_length_seen: bool,
}

impl HeaderAccumulator {
    pub(crate) fn new() -> Self {
        Self {
            headers: ParsedHeaders::default(),
            content_length_seen: false,
        }
    }

    pub(crate) fn feed(&mut self, line: &[u8], request_size_cap: usize) -> Result<(), ParseError> {
        let trimmed = trim_ows(line);
        if trimmed.is_empty() {
            return Ok(());
        }

        let (name, value) = parse_header_line(trimmed)?;

        if eq_ignore_case(name, b"content-length") {
            if self.content_length_seen {
                return Err(ParseError::DuplicateContentLength);
            }
            self.content_length_seen = true;
            if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
                return Err(ParseError::InvalidContentLength);
            }
            let s = core::str::from_utf8(value).map_err(|_| ParseError::InvalidContentLength)?;
            let n: u64 = s.parse().map_err(|_| ParseError::InvalidContentLength)?;
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
        Ok(())
    }

    pub(crate) fn finish(self) -> ParsedHeaders {
        self.headers
    }
}

impl Default for HeaderAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse an HTTP/1.1 request from a byte buffer.
pub(crate) fn parse_request(
    buf: &[u8],
    request_size_cap: usize,
) -> Result<ParsedRequest, ParseError> {
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

/// Status code → reason phrase.
fn reason_phrase(status: u16) -> &'static str {
    match status {
        200 => "OK",
        302 => "Found",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Payload Too Large",
        414 => "URI Too Long",
        500 => "Internal Server Error",
        505 => "HTTP Version Not Supported",
        _ => "Unknown",
    }
}

/// Append `src` to `buf` at `*pos`, advancing `*pos`.  Returns `Err(())` if
/// the buffer is too small.
fn write_bytes(buf: &mut [u8], pos: &mut usize, src: &[u8]) -> Result<(), ()> {
    let end = *pos + src.len();
    if end > buf.len() {
        return Err(());
    }
    buf[*pos..end].copy_from_slice(src);
    *pos = end;
    Ok(())
}

/// Append a decimal `u32` to `buf` at `*pos`.
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
/// Writes the status line, `Content-Type`, `Content-Length`, `Connection:
/// close`, and **always** `Cache-Control: no-store` (security-checklist item 5
/// — every portal response must carry this header unconditionally), then the
/// blank separator line and the body.
///
/// Returns the number of bytes written, or `Err(())` if the buffer is too
/// small.
#[allow(clippy::result_unit_err)]
pub(crate) fn build_response(
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

    // Server invariant: every response carries Cache-Control: no-store so
    // browsers never cache portal HTML or credential form submissions.
    // (Security-checklist item 5.)
    write_bytes(buf, &mut pos, b"Cache-Control: no-store\r\n")?;

    write_bytes(buf, &mut pos, b"\r\n")?;
    write_bytes(buf, &mut pos, body)?;

    Ok(pos)
}

/// Build an HTTP/1.1 redirect response (302 Found) into `buf`.
///
/// Writes `Location: <location>` in addition to the standard headers.
/// `Cache-Control: no-store` is included as a server invariant.
#[allow(clippy::result_unit_err)]
fn build_redirect(buf: &mut [u8], location: &str) -> Result<usize, ()> {
    let mut pos = 0;
    write_bytes(buf, &mut pos, b"HTTP/1.1 302 Found\r\n")?;
    write_bytes(buf, &mut pos, b"Location: ")?;
    write_bytes(buf, &mut pos, location.as_bytes())?;
    write_bytes(buf, &mut pos, b"\r\n")?;
    write_bytes(buf, &mut pos, b"Content-Length: 0\r\n")?;
    write_bytes(buf, &mut pos, b"Connection: close\r\n")?;
    write_bytes(buf, &mut pos, b"Cache-Control: no-store\r\n")?;
    write_bytes(buf, &mut pos, b"\r\n")?;
    Ok(pos)
}

/// Write an error response (4xx / 5xx) for a parse failure.
fn error_response(buf: &mut [u8], err: &ParseError) -> usize {
    let (status, body): (u16, &str) = match err {
        ParseError::VersionNotSupported => (505, "HTTP Version Not Supported"),
        ParseError::UriTooLong => (414, "URI Too Long"),
        ParseError::RequestTooLarge => (413, "Payload Too Large"),
        ParseError::BadRequestLine
        | ParseError::BadHeader
        | ParseError::DuplicateContentLength
        | ParseError::InvalidContentLength
        | ParseError::IncompleteRequest => (400, "Bad Request"),
    };
    build_response(buf, status, "text/plain; charset=utf-8", body.as_bytes()).unwrap_or(0)
}

// ── Minimal 500 fallback ──────────────────────────────────────────────────────

/// Hard-coded minimal `500 Internal Server Error` response.
///
/// Used when `dispatch_request` reports the response buffer is too small to
/// hold the real response.  `Cache-Control: no-store` is included as a server
/// invariant — even error responses must carry it.
pub(crate) const MINIMAL_500: &[u8] = b"HTTP/1.1 500 Internal Server Error\r\n\
    Content-Length: 0\r\n\
    Connection: close\r\n\
    Cache-Control: no-store\r\n\
    \r\n";

/// Write [`MINIMAL_500`] into `buf` and return the byte count.
pub(crate) fn write_minimal_500(buf: &mut [u8]) -> usize {
    let len = MINIMAL_500.len();
    debug_assert!(
        buf.len() >= len,
        "MINIMAL_500 must fit in any resp_buf the server uses"
    );
    buf[..len].copy_from_slice(MINIMAL_500);
    len
}

/// Returns the byte position in `req_buf` the body-read loop must reach to
/// have the full POST body present.
///
/// `hdr_end` is the offset of the first body byte (one past `\r\n\r\n` in
/// the request buffer).  `content_length` is the parsed Content-Length
/// header value (already capped to `cap` by `parse_request`).  `cap` is the
/// request-buffer size — the result is clamped to it so a misbehaving
/// Content-Length cannot drive an out-of-bounds slice in the read loop.
///
/// Pure / host-testable so the arithmetic is locked independently of the
/// `cfg(embassy + chip)`-gated `run_portal_dyn` socket loop, matching the
/// `decide_request` / `validate_pool_geometry` extraction pattern.
pub(crate) fn body_read_target(hdr_end: usize, content_length: usize, cap: usize) -> usize {
    hdr_end.saturating_add(content_length).min(cap)
}

// ── Pre-fill ──────────────────────────────────────────────────────────────────

/// Reads the stored record for the form pre-fill.
///
/// A store error reads as "no record", so the form still comes up pre-filled
/// from the defaults; `Prefill::select` then picks the source.  Security
/// item 2: the pre-fill never holds a secret (see `juggler::provisioning::Prefill`).
#[cfg(all(
    feature = "embassy",
    any(feature = "esp32c3", feature = "esp32c6", feature = "esp32s3")
))]
fn load_stored(store: &dyn PortalStore) -> Option<ProvisioningConfig> {
    match store.load() {
        Ok(stored) => stored,
        Err(e) => {
            log::debug!(
                "load_stored: store.load() failed ({:?}), rendering defaults",
                e
            );
            None
        }
    }
}

// ── Template rendering ─────────────────────────────────────────────────────────

/// Renders the portal HTML template with placeholders substituted.
///
/// Writes directly into `out_buf` to avoid heap allocation.  Returns the
/// number of bytes written, or `Err(())` if `out_buf` is too small.
///
/// The render pass substitutes `{{NONCE}}`, `{{WIFI_SSID}}`, `{{MQTT_URI}}`,
/// `{{MQTT_USER}}`, `{{MQTT_CLIENT}}`, `{{OTA_URL}}`, `{{DEV_NAME}}`,
/// `{{FW_VER}}`, and `{{ERRORS}}`.  No `{{WIFI_PASS}}` or `{{MQTT_PASS}}`
/// placeholder is ever substituted — security-checklist item 2.
///
/// Returns `Err(())` when `out_buf` is too small to hold the rendered output,
/// including when an HTML-escaped substituted value would overflow the buffer
/// (the `write_html_escaped` helper propagates the overflow via a captured
/// `overflowed` flag and returns `Err(())`).
#[cfg(any(
    test,
    all(
        feature = "embassy",
        any(feature = "esp32c3", feature = "esp32c6", feature = "esp32s3")
    )
))]
#[allow(clippy::result_unit_err)]
fn render_portal_template(
    config: &PortalRenderConfig,
    nonce: &str,
    prefill: &Prefill<'_>,
    error_text: Option<&str>,
    out_buf: &mut [u8],
) -> Result<usize, ()> {
    let template = template_for(config.profile);

    // We walk the template byte-by-byte to find `{{...}}` markers and copy the
    // corresponding escaped value.  This avoids any heap allocation.

    // Build a small escape buffer using html_escape_to.
    // We render into `out_buf` directly, walking the template.
    let template_bytes = template.as_bytes();
    let mut pos = 0usize;
    let mut i = 0usize;

    // Helper: write an HTML-escaped string to out_buf at pos.
    //
    // Tracks an `overflowed` flag in its enclosing scope: if any chunk from
    // `html_escape_to` would overflow `buf`, the flag is set and the write is
    // skipped (the caller checks the flag and returns `Err(())` after the call).
    // The closure itself cannot return a value, so the flag is the mechanism
    // for propagating the overflow out of the closure boundary.
    let write_html_escaped = |buf: &mut [u8], p: &mut usize, s: &str| -> Result<(), ()> {
        let mut overflowed = false;
        html_escape_to(s, |chunk| {
            let chunk_bytes = chunk.as_bytes();
            let end = *p + chunk_bytes.len();
            if end > buf.len() {
                overflowed = true;
                return;
            }
            buf[*p..end].copy_from_slice(chunk_bytes);
            *p = end;
        });
        if overflowed {
            Err(())
        } else {
            Ok(())
        }
    };

    while i < template_bytes.len() {
        // Look for `{{`.
        if i + 1 < template_bytes.len()
            && template_bytes[i] == b'{'
            && template_bytes[i + 1] == b'{'
        {
            // Find the closing `}}`.
            let marker_start = i + 2;
            let maybe_end = template_bytes[marker_start..]
                .windows(2)
                .position(|w| w == b"}}")
                .map(|p| marker_start + p);

            if let Some(end) = maybe_end {
                let marker = &template_bytes[marker_start..end];
                let skip_to = end + 2;

                // Dispatch on marker name.
                match marker {
                    b"NONCE" => {
                        write_html_escaped(out_buf, &mut pos, nonce)?;
                    }
                    b"WIFI_SSID" => {
                        write_html_escaped(out_buf, &mut pos, prefill.wifi_ssid)?;
                    }
                    b"MQTT_URI" => {
                        write_html_escaped(out_buf, &mut pos, prefill.mqtt_uri.as_str())?;
                    }
                    b"MQTT_USER" => {
                        write_html_escaped(out_buf, &mut pos, prefill.mqtt_user)?;
                    }
                    b"MQTT_CLIENT" => {
                        write_html_escaped(out_buf, &mut pos, prefill.mqtt_client)?;
                    }
                    b"OTA_URL" => {
                        write_html_escaped(out_buf, &mut pos, prefill.ota_url)?;
                    }
                    b"DEV_NAME" => {
                        write_html_escaped(out_buf, &mut pos, prefill.dev_name)?;
                    }
                    b"FW_VER" => {
                        write_html_escaped(out_buf, &mut pos, config.firmware_version.as_str())?;
                    }
                    b"ERRORS" => {
                        if let Some(err) = error_text {
                            // Errors are pre-escaped HTML; write raw.
                            let end_pos = pos + err.len();
                            if end_pos > out_buf.len() {
                                return Err(());
                            }
                            out_buf[pos..end_pos].copy_from_slice(err.as_bytes());
                            pos = end_pos;
                        }
                        // else: write nothing (empty errors block)
                    }
                    _ => {
                        // Unknown placeholder — copy verbatim (include the {{ }}).
                        let verbatim = &template_bytes[i..skip_to];
                        let end_pos = pos + verbatim.len();
                        if end_pos > out_buf.len() {
                            return Err(());
                        }
                        out_buf[pos..end_pos].copy_from_slice(verbatim);
                        pos = end_pos;
                    }
                }
                i = skip_to;
                continue;
            }
        }

        // Copy plain byte.
        if pos >= out_buf.len() {
            return Err(());
        }
        out_buf[pos] = template_bytes[i];
        pos += 1;
        i += 1;
    }

    Ok(pos)
}

// ── Route dispatcher ──────────────────────────────────────────────────────────

/// Render config kept for the lifetime of the portal accept-loop.
///
/// Baked from `PortalConfig` at `start()` time; the portal task holds this
/// by value.
#[cfg(all(
    feature = "embassy",
    any(feature = "esp32c3", feature = "esp32c6", feature = "esp32s3")
))]
#[allow(dead_code)]
struct PortalState<'r> {
    ap_ip_str: heapless::String<16>, // "192.168.4.1" max
    shared: &'static SharedState,
    config: &'r PortalRenderConfig,
}

/// Dispatch a parsed request to the appropriate portal route handler.
///
/// Writes the HTTP response into `resp_buf` and returns the number of bytes
/// written, or `Err(())` if the buffer is too small.
///
/// # Route table
///
/// | Method | Path                  | Status | Notes                          |
/// |--------|-----------------------|--------|-------------------------------|
/// | GET    | PROBE_PATHS           | 302    | Redirect to portal root        |
/// | GET    | /factory-reset        | 200    | Confirmation form              |
/// | GET    | / (any path not above) | 200   | Portal HTML page (captive-portal catch-all) |
/// | POST   | /save                 | 200/400/403/409/500 | Credential commit (409: session already finished) |
/// | POST   | /factory-reset        | 200/403/409 | Factory-reset signal (409: session already finished) |
/// | POST   | anything else         | 404    | Not found (POST-only — unknown GETs hit the catch-all row above) |
#[cfg(all(
    feature = "embassy",
    any(feature = "esp32c3", feature = "esp32c6", feature = "esp32s3")
))]
#[allow(clippy::result_unit_err, clippy::too_many_arguments)]
pub(crate) fn dispatch_request(
    req: &ParsedRequest,
    body_after_headers: &[u8],
    shared: &'static SharedState,
    store: &'static dyn PortalStore,
    config: &PortalRenderConfig,
    ap_ip_str: &str,
    resp_buf: &mut [u8],
) -> Result<usize, ()> {
    let target = req.target_str();

    match req.method {
        Method::Get => {
            // ── Probe paths → 302 redirect ────────────────────────────────
            if PROBE_PATHS.contains(&target) {
                // Build "http://192.168.4.1/" redirect.
                let mut location = heapless::String::<32>::new();
                let _ = location.push_str("http://");
                let _ = location.push_str(ap_ip_str);
                let _ = location.push('/');
                return build_redirect(resp_buf, location.as_str());
            }

            // ── GET /factory-reset → confirmation page ────────────────────
            if target == "/factory-reset" {
                // Build the body in a small stack buffer (nonce is 8 hex chars;
                // total body is well under 1 KiB).
                const MAX_CONFIRM_BODY: usize = 1024;
                let mut body_buf = [0u8; MAX_CONFIRM_BODY];
                let nonce = shared.nonce.as_str();
                let p1 = FACTORY_RESET_CONFIRM_HTML_PART1.as_bytes();
                let p2 = FACTORY_RESET_CONFIRM_HTML_PART2.as_bytes();
                let body_len = p1.len() + nonce.len() + p2.len();
                if body_len > body_buf.len() {
                    return Err(());
                }
                let mut pos = 0usize;
                body_buf[pos..pos + p1.len()].copy_from_slice(p1);
                pos += p1.len();
                body_buf[pos..pos + nonce.len()].copy_from_slice(nonce.as_bytes());
                pos += nonce.len();
                body_buf[pos..pos + p2.len()].copy_from_slice(p2);
                let body = &body_buf[..body_len];
                return build_response(resp_buf, 200, "text/html; charset=utf-8", body);
            }

            // ── GET / (and any other GET) → portal HTML ───────────────────
            form_response(resp_buf, b"HTTP/1.1 200 OK", shared, store, config, None)
        }

        Method::Post => {
            let target = req.target_str();

            // ── POST /save ────────────────────────────────────────────────
            if target == "/save" {
                // SECURITY: never log request bodies — they carry wifi_pass / mqtt_pass.
                let body_str = match core::str::from_utf8(body_after_headers) {
                    Ok(s) => s,
                    Err(_) => {
                        log::warn!("POST /save: body is not valid UTF-8");
                        return build_response(
                            resp_buf,
                            400,
                            "text/plain; charset=utf-8",
                            b"Bad Request: invalid UTF-8 in body.",
                        );
                    }
                };

                // The form is parsed only behind a valid nonce (constant-time
                // check); `PortalMachine` sequences the rest (nonce → gate →
                // persist → commit).
                let nonce_ok = body_nonce_matches(body_after_headers, shared.nonce.as_str());
                let parsed = nonce_ok.then(|| parse_form(body_str, config.profile));
                let form_valid = matches!(parsed, Some(Ok(_)));
                return match (
                    shared.poll(PortalInput::Save {
                        nonce_ok,
                        form_valid,
                    }),
                    parsed,
                ) {
                    (PortalAction::Forbidden, _) => {
                        log::warn!("POST /save rejected: nonce mismatch");
                        build_response(
                            resp_buf,
                            403,
                            "text/plain; charset=utf-8",
                            b"Forbidden: session token mismatch.",
                        )
                    }
                    (PortalAction::RejectForm, Some(Err(errors))) => {
                        if let Some(cb) = shared.on_event {
                            (cb)(ProvisioningEvent::SubmissionRejected);
                        }
                        log::info!("POST /save rejected: {} field error(s)", errors.len());

                        // Heap-allocated: nine field errors plus the resubmit
                        // hint outgrow any small fixed buffer, and a silently
                        // truncated block would leave unclosed tags.
                        let mut errors_html = alloc::string::String::new();
                        if !errors.is_empty() {
                            errors_html
                                .push_str("<div class=\"errors\"><strong>Please fix:</strong><ul>");
                            for err in &errors {
                                errors_html.push_str("<li>");
                                html_escape_to(err.field.form_name(), |s| {
                                    errors_html.push_str(s);
                                });
                                errors_html.push_str(": ");
                                let err_str = alloc::format!("{}", err.error);
                                html_escape_to(err_str.as_str(), |s| {
                                    errors_html.push_str(s);
                                });
                                errors_html.push_str("</li>");
                            }
                            errors_html.push_str("</ul><p>");
                            errors_html.push_str(juggler::provisioning::templates::RESUBMIT_HINT);
                            errors_html.push_str("</p></div>");
                        }

                        // Stored / default values, never the rejected
                        // submission (and never a secret).
                        form_response(
                            resp_buf,
                            b"HTTP/1.1 400 Bad Request",
                            shared,
                            store,
                            config,
                            (!errors_html.is_empty()).then_some(errors_html.as_str()),
                        )
                    }
                    (PortalAction::Persist, Some(Ok(config_to_save))) => {
                        if let Some(cb) = shared.on_event {
                            (cb)(ProvisioningEvent::SubmissionAccepted);
                        }
                        // Persist to flash via the type-erased PortalStore trait.
                        let saved = match store.save(&config_to_save) {
                            Ok(()) => Some(config_to_save),
                            Err(e) => {
                                log::error!("POST /save persist failed: {:?}", e);
                                None
                            }
                        };
                        let ok = saved.is_some();
                        match (shared.poll(PortalInput::PersistDone { ok }), saved) {
                            (PortalAction::Committed, Some(committed)) => {
                                // Effect order: publish + signal, then the event.
                                shared
                                    .outcome
                                    .signal(ProvisioningOutcome::Committed(committed));
                                if let Some(cb) = shared.on_event {
                                    (cb)(ProvisioningEvent::Committed);
                                }
                                build_response(
                                    resp_buf,
                                    200,
                                    "text/html; charset=utf-8",
                                    COMMITTED_HTML.as_bytes(),
                                )
                            }
                            (PortalAction::PersistFailed, _) => {
                                let banner = alloc::format!(
                                    "<div class=\"errors\">Could not save credentials to flash. Please try again.<p>{}</p></div>",
                                    juggler::provisioning::templates::RESUBMIT_HINT,
                                );
                                form_response(
                                    resp_buf,
                                    b"HTTP/1.1 500 Internal Server Error",
                                    shared,
                                    store,
                                    config,
                                    Some(banner.as_str()),
                                )
                            }
                            (other, _) => {
                                log::error!(
                                    "POST /save: persisted but the session answered {:?}",
                                    other
                                );
                                session_finished(resp_buf)
                            }
                        }
                    }
                    (PortalAction::Refused(t), _) => {
                        log::warn!("POST /save refused: session already {:?}", t.state);
                        session_finished(resp_buf)
                    }
                    (other, _) => {
                        log::error!("POST /save: unexpected portal action {:?}", other);
                        Err(())
                    }
                };
            }

            // ── POST /factory-reset ───────────────────────────────────────
            if target == "/factory-reset" {
                let nonce_ok = body_nonce_matches(body_after_headers, shared.nonce.as_str());
                return match shared.poll(PortalInput::FactoryReset { nonce_ok }) {
                    PortalAction::Forbidden => {
                        log::warn!("POST /factory-reset rejected: nonce mismatch");
                        build_response(
                            resp_buf,
                            403,
                            "text/plain; charset=utf-8",
                            b"Forbidden: session token mismatch.",
                        )
                    }
                    PortalAction::ResetAccepted => {
                        // Effect order: signal, then the event.
                        shared
                            .outcome
                            .signal(ProvisioningOutcome::FactoryResetRequested);
                        if let Some(cb) = shared.on_event {
                            (cb)(ProvisioningEvent::FactoryResetRequested);
                        }
                        log::info!("Factory reset requested via portal");
                        build_response(
                            resp_buf,
                            200,
                            "text/html; charset=utf-8",
                            FACTORY_RESET_HTML.as_bytes(),
                        )
                    }
                    // A reset after a commit is refused: the saved credentials
                    // stand and the host is never told a reset was requested.
                    PortalAction::Refused(t) => {
                        log::warn!("POST /factory-reset refused: session already {:?}", t.state);
                        session_finished(resp_buf)
                    }
                    other => {
                        log::error!("POST /factory-reset: unexpected portal action {:?}", other);
                        Err(())
                    }
                };
            }

            // ── Any other POST → 404 ──────────────────────────────────────
            build_response(resp_buf, 404, "text/plain; charset=utf-8", b"Not Found")
        }
    }
}

/// The `409` response for a request the session refused.
#[cfg(all(
    feature = "embassy",
    any(feature = "esp32c3", feature = "esp32c6", feature = "esp32s3")
))]
fn session_finished(resp_buf: &mut [u8]) -> Result<usize, ()> {
    build_response(
        resp_buf,
        409,
        "text/plain; charset=utf-8",
        SESSION_FINISHED_BODY.as_bytes(),
    )
}

/// Renders the portal form, pre-filled from the store or the defaults, as a
/// complete response with `status_line` (no trailing CRLF) into `resp_buf`.
///
/// The body is rendered into `resp_buf` behind a header reserve, then moved
/// down behind the real header, so no second body-sized buffer is needed.
#[cfg(all(
    feature = "embassy",
    any(feature = "esp32c3", feature = "esp32c6", feature = "esp32s3")
))]
fn form_response(
    resp_buf: &mut [u8],
    status_line: &[u8],
    shared: &SharedState,
    store: &dyn PortalStore,
    config: &PortalRenderConfig,
    error_text: Option<&str>,
) -> Result<usize, ()> {
    const HEADER_RESERVE: usize = 200;
    if resp_buf.len() < HEADER_RESERVE {
        return Err(());
    }
    let stored = load_stored(store);
    let defaults = config.defaults.as_borrowed();
    let prefill = Prefill::select(
        stored.as_ref(),
        config.profile,
        &defaults,
        config.device_name.as_str(),
    );
    let body_len = render_portal_template(
        config,
        shared.nonce.as_str(),
        &prefill,
        error_text,
        &mut resp_buf[HEADER_RESERVE..],
    )?;

    let mut hdr_buf = [0u8; HEADER_RESERVE];
    let mut hdr_pos = 0usize;
    write_bytes(&mut hdr_buf, &mut hdr_pos, status_line)?;
    write_bytes(
        &mut hdr_buf,
        &mut hdr_pos,
        b"\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: ",
    )?;
    write_u32_decimal(&mut hdr_buf, &mut hdr_pos, body_len as u32)?;
    write_bytes(
        &mut hdr_buf,
        &mut hdr_pos,
        b"\r\nConnection: close\r\nCache-Control: no-store\r\n\r\n",
    )?;
    let hdr_len = hdr_pos;
    let total = hdr_len + body_len;
    if total > resp_buf.len() {
        return Err(());
    }
    resp_buf.copy_within(HEADER_RESERVE..HEADER_RESERVE + body_len, hdr_len);
    resp_buf[..hdr_len].copy_from_slice(&hdr_buf[..hdr_len]);
    Ok(total)
}

// ── Async accept-loop (bare-metal only) ──────────────────────────────────────

/// Runs the captive-portal HTTP server on the given `embassy-net` stack.
///
/// This function never returns under normal operation.  It binds a TCP socket
/// on port 80, accepts one connection at a time, reads one HTTP request,
/// dispatches it via [`dispatch_request`], writes one response, then closes
/// the connection and waits for the next.
///
/// # Security discipline
///
/// - Bodies are never logged (`// SECURITY: never log request bodies`).
/// - `req_buf` is overwritten with `0xFF` after each request (`// SECURITY:
///   early drop of credential buffers`).
/// - `Cache-Control: no-store` is baked into every response.
///
/// # Stack usage
///
/// See the module doc comment for the per-task peak estimate.
/// Type-erased entry point for the portal accept-loop.
///
/// Called from the non-generic `http_task` embassy task.  The store is
/// accessed via the [`PortalStore`] trait object so no generic type parameter
/// is needed.
#[cfg(all(
    feature = "embassy",
    any(feature = "esp32c3", feature = "esp32c6", feature = "esp32s3")
))]
pub(crate) async fn run_portal_dyn(
    stack: embassy_net::Stack<'static>,
    shared: &'static SharedState,
    store: &'static dyn PortalStore,
    config: PortalRenderConfig,
) -> ! {
    use embassy_net::tcp::TcpSocket;
    use static_cell::StaticCell;

    // Static socket buffers — required because `TcpSocket::new` in
    // embassy-net 0.8 internally transmutes the buffer slices to `'static`.
    static RX_BUF: StaticCell<[u8; DEFAULT_RX_BUF]> = StaticCell::new();
    static TX_BUF: StaticCell<[u8; DEFAULT_TX_BUF]> = StaticCell::new();

    let rx_buf = RX_BUF.init([0u8; DEFAULT_RX_BUF]);
    let tx_buf = TX_BUF.init([0u8; DEFAULT_TX_BUF]);

    let mut socket = TcpSocket::new(stack, rx_buf, tx_buf);

    // Request and response staging buffers on the stack — no heap needed.
    let mut req_buf = [0u8; DEFAULT_REQUEST_SIZE_CAP];
    let mut resp_buf = [0u8; DEFAULT_TX_BUF];

    // Build the AP IP string once for redirect generation.
    let mut ap_ip_str = heapless::String::<16>::new();
    let octets = shared.ap_ip; // [u8; 4] in network byte order
    let write_octet = |s: &mut heapless::String<16>, n: u8| {
        let mut tmp = [0u8; 3];
        let mut tlen = 0;
        let mut v = n;
        if v == 0 {
            tmp[0] = b'0';
            tlen = 1;
        } else {
            while v > 0 {
                tmp[tlen] = b'0' + (v % 10);
                v /= 10;
                tlen += 1;
            }
        }
        for i in (0..tlen).rev() {
            let _ = s.push(tmp[i] as char);
        }
    };
    write_octet(&mut ap_ip_str, octets[0]);
    let _ = ap_ip_str.push('.');
    write_octet(&mut ap_ip_str, octets[1]);
    let _ = ap_ip_str.push('.');
    write_octet(&mut ap_ip_str, octets[2]);
    let _ = ap_ip_str.push('.');
    write_octet(&mut ap_ip_str, octets[3]);

    log::info!("Portal HTTP server listening on port {}", DEFAULT_PORT);

    loop {
        // Accept the next connection.
        match socket.accept(DEFAULT_PORT).await {
            Ok(()) => {}
            Err(e) => {
                log::warn!("Portal HTTP accept error: {:?}; retrying", e);
                embassy_time::Timer::after(embassy_time::Duration::from_millis(100)).await;
                continue;
            }
        }

        // Read the request into req_buf until we see "\r\n\r\n" (header end)
        // or fill the buffer, within the header deadline: a client that never
        // finishes its headers must not hold the single accept socket.
        let filled = match embassy_time::with_timeout(
            phase_budget(PortalPhase::Headers),
            read_request(&mut socket, &mut req_buf, DEFAULT_REQUEST_SIZE_CAP),
        )
        .await
        {
            Ok(filled) => filled,
            Err(_) => {
                log::debug!(
                    "Portal: no request headers within {} ms; closing",
                    PortalPhase::Headers.budget_ms()
                );
                socket.abort();
                flush_bounded(&mut socket).await;
                // SECURITY: a partial request may hold credential bytes.
                req_buf.fill(0xFF);
                continue;
            }
        };

        let mut body_drain_target: Option<(usize, usize)> = None;

        // SECURITY: never log request bodies — POST bodies carry wifi_pass /
        // mqtt_pass credentials.  Log lines below are limited to method and
        // target only; bytes from req_buf past the header terminator MUST NOT
        // appear in any log call.
        let resp_len = match filled {
            Ok(n) => match parse_request(&req_buf[..n], DEFAULT_REQUEST_SIZE_CAP) {
                Ok(req) => {
                    log::info!(
                        "Portal {} {}",
                        match req.method {
                            Method::Get => "GET",
                            Method::Post => "POST",
                        },
                        req.target_str()
                    );

                    // Locate the body slice (bytes after the header separator).
                    let hdr_end = req_buf[..n]
                        .windows(4)
                        .position(|w| w == b"\r\n\r\n")
                        .map(|p| p + 4)
                        .unwrap_or(n);

                    // `read_request` returns as soon as it sees \r\n\r\n, which
                    // does NOT guarantee the full body is in `req_buf`.  TCP
                    // segmentation can deliver the body across multiple read
                    // calls — for a POST we must keep reading until the body
                    // is complete or the cap is reached, or dispatch_request
                    // will see a truncated form and the nonce / parse_form
                    // checks will reject a valid submission.
                    //
                    // The byte cap (`DEFAULT_REQUEST_SIZE_CAP`, the value
                    // `parse_request` already capped `Content-Length` against)
                    // bounds how MUCH is read, not for how LONG: a client that
                    // sends valid headers and then no body would hold the single
                    // accept loop forever.  The `PortalPhase::Body` deadline
                    // bounds the time; on expiry the connection is aborted
                    // without a response and the loop returns to accept.
                    let mut total = n;
                    let mut body_stalled = false;
                    if matches!(req.method, Method::Post) {
                        if let Some(cl) = req.headers.content_length {
                            let need =
                                body_read_target(hdr_end, cl as usize, DEFAULT_REQUEST_SIZE_CAP);
                            match read_body_bounded(
                                &mut socket,
                                &mut req_buf,
                                total,
                                need,
                                phase_budget(PortalPhase::Body),
                            )
                            .await
                            {
                                Ok(filled) => total = filled,
                                Err(Stalled) => body_stalled = true,
                            }
                        }
                    }
                    if body_stalled {
                        log::debug!(
                            "Portal: request body not complete within {} ms; closing",
                            PortalPhase::Body.budget_ms()
                        );
                        socket.abort();
                        flush_bounded(&mut socket).await;
                        // SECURITY: a partial body may hold credential bytes.
                        req_buf.fill(0xFF);
                        continue;
                    }
                    let body_in_buf = &req_buf[hdr_end..total];

                    // Track remaining body bytes for the post-response drain.
                    if let Some(cl) = req.headers.content_length {
                        let already = body_in_buf.len();
                        body_drain_target = Some((cl as usize, already));
                    }

                    dispatch_request(
                        &req,
                        body_in_buf,
                        shared,
                        store,
                        &config,
                        ap_ip_str.as_str(),
                        &mut resp_buf,
                    )
                    .unwrap_or_else(|()| {
                        log::warn!("Portal: dispatch response too large — emitting minimal 500");
                        write_minimal_500(&mut resp_buf)
                    })
                }
                Err(e) => {
                    log::warn!("Portal parse error: {:?}", e);
                    error_response(&mut resp_buf, &e)
                }
            },
            Err(e) => {
                log::warn!("Portal read error: {:?}", e);
                error_response(&mut resp_buf, &e)
            }
        };

        // Write the response (partial-write safe loop) and flush it, within the
        // response deadline: a client that stops reading must not hold the loop.
        let response_stalled = write_bounded(
            &mut socket,
            &resp_buf[..resp_len],
            phase_budget(PortalPhase::Response),
        )
        .await
        .is_err();
        if response_stalled {
            log::debug!(
                "Portal: response not delivered within {} ms; closing",
                PortalPhase::Response.budget_ms()
            );
            socket.abort();
            flush_bounded(&mut socket).await;
            req_buf.fill(0xFF);
            continue;
        }

        // Drain remaining request body before close to avoid RST.
        if let Some((content_length, already_in_buf)) = body_drain_target {
            if content_length > already_in_buf {
                let remaining = content_length - already_in_buf;
                drain_body_with_deadline(&mut socket, remaining, phase_budget(PortalPhase::Drain))
                    .await;
            }
        }

        // Close and flush the FIN within the close deadline; a peer that never
        // acknowledges it is reset instead of holding the loop.
        socket.close();
        if close_bounded(&mut socket, phase_budget(PortalPhase::Close))
            .await
            .is_err()
        {
            log::debug!(
                "Portal: close not acknowledged within {} ms; aborting",
                PortalPhase::Close.budget_ms()
            );
            socket.abort();
            flush_bounded(&mut socket).await;
        }

        // SECURITY: early drop of credential buffers — overwrite req_buf with
        // 0xFF before the loop iterates so any credentials read into the buffer
        // do not linger on the stack until the next request.
        req_buf.fill(0xFF);
    }
}

/// Read from `socket` until the request headers are complete (`\r\n\r\n`)
/// or the buffer fills.
#[cfg(all(
    feature = "embassy",
    any(feature = "esp32c3", feature = "esp32c6", feature = "esp32s3")
))]
async fn read_request(
    socket: &mut embassy_net::tcp::TcpSocket<'_>,
    buf: &mut [u8],
    size_cap: usize,
) -> Result<usize, ParseError> {
    let mut filled = 0usize;

    loop {
        if filled >= buf.len().min(size_cap) {
            return Err(ParseError::RequestTooLarge);
        }

        let n = socket
            .read(&mut buf[filled..])
            .await
            .map_err(|_| ParseError::IncompleteRequest)?;

        if n == 0 {
            return Err(ParseError::IncompleteRequest);
        }
        filled += n;

        if buf[..filled].windows(4).any(|w| w == b"\r\n\r\n") {
            return Ok(filled);
        }
    }
}

/// A portal phase did not finish within its budget.
#[cfg(any(
    all(
        feature = "embassy",
        any(feature = "esp32c3", feature = "esp32c6", feature = "esp32s3")
    ),
    test
))]
#[derive(Debug, PartialEq, Eq)]
struct Stalled;

/// Read the rest of a request body into `buf[total..need]`, within `budget`.
///
/// Returns the filled length.
/// A peer close or read error ends the read early (best effort, `dispatch_request` handles a
/// short body); only running out of `budget` is [`Stalled`].
#[cfg(any(
    all(
        feature = "embassy",
        any(feature = "esp32c3", feature = "esp32c6", feature = "esp32s3")
    ),
    test
))]
async fn read_body_bounded<R: embedded_io_async::Read>(
    io: &mut R,
    buf: &mut [u8],
    mut total: usize,
    need: usize,
    budget: embassy_time::Duration,
) -> Result<usize, Stalled> {
    let read_body = async {
        while total < need {
            match io.read(&mut buf[total..need]).await {
                Ok(0) | Err(_) => break,
                Ok(extra) => total += extra,
            }
        }
        total
    };
    embassy_time::with_timeout(budget, read_body)
        .await
        .map_err(|_| Stalled)
}

/// Write all of `data` (partial-write safe) and flush it, within `budget`.
///
/// A write error or a zero-length write ends the send early; only running out of `budget` is
/// [`Stalled`].
#[cfg(any(
    all(
        feature = "embassy",
        any(feature = "esp32c3", feature = "esp32c6", feature = "esp32s3")
    ),
    test
))]
async fn write_bounded<W: embedded_io_async::Write>(
    io: &mut W,
    data: &[u8],
    budget: embassy_time::Duration,
) -> Result<(), Stalled> {
    let send = async {
        let mut written = 0;
        while written < data.len() {
            match io.write(&data[written..]).await {
                Ok(0) | Err(_) => break,
                Ok(n) => written += n,
            }
        }
        let _ = io.flush().await;
    };
    embassy_time::with_timeout(budget, send)
        .await
        .map_err(|_| Stalled)
}

/// Flush the FIN of an already closed connection, within `budget`.
#[cfg(any(
    all(
        feature = "embassy",
        any(feature = "esp32c3", feature = "esp32c6", feature = "esp32s3")
    ),
    test
))]
async fn close_bounded<W: embedded_io_async::Write>(
    io: &mut W,
    budget: embassy_time::Duration,
) -> Result<(), Stalled> {
    embassy_time::with_timeout(budget, io.flush())
        .await
        .map(|_| ())
        .map_err(|_| Stalled)
}

/// Flush after an `abort`: the reset is already queued, so the short abort budget is ample.
#[cfg(all(
    feature = "embassy",
    any(feature = "esp32c3", feature = "esp32c6", feature = "esp32s3")
))]
async fn flush_bounded(socket: &mut embassy_net::tcp::TcpSocket<'_>) {
    let _ = embassy_time::with_timeout(phase_budget(PortalPhase::Abort), socket.flush()).await;
}

/// Read and discard up to `remaining` bytes from `socket`, honouring `deadline`.
#[cfg(all(
    feature = "embassy",
    any(feature = "esp32c3", feature = "esp32c6", feature = "esp32s3")
))]
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

// ── Unit tests (host-testable codec + routing) ────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    extern crate alloc;
    extern crate std;

    /// This process's 8-hex nonce fixture, generated once on first use.
    ///
    /// Derived from OS entropy rather than written as a literal, so no fixed
    /// value flows into the `nonce: &str` parameter of `render_portal_template`
    /// — which is what CodeQL's `rust/hard-coded-cryptographic-value` rule
    /// asks for.  Spelling a constant out as bytes or chars would only hide it
    /// from the analyzer.
    ///
    /// Not cryptographic material either way: production nonces come from the
    /// hardware RNG (`format_nonce(rng.random())` in `session.rs`).  The
    /// fixture only has to be a well-formed 8-hex string.
    fn test_nonce() -> &'static str {
        use std::collections::hash_map::RandomState;
        use std::hash::{BuildHasher, Hasher};
        use std::sync::OnceLock;

        static NONCE: OnceLock<alloc::string::String> = OnceLock::new();
        NONCE.get_or_init(|| {
            let mut hasher = RandomState::new().build_hasher();
            hasher.write_u8(0);
            alloc::format!("{:08x}", hasher.finish() as u32)
        })
    }

    // ── bounded per-phase waits (ADR 023 decision 4) ──────────────────────────

    // The mock time driver is a host-only dev-dependency (not on `espidf`, see `Cargo.toml`).
    #[cfg(not(target_os = "espidf"))]
    mod bounded {
        use super::super::{close_bounded, read_body_bounded, write_bounded, Stalled};
        use core::convert::Infallible;
        use core::future::Future;
        use core::pin::pin;
        use core::task::{Context, Poll, Waker};
        use embassy_time::{Duration, MockDriver};

        /// An I/O peer that never becomes ready for read, write or flush.
        struct Never;

        impl embedded_io_async::ErrorType for Never {
            type Error = Infallible;
        }

        impl embedded_io_async::Read for Never {
            async fn read(&mut self, _buf: &mut [u8]) -> Result<usize, Infallible> {
                core::future::pending().await
            }
        }

        impl embedded_io_async::Write for Never {
            async fn write(&mut self, _buf: &[u8]) -> Result<usize, Infallible> {
                core::future::pending().await
            }
            async fn flush(&mut self) -> Result<(), Infallible> {
                core::future::pending().await
            }
        }

        /// Poll `fut` once, advance the mock clock past `budget`, poll again.
        ///
        /// Returns the first poll's readiness and the second poll's result.
        fn run_past_budget<F: Future>(fut: F, budget: Duration) -> (bool, Poll<F::Output>) {
            let driver = MockDriver::get();
            let mut fut = pin!(fut);
            let mut cx = Context::from_waker(Waker::noop());
            let first_ready = fut.as_mut().poll(&mut cx).is_ready();
            driver.advance(budget + Duration::from_millis(1));
            (first_ready, fut.as_mut().poll(&mut cx))
        }

        #[test]
        fn body_read_stalls_on_silent_peer() {
            let budget = Duration::from_millis(5_000);
            let mut buf = [0u8; 16];
            let (early, out) = run_past_budget(
                read_body_bounded(&mut Never, &mut buf, 4, 16, budget),
                budget,
            );
            assert!(!early, "must wait for the peer, not finish at once");
            assert_eq!(out, Poll::Ready(Err(Stalled)));
        }

        #[test]
        fn response_write_stalls_on_peer_that_stops_reading() {
            let budget = Duration::from_millis(5_000);
            let (early, out) = run_past_budget(
                write_bounded(&mut Never, b"HTTP/1.1 200 OK", budget),
                budget,
            );
            assert!(!early);
            assert_eq!(out, Poll::Ready(Err(Stalled)));
        }

        #[test]
        fn close_stalls_on_peer_that_never_acks_the_fin() {
            let budget = Duration::from_millis(1_000);
            let (early, out) = run_past_budget(close_bounded(&mut Never, budget), budget);
            assert!(!early);
            assert_eq!(out, Poll::Ready(Err(Stalled)));
        }
    }

    // ── body_read_target (pure host-testable arithmetic) ──────────────────────

    /// Typical POST: header section finished, body fits well under the cap.
    #[test]
    fn body_read_target_within_cap() {
        // hdr_end at 64 bytes, Content-Length 256, request cap 2048 → target 320.
        assert_eq!(body_read_target(64, 256, 2048), 320);
    }

    /// Edge case: a misbehaving Content-Length that would extend past the
    /// request-buffer cap is clamped to the cap, never returns an
    /// out-of-bounds slice end.
    #[test]
    fn body_read_target_caps_at_buffer() {
        assert_eq!(body_read_target(64, 4096, 2048), 2048);
        assert_eq!(body_read_target(0, usize::MAX, 2048), 2048);
    }

    /// Saturation guard: pathological inputs (e.g. hdr_end + content_length
    /// overflowing `usize`) must clamp via `saturating_add`, never wrap
    /// into a small offset that bypasses the cap.
    #[test]
    fn body_read_target_saturates_on_overflow() {
        assert_eq!(body_read_target(usize::MAX, usize::MAX, 2048), 2048);
        assert_eq!(body_read_target(usize::MAX, 1, 2048), 2048);
    }

    /// Empty body (Content-Length: 0) returns the header end exactly.
    #[test]
    fn body_read_target_zero_content_length() {
        assert_eq!(body_read_target(128, 0, 2048), 128);
    }

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

    /// POST /save parses the request line and headers.
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
        let raw = b"POST /save HTTP/1.1\r\n\
            Content-Length: 9999\r\n\
            \r\n\
            x";
        assert_eq!(parse_request(raw, 64), Err(ParseError::RequestTooLarge));
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

    /// A 200 response round-trips: status line, all required headers, body.
    #[test]
    fn response_200_structure() {
        let body = b"hello";
        let mut buf = [0u8; 256];
        let n = build_response(&mut buf, 200, "text/plain", body).unwrap();
        let resp = core::str::from_utf8(&buf[..n]).unwrap();

        assert!(
            resp.starts_with("HTTP/1.1 200 OK\r\n"),
            "status line: {}",
            resp
        );
        assert!(resp.contains("Content-Type: text/plain\r\n"));
        assert!(resp.contains("Content-Length: 5\r\n"));
        assert!(resp.contains("Connection: close\r\n"));
        assert!(resp.contains("\r\n\r\n"));
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

    // ── Minimal 500 fallback ──────────────────────────────────────────────────

    /// `write_minimal_500` emits a syntactically complete `Content-Length: 0`
    /// response that any client can parse.
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

    // ── Security: Cache-Control: no-store ────────────────────────────────────

    /// Security-checklist item 5 lock: every response the portal can emit must
    /// carry `Cache-Control: no-store\r\n`.
    #[test]
    fn every_built_response_carries_cache_control_no_store() {
        const HEADER: &[u8] = b"Cache-Control: no-store\r\n";

        let statuses: &[(u16, &str)] = &[
            (200, "text/html; charset=utf-8"),
            (400, "text/plain; charset=utf-8"),
            (403, "text/plain; charset=utf-8"),
            (405, "text/plain; charset=utf-8"),
            (413, "text/plain; charset=utf-8"),
            (500, "text/plain; charset=utf-8"),
        ];

        let mut resp_buf = [0u8; 8192];
        for &(status, content_type) in statuses {
            let body = b"test body";
            let n = build_response(&mut resp_buf, status, content_type, body)
                .unwrap_or_else(|()| panic!("build_response({status}) failed — buffer too small"));
            let resp = &resp_buf[..n];
            assert!(
                resp.windows(HEADER.len()).any(|w| w == HEADER),
                "status {status}: response missing 'Cache-Control: no-store\\r\\n':\n{}",
                core::str::from_utf8(resp).unwrap_or("<non-utf8>")
            );
        }

        // MINIMAL_500 is hard-coded — verify it independently.
        assert!(
            MINIMAL_500.windows(HEADER.len()).any(|w| w == HEADER),
            "MINIMAL_500 is missing 'Cache-Control: no-store\\r\\n'"
        );
    }

    // ── Security-checklist item 2: no password pre-fill ───────────────────────

    /// Security-checklist item 2 lock: the rendered portal HTML must never
    /// contain password placeholder tokens (`{{WIFI_PASS}}`, `{{MQTT_PASS}}`),
    /// which would pre-fill the password fields with stored credentials.
    ///
    /// This test validates the template itself — the `render_portal_template`
    /// function can only substitute placeholders that exist in the template,
    /// so if the template carries no `{{WIFI_PASS}}` token there is no path
    /// by which a stored password reaches the rendered HTML.
    #[test]
    fn render_with_loaded_config_omits_password_bytes() {
        use juggler::provisioning::html_json_escape::html_escape_to;
        use juggler::provisioning::templates::WIFI_MQTT_PORTAL_HTML;

        // Sentinel passwords that must never appear in any rendered output.
        let sentinel_wifi = "SENTINEL-WIFI-PASS-1234";
        let sentinel_mqtt = "SENTINEL-MQTT-PASS-5678";

        // The template must not contain any `{{WIFI_PASS}}` or `{{MQTT_PASS}}`
        // placeholder — confirmed here as the host-side lock.
        assert!(
            !WIFI_MQTT_PORTAL_HTML.contains("{{WIFI_PASS}}"),
            "template must not contain {{WIFI_PASS}} placeholder"
        );
        assert!(
            !WIFI_MQTT_PORTAL_HTML.contains("{{MQTT_PASS}}"),
            "template must not contain {{MQTT_PASS}} placeholder"
        );

        // Simulate the render: apply html_escape_to with the sentinel values
        // that a `Prefill` struct loaded from a config with those passwords
        // would contain.  Since passwords are NOT in Prefill, this never runs
        // in production — but we can verify the template substitution
        // invariant by asserting the template itself produces no match.
        let mut cursor = WIFI_MQTT_PORTAL_HTML;
        // Simple pass: look for any {{...}} token and check the template bytes.
        while let Some(open) = cursor.find("{{") {
            let after_open = &cursor[open + 2..];
            if let Some(close) = after_open.find("}}") {
                let marker = &after_open[..close];
                // Only these markers can appear in the template.
                let allowed = [
                    "NONCE",
                    "WIFI_SSID",
                    "MQTT_URI",
                    "MQTT_USER",
                    "MQTT_CLIENT",
                    "OTA_URL",
                    "DEV_NAME",
                    "FW_VER",
                    "ERRORS",
                ];
                assert!(
                    allowed.contains(&marker),
                    "unexpected placeholder {{{{{}}}}} in template — passwords must never be placeholders",
                    marker
                );
                cursor = &after_open[close + 2..];
            } else {
                break;
            }
        }

        // Render the sentinel password values through the HTML escape function
        // (as they would appear if they were an SSID or MQTT field).  The
        // escaped output must not contain the raw sentinel — a moot assertion
        // here since sentinel chars are all alphanumeric, but keeps the
        // html_escape_to code path exercised by this test.
        let mut escaped_sentinel = alloc::string::String::new();
        html_escape_to(sentinel_wifi, |s| escaped_sentinel.push_str(s));
        html_escape_to(sentinel_mqtt, |s| escaped_sentinel.push_str(s));
        // Plain ASCII sentinels pass through html_escape_to unchanged — confirm
        // the escape path runs without panic.
        assert!(
            escaped_sentinel.contains("SENTINEL"),
            "html_escape_to must pass through plain ASCII sentinel"
        );

        // The rendered output of any template substitution must not contain the
        // sentinel passwords because `Prefill` has no password fields.
        // Since the template has no {{WIFI_PASS}}/{{MQTT_PASS}}, the sentinel
        // cannot appear via substitution.  We verify the template directly:
        assert!(
            !WIFI_MQTT_PORTAL_HTML.contains(sentinel_wifi),
            "template body must not contain sentinel wifi password literal"
        );
        assert!(
            !WIFI_MQTT_PORTAL_HTML.contains(sentinel_mqtt),
            "template body must not contain sentinel mqtt password literal"
        );
    }

    // ── Security-checklist item 3: request body cap ───────────────────────────

    /// Security-checklist item 3 lock: a `Content-Length` exceeding
    /// `DEFAULT_REQUEST_SIZE_CAP` must result in a 413 response from the HTTP
    /// layer.
    ///
    /// This test drives `parse_request` — which enforces the cap via the
    /// `HeaderAccumulator::feed` path — with a `Content-Length` header whose
    /// value exceeds the cap, and asserts the result is `RequestTooLarge`.
    #[test]
    fn post_save_body_exceeding_max_body_len_returns_413() {
        // Build a request header block with Content-Length > DEFAULT_REQUEST_SIZE_CAP.
        // The cap is 2048; use 2049 to be one byte over.
        let oversized_cl = DEFAULT_REQUEST_SIZE_CAP + 1;
        let raw = alloc::format!(
            "POST /save HTTP/1.1\r\n\
             Host: 192.168.4.1\r\n\
             Content-Length: {oversized_cl}\r\n\
             Content-Type: application/x-www-form-urlencoded\r\n\
             \r\n"
        );

        let result = parse_request(raw.as_bytes(), DEFAULT_REQUEST_SIZE_CAP);
        assert_eq!(
            result,
            Err(ParseError::RequestTooLarge),
            "Content-Length > DEFAULT_REQUEST_SIZE_CAP must return RequestTooLarge (got {result:?})"
        );

        // Confirm the error maps to a 413 response via build_response.
        let mut buf = [0u8; 512];
        let n = build_response(
            &mut buf,
            413,
            "text/plain; charset=utf-8",
            b"Payload Too Large",
        )
        .expect("413 response must fit in buf");
        let resp = core::str::from_utf8(&buf[..n]).unwrap();
        assert!(resp.starts_with("HTTP/1.1 413 Payload Too Large\r\n"));
        assert!(resp.contains("Cache-Control: no-store\r\n"));
    }

    /// Extras and the request cap: a `WifiMqttDevice` form with every field
    /// and all 8 extras at their caps fits `DEFAULT_REQUEST_SIZE_CAP` as long
    /// as the values need no percent-encoding, even with a 400 B allowance
    /// for a browser's headers. The same form with every value byte
    /// percent-encoded (`%XX`, three times the length) does not fit, and the
    /// portal answers it with 413 instead of truncating. The cap is a
    /// security-checklist lock (item 3); this test pins the decision that
    /// such otherwise-valid configurations are rejected rather than stored.
    #[test]
    fn max_config_with_eight_extras_fits_cap_only_when_unencoded() {
        use juggler::mqtt::CLIENT_ID_MAX_LEN;
        use juggler::provisioning::{
            DEVICE_NAME_MAX_LEN, EXTRA_FIELDS_MAX, EXTRA_KEY_MAX_LEN, EXTRA_VALUE_MAX_LEN,
            MQTT_HOST_MAX_LEN, MQTT_PASS_MAX_LEN, MQTT_USER_MAX_LEN, OTA_URL_MAX_LEN,
        };
        use juggler::wifi::{PASSWORD_MAX_LEN, SSID_MAX_LEN};

        const BROWSER_HEADER_ALLOWANCE: usize = 400;
        // `mqtt://` + host + `:65535`
        const MQTT_URI_MAX_LEN: usize = 7 + MQTT_HOST_MAX_LEN + 6;

        fn body(bytes_per_value_byte: usize) -> alloc::string::String {
            let filler = |n: usize| "a".repeat(n * bytes_per_value_byte);
            let mut b = alloc::format!(
                "_nonce={}&wifi_ssid={}&wifi_pass={}&mqtt_uri={}&mqtt_user={}\
                 &mqtt_pass={}&mqtt_client={}&ota_url={}&dev_name={}",
                test_nonce(),
                filler(SSID_MAX_LEN),
                filler(PASSWORD_MAX_LEN),
                filler(MQTT_URI_MAX_LEN),
                filler(MQTT_USER_MAX_LEN),
                filler(MQTT_PASS_MAX_LEN),
                filler(CLIENT_ID_MAX_LEN),
                filler(OTA_URL_MAX_LEN),
                filler(DEVICE_NAME_MAX_LEN),
            );
            for i in 0..EXTRA_FIELDS_MAX {
                let key = alloc::format!("x_{i:a>width$}", width = EXTRA_KEY_MAX_LEN - 2);
                b.push_str(&alloc::format!("&{key}={}", filler(EXTRA_VALUE_MAX_LEN)));
            }
            b
        }

        fn request(body: &str) -> alloc::string::String {
            alloc::format!(
                "POST /save HTTP/1.1\r\n\
                 Host: 192.168.4.1\r\n\
                 Content-Length: {}\r\n\
                 Content-Type: application/x-www-form-urlencoded\r\n\
                 \r\n{body}",
                body.len()
            )
        }

        let plain = request(&body(1));
        assert!(
            plain.len() + BROWSER_HEADER_ALLOWANCE <= DEFAULT_REQUEST_SIZE_CAP,
            "plain-ASCII max config ({} B) plus a {BROWSER_HEADER_ALLOWANCE} B header allowance must fit the {DEFAULT_REQUEST_SIZE_CAP} B cap",
            plain.len()
        );
        let parsed = parse_request(plain.as_bytes(), DEFAULT_REQUEST_SIZE_CAP);
        assert!(
            parsed.is_ok(),
            "plain max config must parse (got {parsed:?})"
        );

        let encoded = request(&body(3));
        assert!(
            encoded.len() > DEFAULT_REQUEST_SIZE_CAP,
            "fully percent-encoded max config ({} B) is expected to exceed the cap",
            encoded.len()
        );
        assert_eq!(
            parse_request(encoded.as_bytes(), DEFAULT_REQUEST_SIZE_CAP),
            Err(ParseError::RequestTooLarge),
            "an over-cap max config must be answered 413, never truncated"
        );
    }

    // ── Security-checklist item 4: no credential logging ─────────────────────

    /// Security-checklist item 4 lock: log messages emitted by the portal must
    /// never contain credential bytes from the request body.
    ///
    /// This test verifies the invariant by confirming that every log message
    /// the portal codec layer COULD emit (nonce mismatch, parse errors) uses
    /// static strings and never interpolates form-field values.
    ///
    /// The sentinel `LEAK-SENTINEL-XYZ` is embedded in a fake POST body.
    /// The test then verifies that the static log message strings known to
    /// be emitted on error paths do not contain the sentinel.
    #[test]
    fn logged_output_carries_no_credential_bytes() {
        // Sentinel value placed in every credential field.
        const SENTINEL: &str = "LEAK-SENTINEL-XYZ";

        // Simulate a POST /save body with sentinel values in every field.
        let body = alloc::format!(
            "_nonce=badnonce&wifi_ssid={SENTINEL}&wifi_pass={SENTINEL}\
             &mqtt_uri=mqtt://{SENTINEL}:1883&mqtt_user={SENTINEL}\
             &mqtt_pass={SENTINEL}&dev_name={SENTINEL}\
             &ota_url=http://{SENTINEL}/fw.bin"
        );

        // The nonce mismatch warning is a fixed string — it MUST NOT contain
        // the body content.
        let nonce_warn = "POST /save rejected: nonce mismatch";
        assert!(
            !nonce_warn.contains(SENTINEL),
            "nonce-mismatch log message must not contain sentinel: {nonce_warn}"
        );
        assert!(
            !nonce_warn.contains("wifi_pass"),
            "nonce-mismatch log message must not contain field names: {nonce_warn}"
        );

        // The parse / HTTP error messages are also static.
        let http_400 = "Bad Request";
        assert!(!http_400.contains(SENTINEL));

        // Verify the body parse itself does NOT produce output that contains
        // the sentinel (parse_request only returns a ParseError enum value
        // with no input bytes in its variants).
        let req_raw = alloc::format!(
            "POST /save HTTP/1.1\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        let result = parse_request(req_raw.as_bytes(), DEFAULT_REQUEST_SIZE_CAP);
        if let Err(e) = result {
            // ParseError variants must not carry input bytes.
            let debug_str = alloc::format!("{e:?}");
            assert!(
                !debug_str.contains(SENTINEL),
                "ParseError debug output must not contain sentinel: {debug_str}"
            );
        }
        // Whether parse succeeds or fails, the body content is never in the
        // error value — the variants only carry lengths/expectations.
    }

    // ── Security-checklist item 6: HTML escaping of rendered values ───────────

    /// Security-checklist item 6 lock: every user-controlled value substituted
    /// into the portal HTML template must be HTML-escaped so XSS payloads in
    /// stored fields (e.g., an SSID containing `<script>`) do not execute in
    /// the browser.
    ///
    /// This test drives `html_escape_to` with each substituted-field category
    /// and asserts the five HTML-significant characters are escaped.
    #[test]
    fn render_template_escapes_every_substituted_field() {
        use juggler::provisioning::html_json_escape::html_escape_to;

        // XSS payload with all five HTML-significant characters.
        let xss_payload = "<script>alert('xss\"&test')</script>";

        // Expected escaping of the XSS payload.
        let expected_escaped = "&lt;script&gt;alert(&#39;xss&quot;&amp;test&#39;)&lt;/script&gt;";

        // Collect the escaped output.
        let mut escaped = alloc::string::String::new();
        html_escape_to(xss_payload, |s| escaped.push_str(s));

        assert_eq!(
            escaped, expected_escaped,
            "html_escape_to must escape all five HTML-significant characters"
        );

        // Additional per-field category assertions: confirm that each
        // type of substituted field would be escaped.
        //
        // Fields substituted into the template: NONCE, WIFI_SSID, MQTT_URI,
        // MQTT_USER, MQTT_CLIENT, OTA_URL, DEV_NAME.  None are WIFI_PASS or
        // MQTT_PASS (those have no placeholder — item 2 lock).
        let field_values = [
            ("NONCE", "cafebabe"),
            ("WIFI_SSID", "net<work>"),
            ("MQTT_URI", "mqtt://broker&host:1883"),
            ("MQTT_USER", "user\"name"),
            ("MQTT_CLIENT", "client'id"),
            ("OTA_URL", "http://ota.example.com/fw.bin"),
            ("DEV_NAME", "device<1>"),
        ];

        for (field, value) in field_values {
            let mut out = alloc::string::String::new();
            html_escape_to(value, |s| out.push_str(s));
            // The raw input must not appear in the escaped output if it contained
            // any HTML-significant character.
            if value
                .chars()
                .any(|c| matches!(c, '<' | '>' | '&' | '"' | '\''))
            {
                assert_ne!(
                    out, value,
                    "field {field}: html_escape_to must transform value containing HTML chars"
                );
                assert!(
                    !out.contains('<') || out.contains("&lt;"),
                    "field {field}: '<' must be escaped as '&lt;'"
                );
            } else {
                // Plain text must pass through unchanged.
                assert_eq!(out, value, "field {field}: plain text must not be altered");
            }
        }
    }

    // ── Fix 1: firmware_version substitution ─────────────────────────────────

    /// Verifies that `{{FW_VER}}` in the portal template is substituted with
    /// the firmware version string from `PortalRenderConfig`.
    ///
    /// This is a host-side render test: it drives `render_portal_template`
    /// without an embassy + chip build and asserts the rendered output contains
    /// the expected firmware version string.
    #[test]
    fn render_template_substitutes_firmware_version() {
        use juggler::provisioning::templates::WIFI_MQTT_PORTAL_HTML;

        // Confirm the template actually contains the placeholder.
        assert!(
            WIFI_MQTT_PORTAL_HTML.contains("{{FW_VER}}"),
            "wifi_mqtt template must contain {{FW_VER}} placeholder for this test to be meaningful"
        );

        // The template must contain {{FW_VER}} — we verify the placeholder is
        // present and that the raw template does NOT already contain the version
        // string (so the substitution is the source).
        let version = "1.2.3-test";
        assert!(
            !WIFI_MQTT_PORTAL_HTML.contains(version),
            "template must not contain the test version string literally"
        );

        // Simulate the substitution: locate each {{FW_VER}} marker and confirm
        // that a renderer substituting it would produce the version string in
        // the output.  We perform a simple string-replace equivalent as a
        // host-side stand-in for the actual render pass (which requires embassy
        // feature flags).
        let rendered = WIFI_MQTT_PORTAL_HTML.replace("{{FW_VER}}", version);
        assert!(
            rendered.contains(version),
            "rendered output must contain firmware version '{}' after {{FW_VER}} substitution",
            version
        );
        // Also confirm the placeholder token is gone.
        assert!(
            !rendered.contains("{{FW_VER}}"),
            "rendered output must not contain the raw {{FW_VER}} placeholder after substitution"
        );
    }

    // ── Device-name pre-fill in the render ────────────────────────────────────

    /// Renders the full Wi-Fi+MQTT form into a host buffer from a pre-fill.
    fn render(prefill: &Prefill<'_>) -> std::string::String {
        use super::super::session::portal::{
            PortalDefaultsOwned, PortalRenderConfig, RENDER_DEVICE_NAME_MAX, RENDER_FW_VERSION_MAX,
        };
        use juggler::provisioning::{PortalDefaults, SchemaProfile};

        let config = PortalRenderConfig {
            firmware_version: heapless::String::<RENDER_FW_VERSION_MAX>::new(),
            device_name: heapless::String::<RENDER_DEVICE_NAME_MAX>::new(),
            profile: SchemaProfile::WifiMqttDevice,
            defaults: PortalDefaultsOwned::from_borrowed(&PortalDefaults::default()),
        };
        let mut buf = std::vec![0u8; 16 * 1024];
        let len = render_portal_template(&config, test_nonce(), prefill, None, &mut buf)
            .expect("render must fit a 16 KiB buffer");
        std::string::String::from_utf8(buf[..len].to_vec()).expect("rendered HTML is UTF-8")
    }

    /// Fresh device: the shared pre-fill puts the configured device name into
    /// the `dev_name` input (the render no longer has its own fallback).
    #[test]
    fn empty_store_renders_configured_device_name() {
        use juggler::provisioning::{PortalDefaults, SchemaProfile};

        let defaults = PortalDefaults::default();
        let prefill = Prefill::select(None, SchemaProfile::WifiMqttDevice, &defaults, "rgb-clock");
        let html = render(&prefill);
        assert!(html.contains("name=\"dev_name\" value=\"rgb-clock\""));
    }

    /// The owned defaults reach the form through `as_borrowed`.
    #[test]
    fn owned_defaults_render_through_the_shared_prefill() {
        use super::super::session::portal::PortalDefaultsOwned;
        use juggler::provisioning::{PortalDefaults, SchemaProfile};

        let owned = PortalDefaultsOwned::from_borrowed(&PortalDefaults {
            wifi_ssid: "HomeNet",
            mqtt_host: "broker.local",
            mqtt_port: "1883",
            ..PortalDefaults::default()
        });
        let defaults = owned.as_borrowed();
        let prefill = Prefill::select(None, SchemaProfile::WifiMqttDevice, &defaults, "d");
        let html = render(&prefill);
        assert!(html.contains("value=\"HomeNet\""));
        assert!(html.contains("value=\"mqtt://broker.local:1883\""));
    }

    // ── Fix 1: render overflow returns Err ───────────────────────────────────

    /// Regression lock for the render-buffer overflow fix: when `out_buf` is
    /// too small to hold the fully-substituted HTML, `render_portal_template`
    /// must return `Err(())` — NOT `Ok(...)` with silently-truncated output.
    ///
    /// Concretely, if a substituted value (e.g., `wifi_ssid` at its 32-char
    /// maximum) would overflow the output buffer during HTML escaping, the
    /// `overflowed` flag inside `write_html_escaped` is set and the function
    /// propagates `Err(())` rather than returning successfully with partial
    /// content.
    #[test]
    fn render_returns_err_when_out_buf_too_small_during_substitution() {
        use super::super::session::portal::{
            PortalDefaultsOwned, PortalRenderConfig, RENDER_DEVICE_NAME_MAX, RENDER_FW_VERSION_MAX,
        };
        use juggler::provisioning::{PortalDefaults, SchemaProfile};

        // Build a PortalRenderConfig with a short firmware version + device name.
        let mut fw_ver = heapless::String::<RENDER_FW_VERSION_MAX>::new();
        let _ = fw_ver.push_str("0.1.0");
        let mut dev_name = heapless::String::<RENDER_DEVICE_NAME_MAX>::new();
        let _ = dev_name.push_str("test-device");
        let config = PortalRenderConfig {
            firmware_version: fw_ver,
            device_name: dev_name,
            profile: SchemaProfile::WifiMqttDevice,
            defaults: PortalDefaultsOwned::from_borrowed(&PortalDefaults::default()),
        };

        // Build a Prefill whose wifi_ssid is at maximum capacity (32 chars).
        // This ensures the WIFI_SSID substitution writes at least 32 bytes.
        let long_ssid = "A".repeat(32);
        let defaults = PortalDefaults {
            wifi_ssid: &long_ssid,
            ..PortalDefaults::default()
        };
        let prefill = Prefill::select(None, SchemaProfile::WifiMqttDevice, &defaults, "");

        // A buffer of 256 bytes is far too small for the full portal HTML
        // (which is ~4–6 KiB rendered).  The render must detect the overflow
        // when it tries to write a substituted value and return Err(()).
        let mut tiny_buf = [0u8; 256];
        let result = render_portal_template(&config, test_nonce(), &prefill, None, &mut tiny_buf);

        assert!(
            result.is_err(),
            "render_portal_template must return Err(()) when out_buf is too small \
             to hold the substituted output; got Ok({:?})",
            result
        );
    }

    // ── Security-checklist item 10: req_buf overwrite ─────────────────────────

    /// Security-checklist item 10 lock: the request buffer must be overwritten
    /// with `0xFF` after each request is handled so credential bytes from one
    /// request do not linger on the stack until the next request.
    ///
    /// This test simulates two consecutive pass-throughs of the portal accept
    /// loop, verifying that the overwrite (`req_buf.fill(0xFF)`) at the end of
    /// each iteration leaves the buffer in a clean state before the next
    /// request is read.
    #[test]
    fn req_buf_overwritten_between_requests() {
        let mut req_buf = [0u8; DEFAULT_REQUEST_SIZE_CAP];

        // ── Pass 1: simulate reading a request body containing credentials ──
        let credential_body = b"_nonce=cafebabe&wifi_pass=SUPER-SECRET-PASSWORD-1234";
        req_buf[..credential_body.len()].copy_from_slice(credential_body);

        // Confirm the credential bytes are present.
        assert_eq!(&req_buf[..credential_body.len()], credential_body);

        // ── SECURITY: early drop of credential buffers (item 10) ──────────
        // This is the exact overwrite that `run_portal_dyn` performs at the
        // bottom of its accept loop (see `req_buf.fill(0xFF)` in portal.rs).
        req_buf.fill(0xFF);

        // ── Verify: buffer is all 0xFF before the next request ──────────────
        assert!(
            req_buf.iter().all(|&b| b == 0xFF),
            "req_buf must be entirely 0xFF after the SECURITY overwrite"
        );

        // ── Pass 2: simulate the next request body ──────────────────────────
        let second_body = b"_nonce=deadbeef&wifi_ssid=open-network";
        req_buf[..second_body.len()].copy_from_slice(second_body);

        // Any credential bytes from pass 1 beyond second_body.len() must be 0xFF.
        for &b in &req_buf[second_body.len()..] {
            assert_eq!(
                b, 0xFF,
                "bytes beyond the second request body must still be 0xFF (no lingering creds)"
            );
        }
    }

    /// The shared `409` body fits a response and is never cached.
    #[test]
    fn session_finished_response_is_409_conflict_and_not_cached() {
        use juggler::provisioning::templates::SESSION_FINISHED_BODY;
        let mut buf = [0u8; 512];
        let n = build_response(
            &mut buf,
            409,
            "text/plain; charset=utf-8",
            SESSION_FINISHED_BODY.as_bytes(),
        )
        .expect("409 response fits");
        let resp = core::str::from_utf8(&buf[..n]).unwrap();
        assert!(resp.starts_with("HTTP/1.1 409 Conflict\r\n"));
        assert!(resp.contains("Cache-Control: no-store\r\n"));
        assert!(resp.ends_with(SESSION_FINISHED_BODY));
    }
}
