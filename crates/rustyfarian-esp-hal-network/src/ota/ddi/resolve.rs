//! The address of a DDI server: an IPv4 literal as is, a name through one DNS `A` lookup on the station stack.
//!
//! An IPv6 literal is refused: the bare-metal stack is built without IPv6 (a named gap of this tier, ADR 022).

use core::net::Ipv4Addr;
use core::sync::atomic::{AtomicBool, Ordering};
use core::time::Duration;

use embassy_net::dns::DnsQueryType;
use embassy_net::{IpAddress, Stack};
use embassy_time::with_timeout;

use crate::ota::manager::to_embassy;

/// Set once the IPv6 WARN has been logged; a load and a store, no read-modify-write (ESP32-C3).
static IPV6_WARNED: AtomicBool = AtomicBool::new(false);

/// Why a host could not be resolved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResolveError {
    /// The lookup did not answer within the budget.
    Timeout,
    /// The lookup failed or found no `A` record, or the host is an IPv6 literal.
    Unreachable,
}

/// The address of `host` (as `parse_url` returns it: an IPv6 literal keeps its brackets), within `budget`.
///
/// Names are resolved on every request, so a changed address is picked up; the host name is not secret and is logged on a failure.
pub(crate) async fn resolve(
    stack: Stack<'static>,
    host: &str,
    budget: Duration,
) -> Result<IpAddress, ResolveError> {
    if host.starts_with('[') {
        if !IPV6_WARNED.load(Ordering::Relaxed) {
            IPV6_WARNED.store(true, Ordering::Relaxed);
            log::warn!("DDI: IPv6 server addresses are not supported on the esp-hal tier");
        }
        return Err(ResolveError::Unreachable);
    }
    if let Ok(ip) = host.parse::<Ipv4Addr>() {
        return Ok(IpAddress::Ipv4(ip));
    }
    match with_timeout(to_embassy(budget), stack.dns_query(host, DnsQueryType::A)).await {
        Err(_) => {
            log::warn!("DDI: DNS lookup of {} timed out", host);
            Err(ResolveError::Timeout)
        }
        Ok(Err(e)) => {
            log::warn!("DDI: DNS lookup of {} failed: {:?}", host, e);
            Err(ResolveError::Unreachable)
        }
        Ok(Ok(addresses)) => addresses.first().copied().ok_or_else(|| {
            log::warn!("DDI: DNS lookup of {} found no address", host);
            ResolveError::Unreachable
        }),
    }
}
