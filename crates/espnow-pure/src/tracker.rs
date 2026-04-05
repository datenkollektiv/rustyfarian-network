//! Heartbeat-based peer liveness tracking.
//!
//! [`PeerTracker`] monitors whether an ESP-NOW peer is online by tracking
//! heartbeat timestamps. It detects online/offline transitions so callers
//! can log state changes or update a UI without manual bookkeeping.
//!
//! # Design
//!
//! The caller controls the clock by passing monotonic `u64` tick values.
//! The tick unit is up to the caller — milliseconds, microseconds, or raw
//! timer counts all work, as long as the unit is consistent between the
//! constructor and method calls.
//!
//! All internal elapsed-time calculations use [`saturating_sub`](u64::saturating_sub),
//! so if the caller ever supplies a non-monotonic timestamp, the computed
//! elapsed time clamps to zero, keeping the peer online rather than
//! producing spurious offline transitions.
//!
//! # Example
//!
//! ```
//! use espnow_pure::PeerTracker;
//!
//! // Peer is considered offline after 15_000 ms without a heartbeat.
//! let mut tracker = PeerTracker::new(15_000);
//!
//! assert!(!tracker.is_online(0));
//! assert!(!tracker.has_been_seen());
//!
//! // First heartbeat — peer comes online.
//! tracker.record_seen(1_000);
//! assert!(tracker.is_online(1_000));
//! assert_eq!(tracker.poll_transition(1_000), Some(true));
//!
//! // No change on next poll.
//! assert_eq!(tracker.poll_transition(5_000), None);
//!
//! // Timeout elapses — peer goes offline.
//! assert_eq!(tracker.poll_transition(20_000), Some(false));
//! ```

// ─── PeerTracker ────────────────────────────────────────────────────────────

/// Heartbeat-based liveness tracker for a single ESP-NOW peer.
///
/// See the [module-level documentation](self) for design details and examples.
#[derive(Debug, Clone, Copy)]
pub struct PeerTracker {
    timeout: u64,
    last_seen: Option<u64>,
    prev_online: bool,
}

impl PeerTracker {
    /// Create a tracker with the given timeout (in caller-defined ticks).
    ///
    /// A peer is considered online if a heartbeat was received within
    /// `timeout` ticks of the current time.
    pub fn new(timeout: u64) -> Self {
        Self {
            timeout,
            last_seen: None,
            prev_online: false,
        }
    }

    /// Record a heartbeat from this peer at the given timestamp.
    pub fn record_seen(&mut self, now: u64) {
        self.last_seen = Some(now);
    }

    /// Returns `true` if a heartbeat was received within the timeout window.
    pub fn is_online(&self, now: u64) -> bool {
        match self.last_seen {
            Some(seen) => now.saturating_sub(seen) < self.timeout,
            None => false,
        }
    }

    /// Check for an online/offline state transition since the last call.
    ///
    /// Returns:
    /// - `Some(true)` — peer just came online (was offline, now online).
    /// - `Some(false)` — peer just went offline (was online, now offline).
    /// - `None` — no change since the last call.
    ///
    /// Updates internal state, so consecutive calls without a state change
    /// return `None`.
    pub fn poll_transition(&mut self, now: u64) -> Option<bool> {
        let online = self.is_online(now);
        if online != self.prev_online {
            self.prev_online = online;
            Some(online)
        } else {
            None
        }
    }

    /// Returns `true` if this peer has ever sent a heartbeat.
    pub fn has_been_seen(&self) -> bool {
        self.last_seen.is_some()
    }

    /// Returns the tick of the most recent heartbeat, or `None` if the peer
    /// has never been seen.
    pub fn last_seen(&self) -> Option<u64> {
        self.last_seen
    }
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const TIMEOUT: u64 = 15_000;

    fn tracker() -> PeerTracker {
        PeerTracker::new(TIMEOUT)
    }

    // ── Initial state ───────────────────────────────────────────────────

    #[test]
    fn new_tracker_is_offline() {
        let t = tracker();
        assert!(!t.is_online(0));
        assert!(!t.has_been_seen());
        assert_eq!(t.last_seen(), None);
    }

