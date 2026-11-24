//! Host tests of the DDI HTTP exchange: the request head, and the response head and body read from every split of the stream.

use alloc::string::String;
use alloc::vec::Vec;

use juggler::ota::ddi::wire::MAX_RESPONSE_BYTES;
use juggler::ota::ddi::{BodyFailure, RetryAfter, AUTHORIZATION_MAX_LEN};
use juggler::ota::OtaError;

use super::super::{parse_url, ResponseHeaders};
use super::*;

/// What a whole exchange gave: the head (or why it failed), then the body.
type Read = Result<(ResponseHead, Result<Vec<u8>, BodyFailure>), HeadError>;

/// Feeds `stream` in the pieces `cuts` marks (sorted offsets), as the transport does: the head parser first, the rest of each read to the body reader, then the end of the stream.
fn read_cut(stream: &[u8], cuts: &[usize]) -> Read {
    let mut pieces = Vec::new();
    let mut from = 0;
    for &cut in cuts {
        pieces.push(&stream[from..cut]);
        from = cut;
    }
    pieces.push(&stream[from..]);

    let mut parser = HeadParser::new();
    let mut head = None;
    let mut reader: Option<BodyReader> = None;
    for piece in pieces {
        let mut rest = piece;
        if head.is_none() {
            let (used, result) = parser.feed(rest);
            rest = &rest[used..];
            match result? {
                None => continue,
                Some(h) => {
                    match BodyReader::new(h.framing) {
                        Ok(r) => reader = Some(r),
                        Err(f) => return Ok((h, Err(f))),
                    }
                    head = Some(h);
                }
            }
        }
        let r = reader.as_mut().unwrap();
        if r.is_done() {
            break;
        }
        if let Err(f) = r.feed(rest) {
            return Ok((head.unwrap(), Err(f)));
        }
        if r.is_done() {
            break;
        }
    }
    let Some(head) = head else {
        // The stream ended inside the head: the transport reports the connection failure.
        return Err(HeadError::Malformed);
    };
    Ok((head, reader.unwrap().finish()))
}

/// The result for every single split and for byte-at-a-time reads; all must agree.
fn read_all_splits(stream: &[u8]) -> Read {
    let whole = read_cut(stream, &[]);
    for cut in 0..=stream.len() {
        assert_eq!(read_cut(stream, &[cut]), whole, "split at {cut}");
    }
    let bytewise: Vec<usize> = (1..stream.len()).collect();
    assert_eq!(read_cut(stream, &bytewise), whole, "byte at a time");
    whole
}

fn body_of(read: Read) -> (u16, Option<RetryAfter>, Result<Vec<u8>, BodyFailure>) {
    let (head, body) = read.expect("head parses");
    (head.status, head.retry_after, body)
}

// ── Request head ────────────────────────────────────────────────────────────

fn head_text(method: Method, url: &str, auth: &str, body_len: usize) -> String {
    let parsed = parse_url(url).unwrap();
    let head = request_head(method, &parsed, auth, ACCEPT_JSON, body_len);
    assert_eq!(head.len(), head.capacity(), "exact-size buffer");
    String::from_utf8(head).unwrap()
}

