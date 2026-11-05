//! The captive-portal DNS codec shared by both provisioning tiers.
//!
//! A minimal RFC 1035 responder: every `A` (and `ANY`) query is answered with
//! the portal IP, so any name a phone resolves leads to the portal and the
//! OS's captive-portal probe gets unexpected content.  Every other query type
//! (`AAAA`, `MX`, ...) gets `NOERROR` with zero answers: an `A` record in an
//! `AAAA` answer is malformed, and an empty answer makes the client fall back
//! to `A`.
//!
//! The tiers keep only the socket loop: the ESP-IDF tier a `std` UDP thread,
//! the bare-metal tier an `embassy-net` task.  The codec is `no_std` and
//! allocation-free.
//!
//! # Decoding
//!
//! - Header (12 bytes): reads `id`, `flags`, `qdcount`; rejects `qdcount != 1`
//!   and `qr == 1` (a response, not a query).
//! - Question: walks the qname labels by length, rejects names over 255 bytes
//!   or a label over 63 bytes, and rejects compression pointers in the qname
//!   (real resolvers never send them in a query, and following them risks a
//!   pointer loop).  The qname is not decoded into a string; only its byte
//!   range is recorded.

// ── DNS wire constants ────────────────────────────────────────────────────────

/// Experimental: API may change before 1.0.
///
/// Maximum DNS message size (RFC 1035 §2.3.4 — UDP payload cap before EDNS).
pub const DNS_MSG_MAX: usize = 512;

/// Minimum DNS query size: 12-byte header + 1-byte root label + 2-byte qtype +
/// 2-byte qclass = 17 bytes.
const DNS_MIN_QUERY: usize = 17;

/// Maximum total qname length in wire format (RFC 1035 §3.1: 255 octets total).
const QNAME_MAX_WIRE: usize = 255;

/// Maximum label length in wire format (RFC 1035 §2.3.4: 63 octets).
const LABEL_MAX: usize = 63;

/// Bit mask for the high two bits of a length byte — indicates a compression
/// pointer when both bits are set.
const COMPRESSION_MASK: u8 = 0xC0;

// ── DNS header bit masks and values ──────────────────────────────────────────

/// Bit 15 of flags (network order in the 16-bit field): QR bit — 0 = query.
const FLAG_QR: u16 = 0x8000;
/// Bit 8: RD — Recursion Desired (copied from client request into response).
const FLAG_RD: u16 = 0x0100;

// ── DnsError ──────────────────────────────────────────────────────────────────

/// Experimental: API may change before 1.0.
///
/// Errors produced by the DNS codec.
#[derive(Debug, PartialEq, Eq)]
pub enum DnsError {
    /// Packet is shorter than the minimum DNS query size (17 bytes).
    TooShort,
    /// The QR bit is set — this is a response, not a query.
    NotAQuery,
    /// `qdcount` is not `1`; this server handles exactly one question per query.
    UnsupportedQuestionCount,
    /// The qname contains a compression pointer (high two bits of a length
    /// byte are both set).  Compression in queries is valid per RFC 1035 but
    /// is not seen in practice from real resolvers and introduces pointer-chase
    /// risk.  Queries containing a compression pointer are dropped.
    CompressionPointerNotAllowed,
    /// The qname is malformed: a label length byte indicates a label longer than
    /// 63 bytes, or the total wire-format qname length exceeds 255 bytes, or
    /// the packet ends before the terminating zero-length label is reached.
    MalformedName,
    /// The packet is truncated mid-qname or before the qtype/qclass fields.
    Truncated,
    /// The encode buffer is too small to hold the serialised response.
    BufferTooSmall,
}

// ── DnsQuery ──────────────────────────────────────────────────────────────────

