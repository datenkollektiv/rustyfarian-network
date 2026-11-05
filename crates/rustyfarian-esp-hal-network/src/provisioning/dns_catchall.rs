//! DNS catch-all server substrate for the SoftAP captive-portal (ADR 015 §3
//! hand-rolled fallback; promoted from `rustyfarian-esp-hal-network` in Phase 2B).
//!
//! The codec and its answer policy live in [`juggler::provisioning::dns`],
//! shared with the ESP-IDF tier: `A` and `ANY` queries get the AP IP, every
//! other query type gets `NOERROR` with zero answers, and malformed queries
//! (including `qdcount != 1`, `QR=1` and compression pointers in the qname)
//! are dropped.  This module keeps only the `embassy-net` UDP loop.

// When building without the embassy + chip features the async `run` function
// and its UdpSocket usage are compiled away.  Allow dead-code on the types
// that remain so clippy -D warnings does not fail on stub/host builds.
#![cfg_attr(
    not(all(
        feature = "embassy",
        any(feature = "esp32c3", feature = "esp32c6", feature = "esp32s3")
    )),
    allow(dead_code)
)]

/// Standard DNS server port.
const DNS_PORT: u16 = 53;

// ── Public types ──────────────────────────────────────────────────────────────

/// IPv4 address — four octets in network (big-endian) order.
///
/// Mirrors `dhcp::Ipv4Addr` to keep the DNS codec layer free of `embassy-net`
/// types so the codec functions are host-testable without pulling in the stack.
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
}

impl core::fmt::Display for Ipv4Addr {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}.{}.{}.{}", self.0[0], self.0[1], self.0[2], self.0[3])
    }
}

// ── DnsCatchallConfig ─────────────────────────────────────────────────────────

/// Configuration for the DNS catch-all server.
///
/// The defaults cover the standard captive-portal scenario: bind on port 53,
/// answer `A` queries with the SoftAP IP (`192.168.4.1`), and use a short TTL
/// so phones do not cache the address long after the portal session ends.
pub struct DnsCatchallConfig {
    /// UDP port the server binds on (`53` by default).
    pub bind_port: u16,
    /// IP address returned in A-record responses (= AP IP, `192.168.4.1`).
    pub answer_ip: Ipv4Addr,
    /// TTL in seconds for A-record responses (`60` — short for a provisioning
    /// session that lasts only a few minutes).
    pub ttl_secs: u32,
}

impl Default for DnsCatchallConfig {
    fn default() -> Self {
        Self {
            bind_port: DNS_PORT,
            answer_ip: Ipv4Addr::new(192, 168, 4, 1),
            ttl_secs: 60,
        }
    }
}

// ── Async server loop (bare-metal only) ──────────────────────────────────────

/// Runs the DNS catch-all server on the given `embassy-net` stack.
///
/// This function never returns under normal operation.  It binds a UDP socket
/// on `config.bind_port` (default: 53), loops on `recv_from`, decodes each
/// query, and replies with a single A record pointing every name to
/// `config.answer_ip` (default: `192.168.4.1`).
///
/// `A` and `ANY` queries get the AP IP; every other type gets `NOERROR` with
/// zero answers, so a phone's `AAAA` lookup fails cleanly and falls back to
/// `A` (see [`juggler::provisioning::dns`]).
///
/// Malformed queries, compression-pointer queries, and responses (QR=1) are
/// silently dropped with a `warn` log.
///
/// # Spawn this as a dedicated embassy task
///
/// ```ignore
/// use rustyfarian_esp_hal_network::provisioning::dns_catchall::{self, DnsCatchallConfig};
///
/// #[embassy_executor::task]
/// async fn dns_task(stack: embassy_net::Stack<'static>) -> ! {
///     dns_catchall::run(stack, DnsCatchallConfig::default()).await
/// }
/// ```
///
/// # Panic-free
///
/// Bind failure, send errors, and malformed packets are all logged at `warn`
/// and do not abort the server loop.
#[cfg(all(
    feature = "embassy",
    any(feature = "esp32c3", feature = "esp32c6", feature = "esp32s3")
))]
pub async fn run(stack: embassy_net::Stack<'static>, config: DnsCatchallConfig) -> ! {
    use embassy_net::udp::{PacketMetadata, UdpSocket};
    use juggler::provisioning::dns::{decode_query, encode_response, DNS_MSG_MAX};
    use static_cell::StaticCell;

    // Static socket buffers — required because `UdpSocket::new` in
    // `embassy-net 0.8` internally transmutes the buffer slices to `'static`
    // (see `embassy_net::udp::UdpSocket::new` safety contract).
    // Buffer size matches `super::dhcp::SOCKET_BUF_LEN` so both servers use
    // the same tuned constant; raise both together if the MTU floor changes.
    static RX_META: StaticCell<[PacketMetadata; 4]> = StaticCell::new();
    static RX_BUF: StaticCell<[u8; super::dhcp::SOCKET_BUF_LEN]> = StaticCell::new();
    static TX_META: StaticCell<[PacketMetadata; 4]> = StaticCell::new();
    static TX_BUF: StaticCell<[u8; super::dhcp::SOCKET_BUF_LEN]> = StaticCell::new();

    let rx_meta = RX_META.init([PacketMetadata::EMPTY; 4]);
    let rx_buf = RX_BUF.init([0u8; super::dhcp::SOCKET_BUF_LEN]);
    let tx_meta = TX_META.init([PacketMetadata::EMPTY; 4]);
    let tx_buf = TX_BUF.init([0u8; super::dhcp::SOCKET_BUF_LEN]);

    let mut sock = UdpSocket::new(stack, rx_meta, rx_buf, tx_meta, tx_buf);

    match sock.bind(config.bind_port) {
        Ok(()) => log::info!("DNS catch-all bound on port {}", config.bind_port),
        Err(e) => {
            log::warn!(
                "DNS catch-all: failed to bind port {} ({:?}); server will not run",
                config.bind_port,
                e
            );
            // Park the task rather than returning — callers rely on `-> !`.
            loop {
                embassy_time::Timer::after(embassy_time::Duration::from_secs(60)).await;
            }
        }
    }

    let mut rx_pkt = [0u8; DNS_MSG_MAX];
    let mut tx_pkt = [0u8; DNS_MSG_MAX];

    loop {
        let (n, meta) = match sock.recv_from(&mut rx_pkt).await {
            Ok(r) => r,
            Err(e) => {
                log::warn!("DNS recv_from error: {:?}", e);
                continue;
            }
        };

        // Defensive clamp — `embassy-net` `recv_from` already returns
        // `min(datagram, buf)`, but the clamp here documents the invariant
        // and protects against any future stack change that might report a
        // count exceeding the destination buffer.
        let n = n.min(rx_pkt.len());

        let query = match decode_query(&rx_pkt[..n]) {
            Ok(q) => q,
            Err(e) => {
                log::warn!("DNS: malformed query from {:?}: {:?}", meta.endpoint, e);
                continue;
            }
        };

        let qname_len = query.qname_range.len();
        log::info!(
            "DNS query qtype={} qname_len={} (portal IP {})",
            query.qtype,
            qname_len,
            config.answer_ip
        );

        match encode_response(
            &query,
            &rx_pkt[..n],
            config.answer_ip.octets(),
            config.ttl_secs,
            &mut tx_pkt,
        ) {
            Ok(len) => {
                if let Err(e) = sock.send_to(&tx_pkt[..len], meta.endpoint).await {
                    log::warn!("DNS send_to failed: {:?}", e);
                }
            }
            Err(e) => {
                log::warn!("DNS response encode failed: {:?}", e);
            }
        }
    }
}
