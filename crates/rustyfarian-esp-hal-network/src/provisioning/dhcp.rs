//! Minimal DHCP server substrate for the SoftAP captive-portal (ADR 015 §3
//! hand-rolled fallback; promoted from `rustyfarian-esp-hal-network` in Phase 2B).
//!
//! ## Why it exists
//!
//! The `edge-net` family (`edge-dhcp`, `edge-http`, `edge-captive`) was the
//! preferred Phase 2 substrate (ADR 015 §3), but its `embassy-sync 0.7`
//! dependency conflicts with the workspace-pinned `embassy-sync 0.8`.
//! ADR 015 §3 explicitly permits the hand-rolled fallback to proceed without a
//! new ADR — the architectural commitment is the private-substrate boundary,
//! not the crate family.
//!
//! ## What it covers
//!
//! A minimal RFC 2131 DHCP server handling the
//! DISCOVER → OFFER → REQUEST → ACK exchange for a single-AP captive-portal
//! scenario.  Out of scope: DECLINE, INFORM, multi-server arbitration,
//! BOOTP relay.
//!
//! ## Protocol coverage
//!
//! Options sent (OFFER and ACK):
//! - Option 53: DHCP Message Type
//! - Option 54: Server Identifier (AP_IP)
//! - Option 51: IP Address Lease Time
//! - Option 1:  Subnet Mask (255.255.255.0)
//! - Option 3:  Router (AP_IP)
//! - Option 6:  DNS Server (AP_IP — DNS catch-all in a future spike)
//! - Option 255: End
//!
//! Options read (DISCOVER and REQUEST):
//! - Option 53: DHCP Message Type (gates the state machine)
//! - Option 50: Requested IP Address (honoured if in pool and free)
//! - Option 54: Server Identifier (REQUEST must target our IP; else NAK)
//! - Option 61: Client Identifier (optional; falls back to `chaddr`)

// When building without the embassy + chip features the async `run` function
// and its UdpSocket usage are compiled away.  Allow dead-code on the types
// that remain so clippy -D warnings does not fail on stub/host builds.
#![cfg_attr(
    not(all(feature = "embassy", any(feature = "esp32c3", feature = "esp32c6"))),
    allow(dead_code)
)]

// ── DHCP wire constants ───────────────────────────────────────────────────────

/// Standard DHCP server port.
const DHCP_SERVER_PORT: u16 = 67;
/// Standard DHCP client port (reply destination).
const DHCP_CLIENT_PORT: u16 = 68;

/// RFC 2131 minimum BOOTP packet size (BOOTP fixed header = 236 bytes).
const BOOTP_FIXED_LEN: usize = 236;
/// DHCP magic cookie appended immediately after the BOOTP header.
const MAGIC_COOKIE: [u8; 4] = [0x63, 0x82, 0x53, 0x63];
/// Packet buffer size — 548 bytes (BOOTP 236 + magic 4 + options 308).
///
/// Used by the encode helpers and the on-stack `tx_pkt`; the server only
/// emits OFFER, ACK, and NAK with a fixed option set well under this cap.
pub(crate) const PACKET_BUF: usize = 548;

/// Per-direction UDP socket buffer length (bytes), shared by the
/// `StaticCell` RX/TX buffers passed to `UdpSocket::new` and by the
/// on-stack `rx_pkt` array the loop reads into.
///
/// Keeping the two coupled at the source — rather than via comments — means
/// a future tuning pass cannot raise one without raising the other and
/// silently reintroducing the 548-vs-1024 truncation hazard the Phase 2A
/// hardening closed.  Option-heavy DHCP REQUESTs (DDNS, vendor-class
/// identifiers, PXE) routinely exceed the BOOTP 548-byte minimum, so the
/// receive buffer must match the socket buffer exactly.
pub(crate) const SOCKET_BUF_LEN: usize = 1024;

/// BOOTP op-codes.
pub(crate) const OP_REQUEST: u8 = 1;
pub(crate) const OP_REPLY: u8 = 2;
/// Ethernet hardware type.
const HTYPE_ETHERNET: u8 = 1;
/// Ethernet address length (6 bytes MAC).
const HLEN_ETHERNET: u8 = 6;

// ── DHCP option tag constants ────────────────────────────────────────────────

const OPT_PAD: u8 = 0;
const OPT_SUBNET_MASK: u8 = 1;
const OPT_ROUTER: u8 = 3;
const OPT_DNS: u8 = 6;
const OPT_REQUESTED_IP: u8 = 50;
const OPT_LEASE_TIME: u8 = 51;
const OPT_MSG_TYPE: u8 = 53;
const OPT_SERVER_ID: u8 = 54;
const OPT_CLIENT_ID: u8 = 61;
const OPT_END: u8 = 255;

// ── DHCP message-type values ─────────────────────────────────────────────────

pub(crate) const MSG_DISCOVER: u8 = 1;
pub(crate) const MSG_OFFER: u8 = 2;
pub(crate) const MSG_REQUEST: u8 = 3;
pub(crate) const MSG_DECLINE: u8 = 4;
pub(crate) const MSG_ACK: u8 = 5;
pub(crate) const MSG_NAK: u8 = 6;

// ── Public types ──────────────────────────────────────────────────────────────

/// IPv4 address — four octets in network (big-endian) order.
///
/// A newtype over `[u8; 4]` that keeps the host-testable codec layer free of
/// `embassy-net` types.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct Ipv4Addr(pub [u8; 4]);

impl Ipv4Addr {
    /// Constructs an `Ipv4Addr` from four decimal octets.
    pub const fn new(a: u8, b: u8, c: u8, d: u8) -> Self {
        Self([a, b, c, d])
    }

    /// Returns the raw byte representation.
    pub const fn octets(self) -> [u8; 4] {
        self.0
    }

    /// Returns `true` if this address is the unspecified address (`0.0.0.0`).
    pub const fn is_unspecified(self) -> bool {
        matches!(self.0, [0, 0, 0, 0])
    }
}

impl core::fmt::Display for Ipv4Addr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{}.{}.{}", self.0[0], self.0[1], self.0[2], self.0[3])
    }
}

// ── DhcpServerConfig ─────────────────────────────────────────────────────────

/// Configuration for the minimal DHCP server.
///
/// All fields use network-order `Ipv4Addr` values.  The default matches the
/// standard captive-portal `192.168.4.x/24` subnet with the AP at
/// `192.168.4.1` and a pool of `192.168.4.10` – `192.168.4.20`.
pub struct DhcpServerConfig {
    /// IP address of the DHCP server itself (= the SoftAP IP, `192.168.4.1`).
    pub server_ip: Ipv4Addr,
    /// Subnet mask applied to OFFER and ACK responses (`255.255.255.0`).
    pub subnet_mask: Ipv4Addr,
    /// First IP address in the dynamic pool (`192.168.4.10`).
    pub pool_start: Ipv4Addr,
    /// Last IP address in the dynamic pool, inclusive (`192.168.4.20`).
    pub pool_end: Ipv4Addr,
    /// Lease duration in seconds (`300` = 5 minutes).
    pub lease_secs: u32,
}

impl Default for DhcpServerConfig {
    fn default() -> Self {
        Self {
            server_ip: Ipv4Addr::new(192, 168, 4, 1),
            subnet_mask: Ipv4Addr::new(255, 255, 255, 0),
            pool_start: Ipv4Addr::new(192, 168, 4, 10),
            pool_end: Ipv4Addr::new(192, 168, 4, 20),
            lease_secs: 300,
        }
    }
}

// ── DhcpError ─────────────────────────────────────────────────────────────────

/// Errors produced by the DHCP codec.
#[derive(Debug, PartialEq, Eq)]
pub enum DhcpError {
    /// Packet is shorter than the BOOTP minimum (240 bytes with magic cookie).
    TooShort,
    /// Magic cookie at offset 236 does not equal `0x63 0x82 0x53 0x63`.
    BadMagicCookie,
    /// `htype` field is not `1` (Ethernet) or `hlen` is not `6`.
    BadHardwareType,
    /// The DHCP Message Type option (53) is absent.
    MissingMessageType,
    /// Options encoding is malformed (truncated TLV).
    MalformedOptions,
    /// Encode buffer is too small to hold the serialised message.
    BufferTooSmall,
}

// ── DHCP parsed options ───────────────────────────────────────────────────────

/// The small set of DHCP options the server reads from incoming packets.
///
/// The server never reads Option 51 (Lease Time) from clients — clients do not
/// send it back.  Server-generated lease time is encoded only in OFFER and ACK
/// via [`encode_offer_or_ack`].
#[derive(Default, Debug, PartialEq, Eq)]
pub struct ParsedOptions {
    /// Option 53 — mandatory; gates the state machine.
    pub message_type: Option<u8>,
    /// Option 50 — requested IP address.
    pub requested_ip: Option<Ipv4Addr>,
    /// Option 54 — server identifier.
    pub server_id: Option<Ipv4Addr>,
    /// Option 61 — client identifier raw bytes (first 16, length in `client_id_len`).
    pub client_id: Option<[u8; 16]>,
    /// Number of valid bytes in `client_id`.
    pub client_id_len: u8,
}

