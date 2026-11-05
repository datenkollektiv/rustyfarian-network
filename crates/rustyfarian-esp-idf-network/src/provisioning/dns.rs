//! Minimal captive-portal UDP DNS catch-all.
//!
//! A wildcard responder that answers every `A`/`ANY` query with the AP IP so
//! the OS captive-portal sheet appears automatically (per feature-doc question
//! 7). It is `pub(crate)` and lifecycle-bound to the session.
//!
//! The codec and its answer policy live in [`juggler::provisioning::dns`],
//! shared with the bare-metal tier: other query types get `NOERROR` with zero
//! answers and a malformed packet is silently skipped. This module keeps only
//! the UDP recv/send thread and its shutdown flag.
//!
//! OS captive-portal heuristics vary by platform; the catch-all improves
//! detection but does not guarantee the sheet opens. Manual navigation to the
//! AP IP remains the documented fallback.

use std::net::{Ipv4Addr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use juggler::provisioning::dns::{decode_query, encode_response, DNS_MSG_MAX};

/// Read timeout so the recv loop can observe the shutdown flag periodically.
const RECV_TIMEOUT: Duration = Duration::from_millis(500);

/// TTL (seconds) advertised on every synthesized answer.
const ANSWER_TTL: u32 = 60;

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
    /// precedent for the size; 8 KB is chosen for codec locals plus the
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
    let mut buf = [0u8; DNS_MSG_MAX];
    let mut out = [0u8; DNS_MSG_MAX];
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
        let request = &buf[..len.min(buf.len())];
        let Ok(query) = decode_query(request) else {
            continue;
        };
        if let Ok(n) = encode_response(&query, request, ap_ip.octets(), ANSWER_TTL, &mut out) {
            let _ = socket.send_to(&out[..n], peer);
        }
    }
}