#[test]
fn get_head_carries_authorization_and_no_length() {
    let text = head_text(
        Method::Get,
        "http://h:8080/c/v1/dev-1",
        "TargetToken abc",
        0,
    );
    assert_eq!(
        text,
        "GET /c/v1/dev-1 HTTP/1.1\r\nHost: h:8080\r\nAuthorization: TargetToken abc\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );
}

#[test]
fn post_and_put_always_carry_content_length() {
    for (method, name) in [(Method::Post, "POST"), (Method::Put, "PUT")] {
        for len in [0usize, 1, 4096] {
            let text = head_text(method, "http://h/x", "TargetToken a", len);
            assert!(text.starts_with(name), "{text}");
            assert!(
                text.contains(&alloc::format!(
                    "\r\nContent-Type: application/json\r\nContent-Length: {len}\r\n"
                )),
                "{text}"
            );
            assert!(text.ends_with("Connection: close\r\n\r\n"));
        }
    }
}

#[test]
fn longest_link_and_authorization_make_a_complete_head() {
    let path: String = core::iter::once('/')
        .chain(core::iter::repeat_n('a', 480))
        .collect();
    let url = alloc::format!("http://192.168.100.200:65535{path}?c=-1234567890");
    assert!(url.len() >= 512);
    let auth: String = core::iter::once("TargetToken ")
        .chain(core::iter::repeat_n("f", AUTHORIZATION_MAX_LEN - 12))
        .collect();
    assert_eq!(auth.len(), AUTHORIZATION_MAX_LEN);
    let text = head_text(Method::Post, &url, &auth, 4096);
    assert!(text.starts_with(&alloc::format!("POST {path}?c=-1234567890 HTTP/1.1\r\n")));
    assert!(text.contains("\r\nHost: 192.168.100.200:65535\r\n"));
    assert!(text.contains(&alloc::format!("\r\nAuthorization: {auth}\r\n")));
    assert!(text.contains("\r\nContent-Length: 4096\r\n"));
    assert!(text.ends_with("\r\n\r\n"));
    assert_eq!(text.matches("\r\n\r\n").count(), 1);
}

#[test]
fn default_port_is_left_out_of_host() {
    let text = head_text(Method::Get, "http://h:80/x", "a", 0);
    assert!(text.contains("\r\nHost: h\r\n"), "{text}");
}

#[test]
fn download_head_asks_for_firmware() {
    let parsed = parse_url("http://h/a").unwrap();
    let head = request_head(Method::Get, &parsed, "TargetToken a", ACCEPT_FIRMWARE, 0);
    let text = String::from_utf8(head).unwrap();
    assert!(text.contains("\r\nAccept: application/octet-stream\r\n"));
}

// ── Response head ───────────────────────────────────────────────────────────

#[test]
fn any_status_is_handed_back() {
    for status in [200u16, 201, 400, 401, 404, 409, 410, 429, 500, 503, 599] {
        let stream = alloc::format!("HTTP/1.1 {status} X\r\nContent-Length: 2\r\n\r\n{{}}");
        let (got, _, body) = body_of(read_all_splits(stream.as_bytes()));
        assert_eq!(got, status);
        assert_eq!(body.unwrap(), b"{}");
    }
}

#[test]
fn http10_and_bare_lf_heads_parse() {
    let (status, _, body) = body_of(read_all_splits(b"HTTP/1.0 200 OK\nContent-Length: 1\n\nx"));
    assert_eq!(status, 200);
    assert_eq!(body.unwrap(), b"x");
}

#[test]
fn retry_after_is_captured_within_its_bounds() {
    let (_, retry, _) = body_of(read_all_splits(
        b"HTTP/1.1 503 Busy\r\nRetry-After: 120\r\nContent-Length: 0\r\n\r\n",
    ));
    assert_eq!(retry.unwrap().as_str(), "120");

    let date = "Wed, 21 Oct 2026 07:28:00 GMT";
    let stream = alloc::format!("HTTP/1.1 429 X\r\nretry-after:  {date} \r\n\r\n");
    let (_, retry, _) = body_of(read_all_splits(stream.as_bytes()));
    assert_eq!(retry.unwrap().as_str(), date);

    let long = "1".repeat(33);
    let stream =
        alloc::format!("HTTP/1.1 429 X\r\nRetry-After: {long}\r\nContent-Length: 0\r\n\r\n");
    let (_, retry, _) = body_of(read_all_splits(stream.as_bytes()));
    assert_eq!(retry, None);

    let exact = "2".repeat(32);
    let stream =
        alloc::format!("HTTP/1.1 429 X\r\nRetry-After: {exact}\r\nContent-Length: 0\r\n\r\n");
    let (_, retry, _) = body_of(read_all_splits(stream.as_bytes()));
    assert_eq!(retry.unwrap().as_str(), exact);
}

#[test]
fn the_first_retry_after_wins() {
    let (_, retry, _) = body_of(read_all_splits(
        b"HTTP/1.1 503 X\r\nRetry-After: 5\r\nRetry-After: 9\r\nContent-Length: 0\r\n\r\n",
    ));
    assert_eq!(retry.unwrap().as_str(), "5");
}

#[test]
fn a_header_block_over_one_kib_is_accepted() {
    let mut stream = String::from("HTTP/1.1 200 OK\r\n");
    for i in 0..40 {
        stream.push_str(&alloc::format!("X-Filler-{i}: {}\r\n", "v".repeat(40)));
    }
    stream.push_str(&alloc::format!("Set-Cookie: {}\r\n", "c".repeat(900)));
    stream.push_str("Content-Length: 2\r\n\r\n{}");
    assert!(stream.len() > 2048);
    let (status, _, body) = body_of(read_all_splits(stream.as_bytes()));
    assert_eq!(status, 200);
    assert_eq!(body.unwrap(), b"{}");
}

#[test]
fn an_overlong_framing_header_is_malformed() {
    let stream = alloc::format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}1\r\n\r\n",
        "0".repeat(LINE_CAP)
    );
    assert_eq!(
        read_all_splits(stream.as_bytes()),
        Err(HeadError::Malformed)
    );
}