// ── DhcpMessage ──────────────────────────────────────────────────────────────

/// Parsed representation of a DHCP packet.
///
/// Carries only the fields the server state-machine actually inspects.  The
/// `sname` and `file` BOOTP fields are ignored on decode and zeroed on encode.
#[derive(Debug, PartialEq, Eq)]
pub struct DhcpMessage {
    /// BOOTP op code.  `1` = BOOTREQUEST, `2` = BOOTREPLY.
    pub op: u8,
    /// Hardware address type — always `1` (Ethernet) for Wi-Fi.
    pub htype: u8,
    /// Hardware address length — always `6` for Ethernet MACs.
    pub hlen: u8,
    /// Transaction ID — echoed back in replies.
    pub xid: u32,
    /// Client IP address (non-zero only when client already has an address).
    pub ciaddr: Ipv4Addr,
    /// "Your" (offered / acked) IP address — filled by the server in replies.
    pub yiaddr: Ipv4Addr,
    /// Server IP address — filled by the server in replies.
    pub siaddr: Ipv4Addr,
    /// Client hardware (MAC) address.  The BOOTP `chaddr` field is 16 bytes;
    /// we store only the first 6 (the actual MAC) and zero-pad the rest on
    /// encode.
    pub chaddr: [u8; 6],
    /// Decoded DHCP options.
    pub options: ParsedOptions,
}

// ── Codec — decode ────────────────────────────────────────────────────────────

/// Decodes a raw UDP payload into a [`DhcpMessage`].
///
/// Returns `Err` for:
/// - packets shorter than the BOOTP minimum (236 header + 4 magic = 240 bytes)
/// - a missing or wrong magic cookie
/// - non-Ethernet `htype` / `hlen`
/// - a missing Option 53 (DHCP Message Type)
/// - malformed TLV encoding (truncated length + value)
///
/// Unknown option tags are **skipped** per RFC 2131 — the decoder never fails
/// on an option it does not recognise.
pub fn decode(buf: &[u8]) -> Result<DhcpMessage, DhcpError> {
    let min_len = BOOTP_FIXED_LEN + MAGIC_COOKIE.len();
    if buf.len() < min_len {
        return Err(DhcpError::TooShort);
    }

    // Validate magic cookie at offset 236.
    if buf[BOOTP_FIXED_LEN..BOOTP_FIXED_LEN + 4] != MAGIC_COOKIE {
        return Err(DhcpError::BadMagicCookie);
    }

    let htype = buf[1];
    let hlen = buf[2];
    if htype != HTYPE_ETHERNET || hlen != HLEN_ETHERNET {
        return Err(DhcpError::BadHardwareType);
    }

    let op = buf[0];
    let xid = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);

    // ciaddr at offset 12, yiaddr at 16, siaddr at 20.
    let ciaddr = Ipv4Addr([buf[12], buf[13], buf[14], buf[15]]);
    let yiaddr = Ipv4Addr([buf[16], buf[17], buf[18], buf[19]]);
    let siaddr = Ipv4Addr([buf[20], buf[21], buf[22], buf[23]]);

    // chaddr at offset 28, 16-byte BOOTP field; we take the first 6 bytes.
    let mut chaddr = [0u8; 6];
    chaddr.copy_from_slice(&buf[28..34]);

    // Parse options starting at offset 240 (BOOTP 236 + magic 4).
    let options_buf = &buf[BOOTP_FIXED_LEN + 4..];
    let options = parse_options(options_buf)?;

    if options.message_type.is_none() {
        return Err(DhcpError::MissingMessageType);
    }

    Ok(DhcpMessage {
        op,
        htype,
        hlen,
        xid,
        ciaddr,
        yiaddr,
        siaddr,
        chaddr,
        options,
    })
}

/// Parses the DHCP options region (after the magic cookie) into a
/// [`ParsedOptions`] struct.
///
/// Unknown tags are skipped.  Returns `Err(DhcpError::MalformedOptions)` only
/// when a length field reaches beyond the end of the buffer.
fn parse_options(buf: &[u8]) -> Result<ParsedOptions, DhcpError> {
    let mut opts = ParsedOptions::default();
    let mut i = 0;

    while i < buf.len() {
        let tag = buf[i];
        i += 1;

        match tag {
            OPT_END => break,
            OPT_PAD => continue,
            _ => {}
        }

        // Every non-pad, non-end option: read the length byte.
        if i >= buf.len() {
            return Err(DhcpError::MalformedOptions);
        }
        let len = buf[i] as usize;
        i += 1;

        // Validate that the value fits in the remaining buffer.
        if i + len > buf.len() {
            return Err(DhcpError::MalformedOptions);
        }
        let val = &buf[i..i + len];
        i += len;

        match tag {
            OPT_MSG_TYPE if len >= 1 => {
                opts.message_type = Some(val[0]);
            }
            OPT_REQUESTED_IP if len == 4 => {
                opts.requested_ip = Some(Ipv4Addr([val[0], val[1], val[2], val[3]]));
            }
            OPT_SERVER_ID if len == 4 => {
                opts.server_id = Some(Ipv4Addr([val[0], val[1], val[2], val[3]]));
            }
            OPT_CLIENT_ID => {
                let copy_len = len.min(16);
                let mut id = [0u8; 16];
                id[..copy_len].copy_from_slice(&val[..copy_len]);
                opts.client_id = Some(id);
                opts.client_id_len = copy_len as u8;
            }
            // All other option tags are skipped per RFC 2131 §4.
            _ => {}
        }
    }

    Ok(opts)
}

// ── Codec — encode ────────────────────────────────────────────────────────────

/// Appends a DHCP TLV option to `buf` at `offset`.
///
/// Returns the new write offset after the option, or
/// `Err(DhcpError::BufferTooSmall)` if there is insufficient space.
fn write_option(buf: &mut [u8], offset: usize, tag: u8, val: &[u8]) -> Result<usize, DhcpError> {
    let needed = 2 + val.len(); // tag byte + length byte + value
    if offset + needed > buf.len() {
        return Err(DhcpError::BufferTooSmall);
    }
    buf[offset] = tag;
    buf[offset + 1] = val.len() as u8;
    buf[offset + 2..offset + 2 + val.len()].copy_from_slice(val);
    Ok(offset + needed)
}

/// Encodes a [`DhcpMessage`] into `buf`.
///
/// Only the BOOTP fixed header, the magic cookie, Option 53 (Message Type),
/// and the End option are written.  For OFFER and ACK use
/// [`encode_offer_or_ack`], which writes the full option set.
///
/// Returns the number of bytes written, or `Err(DhcpError::BufferTooSmall)`.
///
/// The `sname` and `file` fields are zeroed.  The `chaddr` is placed in the
/// first 6 bytes of the 16-byte BOOTP chaddr field; the remaining 10 bytes
/// are zero-padded.
// Only used by the round-trip unit test; gated to avoid a dead-code warning
// in release builds where the async server uses encode_offer_or_ack directly.
#[cfg(test)]
pub(crate) fn encode(msg: &DhcpMessage, buf: &mut [u8]) -> Result<usize, DhcpError> {
    let min_len = BOOTP_FIXED_LEN + 4; // BOOTP header + magic cookie
    if buf.len() < min_len {
        return Err(DhcpError::BufferTooSmall);
    }

    // Zero the buffer so sname/file are clean and unused bytes are predictable.
    let zero_len = PACKET_BUF.min(buf.len());
    buf[..zero_len].fill(0);

    buf[0] = msg.op;
    buf[1] = msg.htype;
    buf[2] = msg.hlen;
    buf[3] = 0; // hops
    buf[4..8].copy_from_slice(&msg.xid.to_be_bytes());
    // secs = 0 (bytes 8-9 already zeroed).
    // Broadcast flag: set bit 15 of flags when ciaddr is 0.0.0.0 so the reply
    // is sent as a Layer-2 broadcast before the client has an IP address
    // (RFC 2131 §4.1).
    if msg.ciaddr.is_unspecified() {
        buf[10] = 0x80;
        buf[11] = 0x00;
    }
    buf[12..16].copy_from_slice(&msg.ciaddr.octets());
    buf[16..20].copy_from_slice(&msg.yiaddr.octets());
    buf[20..24].copy_from_slice(&msg.siaddr.octets());
    // giaddr = 0 (bytes 24-27 already zeroed).
    buf[28..34].copy_from_slice(&msg.chaddr);
    // chaddr[6..16] zeroed; sname[64] zeroed; file[128] zeroed.

    // Magic cookie at offset 236.
    buf[BOOTP_FIXED_LEN..BOOTP_FIXED_LEN + 4].copy_from_slice(&MAGIC_COOKIE);

    let mut pos = BOOTP_FIXED_LEN + 4;

    // Option 53 — DHCP Message Type (always first per RFC 2131).
    if let Some(mt) = msg.options.message_type {
        pos = write_option(buf, pos, OPT_MSG_TYPE, &[mt])?;
    }

    // Option 255 — End.
    if pos >= buf.len() {
        return Err(DhcpError::BufferTooSmall);
    }
    buf[pos] = OPT_END;
    pos += 1;

    Ok(pos)
}