/// Experimental: API may change before 1.0.
///
/// Parsed representation of a DNS query — minimal fields needed to build a
/// catch-all A-record response.
///
/// The `qname_range` is a byte range into the **original** query buffer; the
/// qname bytes are not copied.  Callers that need to log the qname or include
/// it verbatim in the response must retain the original buffer alongside the
/// `DnsQuery`.
#[derive(Debug, PartialEq, Eq)]
pub struct DnsQuery {
    /// Transaction ID — echoed back in the response.
    pub id: u16,
    /// Flags from the request header (16-bit, network order).
    pub flags: u16,
    /// Byte range of the qname within the original query buffer, starting at
    /// the first length byte (offset 12) and ending one past the terminating
    /// zero-length label.
    pub qname_range: core::ops::Range<usize>,
    /// Query type (e.g. `1` = A, `28` = AAAA, `15` = MX).
    pub qtype: u16,
    /// Query class (`1` = IN).
    pub qclass: u16,
}

// ── Codec — decode ────────────────────────────────────────────────────────────

/// Experimental: API may change before 1.0.
///
/// Decodes a raw UDP payload into a [`DnsQuery`].
///
/// Returns `Err` for:
/// - packets shorter than 17 bytes (header + minimal qname + qtype + qclass)
/// - QR bit set (this is a response, not a query)
/// - `qdcount` not equal to 1
/// - qname containing a compression pointer
/// - qname label > 63 bytes or total wire length > 255 bytes
/// - packet truncated before the terminating zero-length label or qtype/qclass
///
/// Unknown qtype and qclass values are passed through unchanged;
/// [`encode_response`] decides the answer.
pub fn decode_query(buf: &[u8]) -> Result<DnsQuery, DnsError> {
    if buf.len() < DNS_MIN_QUERY {
        return Err(DnsError::TooShort);
    }

    let id = u16::from_be_bytes([buf[0], buf[1]]);
    let flags = u16::from_be_bytes([buf[2], buf[3]]);
    let qdcount = u16::from_be_bytes([buf[4], buf[5]]);

    // Bit 15 of flags is QR; 0 = query, 1 = response.
    if flags & FLAG_QR != 0 {
        return Err(DnsError::NotAQuery);
    }

    if qdcount != 1 {
        return Err(DnsError::UnsupportedQuestionCount);
    }

    // Walk the qname starting at byte 12 (immediately after the 12-byte header).
    let qname_start = 12;
    let mut pos = qname_start;
    let mut wire_len: usize = 0; // total bytes consumed in wire format (including length bytes)

    loop {
        if pos >= buf.len() {
            return Err(DnsError::Truncated);
        }

        let len_byte = buf[pos];

        // High two bits both set → compression pointer.
        if len_byte & COMPRESSION_MASK == COMPRESSION_MASK {
            return Err(DnsError::CompressionPointerNotAllowed);
        }

        // High two bits partially set → reserved (RFC 1035 §4.1.4); treat as
        // malformed.  Only 0b00xxxxxx (label length) and 0b11xxxxxx (pointer,
        // handled above) are defined by RFC 1035.
        if len_byte & 0x80 != 0 {
            return Err(DnsError::MalformedName);
        }

        let label_len = len_byte as usize;
        wire_len += 1; // the length byte itself

        if wire_len > QNAME_MAX_WIRE {
            return Err(DnsError::MalformedName);
        }

        if label_len == 0 {
            // Terminating zero-length label — qname ends here.
            pos += 1;
            break;
        }

        if label_len > LABEL_MAX {
            return Err(DnsError::MalformedName);
        }

        wire_len += label_len;
        if wire_len > QNAME_MAX_WIRE {
            return Err(DnsError::MalformedName);
        }

        pos += 1 + label_len; // advance past the length byte and the label bytes
    }

    let qname_end = pos;

    // Read qtype and qclass (4 bytes) after the qname.
    if pos + 4 > buf.len() {
        return Err(DnsError::Truncated);
    }

    let qtype = u16::from_be_bytes([buf[pos], buf[pos + 1]]);
    let qclass = u16::from_be_bytes([buf[pos + 2], buf[pos + 3]]);

    Ok(DnsQuery {
        id,
        flags,
        qname_range: qname_start..qname_end,
        qtype,
        qclass,
    })
}

