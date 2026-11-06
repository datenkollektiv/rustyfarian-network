//! The time budget of one captive-portal connection (ADR 023 decision 4: every wait is bounded).
//!
//! The bare-metal portal serves one connection at a time from a single accept loop, so a
//! client that stalls in any phase would hold the portal for everyone.
//! Each phase of a connection therefore has its own budget, and the adapter wraps that phase's
//! I/O in a timeout of [`PortalPhase::budget_ms`]: when it expires the adapter aborts the
//! connection and returns to accept.
//!
//! The ESP-IDF tier gets the same bound from `httpd`: `esp-idf-svc` 0.53 hard-codes
//! `recv_wait_timeout` and `send_wait_timeout` to 5 s (the `Configuration` has no field for
//! either), applied to each socket operation, and `httpd` serves several sockets, so a stalled
//! client costs one session slot (with LRU purge) instead of the whole portal.
//! [`IDF_HTTPD_SOCKET_TIMEOUT_MS`] is that value; the three phases that wait on the peer use it
//! as their budget.
//! The two tiers do not bound the same thing: on `httpd` the 5 s restarts with every socket
//! operation, so a slow client that keeps trickling bytes can outlast it, while the esp-hal
//! budget caps the whole phase.

/// Experimental: API may change before 1.0.
///
/// The per-operation receive and send timeout of the ESP-IDF `httpd` as configured by
/// `esp-idf-svc` 0.53, in milliseconds.
pub const IDF_HTTPD_SOCKET_TIMEOUT_MS: u32 = 5_000;

/// The budget of [`PortalPhase::Drain`], in milliseconds.
///
/// The client already has its response, and the drain only keeps the close from turning into a
/// reset, so a short wait is enough: a body that is not already in flight is not worth holding
/// the single accept loop for.
const DRAIN_BUDGET_MS: u32 = 500;

/// The budget of [`PortalPhase::Close`], in milliseconds.
///
/// Flushing the FIN needs the peer's ACK, one round trip over the SoftAP link, so it gets twice
/// the drain; a client that has gone away is aborted rather than waited for.
const CLOSE_BUDGET_MS: u32 = 1_000;

/// The budget of [`PortalPhase::Abort`], in milliseconds.
///
/// Flushing a reset needs no answer from the peer, so it only has to cover the local send
/// queue; it is kept short because it runs after another phase already spent its budget.
const ABORT_BUDGET_MS: u32 = 500;

/// Experimental: API may change before 1.0.
///
/// A phase of one portal connection, in the order the adapter runs them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortalPhase {
    /// Reading the request line and headers up to the blank line.
    Headers,
    /// Reading the rest of a POST body that did not arrive with the headers.
    Body,
    /// Writing and flushing the response.
    Response,
    /// Discarding an unread request body so that closing does not reset the connection.
    Drain,
    /// Closing the socket and flushing the FIN.
    Close,
    /// Flushing the reset after the adapter aborted a connection, whichever phase timed out.
    Abort,
}

impl PortalPhase {
    /// Every phase, in the order a connection runs them.
    pub const ALL: [Self; 6] = [
        Self::Headers,
        Self::Body,
        Self::Response,
        Self::Drain,
        Self::Close,
        Self::Abort,
    ];

    /// Experimental: API may change before 1.0.
    ///
    /// The most time this phase may take, in milliseconds.
    pub const fn budget_ms(self) -> u32 {
        match self {
            Self::Headers | Self::Body | Self::Response => IDF_HTTPD_SOCKET_TIMEOUT_MS,
            Self::Drain => DRAIN_BUDGET_MS,
            Self::Close => CLOSE_BUDGET_MS,
            Self::Abort => ABORT_BUDGET_MS,
        }
    }
}

/// Experimental: API may change before 1.0.
///
/// The longest one connection can hold the portal: the sum of every phase budget.
///
/// A connection that times out in the last waiting phase (`Close`) is aborted and its reset
/// flushed within [`PortalPhase::Abort`], so that budget is part of the sum.
/// Earlier aborts skip the later phases, so they are shorter.
pub const fn worst_case_connection_ms() -> u32 {
    let mut total = 0;
    let mut i = 0;
    while i < PortalPhase::ALL.len() {
        total += PortalPhase::ALL[i].budget_ms();
        i += 1;
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_phase_has_a_nonzero_bounded_budget() {
        for phase in PortalPhase::ALL {
            let ms = phase.budget_ms();
            assert!(ms > 0, "{phase:?} must not be unbounded or zero");
            assert!(ms <= IDF_HTTPD_SOCKET_TIMEOUT_MS, "{phase:?} outlasts IDF");
        }
    }

    #[test]
    fn the_phases_that_wait_on_the_peer_match_the_idf_httpd_timeout() {
        for phase in [
            PortalPhase::Headers,
            PortalPhase::Body,
            PortalPhase::Response,
        ] {
            assert_eq!(phase.budget_ms(), IDF_HTTPD_SOCKET_TIMEOUT_MS);
        }
        assert_eq!(IDF_HTTPD_SOCKET_TIMEOUT_MS, 5_000);
    }

    #[test]
    fn the_cleanup_phases_stay_short() {
        assert_eq!(PortalPhase::Drain.budget_ms(), 500);
        assert_eq!(PortalPhase::Close.budget_ms(), 1_000);
        assert_eq!(PortalPhase::Abort.budget_ms(), 500);
    }

    #[test]
    fn the_worst_case_includes_the_reset_flush_after_a_close_timeout() {
        let through_close = PortalPhase::Headers.budget_ms()
            + PortalPhase::Body.budget_ms()
            + PortalPhase::Response.budget_ms()
            + PortalPhase::Drain.budget_ms()
            + PortalPhase::Close.budget_ms();
        assert_eq!(
            worst_case_connection_ms(),
            through_close + PortalPhase::Abort.budget_ms()
        );
    }

    #[test]
    fn a_connection_cannot_hold_the_portal_longer_than_the_sum_of_its_phases() {
        assert_eq!(worst_case_connection_ms(), 5_000 * 3 + 500 + 1_000 + 500);
        assert!(worst_case_connection_ms() <= 20_000);
    }
}