/// Encodes a DHCP OFFER or ACK reply with the full option set.
///
/// Writes Options 53, 54 (Server ID), 51 (Lease Time), 1 (Subnet Mask),
/// 3 (Router), 6 (DNS), and 255 (End).
///
/// Returns the number of bytes written.
pub fn encode_offer_or_ack(
    msg: &DhcpMessage,
    buf: &mut [u8],
    server_ip: Ipv4Addr,
    subnet_mask: Ipv4Addr,
    lease_secs: u32,
) -> Result<usize, DhcpError> {
    let min_len = BOOTP_FIXED_LEN + 4;
    if buf.len() < min_len {
        return Err(DhcpError::BufferTooSmall);
    }

    let zero_len = PACKET_BUF.min(buf.len());
    buf[..zero_len].fill(0);

    buf[0] = msg.op;
    buf[1] = msg.htype;
    buf[2] = msg.hlen;
    buf[3] = 0;
    buf[4..8].copy_from_slice(&msg.xid.to_be_bytes());
    if msg.ciaddr.is_unspecified() {
        buf[10] = 0x80;
        buf[11] = 0x00;
    }
    buf[12..16].copy_from_slice(&msg.ciaddr.octets());
    buf[16..20].copy_from_slice(&msg.yiaddr.octets());
    buf[20..24].copy_from_slice(&server_ip.octets());
    buf[28..34].copy_from_slice(&msg.chaddr);

    buf[BOOTP_FIXED_LEN..BOOTP_FIXED_LEN + 4].copy_from_slice(&MAGIC_COOKIE);

    let mut pos = BOOTP_FIXED_LEN + 4;

    // Option 53 — Message Type.
    if let Some(mt) = msg.options.message_type {
        pos = write_option(buf, pos, OPT_MSG_TYPE, &[mt])?;
    }
    // Option 54 — Server Identifier.
    pos = write_option(buf, pos, OPT_SERVER_ID, &server_ip.octets())?;
    // Option 51 — Lease Time.
    pos = write_option(buf, pos, OPT_LEASE_TIME, &lease_secs.to_be_bytes())?;
    // Option 1 — Subnet Mask.
    pos = write_option(buf, pos, OPT_SUBNET_MASK, &subnet_mask.octets())?;
    // Option 3 — Router = server IP (the AP is the gateway).
    pos = write_option(buf, pos, OPT_ROUTER, &server_ip.octets())?;
    // Option 6 — DNS = server IP (DNS catch-all for captive-portal redirect).
    pos = write_option(buf, pos, OPT_DNS, &server_ip.octets())?;
    // Option 255 — End.
    if pos >= buf.len() {
        return Err(DhcpError::BufferTooSmall);
    }
    buf[pos] = OPT_END;
    pos += 1;

    Ok(pos)
}

/// Encodes a DHCP NAK reply (Option 53 = 6, Option 54 = server ID, End).
///
/// NAK replies are always broadcast (broadcast flag set), yiaddr = 0.
pub fn encode_nak(
    msg: &DhcpMessage,
    buf: &mut [u8],
    server_ip: Ipv4Addr,
) -> Result<usize, DhcpError> {
    let min_len = BOOTP_FIXED_LEN + 4;
    if buf.len() < min_len {
        return Err(DhcpError::BufferTooSmall);
    }

    let zero_len = PACKET_BUF.min(buf.len());
    buf[..zero_len].fill(0);

    buf[0] = OP_REPLY;
    buf[1] = msg.htype;
    buf[2] = msg.hlen;
    buf[3] = 0;
    buf[4..8].copy_from_slice(&msg.xid.to_be_bytes());
    // NAK: broadcast flag always set; ciaddr, yiaddr, siaddr all zero.
    buf[10] = 0x80;
    buf[11] = 0x00;
    buf[28..34].copy_from_slice(&msg.chaddr);

    buf[BOOTP_FIXED_LEN..BOOTP_FIXED_LEN + 4].copy_from_slice(&MAGIC_COOKIE);

    let mut pos = BOOTP_FIXED_LEN + 4;
    pos = write_option(buf, pos, OPT_MSG_TYPE, &[MSG_NAK])?;
    pos = write_option(buf, pos, OPT_SERVER_ID, &server_ip.octets())?;
    if pos >= buf.len() {
        return Err(DhcpError::BufferTooSmall);
    }
    buf[pos] = OPT_END;
    pos += 1;

    Ok(pos)
}

// ── Lease table ───────────────────────────────────────────────────────────────

/// Maximum number of simultaneous leases in the default pool (`.10` to `.20`).
const POOL_SIZE: usize = 11;

/// A single DHCP lease entry.
///
/// `offered_at_secs` is a raw seconds counter.  On bare-metal it is filled
/// from `embassy_time::Instant::now().as_secs()`; in host tests a monotonic
/// `u64` is injected directly so no embassy time driver is required.
#[derive(Copy, Clone, Debug)]
pub struct Lease {
    /// Client hardware address that holds this lease.
    pub mac: [u8; 6],
    /// Monotonic seconds at which the lease was offered or last extended.
    pub offered_at_secs: u64,
}

/// Fixed-size DHCP lease table for the default 11-address pool.
pub struct LeaseTable {
    entries: [Option<Lease>; POOL_SIZE],
    /// First address in the pool — bound at construction; per-call sites cannot
    /// override it.  See `docs/project-lore.md` "esp-hal April 2026 Stack" for
    /// the rationale (the original API passed this per-call and caused the
    /// 2026-06-15 hardware-only-surfacing offer-the-server's-own-IP bug).
    pool_start: Ipv4Addr,
    /// Lease duration in seconds — bound at construction for the same reason.
    lease_secs: u32,
}

impl LeaseTable {
    /// Constructs an empty lease table bound to a specific pool start address
    /// and lease duration.
    ///
    /// `pool_start` is the first IP address handed out by this table; the
    /// table itself is `[Option<Lease>; POOL_SIZE]`, so the inclusive last
    /// address is `pool_start + POOL_SIZE - 1`.  `lease_secs` is the lifetime
    /// applied to each issued lease (used by [`Self::allocate`] for the
    /// expired-slot reuse policy).
    pub const fn new(pool_start: Ipv4Addr, lease_secs: u32) -> Self {
        Self {
            entries: [None; POOL_SIZE],
            pool_start,
            lease_secs,
        }
    }

    /// Returns the pool index for an IP address, or `None` if out of range.
    ///
    /// The pool is within one /24 subnet — only the last octet varies.
    fn index_of(&self, addr: Ipv4Addr) -> Option<usize> {
        if addr.0[0] != self.pool_start.0[0]
            || addr.0[1] != self.pool_start.0[1]
            || addr.0[2] != self.pool_start.0[2]
        {
            return None;
        }
        let offset = addr.0[3].checked_sub(self.pool_start.0[3])? as usize;
        if offset < POOL_SIZE {
            Some(offset)
        } else {
            None
        }
    }

    /// Returns the `Ipv4Addr` for pool slot `index`.
    fn addr_of(&self, index: usize) -> Ipv4Addr {
        Ipv4Addr([
            self.pool_start.0[0],
            self.pool_start.0[1],
            self.pool_start.0[2],
            self.pool_start.0[3].wrapping_add(index as u8),
        ])
    }

    /// Returns `true` if the lease has expired given `now_secs` and the
    /// table's configured lease duration.
    fn is_expired(&self, lease: &Lease, now_secs: u64) -> bool {
        now_secs.saturating_sub(lease.offered_at_secs) >= u64::from(self.lease_secs)
    }

    /// Looks up an existing lease entry for `mac` and returns its pool index.
    pub(crate) fn find_by_mac(&self, mac: &[u8; 6]) -> Option<usize> {
        self.entries
            .iter()
            .position(|e| e.is_some_and(|l| &l.mac == mac))
    }