#[test]
fn a_head_over_the_cap_is_refused() {
    let mut stream = String::from("HTTP/1.1 200 OK\r\n");
    while stream.len() <= HEAD_CAP {
        stream.push_str("X-Pad: 0123456789012345678901234567890123456789\r\n");
    }
    stream.push_str("\r\n");
    let mut parser = HeadParser::new();
    let (used, result) = parser.feed(stream.as_bytes());
    assert_eq!(result, Err(HeadError::TooLarge));
    assert_eq!(used, HEAD_CAP + 1);
}

#[test]
fn malformed_heads_are_refused() {
    for stream in [
        &b"HTTP/2 200 OK\r\n\r\n"[..],
        b"SSH-2.0-OpenSSH\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nno colon here\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length : 1\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nX: a\r\n folded\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length: 1\r\nContent-Length: 1\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length\t: 1\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding : chunked\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length: +1\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length: -1\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length: abc\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length: \r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length:\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nContent-Length: 18446744073709551616\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: identity\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: gzip, chunked\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nTransfer-Encoding: chunked\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Length: 3\r\n\r\n",
    ] {
        assert_eq!(
            read_all_splits(stream),
            Err(HeadError::Malformed),
            "{:?}",
            core::str::from_utf8(stream)
        );
    }
}

#[test]
fn a_stream_that_ends_inside_the_head_fails() {
    assert!(read_all_splits(b"HTTP/1.1 200 OK\r\nContent-Le").is_err());
}

#[test]
fn informational_heads_are_skipped() {
    let (status, _, body) = body_of(read_all_splits(
        b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
    ));
    assert_eq!(status, 200);
    assert_eq!(body.unwrap(), b"ok");
}

#[test]
fn the_parser_consumes_nothing_past_the_head() {
    let stream = b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\nabc";
    let mut parser = HeadParser::new();
    let (used, head) = parser.feed(stream);
    assert_eq!(used, stream.len() - 3);
    assert_eq!(head.unwrap().unwrap().framing, Framing::Length(3));
}

// ── Body ────────────────────────────────────────────────────────────────────