    #[test]
    fn new_tracker_no_transition() {
        let mut t = tracker();
        assert_eq!(t.poll_transition(0), None);
    }

    // ── Coming online ───────────────────────────────────────────────────

    #[test]
    fn record_seen_makes_peer_online() {
        let mut t = tracker();
        t.record_seen(1_000);
        assert!(t.is_online(1_000));
        assert!(t.has_been_seen());
        assert_eq!(t.last_seen(), Some(1_000));
    }

    #[test]
    fn first_heartbeat_triggers_online_transition() {
        let mut t = tracker();
        t.record_seen(1_000);
        assert_eq!(t.poll_transition(1_000), Some(true));
    }

    #[test]
    fn second_poll_without_change_returns_none() {
        let mut t = tracker();
        t.record_seen(1_000);
        assert_eq!(t.poll_transition(1_000), Some(true));
        assert_eq!(t.poll_transition(5_000), None);
    }

    // ── Going offline ───────────────────────────────────────────────────

    #[test]
    fn peer_goes_offline_after_timeout() {
        let mut t = tracker();
        t.record_seen(1_000);
        assert!(t.is_online(1_000));
        assert!(!t.is_online(1_000 + TIMEOUT));
    }

    #[test]
    fn still_online_one_tick_before_timeout() {
        let mut t = tracker();
        t.record_seen(1_000);
        assert!(t.is_online(1_000 + TIMEOUT - 1));
    }

    #[test]
    fn timeout_triggers_offline_transition() {
        let mut t = tracker();
        t.record_seen(1_000);
        t.poll_transition(1_000); // consume online transition
        assert_eq!(t.poll_transition(1_000 + TIMEOUT), Some(false));
    }

    #[test]
    fn repeated_offline_poll_returns_none() {
        let mut t = tracker();
        t.record_seen(1_000);
        t.poll_transition(1_000); // online
        t.poll_transition(1_000 + TIMEOUT); // offline
        assert_eq!(t.poll_transition(1_000 + TIMEOUT + 1_000), None);
    }

    // ── Re-appearing ────────────────────────────────────────────────────

    #[test]
    fn peer_reappears_after_going_offline() {
        let mut t = tracker();
        t.record_seen(1_000);
        t.poll_transition(1_000); // online
        t.poll_transition(1_000 + TIMEOUT); // offline

        t.record_seen(20_000);
        assert!(t.is_online(20_000));
        assert_eq!(t.poll_transition(20_000), Some(true));
    }

    // ── Heartbeat refresh ───────────────────────────────────────────────

    #[test]
    fn heartbeat_refresh_extends_online_window() {
        let mut t = tracker();
        t.record_seen(1_000);
        t.record_seen(10_000); // refresh
                               // Should still be online at 10_000 + TIMEOUT - 1
        assert!(t.is_online(10_000 + TIMEOUT - 1));
        // Offline at 10_000 + TIMEOUT
        assert!(!t.is_online(10_000 + TIMEOUT));
    }

    // ── Zero timeout ────────────────────────────────────────────────────

    #[test]
    fn zero_timeout_offline_at_same_tick() {
        let mut t = PeerTracker::new(0);
        t.record_seen(100);
        // elapsed = 0, which is NOT < 0, so offline immediately.
        assert!(!t.is_online(100));
    }

    // ── Tick edge cases ─────────────────────────────────────────────────

    #[test]
    fn near_u64_max_online_check() {
        let mut t = tracker();
        let seen = u64::MAX - TIMEOUT;
        t.record_seen(seen);
        assert!(t.is_online(seen));
        assert!(t.is_online(u64::MAX - 1));
        assert!(!t.is_online(u64::MAX));
    }

    #[test]
    fn non_monotonic_timestamp_keeps_peer_online() {
        let mut t = tracker();
        t.record_seen(10_000);
        // Time jumps backward — saturating_sub(5_000, 10_000) = 0 < TIMEOUT.
        assert!(t.is_online(5_000));
    }
}