    /// Allocates or reuses a pool slot for a client.
    ///
    /// Allocation policy (in order):
    /// 1. If the client's MAC already has a lease, reuse it and refresh the
    ///    timestamp.
    /// 2. If `requested` is in pool and the slot is free or expired, honour it.
    /// 3. Allocate the first free or expired slot.
    /// 4. If all slots hold valid leases, evict the oldest one.
    ///
    /// Returns the allocated `Ipv4Addr`, or `None` only in the pathological
    /// case where the table holds no `Some` entries — i.e. the for-loop in
    /// step 3 finds every slot free but the early-return doesn't fire, which
    /// can't happen with the current code; the `Option` is preserved as a
    /// safety hatch.
    pub fn allocate(
        &mut self,
        mac: &[u8; 6],
        requested: Option<Ipv4Addr>,
        now_secs: u64,
    ) -> Option<Ipv4Addr> {
        // 1. Existing lease for this MAC — reuse and refresh.
        if let Some(idx) = self.find_by_mac(mac) {
            self.entries[idx] = Some(Lease {
                mac: *mac,
                offered_at_secs: now_secs,
            });
            return Some(self.addr_of(idx));
        }

        // 2. Honour the requested IP if it is in pool and available.
        if let Some(req) = requested {
            if let Some(idx) = self.index_of(req) {
                let available = self.entries[idx].is_none_or(|l| self.is_expired(&l, now_secs));
                if available {
                    self.entries[idx] = Some(Lease {
                        mac: *mac,
                        offered_at_secs: now_secs,
                    });
                    return Some(req);
                }
            }
        }

        // 3. First free or expired slot.
        for idx in 0..POOL_SIZE {
            let available = self.entries[idx].is_none_or(|l| self.is_expired(&l, now_secs));
            if available {
                let addr = self.addr_of(idx);
                self.entries[idx] = Some(Lease {
                    mac: *mac,
                    offered_at_secs: now_secs,
                });
                return Some(addr);
            }
        }

        // 4. Pool exhausted — evict the oldest lease.
        let oldest_idx = self
            .entries
            .iter()
            .enumerate()
            .filter_map(|(i, e)| e.map(|l| (i, l.offered_at_secs)))
            .min_by_key(|&(_, ts)| ts)
            .map(|(i, _)| i)?;

        log::warn!(
            "DHCP: pool full — evicting oldest lease (slot {})",
            oldest_idx
        );

        let addr = self.addr_of(oldest_idx);
        self.entries[oldest_idx] = Some(Lease {
            mac: *mac,
            offered_at_secs: now_secs,
        });
        Some(addr)
    }
}

// ── REQUEST decision (pure, host-testable) ───────────────────────────────────

/// Outcome of evaluating a DHCP REQUEST under RFC 2131 §4.3.2.
///
/// The decision is derived from the parsed request, the current lease table,
/// and the server's identity.  The async loop in [`run`] interprets each
/// variant into a network action; the same enum is host-tested without an
/// `embassy-net` stack.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RequestOutcome {
    /// Reply with ACK and this address.
    Ack(Ipv4Addr),
    /// Reply with NAK — the requested IP cannot be granted.
    Nak,
    /// Silently drop the packet — bindingless REQUEST with no prior record
    /// (RFC 2131 §4.3.2 "If the DHCP server has no record of this client,
    /// then it MUST remain silent").
    Drop,
    /// Silently drop — Option 54 (Server Identifier) names a different server.
    Ignore,
}

/// Pure REQUEST decision.
///
/// Decision tree:
/// 1. Option 54 present and not equal to `server_ip` → [`RequestOutcome::Ignore`]
///    (the REQUEST is targeted at another DHCP server).
/// 2. Locate the address the client wants:
///    - Option 50 (Requested IP Address) present → use it
///      (SELECTING / INIT-REBOOT path after a prior OFFER).
///    - Option 50 absent but `ciaddr` non-zero → use `ciaddr`
///      (RENEWING / REBINDING path).
///    - Neither present → bindingless REQUEST.
/// 3. Bindingless REQUEST:
///    - If `chaddr` has an existing lease → fall through to allocate, which
///      refreshes it (lenient — a real phone may re-REQUEST after losing
///      address state but keeping the MAC).
///    - Otherwise → [`RequestOutcome::Drop`] (§4.3.2 silent-server rule).
/// 4. Allocate (or refresh) via [`LeaseTable::allocate`]:
///    - If the allocated IP equals the explicitly requested one (or no
///      specific IP was requested) → [`RequestOutcome::Ack`].
///    - If the client insisted on a specific IP but the table allocated a
///      different one → [`RequestOutcome::Nak`].
///    - On pool exhaustion → [`RequestOutcome::Nak`].
///
/// # Mutation note — NAK-still-mutates is **preserved, not endorsed**
///
/// This function takes `&mut LeaseTable` because [`LeaseTable::allocate`]
/// mutates the table (claims a slot) even when this function returns
/// [`RequestOutcome::Nak`].
///
/// **This is temporary, not the long-term design.**  The current shape
/// preserves the prior spike behaviour where a NAK'd REQUEST still
/// records the offered slot under the client's MAC, so the next DISCOVER
/// from that MAC resolves to the same address (a happy-accident recovery
/// for malformed clients).  The behaviour is locked by the
/// `decide_request_nak_still_records_mac_lease` host test so any change
/// fails CI loudly rather than drifting silently.
///
/// Phase 2B portal promotion will decouple candidate-lookup from commit
/// (introduce `LeaseTable::probe` + `LeaseTable::commit_lease`); at that
/// point this function becomes side-effect-free on Nak/Drop/Ignore and
/// the lock-down test will be updated to assert the absence of the
/// mutation.  Until then: do not rely on this side effect outside the
/// happy-accident recovery, and do not extend the spike with new logic
/// that would compound the surprise.
pub(crate) fn decide_request(
    msg: &DhcpMessage,
    leases: &mut LeaseTable,
    server_ip: Ipv4Addr,
    now_secs: u64,
) -> RequestOutcome {
    if let Some(sid) = msg.options.server_id {
        if sid != server_ip {
            return RequestOutcome::Ignore;
        }
    }

    let explicit = msg.options.requested_ip.or_else(|| {
        if !msg.ciaddr.is_unspecified() {
            Some(msg.ciaddr)
        } else {
            None
        }
    });

    if explicit.is_none() && leases.find_by_mac(&msg.chaddr).is_none() {
        return RequestOutcome::Drop;
    }

    let Some(offered_ip) = leases.allocate(&msg.chaddr, explicit, now_secs) else {
        return RequestOutcome::Nak;
    };

    if let Some(req) = explicit {
        if req != offered_ip && !req.is_unspecified() {
            return RequestOutcome::Nak;
        }
    }

    RequestOutcome::Ack(offered_ip)
}

// ── Async server loop (bare-metal only) ──────────────────────────────────────

/// Runs the DHCP server on the given `embassy-net` stack.
///
/// This function never returns under normal operation.  It binds a UDP socket
/// on port 67, loops on `recv_from`, and replies with OFFER, ACK, or NAK
/// according to the RFC 2131 §4.3 state machine.
///
/// # Spawn this as a dedicated embassy task
///
/// ```ignore
/// use rustyfarian_esp_hal_network::provisioning::dhcp::{self, DhcpServerConfig};
///
/// #[embassy_executor::task]
/// async fn dhcp_task(stack: embassy_net::Stack<'static>) -> ! {
///     dhcp::run(stack, DhcpServerConfig::default()).await
/// }
/// ```
///
/// # Panic-free
///
/// Bind failure, send errors, and malformed packets are all logged at `warn`
/// and do not abort the server loop.
/// Reasons a configured DHCP pool fails `LeaseTable`'s shape constraints.
///
/// `LeaseTable` is a `[Option<Lease>; POOL_SIZE]` where `addr_of` derives
/// each pool slot by `wrapping_add`-ing the index onto `pool_start`'s last
/// octet and leaving the first three octets unchanged. The pool must
/// therefore lie wholly within a single /24 and span exactly `pool_size`
/// slots. A `pool_start[3] + pool_size - 1 > 255` overflow is structurally
/// unreachable under this contract — if it would happen, then by definition
/// `pool_end` either lands in the next /24 (caught by `CrossesSubnet`) or
/// the implied `end - start + 1` no longer equals `pool_size` (caught by
/// `SizeMismatch`), so no separate `LastOctetOverflow` variant is needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PoolGeometryError {
    SizeMismatch { configured: usize, required: usize },
    CrossesSubnet,
}

impl PoolGeometryError {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            PoolGeometryError::SizeMismatch { .. } => "configured pool size != POOL_SIZE",
            PoolGeometryError::CrossesSubnet => "pool crosses a /24 boundary",
        }
    }
}

/// Validate that the pool range fits `LeaseTable`'s within-/24 layout.
///
/// Pure helper extracted so the boundary cases can be locked by host tests
/// rather than only discovered at server startup. Returns `Ok(())` for the
/// well-formed default config and `Err` for every shape `LeaseTable` cannot
/// faithfully address.
pub(crate) fn validate_pool_geometry(
    pool_start: Ipv4Addr,
    pool_end: Ipv4Addr,
    pool_size: usize,
) -> Result<(), PoolGeometryError> {
    let start_u32 = u32::from_be_bytes(pool_start.0);
    let end_u32 = u32::from_be_bytes(pool_end.0);
    let configured = end_u32.saturating_sub(start_u32).saturating_add(1) as usize;
    if configured != pool_size {
        return Err(PoolGeometryError::SizeMismatch {
            configured,
            required: pool_size,
        });
    }
    if pool_start.0[0..3] != pool_end.0[0..3] {
        return Err(PoolGeometryError::CrossesSubnet);
    }
    Ok(())
}