#[test]
fn content_length_body() {
    let (_, _, body) = body_of(read_all_splits(
        b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\n\r\n{\"a\":\"bcdef\"}",
    ));
    assert_eq!(body.unwrap(), b"{\"a\":\"bcdef\"}");
}

#[test]
fn framing_header_names_are_case_insensitive() {
    for name in ["content-length", "CONTENT-LENGTH", "Content-Length"] {
        let stream = alloc::format!("HTTP/1.1 200 OK\r\n{name}: 2\r\n\r\n{{}}");
        let (_, _, body) = body_of(read_all_splits(stream.as_bytes()));
        assert_eq!(body.unwrap(), b"{}", "{name}");
    }
    let (_, _, body) = body_of(read_all_splits(
        b"HTTP/1.1 200 OK\r\ntransfer-encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n",
    ));
    assert_eq!(body.unwrap(), b"{}");
}

#[test]
fn download_admission_refuses_missing_zero_chunked_and_oversized_lengths() {
    use juggler::ota::check_content_length;

    let facts = |framing| download_headers(head(200, framing, None), &mut None).unwrap();
    let missing = facts(Framing::Close);
    assert_eq!(
        check_content_length(missing.content_length, missing.chunked, u64::MAX),
        Err(OtaError::DownloadFailed { status: 0 })
    );
    let zero = facts(Framing::Length(0));
    assert_eq!(
        check_content_length(zero.content_length, zero.chunked, u64::MAX),
        Err(OtaError::DownloadFailed { status: 0 })
    );
    let chunked = facts(Framing::Chunked);
    assert_eq!(
        check_content_length(chunked.content_length, chunked.chunked, u64::MAX),
        Err(OtaError::DownloadFailed { status: 0 })
    );
    let oversized = facts(Framing::Length(200));
    assert_eq!(
        check_content_length(oversized.content_length, oversized.chunked, 100),
        Err(OtaError::InsufficientSpace)
    );
}

#[test]
fn bytes_after_the_declared_length_are_ignored() {
    let (_, _, body) = body_of(read_all_splits(
        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}garbage",
    ));
    assert_eq!(body.unwrap(), b"{}");
}

#[test]
fn a_short_body_is_a_length_mismatch() {
    let (status, _, body) = body_of(read_all_splits(
        b"HTTP/1.1 200 OK\r\nRetry-After: 7\r\nContent-Length: 10\r\n\r\n{}",
    ));
    assert_eq!(status, 200);
    assert_eq!(body, Err(BodyFailure::LengthMismatch));
}

#[test]
fn a_declared_body_over_the_cap_fails_before_reading() {
    let stream = alloc::format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
        MAX_RESPONSE_BYTES + 1
    );
    let (_, _, body) = body_of(read_all_splits(stream.as_bytes()));
    assert_eq!(body, Err(BodyFailure::TooLarge));
}

#[test]
fn a_body_at_the_cap_passes_and_one_byte_more_fails() {
    for (len, ok) in [(MAX_RESPONSE_BYTES, true), (MAX_RESPONSE_BYTES + 1, false)] {
        let mut chunked = String::from("HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n");
        chunked.push_str(&alloc::format!(
            "{:x}\r\n{}\r\n0\r\n\r\n",
            len,
            "x".repeat(len)
        ));
        let mut close = String::from("HTTP/1.1 200 OK\r\n\r\n");
        close.push_str(&"y".repeat(len));
        for stream in [chunked, close] {
            // Every split of a 8 KiB stream is slow; the split matrix runs on the small cases.
            let (_, _, body) = body_of(read_cut(stream.as_bytes(), &[100, 4000, 7000]));
            if ok {
                assert_eq!(body.unwrap().len(), len);
            } else {
                assert_eq!(body, Err(BodyFailure::TooLarge));
            }
        }
    }
}

#[test]
fn chunked_body_with_extensions_and_trailer() {
    let (status, _, body) = body_of(read_all_splits(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: Chunked\r\n\r\n\
          4;name=value\r\n{\"a\"\r\n\
          A ; ext\r\n:\"0123456\"\r\n\
          1\r\n}\r\n\
          0\r\nX-Trailer: 1\r\nY: 2\r\n\r\n",
    ));
    assert_eq!(status, 200);
    assert_eq!(body.unwrap(), b"{\"a\":\"0123456\"}");
}

#[test]
fn chunked_body_without_trailer_and_with_uppercase_hex() {
    let (_, _, body) = body_of(read_all_splits(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n\r\n",
    ));
    assert_eq!(body.unwrap(), b"{}");
    let payload = "z".repeat(0x1F);
    let stream = alloc::format!(
        "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1F\r\n{payload}\r\n0\r\n\r\n"
    );
    let (_, _, body) = body_of(read_all_splits(stream.as_bytes()));
    assert_eq!(body.unwrap(), payload.as_bytes());
}

#[test]
fn a_cut_chunked_body_fails() {
    let (_, _, body) = body_of(read_all_splits(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\n{}",
    ));
    assert_eq!(body, Err(BodyFailure::ServerUnreachable));
    let (_, _, body) = body_of(read_all_splits(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}\r\n0\r\n",
    ));
    assert_eq!(body, Err(BodyFailure::ServerUnreachable));
}

