//! Network gate shared by both tiers (bug 010, ADR 022 decision 10).
//!
//! The session machine must be told when the link becomes ready and when it
//! stops being ready, and nothing else. A duplicate [`SessionInput::NetworkUp`]
//! is harmless, but a spurious [`SessionInput::NetworkDown`] closes a live
//! session, so the tier never decides this on its own: [`NetworkGate`] turns
//! readiness observations into exactly those edges.
//!
//! The tier feeds every observation it gets (a polled sample on esp-hal, a
//! readiness notification on ESP-IDF) and performs whatever
//! [`SessionInput`](super::SessionInput) comes back.

use super::SessionInput;

/// Turns readiness observations into network-gate inputs.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct NetworkGate {
    up: bool,
}

impl NetworkGate {
    /// A gate that starts closed: nothing is known about the link yet.
    pub const fn new() -> Self {
        Self { up: false }
    }

    /// Feeds one readiness observation.
    ///
    /// Returns [`SessionInput::NetworkUp`] on the first `true` observation and
    /// on every `false` → `true` transition, [`SessionInput::NetworkDown`] on
    /// every `true` → `false` transition, and `None` while the observation
    /// matches the previous one.
    ///
    /// An observation of `false` before the link has ever been up reports
    /// nothing: the machine already starts offline, and a `NetworkDown` there
    /// would only add noise.
    pub fn feed(&mut self, ready: bool) -> Option<SessionInput> {
        if ready == self.up {
            return None;
        }
        self.up = ready;
        Some(if ready {
            SessionInput::NetworkUp
        } else {
            SessionInput::NetworkDown
        })
    }

    /// `true` once the link was last observed ready.
    pub fn is_up(&self) -> bool {
        self.up
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use SessionInput as I;

    #[test]
    fn a_closed_link_reports_nothing() {
        let mut gate = NetworkGate::new();
        assert_eq!(gate.feed(false), None);
        assert_eq!(gate.feed(false), None);
        assert!(!gate.is_up());
    }

    #[test]
    fn the_first_ready_observation_opens_the_gate() {
        let mut gate = NetworkGate::new();
        assert_eq!(gate.feed(true), Some(I::NetworkUp));
        assert!(gate.is_up());
    }

    #[test]
    fn repeated_observations_report_nothing() {
        let mut gate = NetworkGate::new();
        assert_eq!(gate.feed(true), Some(I::NetworkUp));
        for _ in 0..5 {
            assert_eq!(
                gate.feed(true),
                None,
                "a duplicate must not repeat the edge"
            );
        }
    }

    #[test]
    fn losing_and_regaining_the_link_reports_each_edge_once() {
        let mut gate = NetworkGate::new();
        assert_eq!(gate.feed(true), Some(I::NetworkUp));
        assert_eq!(gate.feed(false), Some(I::NetworkDown));
        assert_eq!(gate.feed(false), None);
        assert_eq!(gate.feed(true), Some(I::NetworkUp));
        assert_eq!(gate.feed(true), None);
        assert_eq!(gate.feed(false), Some(I::NetworkDown));
    }

    #[test]
    fn a_flapping_link_reports_every_transition_in_order() {
        let mut gate = NetworkGate::new();
        let expected = [
            Some(I::NetworkUp),
            Some(I::NetworkDown),
            Some(I::NetworkUp),
            None,
            Some(I::NetworkDown),
            None,
            Some(I::NetworkUp),
        ];
        for (observation, want) in [true, false, true, true, false, false, true]
            .into_iter()
            .zip(expected)
        {
            assert_eq!(gate.feed(observation), want, "observation {observation}");
        }
    }

    #[test]
    fn a_lost_link_before_the_first_ip_reports_nothing() {
        // Boot without an IP, then a DHCP timeout reported as "still not ready":
        // the machine is offline already, so no `NetworkDown` may be emitted.
        let mut gate = NetworkGate::new();
        for _ in 0..3 {
            assert_eq!(gate.feed(false), None);
        }
        assert_eq!(gate.feed(true), Some(I::NetworkUp));
    }
}