#[cfg(all(feature = "embassy", any(feature = "esp32c3", feature = "esp32c6")))]
pub async fn run(stack: embassy_net::Stack<'static>, config: DhcpServerConfig) -> ! {
    use embassy_net::udp::{PacketMetadata, UdpSocket};
    use static_cell::StaticCell;

    // Validate the configured pool range against `LeaseTable`'s shape — the
    // table is a `[Option<Lease>; POOL_SIZE]` and `addr_of` only varies the
    // last octet via `wrapping_add`, so a misconfigured pool would silently
    // allocate the wrong IPs.  Refuse to start instead.
    if let Err(reason) = validate_pool_geometry(config.pool_start, config.pool_end, POOL_SIZE) {
        log::error!(
            "DHCP config invalid: pool_start={} pool_end={} — {} (LeaseTable is fixed at POOL_SIZE={}, lives within a single /24, and must not cross the last-octet 255 wraparound; Phase 2 will lift these constraints via const generics)",
            config.pool_start,
            config.pool_end,
            reason.as_str(),
            POOL_SIZE,
        );
        loop {
            embassy_time::Timer::after(embassy_time::Duration::from_secs(60)).await;
        }
    }

    // Static socket buffers — required because `UdpSocket::new` in
    // `embassy-net 0.8` internally transmutes the buffer slices to `'static`
    // (see `embassy_net::udp::UdpSocket::new` safety contract).
    static RX_META: StaticCell<[PacketMetadata; 4]> = StaticCell::new();
    static RX_BUF: StaticCell<[u8; SOCKET_BUF_LEN]> = StaticCell::new();
    static TX_META: StaticCell<[PacketMetadata; 4]> = StaticCell::new();
    static TX_BUF: StaticCell<[u8; SOCKET_BUF_LEN]> = StaticCell::new();

    let rx_meta = RX_META.init([PacketMetadata::EMPTY; 4]);
    let rx_buf = RX_BUF.init([0u8; SOCKET_BUF_LEN]);
    let tx_meta = TX_META.init([PacketMetadata::EMPTY; 4]);
    let tx_buf = TX_BUF.init([0u8; SOCKET_BUF_LEN]);

    let mut sock = UdpSocket::new(stack, rx_meta, rx_buf, tx_meta, tx_buf);

    match sock.bind(DHCP_SERVER_PORT) {
        Ok(()) => log::info!("DHCP server bound on port {}", DHCP_SERVER_PORT),
        Err(e) => {
            log::warn!(
                "DHCP server: failed to bind port {} ({:?}); server will not run",
                DHCP_SERVER_PORT,
                e
            );
            // Park the task rather than returning — callers rely on `-> !`.
            loop {
                embassy_time::Timer::after(embassy_time::Duration::from_secs(60)).await;
            }
        }
    }

    // `rx_pkt` is sized to `SOCKET_BUF_LEN` — the same constant that backs
    // the static UDP socket buffer above — so a future tuning pass cannot
    // raise the socket buffer without also raising the on-stack `rx_pkt`
    // and silently reintroducing the 548-vs-1024 truncation hazard.
    // `embassy-net` `recv_from` copies `min(datagram, rx_pkt)` and reports
    // the actual `n`, so the two must stay equal.
    //
    // `tx_pkt` stays at the BOOTP cap — the server only emits OFFER, ACK, and
    // NAK with the fixed option set encoded by `encode_offer_or_ack` /
    // `encode_nak`, all well under 548 B.
    let mut rx_pkt = [0u8; SOCKET_BUF_LEN];
    let mut tx_pkt = [0u8; PACKET_BUF];
    let mut leases = LeaseTable::new(config.pool_start, config.lease_secs);

    let server_ip = config.server_ip;

    loop {
        let (n, meta) = match sock.recv_from(&mut rx_pkt).await {
            Ok(r) => r,
            Err(e) => {
                log::warn!("DHCP recv_from error: {:?}", e);
                continue;
            }
        };

        let msg = match decode(&rx_pkt[..n]) {
            Ok(m) => m,
            Err(e) => {
                log::warn!("DHCP: malformed packet from {:?}: {:?}", meta.endpoint, e);
                continue;
            }
        };

        // Ignore BOOTREPLY packets (should not arrive on port 67).
        if msg.op != OP_REQUEST {
            log::trace!("DHCP: ignoring non-BOOTREQUEST packet (op={})", msg.op);
            continue;
        }

        let msg_type = match msg.options.message_type {
            Some(t) => t,
            None => {
                log::warn!("DHCP: packet with no message type — dropping");
                continue;
            }
        };

        log::trace!(
            "DHCP rx: type={} xid={:#010x} chaddr={:02x?}",
            msg_type,
            msg.xid,
            msg.chaddr
        );

        // `embassy_time::Instant::now().as_secs()` — requires a time driver to
        // be linked, which esp-rtos provides on bare-metal targets.
        let now_secs = embassy_time::Instant::now().as_secs();

        match msg_type {
            MSG_DISCOVER => {
                let offered_ip =
                    match leases.allocate(&msg.chaddr, msg.options.requested_ip, now_secs) {
                        Some(ip) => ip,
                        None => {
                            log::warn!("DHCP: pool exhausted — cannot send OFFER");
                            continue;
                        }
                    };

                let reply = DhcpMessage {
                    op: OP_REPLY,
                    htype: HTYPE_ETHERNET,
                    hlen: HLEN_ETHERNET,
                    xid: msg.xid,
                    ciaddr: Ipv4Addr::new(0, 0, 0, 0),
                    yiaddr: offered_ip,
                    siaddr: server_ip,
                    chaddr: msg.chaddr,
                    options: ParsedOptions {
                        message_type: Some(MSG_OFFER),
                        ..Default::default()
                    },
                };

                match encode_offer_or_ack(
                    &reply,
                    &mut tx_pkt,
                    server_ip,
                    config.subnet_mask,
                    config.lease_secs,
                ) {
                    Ok(len) => {
                        let dest = dhcp_broadcast_endpoint();
                        if let Err(e) = sock.send_to(&tx_pkt[..len], dest).await {
                            log::warn!("DHCP OFFER send failed: {:?}", e);
                        } else {
                            log::info!("DHCP OFFER: {} → chaddr={:02x?}", offered_ip, msg.chaddr);
                        }
                    }
                    Err(e) => log::warn!("DHCP OFFER encode failed: {:?}", e),
                }
            }

            MSG_REQUEST => match decide_request(&msg, &mut leases, server_ip, now_secs) {
                RequestOutcome::Ignore => {
                    // `decide_request` returns `Ignore` only when Option 54
                    // was present and did not match `server_ip`, so the
                    // `unwrap_or` arm of the format below is unreachable;
                    // it exists so the log call never panics if the
                    // decision tree gains a new path that maps to Ignore.
                    log::trace!(
                        "DHCP REQUEST for different server (client_server_id={}, local_server_ip={}) — ignoring (chaddr={:02x?})",
                        msg.options
                            .server_id
                            .unwrap_or(Ipv4Addr::new(0, 0, 0, 0)),
                        server_ip,
                        msg.chaddr,
                    );
                }
                RequestOutcome::Drop => {
                    log::trace!(
                            "DHCP REQUEST without binding (chaddr={:02x?}) — silent per RFC 2131 §4.3.2",
                            msg.chaddr
                        );
                }
                RequestOutcome::Nak => {
                    log::warn!(
                        "DHCP REQUEST cannot be satisfied — sending NAK (chaddr={:02x?})",
                        msg.chaddr
                    );
                    do_send_nak(&sock, &msg, server_ip, &mut tx_pkt).await;
                }
                RequestOutcome::Ack(offered_ip) => {
                    let reply = DhcpMessage {
                        op: OP_REPLY,
                        htype: HTYPE_ETHERNET,
                        hlen: HLEN_ETHERNET,
                        xid: msg.xid,
                        ciaddr: Ipv4Addr::new(0, 0, 0, 0),
                        yiaddr: offered_ip,
                        siaddr: server_ip,
                        chaddr: msg.chaddr,
                        options: ParsedOptions {
                            message_type: Some(MSG_ACK),
                            ..Default::default()
                        },
                    };

                    match encode_offer_or_ack(
                        &reply,
                        &mut tx_pkt,
                        server_ip,
                        config.subnet_mask,
                        config.lease_secs,
                    ) {
                        Ok(len) => {
                            let dest = dhcp_broadcast_endpoint();
                            if let Err(e) = sock.send_to(&tx_pkt[..len], dest).await {
                                log::warn!("DHCP ACK send failed: {:?}", e);
                            } else {
                                log::info!("DHCP ACK: {} → chaddr={:02x?}", offered_ip, msg.chaddr);
                            }
                        }
                        Err(e) => log::warn!("DHCP ACK encode failed: {:?}", e),
                    }
                }
            },

            MSG_DECLINE => {
                // Client says the offered IP is already in use — free the lease.
                if let Some(idx) = leases.find_by_mac(&msg.chaddr) {
                    leases.entries[idx] = None;
                    log::warn!("DHCP DECLINE from chaddr={:02x?} — lease freed", msg.chaddr);
                }
            }

            _ => {
                log::trace!("DHCP: unhandled message type {} — ignoring", msg_type);
            }
        }
    }
}