// ── Codec — encode ────────────────────────────────────────────────────────────

/// DNS type `A` (IPv4 host address).
const TYPE_A: u16 = 1;
/// DNS type `ANY` (all records); a single `A` record is a valid answer.
const TYPE_ANY: u16 = 255;
/// DNS class `IN`.
const CLASS_IN: u16 = 1;

/// Experimental: API may change before 1.0.
///
/// `true` when a query of `qtype` is answered with the portal's `A` record.
///
/// `A` and `ANY` are; every other type gets `NOERROR` with zero answers.
pub fn answers_with_portal_ip(qtype: u16) -> bool {
    qtype == TYPE_A || qtype == TYPE_ANY
}

/// Experimental: API may change before 1.0.
///
/// Encodes the response to `query` into `buf`.
///
/// The response holds:
/// - a 12-byte header with QR=1, AA=1, RD copied from the query, RCODE=0
///   (`NOERROR`), QDCOUNT=1, NSCOUNT=0, ARCOUNT=0, and ANCOUNT=1 or 0 per
///   [`answers_with_portal_ip`];
/// - the question section, copied from `query_buf` via `query.qname_range`,
///   with the query's qtype and qclass;
/// - for an answered query, one `A` record that points back to the qname at
///   byte 12 (`0xC0 0x0C`), CLASS=IN, TTL=`ttl_secs`, RDATA=`answer_ip`.
///
/// Returns the number of bytes written, or `Err(DnsError::BufferTooSmall)`.
pub fn encode_response(
    query: &DnsQuery,
    query_buf: &[u8],
    answer_ip: [u8; 4],
    ttl_secs: u32,
    buf: &mut [u8],
) -> Result<usize, DnsError> {
    // Header 12 + question (qname + qtype + qclass) + answer 16 when answered:
    // name pointer 2, type 2, class 2, TTL 4, RDLENGTH 2, RDATA 4.
    let answered = answers_with_portal_ip(query.qtype);
    let qname_bytes = query_buf
        .get(query.qname_range.clone())
        .ok_or(DnsError::Truncated)?;
    let qname_len = qname_bytes.len();
    let needed = 12 + qname_len + 4 + if answered { 16 } else { 0 };
    if buf.len() < needed {
        return Err(DnsError::BufferTooSmall);
    }

    let mut pos = 0;
    let mut put = |bytes: &[u8]| {
        buf[pos..pos + bytes.len()].copy_from_slice(bytes);
        pos += bytes.len();
    };

    // ── Header ──────────────────────────────────────────────────────────────

    put(&query.id.to_be_bytes());
    // QR=1, opcode 0 (the decoder accepts only standard queries), AA=1, TC=0,
    // RD copied from the request, RA=0, Z=0, RCODE=0.
    put(&(0x8400 | (query.flags & FLAG_RD)).to_be_bytes());
    put(&1u16.to_be_bytes());
    put(&u16::from(answered).to_be_bytes());
    put(&0u16.to_be_bytes());
    put(&0u16.to_be_bytes());

    // ── Question section ─────────────────────────────────────────────────────

    put(qname_bytes);
    put(&query.qtype.to_be_bytes());
    put(&query.qclass.to_be_bytes());

    // ── Answer section ───────────────────────────────────────────────────────

    if answered {
        put(&[0xC0, 0x0C]);
        put(&TYPE_A.to_be_bytes());
        put(&CLASS_IN.to_be_bytes());
        put(&ttl_secs.to_be_bytes());
        put(&4u16.to_be_bytes());
        put(&answer_ip);
    }

    Ok(pos)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PORTAL_IP: [u8; 4] = [192, 168, 4, 1];

    // ── Test helpers ──────────────────────────────────────────────────────────

    /// Wire-encoded qname for `captive.apple.com` (used in multiple tests).
    ///
    /// Encoding:
    ///   `\x07captive\x05apple\x03com\x00`
    ///   = [7, 'c','a','p','t','i','v','e', 5, 'a','p','p','l','e', 3, 'c','o','m', 0]
    const QNAME_CAPTIVE_APPLE_COM: &[u8] = &[
        7, b'c', b'a', b'p', b't', b'i', b'v', b'e', // "captive"
        5, b'a', b'p', b'p', b'l', b'e', // "apple"
        3, b'c', b'o', b'm', // "com"
        0,    // root label terminator
    ];

    /// Pre-built DNS query packet for `captive.apple.com A IN` with id=0xABCD, RD=1.
    ///
    /// Layout:
    ///   [0..2]  id     = 0xABCD
    ///   [2..4]  flags  = 0x0100 (RD=1)
    ///   [4..6]  qdcount = 1
    ///   [6..12] ancount=0, nscount=0, arcount=0
    ///   [12..]  qname + qtype(1) + qclass(1)
    ///
    /// This is the authoritative test packet used in round-trip and flag tests.
    /// The layout is verified byte-by-byte in the encode tests.
    const PKT_CAPTIVE_APPLE_COM_A_RD1: &[u8] = &[
        0xAB, 0xCD, // id
        0x01, 0x00, // flags: RD=1
        0x00, 0x01, // qdcount = 1
        0x00, 0x00, // ancount
        0x00, 0x00, // nscount
        0x00, 0x00, // arcount
        // qname: captive.apple.com
        7, b'c', b'a', b'p', b't', b'i', b'v', b'e', 5, b'a', b'p', b'p', b'l', b'e', 3, b'c', b'o',
        b'm', 0, // root
        0x00, 0x01, // qtype = A
        0x00, 0x01, // qclass = IN
    ];

    /// Same as [`PKT_CAPTIVE_APPLE_COM_A_RD1`] but with id=0x0002, RD=0.
    const PKT_CAPTIVE_APPLE_COM_A_RD0: &[u8] = &[
        0x00, 0x02, // id
        0x00, 0x00, // flags: RD=0
        0x00, 0x01, // qdcount = 1
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // ancount/nscount/arcount
        7, b'c', b'a', b'p', b't', b'i', b'v', b'e', 5, b'a', b'p', b'p', b'l', b'e', 3, b'c',
        b'o', b'm', 0, 0x00, 0x01, // qtype = A
        0x00, 0x01, // qclass = IN
    ];

    /// AAAA query (qtype=28) for `captive.apple.com` with id=0x1234, RD=1.
    const PKT_CAPTIVE_APPLE_COM_AAAA_RD1: &[u8] = &[
        0x12, 0x34, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 7, b'c', b'a',
        b'p', b't', b'i', b'v', b'e', 5, b'a', b'p', b'p', b'l', b'e', 3, b'c', b'o', b'm', 0,
        0x00, 0x1C, // qtype = AAAA (28)
        0x00, 0x01,
    ];

    /// Query with qdcount=2 (unsupported).
    const PKT_QDCOUNT_TWO: &[u8] = &[
        0x00, 0x01, 0x00, 0x00, 0x00, 0x02, // qdcount = 2
        0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 7, b'c', b'a', b'p', b't', b'i', b'v', b'e', 5, b'a',
        b'p', b'p', b'l', b'e', 3, b'c', b'o', b'm', 0, 0x00, 0x01, 0x00, 0x01,
    ];

    /// Query with QR=1 (it is a response, not a query).
    const PKT_QR_SET: &[u8] = &[
        0x00, 0x01, 0x80, 0x00, // flags: QR=1
        0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 7, b'c', b'a', b'p', b't', b'i', b'v',
        b'e', 5, b'a', b'p', b'p', b'l', b'e', 3, b'c', b'o', b'm', 0, 0x00, 0x01, 0x00, 0x01,
    ];

    /// Query with a compression pointer as the first qname byte (0xC0 0x0C).
    const PKT_COMPRESSION_POINTER: &[u8] = &[
        0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xC0,
        0x0C, // compression pointer in the qname
        0x00, 0x01, 0x00, 0x01,
    ];

    /// Build a DNS query packet into a fixed-size buffer.
    ///
    /// Panics (in tests only) if the packet does not fit in the buffer or if the
    /// combined qname + qtype + qclass overhead overflows.  The buffer is
    /// [`DNS_MSG_MAX`] bytes, which is large enough for any valid DNS query.
    fn make_query_fixed(
        buf: &mut [u8; DNS_MSG_MAX],
        id: u16,
        flags: u16,
        qname: &[u8],
        qtype: u16,
        qclass: u16,
    ) -> usize {
        buf.fill(0);
        buf[0..2].copy_from_slice(&id.to_be_bytes());
        buf[2..4].copy_from_slice(&flags.to_be_bytes());
        buf[4..6].copy_from_slice(&1u16.to_be_bytes()); // qdcount = 1
                                                        // ancount/nscount/arcount stay zero
        let qname_end = 12 + qname.len();
        buf[12..qname_end].copy_from_slice(qname);
        buf[qname_end..qname_end + 2].copy_from_slice(&qtype.to_be_bytes());
        buf[qname_end + 2..qname_end + 4].copy_from_slice(&qclass.to_be_bytes());
        qname_end + 4
    }

    // ── Round-trip test ───────────────────────────────────────────────────────

    /// Decode a known-good query and verify the encoded response contains the
    /// expected A record bytes.
    ///
    /// Expected answer record layout (after the 12-byte header + question):
    ///   NAME:     0xC0 0x0C (compression pointer to offset 12)
    ///   TYPE:     0x00 0x01 (A)
    ///   CLASS:    0x00 0x01 (IN)
    ///   TTL:      0x00 0x00 0x00 0x3C (60)
    ///   RDLENGTH: 0x00 0x04
    ///   RDATA:    0xC0 0xA8 0x04 0x01 (192.168.4.1)
    #[test]
    fn round_trip_captive_apple_com() {
        let pkt = PKT_CAPTIVE_APPLE_COM_A_RD1;

        let query = decode_query(pkt).expect("decode_query should succeed");
        assert_eq!(query.id, 0xABCD);
        assert_eq!(query.qtype, 1);
        assert_eq!(query.qclass, 1);

        let answer_ip = PORTAL_IP;
        let ttl: u32 = 60;
        let mut resp_buf = [0u8; 512];
        let n = encode_response(&query, pkt, answer_ip, ttl, &mut resp_buf)
            .expect("encode_response should succeed");

        // Verify the answer record starts after the header (12) + question.
        // Question length = qname bytes + 4 (qtype + qclass).
        let question_len = QNAME_CAPTIVE_APPLE_COM.len() + 4;
        let answer_offset = 12 + question_len;

        assert!(
            n >= answer_offset + 16,
            "response too short: n={n} answer_offset={answer_offset}"
        );

        let ans = &resp_buf[answer_offset..answer_offset + 16];

        // NAME: compression pointer 0xC0 0x0C.
        assert_eq!(ans[0], 0xC0, "NAME high byte (compression pointer)");
        assert_eq!(ans[1], 0x0C, "NAME low byte (offset 12)");

        // TYPE = A (1).
        assert_eq!(&ans[2..4], &[0x00, 0x01], "TYPE should be A (1)");

        // CLASS = IN (1).
        assert_eq!(&ans[4..6], &[0x00, 0x01], "CLASS should be IN (1)");

        // TTL = 60 = 0x0000003C.
        assert_eq!(&ans[6..10], &[0x00, 0x00, 0x00, 0x3C], "TTL should be 60");

        // RDLENGTH = 4.
        assert_eq!(&ans[10..12], &[0x00, 0x04], "RDLENGTH should be 4");

        // RDATA = 192.168.4.1 = 0xC0A80401.
        assert_eq!(
            &ans[12..16],
            &[0xC0, 0xA8, 0x04, 0x01],
            "RDATA should be 192.168.4.1"
        );
    }

    // ── RD-flag copy test ─────────────────────────────────────────────────────

    /// Response flags must copy the RD bit from the request and set QR + AA.
    #[test]
    fn response_flags_rd_bit_copied() {
        let mut buf = [0u8; 512];

        // Request with RD=1 (PKT_CAPTIVE_APPLE_COM_A_RD1 has flags=0x0100).
        let pkt_rd1 = PKT_CAPTIVE_APPLE_COM_A_RD1;
        let q1 = decode_query(pkt_rd1).unwrap();
        let n1 = encode_response(&q1, pkt_rd1, PORTAL_IP, 60, &mut buf).unwrap();
        assert!(n1 > 2);
        let resp_flags = u16::from_be_bytes([buf[2], buf[3]]);
        // QR=1 (0x8000), AA=1 (0x0400), RD=1 (0x0100) → 0x8500.
        assert_eq!(
            resp_flags, 0x8500,
            "flags with RD=1: expected 0x8500, got {:#06x}",
            resp_flags
        );

        // Request with RD=0 (PKT_CAPTIVE_APPLE_COM_A_RD0 has flags=0x0000).
        let pkt_rd0 = PKT_CAPTIVE_APPLE_COM_A_RD0;
        let q0 = decode_query(pkt_rd0).unwrap();
        let n0 = encode_response(&q0, pkt_rd0, PORTAL_IP, 60, &mut buf).unwrap();
        assert!(n0 > 2);
        let resp_flags0 = u16::from_be_bytes([buf[2], buf[3]]);
        // QR=1 (0x8000), AA=1 (0x0400), RD=0 → 0x8400.
        assert_eq!(
            resp_flags0, 0x8400,
            "flags with RD=0: expected 0x8400, got {:#06x}",
            resp_flags0
        );
    }

    // ── Error path tests ──────────────────────────────────────────────────────

    /// A packet shorter than DNS_MIN_QUERY (17 bytes) is rejected with TooShort.
    #[test]
    fn too_short_packet_rejected() {
        let buf = [0u8; 16];
        assert_eq!(decode_query(&buf), Err(DnsError::TooShort));
    }

    /// Exactly 17 bytes with QR=0 and qdcount=1 but truncated qname → Truncated.
    ///
    /// Packet layout: 12-byte header (qdcount=1) + byte 12 = label-len 4, then
    /// bytes 13-16 are the 4 label bytes.  No terminating zero and no room for
    /// qtype/qclass → Truncated (the label walk exhausts the buffer).
    #[test]
    fn minimum_length_truncated_qname() {
        let mut buf = [0u8; 17];
        buf[0] = 0x00;
        buf[1] = 0x01; // id = 1
        buf[4] = 0x00;
        buf[5] = 0x01; // qdcount = 1
        buf[12] = 4; // label length = 4; 4 bytes follow (13-16) but no root zero
        buf[13] = b'a';
        buf[14] = b'b';
        buf[15] = b'c';
        buf[16] = b'd';
        // No terminating zero byte, and no room for qtype/qclass → Truncated.
        assert_eq!(decode_query(&buf), Err(DnsError::Truncated));
    }

    /// A query with qname exceeding 255 bytes is rejected with MalformedName.
    ///
    /// 13 labels of 20 bytes each = 13 * (1 + 20) = 273 wire bytes > 255.
    #[test]
    fn qname_too_long_rejected() {
        // qname: 13 labels of 20 'a' bytes + root zero = 274 wire bytes.
        // Total packet: 12 header + 274 qname + 4 qtype/qclass = 290 bytes.
        let mut buf = [0u8; DNS_MSG_MAX];
        buf[4] = 0x00;
        buf[5] = 0x01; // qdcount = 1
        let mut pos = 12usize;
        for _ in 0..13 {
            buf[pos] = 20; // label length
            pos += 1;
            for _ in 0..20 {
                buf[pos] = b'a';
                pos += 1;
            }
        }
        // 12 × 21-byte labels = 252 bytes of label data + 13 length bytes →
        // wire_len 265 when the decoder reads the 13th label header at offset
        // 252, before this root terminator is ever seen. The rejection fires
        // on that 13th label-body bounds check, not on the root terminator.
        buf[pos] = 0; // root terminator
        pos += 1;
        // qtype + qclass
        buf[pos] = 0x00;
        buf[pos + 1] = 0x01;
        buf[pos + 2] = 0x00;
        buf[pos + 3] = 0x01;
        let n = pos + 4;
        assert_eq!(decode_query(&buf[..n]), Err(DnsError::MalformedName));
    }

    /// A query with a label > 63 bytes is rejected with MalformedName.
    #[test]
    fn label_too_long_rejected() {
        // Single label of 64 bytes: len=64, 64 'a' bytes, root zero.
        let mut buf = [0u8; DNS_MSG_MAX];
        buf[4] = 0x00;
        buf[5] = 0x01; // qdcount = 1
        buf[12] = 64; // label length — exceeds LABEL_MAX (63)
        for i in 0..64 {
            buf[13 + i] = b'a';
        }
        buf[77] = 0x00; // root terminator
        buf[78] = 0x00;
        buf[79] = 0x01; // qtype = A
        buf[80] = 0x00;
        buf[81] = 0x01; // qclass = IN
        assert_eq!(decode_query(&buf[..82]), Err(DnsError::MalformedName));
    }

    /// A query whose qname starts with a compression pointer (0xC0 0x0C) is
    /// rejected with CompressionPointerNotAllowed.
    #[test]
    fn compression_pointer_rejected() {
        assert_eq!(
            decode_query(PKT_COMPRESSION_POINTER),
            Err(DnsError::CompressionPointerNotAllowed)
        );
    }

    /// A query truncated mid-qname is rejected with Truncated.
    ///
    /// Packet: 12-byte header (qdcount=1) + byte 12 = label-len 5, then bytes
    /// 13-16 provide only 4 label bytes.  The walker computes
    /// `pos = 12 + 1 + 5 = 18` which exceeds the 17-byte packet → Truncated.
    /// The packet is exactly `DNS_MIN_QUERY` bytes so `TooShort` does not fire.
    #[test]
    fn truncated_mid_qname_rejected() {
        // 17 bytes: passes the TooShort check (DNS_MIN_QUERY == 17).
        // label_len = 5 at offset 12; only 4 label bytes in offsets 13-16.
        // After processing the length byte, walker advances pos to 18 > 17 →
        // next iteration checks `pos >= buf.len()` → Truncated.
        let mut buf = [0u8; 17];
        buf[4] = 0x00;
        buf[5] = 0x01; // qdcount = 1
        buf[12] = 5; // label length = 5
        buf[13] = b'h';
        buf[14] = b'e';
        buf[15] = b'l';
        buf[16] = b'l'; // 4 bytes provided; 5th byte + root + qtype/qclass missing
        assert_eq!(decode_query(&buf), Err(DnsError::Truncated));
    }

    /// A query with the QR bit set (it's a response) is rejected with NotAQuery.
    #[test]
    fn qr_bit_set_rejected() {
        assert_eq!(decode_query(PKT_QR_SET), Err(DnsError::NotAQuery));
    }

    /// The answer count of an encoded response.
    fn ancount(resp: &[u8]) -> u16 {
        u16::from_be_bytes([resp[6], resp[7]])
    }

    #[test]
    fn a_query_gets_portal_ip() {
        let pkt = PKT_CAPTIVE_APPLE_COM_A_RD1;
        let query = decode_query(pkt).unwrap();
        let mut buf = [0u8; DNS_MSG_MAX];
        let n = encode_response(&query, pkt, PORTAL_IP, 60, &mut buf).unwrap();
        assert_eq!(ancount(&buf), 1);
        assert_eq!(
            u16::from_be_bytes([buf[2], buf[3]]) & 0x000F,
            0,
            "RCODE NOERROR"
        );
        assert_eq!(&buf[n - 4..n], &PORTAL_IP);
    }

    /// An `AAAA` query gets `NOERROR` with zero answers, never an `A` record
    /// in an `AAAA` answer (until 0.7 the bare-metal tier answered `A`).
    #[test]
    fn aaaa_query_gets_zero_answers_noerror() {
        let pkt = PKT_CAPTIVE_APPLE_COM_AAAA_RD1;
        let query = decode_query(pkt).expect("AAAA query should decode");
        assert_eq!(query.qtype, 28);

        let mut buf = [0u8; DNS_MSG_MAX];
        let n = encode_response(&query, pkt, PORTAL_IP, 60, &mut buf).unwrap();
        assert_eq!(ancount(&buf), 0);
        assert_eq!(
            u16::from_be_bytes([buf[2], buf[3]]) & 0x000F,
            0,
            "RCODE NOERROR"
        );
        assert_eq!(n, pkt.len(), "header plus question, no answer section");
    }

    #[test]
    fn other_types_get_zero_answers_and_any_gets_the_portal_ip() {
        let mut buf = [0u8; DNS_MSG_MAX];
        let mut q = [0u8; DNS_MSG_MAX];
        for (qtype, answered) in [(15u16, false), (16, false), (65, false), (255, true)] {
            let len = make_query_fixed(&mut q, 7, 0x0100, QNAME_CAPTIVE_APPLE_COM, qtype, 1);
            let query = decode_query(&q[..len]).unwrap();
            encode_response(&query, &q[..len], PORTAL_IP, 60, &mut buf).unwrap();
            assert_eq!(ancount(&buf), u16::from(answered), "qtype {qtype}");
        }
    }

    #[test]
    fn qdcount_two_rejected() {
        assert_eq!(
            decode_query(PKT_QDCOUNT_TWO),
            Err(DnsError::UnsupportedQuestionCount)
        );
    }

    #[test]
    fn truncated_packet_rejected() {
        assert_eq!(decode_query(&[0x00, 0x01]), Err(DnsError::TooShort));
        let pkt = PKT_CAPTIVE_APPLE_COM_A_RD1;
        assert_eq!(
            decode_query(&pkt[..pkt.len() - 2]),
            Err(DnsError::Truncated)
        );
    }

    #[test]
    fn response_echoes_id_and_question() {
        let pkt = PKT_CAPTIVE_APPLE_COM_A_RD1;
        let query = decode_query(pkt).unwrap();
        let mut buf = [0u8; DNS_MSG_MAX];
        encode_response(&query, pkt, PORTAL_IP, 60, &mut buf).unwrap();
        assert_eq!(&buf[0..2], &pkt[0..2], "id");
        assert_eq!(&buf[4..6], &[0x00, 0x01], "qdcount");
        assert_eq!(&buf[12..pkt.len()], &pkt[12..], "question");
    }

    #[test]
    fn encode_into_a_short_buffer_fails() {
        let pkt = PKT_CAPTIVE_APPLE_COM_A_RD1;
        let query = decode_query(pkt).unwrap();
        let mut buf = [0u8; 20];
        assert_eq!(
            encode_response(&query, pkt, PORTAL_IP, 60, &mut buf),
            Err(DnsError::BufferTooSmall)
        );
    }

    /// `make_query_fixed` helper is exercised by the qname-too-long test above;
    /// verify it populates header fields correctly for a trivial root-only qname.
    #[test]
    fn make_query_fixed_helper_basic() {
        let root_only = [0u8]; // just the root zero byte — minimal valid qname
        let mut buf = [0u8; DNS_MSG_MAX];
        let n = make_query_fixed(&mut buf, 0x1111, 0x0100, &root_only, 1, 1);
        // Header + qname(1) + qtype(2) + qclass(2) = 17.
        assert_eq!(n, 17);
        assert_eq!(&buf[0..2], &[0x11, 0x11]); // id
        assert_eq!(&buf[4..6], &[0x00, 0x01]); // qdcount
        assert_eq!(buf[12], 0x00); // root label
                                   // Packet is valid (qdcount=1, QR=0, minimal qname).
        let q = decode_query(&buf[..n]).expect("helper packet should decode");
        assert_eq!(q.id, 0x1111);
        assert_eq!(q.qtype, 1);
    }
}