#[test]
fn malformed_chunks_fail() {
    for stream in [
        &b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n{}\r\n0\r\n\r\n"[..],
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\n{}XX0\r\n\r\n",
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1000000000000000\r\n",
    ] {
        let (_, _, body) = body_of(read_all_splits(stream));
        assert_eq!(body, Err(BodyFailure::ServerUnreachable));
    }
}

#[test]
fn a_huge_chunk_size_is_too_large_before_its_data() {
    let (_, _, body) = body_of(read_all_splits(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nFFFFFFFF\r\n",
    ));
    assert_eq!(body, Err(BodyFailure::TooLarge));
}

#[test]
fn a_body_until_close() {
    let (_, _, body) = body_of(read_all_splits(b"HTTP/1.0 200 OK\r\n\r\n{\"x\":1}"));
    assert_eq!(body.unwrap(), b"{\"x\":1}");
}

#[test]
fn no_content_and_not_modified_have_no_body() {
    for stream in [
        &b"HTTP/1.1 204 No Content\r\n\r\n"[..],
        b"HTTP/1.1 304 Not Modified\r\nContent-Length: 99\r\n\r\n",
    ] {
        let (_, _, body) = body_of(read_all_splits(stream));
        assert_eq!(body.unwrap(), b"");
    }
}

// ── Redirects and the download head ─────────────────────────────────────────

fn head(status: u16, framing: Framing, retry: Option<&str>) -> ResponseHead {
    ResponseHead {
        status,
        retry_after: retry.and_then(RetryAfter::capture),
        framing,
    }
}

#[test]
fn redirects_are_refused_and_304_is_not_one() {
    for status in [301u16, 302, 303, 307, 308] {
        let mut retry = None;
        assert_eq!(
            download_headers(head(status, Framing::Close, Some("5")), &mut retry),
            Err(OtaError::Redirect { status })
        );
        assert_eq!(retry, None);
        assert!(juggler::ota::ddi::is_redirect(status));
    }
    assert!(!juggler::ota::ddi::is_redirect(304));
    let mut retry = None;
    assert_eq!(
        download_headers(head(304, Framing::Empty, None), &mut retry),
        Err(OtaError::DownloadFailed { status: 304 })
    );
}

#[test]
fn a_redirect_head_parses_with_its_status() {
    let (status, _, _) = body_of(read_all_splits(
        b"HTTP/1.1 302 Found\r\nLocation: http://elsewhere/\r\nContent-Length: 0\r\n\r\n",
    ));
    assert_eq!(status, 302);
}

#[test]
fn a_non_200_download_fills_retry_after() {
    let mut retry = None;
    assert_eq!(
        download_headers(head(503, Framing::Length(0), Some("30")), &mut retry),
        Err(OtaError::DownloadFailed { status: 503 })
    );
    assert_eq!(retry.unwrap().as_str(), "30");
}

#[test]
fn a_200_download_hands_its_length_on() {
    let mut retry = RetryAfter::capture("1");
    assert_eq!(
        download_headers(head(200, Framing::Length(4096), None), &mut retry),
        Ok(ResponseHeaders {
            content_length: Some(4096),
            chunked: false
        })
    );
    assert_eq!(retry.unwrap().as_str(), "1", "left unchanged on a 200");
    assert_eq!(
        download_headers(head(200, Framing::Chunked, None), &mut None),
        Ok(ResponseHeaders {
            content_length: None,
            chunked: true
        })
    );
}