/// Returns the broadcast [`IpEndpoint`] used for all DHCP replies in this spike.
///
/// RFC 2131 §4.1 specifies the broadcast address for OFFER and ACK when the
/// client's broadcast flag is set (i.e. `ciaddr` is `0.0.0.0`).
///
/// # Intentional simplification: every reply is broadcast
///
/// This spike sends **every** OFFER / ACK / NAK to `255.255.255.255:68`,
/// even when the spec would allow unicast (a renewing client with a valid
/// `ciaddr` should receive replies at that address per RFC 2131 §4.3.2).
/// For a single-client captive-portal scenario this is acceptable — there
/// is exactly one phone associating at a time, no DHCP relay, and the AP
/// netif is the only adjacent broadcast domain.  Phase 2 proper, which
/// migrates this module into `rustyfarian-esp-hal-network`, should
/// add a unicast path for renew/rebind transitions.
#[cfg(all(feature = "embassy", any(feature = "esp32c3", feature = "esp32c6")))]
fn dhcp_broadcast_endpoint() -> embassy_net::IpEndpoint {
    use embassy_net::{IpAddress, IpEndpoint, Ipv4Address};
    IpEndpoint {
        addr: IpAddress::Ipv4(Ipv4Address::new(255, 255, 255, 255)),
        port: DHCP_CLIENT_PORT,
    }
}

/// Encodes and sends a DHCP NAK to `255.255.255.255:68`.
#[cfg(all(feature = "embassy", any(feature = "esp32c3", feature = "esp32c6")))]
async fn do_send_nak(
    sock: &embassy_net::udp::UdpSocket<'_>,
    msg: &DhcpMessage,
    server_ip: Ipv4Addr,
    tx_pkt: &mut [u8; PACKET_BUF],
) {
    match encode_nak(msg, tx_pkt, server_ip) {
        Ok(len) => {
            let dest = dhcp_broadcast_endpoint();
            if let Err(e) = sock.send_to(&tx_pkt[..len], dest).await {
                log::warn!("DHCP NAK send failed: {:?}", e);
            } else {
                log::info!("DHCP NAK → chaddr={:02x?}", msg.chaddr);
            }
        }
        Err(e) => log::warn!("DHCP NAK encode failed: {:?}", e),
    }
}

