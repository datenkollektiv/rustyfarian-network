//! Minimal captive-portal UDP DNS catch-all.
//!
//! A wildcard responder that answers every `A`/`ANY` query with the AP IP so
//! the OS captive-portal sheet appears automatically (per feature-doc question
//! 7). It is `pub(crate)` and lifecycle-bound to the session.
//!
//! # Robustness
//!
//! The parser never panics: every header and question-section access is
//! bounds-checked and a malformed packet is silently skipped. Queries it cannot
//! answer as `A` records are replied to with `NOERROR` and zero answers.
//!
//! OS captive-portal heuristics vary by platform; the catch-all improves
//! detection but does not guarantee the sheet opens. Manual navigation to the
//! AP IP remains the documented fallback.

use std::net::{Ipv4Addr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

/// Classic DNS message size cap; the catch-all never needs more.
const DNS_BUF_LEN: usize = 512;

/// Read timeout so the recv loop can observe the shutdown flag periodically.
const RECV_TIMEOUT: Duration = Duration::from_millis(500);

/// TTL (seconds) advertised on every synthesized answer.
const ANSWER_TTL: u32 = 60;

/// DNS type `A` (IPv4 host address).
const TYPE_A: u16 = 1;
/// DNS QClass `IN`.
const CLASS_IN: u16 = 1;

/// A running DNS responder thread plus the flag that stops it.
pub(crate) struct DnsResponder {
    handle: Option<JoinHandle<()>>,
    shutdown: Arc<AtomicBool>,
}

impl DnsResponder {
    /// Spawns the catch-all responder bound to `0.0.0.0:53`, answering every
    /// `A` query with `ap_ip`.
    ///
    /// The thread is named `prov-dns` with an 8 KB stack — there is no in-repo
    /// precedent for the size; 8 KB is chosen for parser locals plus the
    /// 512-byte request/response buffers with headroom.
    pub(crate) fn start(ap_ip: Ipv4Addr) -> anyhow::Result<Self> {
        let socket = UdpSocket::bind("0.0.0.0:53")?;
        socket.set_read_timeout(Some(RECV_TIMEOUT))?;

        let shutdown = Arc::new(AtomicBool::new(false));
        let stop = shutdown.clone();

        let handle = std::thread::Builder::new()
            .name("prov-dns".into())
            .stack_size(8192)
            .spawn(move || run(socket, ap_ip, stop))?;

        Ok(Self {
            handle: Some(handle),
            shutdown,
        })
    }

    /// Signals the responder to stop and joins its thread.
    pub(crate) fn stop(mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for DnsResponder {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// The recv loop. Exits when the shutdown flag is set.
fn run(socket: UdpSocket, ap_ip: Ipv4Addr, shutdown: Arc<AtomicBool>) {
    let mut buf = [0u8; DNS_BUF_LEN];
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        let (len, peer) = match socket.recv_from(&mut buf) {
            Ok(v) => v,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(_) => continue,
        };

        if let Some(response) = build_response(&buf[..len], ap_ip) {
            let _ = socket.send_to(&response, peer);
        }
    }
}

/// Builds a DNS response for a request, or `None` if the request is too
/// malformed to answer.
///
/// The response copies the request's header (flipping QR/AA, clearing RA and
/// RCODE) and its single question, then appends an `A` answer pointing at
/// `ap_ip` when the question is an `A`/`ANY` `IN` query. All other queries get
/// a `NOERROR` reply with zero answers — the simplest captive-portal
/// behaviour.
fn build_response(request: &[u8], ap_ip: Ipv4Addr) -> Option<Vec<u8>> {
    if request.len() < 12 {
        return None;
    }
    let qdcount = u16::from_be_bytes([request[4], request[5]]);
    if qdcount != 1 {
        return None;
    }

    let (qname_end, qtype) = parse_question(request)?;

    let mut out: Vec<u8> = Vec::with_capacity(request.len() + 16);

    out.extend_from_slice(&request[0..2]);
    out.push(0x84);
    out.push(0x00);
    out.extend_from_slice(&[0x00, 0x01]);

    let answer = qtype == TYPE_A || qtype == 255;
    let ancount: u16 = if answer { 1 } else { 0 };
    out.extend_from_slice(&ancount.to_be_bytes());
    out.extend_from_slice(&[0x00, 0x00]);
    out.extend_from_slice(&[0x00, 0x00]);

    out.extend_from_slice(&request[12..qname_end + 4]);

    if answer {
        out.extend_from_slice(&[0xC0, 0x0C]);
        out.extend_from_slice(&TYPE_A.to_be_bytes());
        out.extend_from_slice(&CLASS_IN.to_be_bytes());
        out.extend_from_slice(&ANSWER_TTL.to_be_bytes());
        out.extend_from_slice(&4u16.to_be_bytes());
        out.extend_from_slice(&ap_ip.octets());
    }

    Some(out)
}

/// Walks the single question's QNAME labels and returns `(qname_end, qtype)`,
/// where `qname_end` is the offset just past the terminating zero byte.
///
/// Returns `None` on any out-of-bounds access (truncated packet, compression
/// pointer, or a missing QTYPE/QCLASS).
fn parse_question(request: &[u8]) -> Option<(usize, u16)> {
    let mut i = 12;
    loop {
        let len = *request.get(i)? as usize;
        if len == 0 {
            i += 1;
            break;
        }
        if len & 0xC0 != 0 {
            return None;
        }
        i += 1 + len;
        if i > request.len() {
            return None;
        }
    }
    let qtype_hi = *request.get(i)?;
    let qtype_lo = *request.get(i + 1)?;
    request.get(i + 3)?;
    let qtype = u16::from_be_bytes([qtype_hi, qtype_lo]);
    Some((i, qtype))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal `A` query for `a.com`: header + QNAME `1 'a' 3 'c' 'o' 'm' 0`
    /// + QTYPE=A + QCLASS=IN.
    fn a_query() -> Vec<u8> {
        let mut q = vec![
            0x12, 0x34, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        q.extend_from_slice(&[1, b'a', 3, b'c', b'o', b'm', 0]);
        q.extend_from_slice(&TYPE_A.to_be_bytes());
        q.extend_from_slice(&CLASS_IN.to_be_bytes());
        q
    }

    #[test]
    fn a_query_gets_answer_with_ap_ip() {
        let ip = Ipv4Addr::new(192, 168, 4, 1);
        let resp = build_response(&a_query(), ip).expect("response");
        assert_eq!(&resp[0..2], &[0x12, 0x34]);
        assert_eq!(u16::from_be_bytes([resp[6], resp[7]]), 1);
        assert_eq!(&resp[resp.len() - 4..], &ip.octets());
    }

    #[test]
    fn truncated_header_is_skipped() {
        assert!(build_response(&[0x00, 0x01], Ipv4Addr::LOCALHOST).is_none());
    }

    #[test]
    fn compression_pointer_in_qname_is_rejected() {
        let mut q = a_query();
        q[12] = 0xC0;
        assert!(build_response(&q, Ipv4Addr::LOCALHOST).is_none());
    }

    #[test]
    fn non_a_query_gets_zero_answers() {
        let mut q = a_query();
        let qtype_off = q.len() - 4;
        q[qtype_off] = 0x00;
        q[qtype_off + 1] = 0x0F;
        let resp = build_response(&q, Ipv4Addr::LOCALHOST).expect("response");
        assert_eq!(u16::from_be_bytes([resp[6], resp[7]]), 0);
    }
}