// ── Unit tests (host-testable codec + allocation policy) ─────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Test helpers ──────────────────────────────────────────────────────────

    /// Writes a minimal DHCPDISCOVER into `buf` and returns the byte count.
    fn make_discover(buf: &mut [u8; PACKET_BUF], xid: u32, mac: [u8; 6]) -> usize {
        buf.fill(0);
        buf[0] = OP_REQUEST;
        buf[1] = HTYPE_ETHERNET;
        buf[2] = HLEN_ETHERNET;
        buf[3] = 0;
        buf[4..8].copy_from_slice(&xid.to_be_bytes());
        buf[28..34].copy_from_slice(&mac);
        buf[BOOTP_FIXED_LEN..BOOTP_FIXED_LEN + 4].copy_from_slice(&MAGIC_COOKIE);
        let mut pos = BOOTP_FIXED_LEN + 4;
        buf[pos] = OPT_MSG_TYPE;
        buf[pos + 1] = 1;
        buf[pos + 2] = MSG_DISCOVER;
        pos += 3;
        buf[pos] = OPT_END;
        pos += 1;
        pos
    }

    // ── Codec round-trip tests ────────────────────────────────────────────────

    /// Encode → decode round-trip preserves all parsed fields.
    #[test]
    fn round_trip_basic() {
        let original = DhcpMessage {
            op: OP_REQUEST,
            htype: HTYPE_ETHERNET,
            hlen: HLEN_ETHERNET,
            xid: 0xDEAD_BEEF,
            ciaddr: Ipv4Addr::new(0, 0, 0, 0),
            yiaddr: Ipv4Addr::new(0, 0, 0, 0),
            siaddr: Ipv4Addr::new(0, 0, 0, 0),
            chaddr: [0x11, 0x22, 0x33, 0x44, 0x55, 0x66],
            options: ParsedOptions {
                message_type: Some(MSG_DISCOVER),
                ..Default::default()
            },
        };

        let mut buf = [0u8; PACKET_BUF];
        let len = encode(&original, &mut buf).expect("encode should succeed");
        let decoded = decode(&buf[..len]).expect("decode should succeed");

        assert_eq!(decoded.op, original.op);
        assert_eq!(decoded.xid, original.xid);
        assert_eq!(decoded.chaddr, original.chaddr);
        assert_eq!(decoded.options.message_type, original.options.message_type);
    }

    /// encode_offer_or_ack output can be decoded; Option 53 = OFFER, 54 present.
    #[test]
    fn offer_round_trip() {
        let server = Ipv4Addr::new(192, 168, 4, 1);
        let mask = Ipv4Addr::new(255, 255, 255, 0);
        let mac = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF];

        let offer = DhcpMessage {
            op: OP_REPLY,
            htype: HTYPE_ETHERNET,
            hlen: HLEN_ETHERNET,
            xid: 0x1234_5678,
            ciaddr: Ipv4Addr::new(0, 0, 0, 0),
            yiaddr: Ipv4Addr::new(192, 168, 4, 10),
            siaddr: server,
            chaddr: mac,
            options: ParsedOptions {
                message_type: Some(MSG_OFFER),
                ..Default::default()
            },
        };

        let mut buf = [0u8; PACKET_BUF];
        let len = encode_offer_or_ack(&offer, &mut buf, server, mask, 300)
            .expect("encode should succeed");

        // Verify Option 51 (Lease Time) bytes are present in the encoded output.
        // The option set is: 53(1) 54(4) 51(4) 1(4) 3(4) 6(4) 255
        // We can scan the raw options region for tag 51.
        let opts_region = &buf[BOOTP_FIXED_LEN + 4..len];
        let mut found_lease = false;
        let mut i = 0;
        while i < opts_region.len() {
            let tag = opts_region[i];
            if tag == OPT_END {
                break;
            }
            if tag == OPT_PAD {
                i += 1;
                continue;
            }
            let opt_len = opts_region[i + 1] as usize;
            if tag == OPT_LEASE_TIME && opt_len == 4 {
                let secs = u32::from_be_bytes([
                    opts_region[i + 2],
                    opts_region[i + 3],
                    opts_region[i + 4],
                    opts_region[i + 5],
                ]);
                assert_eq!(secs, 300);
                found_lease = true;
            }
            i += 2 + opt_len;
        }
        assert!(
            found_lease,
            "Option 51 (Lease Time) not found in encoded OFFER"
        );

        // Also verify the decoded fields (type, server id).
        let decoded = decode(&buf[..len]).expect("decode should succeed");
        assert_eq!(decoded.options.message_type, Some(MSG_OFFER));
        assert_eq!(decoded.options.server_id, Some(server));
    }

    /// Decode a hand-constructed DISCOVER (Wireshark-style byte construction).
    #[test]
    fn decode_hand_constructed_discover() {
        let mac = [0x00, 0x11, 0x22, 0x33, 0x44, 0x55];
        let xid: u32 = 0x3903_F326;
        let mut buf = [0u8; PACKET_BUF];
        make_discover(&mut buf, xid, mac);

        let msg = decode(&buf).expect("decode should succeed");
        assert_eq!(msg.op, OP_REQUEST);
        assert_eq!(msg.xid, xid);
        assert_eq!(msg.chaddr, mac);
        assert_eq!(msg.options.message_type, Some(MSG_DISCOVER));
    }

    /// Wrong magic cookie must be rejected with `BadMagicCookie`.
    #[test]
    fn wrong_magic_cookie_is_rejected() {
        let mut buf = [0u8; PACKET_BUF];
        buf[0] = OP_REQUEST;
        buf[1] = HTYPE_ETHERNET;
        buf[2] = HLEN_ETHERNET;
        // Magic cookie bytes remain zero — wrong.
        assert_eq!(decode(&buf), Err(DhcpError::BadMagicCookie));
    }

    /// Packet shorter than BOOTP minimum must be rejected with `TooShort`.
    #[test]
    fn too_short_packet_is_rejected() {
        let buf = [0u8; BOOTP_FIXED_LEN - 1];
        assert_eq!(decode(&buf), Err(DhcpError::TooShort));
    }

    /// Packet exactly at the minimum length (240 bytes) with correct cookie is
    /// rejected for missing Option 53, not for length.
    #[test]
    fn minimum_length_with_correct_cookie_fails_on_missing_type() {
        let mut buf = [0u8; BOOTP_FIXED_LEN + 4]; // 240 bytes — just the header
        buf[0] = OP_REQUEST;
        buf[1] = HTYPE_ETHERNET;
        buf[2] = HLEN_ETHERNET;
        buf[BOOTP_FIXED_LEN..BOOTP_FIXED_LEN + 4].copy_from_slice(&MAGIC_COOKIE);
        // No options at all → missing message type.
        assert_eq!(decode(&buf), Err(DhcpError::MissingMessageType));
    }

    /// An unknown option tag is skipped; the packet is still decoded successfully.
    #[test]
    fn unknown_option_is_skipped_not_rejected() {
        let mac = [0x01, 0x02, 0x03, 0x04, 0x05, 0x06];
        let mut buf = [0u8; PACKET_BUF];
        buf[0] = OP_REQUEST;
        buf[1] = HTYPE_ETHERNET;
        buf[2] = HLEN_ETHERNET;
        buf[4..8].copy_from_slice(&42u32.to_be_bytes());
        buf[28..34].copy_from_slice(&mac);
        buf[BOOTP_FIXED_LEN..BOOTP_FIXED_LEN + 4].copy_from_slice(&MAGIC_COOKIE);

        let mut pos = BOOTP_FIXED_LEN + 4;
        // Option 77 (User Class) — not handled.
        buf[pos] = 77;
        buf[pos + 1] = 3;
        buf[pos + 2] = b'a';
        buf[pos + 3] = b'b';
        buf[pos + 4] = b'c';
        pos += 5;
        // Option 53 — DISCOVER.
        buf[pos] = OPT_MSG_TYPE;
        buf[pos + 1] = 1;
        buf[pos + 2] = MSG_DISCOVER;
        pos += 3;
        buf[pos] = OPT_END;

        let msg = decode(&buf).expect("unknown option should not cause decode failure");
        assert_eq!(msg.options.message_type, Some(MSG_DISCOVER));
    }

    // ── Allocation policy tests ───────────────────────────────────────────────

    const POOL_START: Ipv4Addr = Ipv4Addr::new(192, 168, 4, 10);
    const LEASE_SECS: u32 = 300;

    /// First allocation from an empty table returns pool_start.
    #[test]
    fn first_allocation_returns_pool_start() {
        let mut table = LeaseTable::new(POOL_START, LEASE_SECS);
        let mac = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x01];
        let addr = table
            .allocate(&mac, None, 0)
            .expect("allocation should succeed");
        assert_eq!(addr, Ipv4Addr::new(192, 168, 4, 10));
    }

    /// Same MAC gets the same IP on a second allocation (lease renewal).
    #[test]
    fn same_mac_reuses_lease() {
        let mut table = LeaseTable::new(POOL_START, LEASE_SECS);
        let mac = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x02];
        let first = table
            .allocate(&mac, None, 0)
            .expect("first alloc should succeed");
        let second = table
            .allocate(&mac, None, 100)
            .expect("second alloc should succeed");
        assert_eq!(first, second, "same MAC must get the same IP");
    }

    /// Requested IP that is in pool and free is honoured.
    #[test]
    fn requested_ip_in_pool_is_honoured() {
        let mut table = LeaseTable::new(POOL_START, LEASE_SECS);
        let mac = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x03];
        let want = Ipv4Addr::new(192, 168, 4, 15);
        let got = table
            .allocate(&mac, Some(want), 0)
            .expect("allocation should succeed");
        assert_eq!(got, want);
    }

    /// Requested IP outside the pool falls through to first-free.
    #[test]
    fn requested_ip_out_of_pool_falls_through_to_first_free() {
        let mut table = LeaseTable::new(POOL_START, LEASE_SECS);
        let mac = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0x04];
        // 192.168.4.200 is well outside the .10–.20 pool.
        let out_of_pool = Some(Ipv4Addr::new(192, 168, 4, 200));
        let got = table
            .allocate(&mac, out_of_pool, 0)
            .expect("allocation should succeed");
        // Falls back to first free = pool_start = .10.
        assert_eq!(got, Ipv4Addr::new(192, 168, 4, 10));
    }

    /// An expired lease slot is reused by a new client.
    #[test]
    fn expired_lease_slot_is_reused() {
        let mut table = LeaseTable::new(POOL_START, LEASE_SECS);
        let mac1 = [0x01, 0x01, 0x01, 0x01, 0x01, 0x01];
        let mac2 = [0x02, 0x02, 0x02, 0x02, 0x02, 0x02];
        // Allocate at t=0 with 300 s lease.
        let addr1 = table
            .allocate(&mac1, None, 0)
            .expect("first alloc should succeed");
        // At t=400 the lease has expired; mac2 requests the same IP.
        let addr2 = table
            .allocate(&mac2, Some(addr1), 400)
            .expect("second alloc should succeed");
        assert_eq!(addr2, addr1, "expired slot should be reused");
    }

    /// Two different MACs get different IP addresses.
    #[test]
    fn two_clients_get_different_ips() {
        let mut table = LeaseTable::new(POOL_START, LEASE_SECS);
        let mac1 = [0x11, 0x11, 0x11, 0x11, 0x11, 0x11];
        let mac2 = [0x22, 0x22, 0x22, 0x22, 0x22, 0x22];
        let a1 = table.allocate(&mac1, None, 0).unwrap();
        let a2 = table.allocate(&mac2, None, 0).unwrap();
        assert_ne!(a1, a2, "different MACs must get different IPs");
    }

    /// Full pool with all slots valid — oldest lease is evicted.
    #[test]
    fn full_pool_evicts_oldest_lease() {
        let mut table = LeaseTable::new(POOL_START, LEASE_SECS);
        // Fill all 11 slots with distinct offered_at_secs values.
        for i in 0..POOL_SIZE {
            let mac = [0xAA, 0xAA, 0xAA, 0xAA, 0xAA, i as u8];
            // t = i so slot 0 is the oldest (t=0).
            table
                .allocate(&mac, None, i as u64)
                .expect("alloc should succeed");
        }
        // A new MAC needs a slot — the oldest (slot 0, t=0) must be evicted.
        let newcomer = [0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF];
        let addr = table
            .allocate(&newcomer, None, 1000)
            .expect("eviction alloc should succeed");
        // Slot 0 → pool_start = 192.168.4.10.
        assert_eq!(addr, Ipv4Addr::new(192, 168, 4, 10));
    }

    /// NAK encode output decodes cleanly; yiaddr and ciaddr are zero.
    #[test]
    fn nak_encode_is_decodable() {
        let server = Ipv4Addr::new(192, 168, 4, 1);
        let mac = [0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x01];
        let request_msg = DhcpMessage {
            op: OP_REQUEST,
            htype: HTYPE_ETHERNET,
            hlen: HLEN_ETHERNET,
            xid: 0xABCD_1234,
            ciaddr: Ipv4Addr::new(0, 0, 0, 0),
            yiaddr: Ipv4Addr::new(0, 0, 0, 0),
            siaddr: Ipv4Addr::new(0, 0, 0, 0),
            chaddr: mac,
            options: ParsedOptions {
                message_type: Some(MSG_REQUEST),
                ..Default::default()
            },
        };

        let mut buf = [0u8; PACKET_BUF];
        let len = encode_nak(&request_msg, &mut buf, server).expect("encode_nak should succeed");
        let decoded = decode(&buf[..len]).expect("decode should succeed");

        assert_eq!(decoded.op, OP_REPLY);
        assert_eq!(decoded.xid, 0xABCD_1234);
        assert_eq!(decoded.chaddr, mac);
        assert_eq!(decoded.options.message_type, Some(MSG_NAK));
        assert_eq!(decoded.options.server_id, Some(server));
        assert!(
            decoded.yiaddr.is_unspecified(),
            "NAK yiaddr must be 0.0.0.0"
        );
    }

    // ── Pool-geometry validation tests ────────────────────────────────────────

    #[test]
    fn pool_geometry_accepts_default_within_24_pool() {
        // 192.168.4.10..=192.168.4.20 (POOL_SIZE = 11) — the shipping default.
        let start = Ipv4Addr([192, 168, 4, 10]);
        let end = Ipv4Addr([192, 168, 4, 20]);
        assert_eq!(validate_pool_geometry(start, end, POOL_SIZE), Ok(()));
    }

    #[test]
    fn pool_geometry_rejects_size_mismatch() {
        let start = Ipv4Addr([192, 168, 4, 10]);
        let end = Ipv4Addr([192, 168, 4, 24]); // 15 slots, not 11
        assert_eq!(
            validate_pool_geometry(start, end, POOL_SIZE),
            Err(PoolGeometryError::SizeMismatch {
                configured: 15,
                required: POOL_SIZE,
            })
        );
    }

    #[test]
    fn pool_geometry_rejects_crosses_24_boundary() {
        // 192.168.4.250..=192.168.5.4 — 11 slots numerically, but `addr_of(6)`
        // would wrap last-octet to .0 instead of advancing to .5.0. The
        // size check would pass; the cross-subnet check must catch it.
        let start = Ipv4Addr([192, 168, 4, 250]);
        let end = Ipv4Addr([192, 168, 5, 4]);
        assert_eq!(
            validate_pool_geometry(start, end, POOL_SIZE),
            Err(PoolGeometryError::CrossesSubnet)
        );
    }

    // ── decide_request — RFC 2131 §4.3.2 decision-tree tests ─────────────────

    /// Build a minimal REQUEST `DhcpMessage` for the decision tests.
    fn make_request(
        mac: [u8; 6],
        ciaddr: Ipv4Addr,
        requested_ip: Option<Ipv4Addr>,
        server_id: Option<Ipv4Addr>,
    ) -> DhcpMessage {
        DhcpMessage {
            op: OP_REQUEST,
            htype: HTYPE_ETHERNET,
            hlen: HLEN_ETHERNET,
            xid: 0x1234_5678,
            ciaddr,
            yiaddr: Ipv4Addr::new(0, 0, 0, 0),
            siaddr: Ipv4Addr::new(0, 0, 0, 0),
            chaddr: mac,
            options: ParsedOptions {
                message_type: Some(MSG_REQUEST),
                requested_ip,
                server_id,
                ..Default::default()
            },
        }
    }

    const SERVER_IP: Ipv4Addr = Ipv4Addr::new(192, 168, 4, 1);

    /// SELECTING path: REQUEST carries Option 50 naming the prior OFFER's IP
    /// and the server's Option 54.  Allocation returns the same IP → Ack.
    #[test]
    fn decide_request_selecting_with_option_50_acks() {
        let mut table = LeaseTable::new(POOL_START, LEASE_SECS);
        let mac = [0xA1, 0xA2, 0xA3, 0xA4, 0xA5, 0xA6];
        // Pre-load the slot the OFFER would have claimed.
        table
            .allocate(&mac, None, 0)
            .expect("preallocate OFFER slot");

        let msg = make_request(
            mac,
            Ipv4Addr::new(0, 0, 0, 0),
            Some(Ipv4Addr::new(192, 168, 4, 10)),
            Some(SERVER_IP),
        );
        assert_eq!(
            decide_request(&msg, &mut table, SERVER_IP, 10),
            RequestOutcome::Ack(Ipv4Addr::new(192, 168, 4, 10))
        );
    }

    /// RENEWING path: client already has a binding, sends REQUEST with
    /// non-zero ciaddr and no Option 50.  Allocation refreshes the slot → Ack.
    #[test]
    fn decide_request_renewing_with_ciaddr_acks() {
        let mut table = LeaseTable::new(POOL_START, LEASE_SECS);
        let mac = [0xB1, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6];
        let addr = table.allocate(&mac, None, 0).expect("preallocate slot");

        let msg = make_request(mac, addr, None, None);
        assert_eq!(
            decide_request(&msg, &mut table, SERVER_IP, 100),
            RequestOutcome::Ack(addr)
        );
    }

    /// Bindingless REQUEST (no Option 50, ciaddr=0, no prior lease) →
    /// silent Drop per RFC 2131 §4.3.2.
    #[test]
    fn decide_request_bindingless_with_no_record_is_dropped() {
        let mut table = LeaseTable::new(POOL_START, LEASE_SECS);
        let mac = [0xC1, 0xC2, 0xC3, 0xC4, 0xC5, 0xC6];

        let msg = make_request(mac, Ipv4Addr::new(0, 0, 0, 0), None, None);
        assert_eq!(
            decide_request(&msg, &mut table, SERVER_IP, 0),
            RequestOutcome::Drop
        );
        // Drop must not commit a lease slot.
        assert!(
            table.find_by_mac(&mac).is_none(),
            "Drop path must not write a lease entry"
        );
    }

    /// Bindingless REQUEST when the MAC already has a binding → lenient Ack
    /// with refresh (matches real-world phones that re-REQUEST after losing
    /// address-knowledge but keeping the MAC).
    #[test]
    fn decide_request_bindingless_with_existing_binding_acks() {
        let mut table = LeaseTable::new(POOL_START, LEASE_SECS);
        let mac = [0xD1, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6];
        let addr = table.allocate(&mac, None, 0).expect("preallocate slot");

        let msg = make_request(mac, Ipv4Addr::new(0, 0, 0, 0), None, None);
        assert_eq!(
            decide_request(&msg, &mut table, SERVER_IP, 5),
            RequestOutcome::Ack(addr)
        );
    }

    /// Option 54 names a different server → Ignore; the table is untouched.
    #[test]
    fn decide_request_different_server_id_is_ignored() {
        let mut table = LeaseTable::new(POOL_START, LEASE_SECS);
        let mac = [0xE1, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6];
        let other_server = Ipv4Addr::new(10, 0, 0, 1);

        let msg = make_request(
            mac,
            Ipv4Addr::new(0, 0, 0, 0),
            Some(Ipv4Addr::new(192, 168, 4, 10)),
            Some(other_server),
        );
        assert_eq!(
            decide_request(&msg, &mut table, SERVER_IP, 0),
            RequestOutcome::Ignore
        );
        assert!(
            table.find_by_mac(&mac).is_none(),
            "Ignore path must not write a lease entry"
        );
    }

    /// Client insists on an IP outside the pool — the allocator falls back to
    /// the first free slot, which differs from the requested IP → Nak.
    #[test]
    fn decide_request_unrequestable_specific_ip_naks() {
        let mut table = LeaseTable::new(POOL_START, LEASE_SECS);
        let mac = [0xF1, 0xF2, 0xF3, 0xF4, 0xF5, 0xF6];
        let out_of_pool = Ipv4Addr::new(192, 168, 4, 99);

        let msg = make_request(
            mac,
            Ipv4Addr::new(0, 0, 0, 0),
            Some(out_of_pool),
            Some(SERVER_IP),
        );
        assert_eq!(
            decide_request(&msg, &mut table, SERVER_IP, 0),
            RequestOutcome::Nak
        );
    }

    /// Lock the temporary "Nak-still-mutates-leases" side effect.
    ///
    /// See `decide_request` # Mutation note: `allocate` writes a slot
    /// under the client's MAC even when the response is Nak, because the
    /// spike preserves prior behaviour where a malformed REQUEST gets
    /// happy-accident recovery from the next DISCOVER.  Phase 2B will
    /// introduce probe/commit; until then this test pins the current
    /// behaviour so any silent drift fails CI loudly.
    #[test]
    fn decide_request_nak_still_records_mac_lease() {
        let mut table = LeaseTable::new(POOL_START, LEASE_SECS);
        let mac = [0xAB, 0xAD, 0xCA, 0xFE, 0x00, 0x01];
        let out_of_pool = Ipv4Addr::new(192, 168, 4, 99);

        let msg = make_request(
            mac,
            Ipv4Addr::new(0, 0, 0, 0),
            Some(out_of_pool),
            Some(SERVER_IP),
        );
        assert_eq!(
            decide_request(&msg, &mut table, SERVER_IP, 0),
            RequestOutcome::Nak
        );
        // Side effect we are pinning here: even though the response is
        // Nak, `allocate` claimed a pool slot under the client's MAC.
        // Phase 2B probe/commit will replace this assertion with
        // `assert!(table.find_by_mac(&mac).is_none())`.
        assert!(
            table.find_by_mac(&mac).is_some(),
            "spike preserves Nak-still-mutates-leases (see `decide_request` # Mutation note); Phase 2B will introduce probe/commit"
        );
    }

    #[test]
    fn pool_geometry_accepts_boundary_at_255() {
        // The boundary case the dropped `LastOctetOverflow` variant claimed to
        // catch — pool_start.0[3] = 250, pool_size = 6 → last slot lands on
        // .255 exactly. The remaining two checks (size match, same subnet)
        // accept it as valid, which is correct: nothing wraps.
        let start = Ipv4Addr([10, 0, 0, 250]);
        let end = Ipv4Addr([10, 0, 0, 255]);
        assert_eq!(validate_pool_geometry(start, end, 6), Ok(()));
        // Claiming one more slot than the addressable range trips the size
        // check — which is the correct error surface; there is no separate
        // overflow variant to reach.
        assert!(matches!(
            validate_pool_geometry(start, end, 7),
            Err(PoolGeometryError::SizeMismatch { .. })
        ));
    }
}
