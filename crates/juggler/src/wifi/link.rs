//! Station link supervision shared by both tiers.
//!
//! [`LinkMachine`] decides *when* to (re)connect; the tier decides *how*.
//! Feed it a [`LinkInput`], perform the returned [`LinkAction`], repeat.
//! The ESP-IDF tier runs it on a worker thread fed by the system event loop,
//! the esp-hal tier inside `WifiLink::run`.
//!
//! Back-off starts at [`RECONNECT_BACKOFF_BASE_MS`], doubles per failed
//! attempt or link loss, caps at [`RECONNECT_BACKOFF_MAX_MS`] and resets on
//! [`LinkInput::Connected`]. The delays are deterministic (no jitter).
//!
//! # Suspend
//!
//! [`LinkInput::Suspend`] parks the link from any state: the machine answers
//! [`LinkAction::Disconnect`] and ignores link events and back-off timers until
//! [`LinkInput::Resume`], which answers [`LinkAction::Connect`] with a fresh
//! back-off. A `Connected` that lands while suspended (a connect that was already
//! in flight) is answered with another `Disconnect`. Duplicate suspends and
//! resumes change nothing.
//!
//! What `Disconnect` does to the radio is tier-specific: the ESP-IDF tier stops
//! it, the esp-hal tier only disconnects the station and the radio stays powered.
//!
//! # Readiness
//!
//! Association alone is not enough to use the network: [`LinkMachine::is_ready`]
//! turns `true` only once the tier also reports the IP configuration with
//! [`LinkInput::NetworkUp`], and `false` again on [`LinkInput::NetworkDown`],
//! any link loss or a suspend. [`LinkMachine::connect_count`] counts every
//! association since boot, except one that lands while suspended.
//!
//! # Adapter contract
//!
//! Inputs that do not fit the current state are ignored (a duplicate `Start`,
//! a `Disconnected` before the first connect, a `BackoffElapsed` outside
//! `Backoff`). The machine cannot tell an old `BackoffElapsed` from the current
//! one, so an adapter keeps at most one back-off timer and cancels it when it
//! sees `Connected` or issues a connect.

use crate::backoff::ExponentialBackoff;

/// First reconnect delay after a link loss or a failed attempt (milliseconds).
pub const RECONNECT_BACKOFF_BASE_MS: u64 = 1_000;

/// Upper bound of the reconnect delay (milliseconds).
pub const RECONNECT_BACKOFF_MAX_MS: u64 = 30_000;

/// Event fed to [`LinkMachine::poll`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkInput {
    /// Begin supervising; the machine answers with [`LinkAction::Connect`].
    Start,
    /// The driver reported the station associated.
    Connected,
    /// The driver reported the station gone, or a connect attempt failed.
    Disconnected,
    /// The delay of the last [`LinkAction::Wait`] has passed.
    BackoffElapsed,
    /// Park the link (for example before deep sleep); answered with [`LinkAction::Disconnect`].
    Suspend,
    /// Leave [`LinkState::Suspended`]; answered with [`LinkAction::Connect`].
    Resume,
    /// The station has its IP configuration; ignored unless `Connected`.
    NetworkUp,
    /// The station lost its IP configuration while still associated.
    NetworkDown,
}

/// Action the tier performs before polling again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkAction {
    /// Nothing to do.
    None,
    /// Issue a connect request to the driver.
    Connect,
    /// Take the station down (disconnect, and on ESP-IDF stop the radio).
    Disconnect,
    /// Sleep this long, then feed [`LinkInput::BackoffElapsed`].
    Wait {
        /// Delay in milliseconds.
        ms: u64,
    },
}

/// Where the supervised link currently is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    /// Not started; inputs other than `Start`, `Connected` and `Suspend` are ignored.
    Idle,
    /// A connect request is in flight.
    Connecting,
    /// Associated.
    Connected,
    /// Waiting out the back-off before the next attempt.
    Backoff,
    /// Parked by [`LinkInput::Suspend`]; only `Resume` leaves it.
    Suspended,
}

/// Pure reconnect policy: back-off, reset on success, suspend, ignores out-of-state inputs.
#[derive(Debug)]
pub struct LinkMachine {
    state: LinkState,
    backoff: ExponentialBackoff,
    max_ms: u64,
    failed_attempts: u32,
    /// The tier reported the IP configuration for the current association.
    network_up: bool,
    /// Associations since boot, not counting one that lands while suspended.
    connects: u32,
}

impl LinkMachine {
    /// A machine with the shared defaults.
    pub const fn new() -> Self {
        Self::with_backoff(RECONNECT_BACKOFF_BASE_MS, RECONNECT_BACKOFF_MAX_MS)
    }

    /// A machine with a custom back-off window.
    pub const fn with_backoff(base_ms: u64, max_ms: u64) -> Self {
        Self {
            state: LinkState::Idle,
            backoff: ExponentialBackoff::new(base_ms, max_ms),
            max_ms,
            failed_attempts: 0,
            network_up: false,
            connects: 0,
        }
    }

    /// Current state.
    pub fn state(&self) -> LinkState {
        self.state
    }

    /// Consecutive failed attempts and link losses since the last successful association.
    pub fn failed_attempts(&self) -> u32 {
        self.failed_attempts
    }

    /// `true` while associated and the tier has reported the IP configuration.
    pub fn is_ready(&self) -> bool {
        self.state == LinkState::Connected && self.network_up
    }

    /// Associations since boot, not counting one that lands while suspended.
    pub fn connect_count(&self) -> u32 {
        self.connects
    }

    /// Returns to [`LinkState::Idle`] with a fresh back-off, keeping [`connect_count`](Self::connect_count).
    pub fn restart(&mut self) {
        self.state = LinkState::Idle;
        self.backoff.reset();
        self.failed_attempts = 0;
        self.network_up = false;
    }

    /// Advances the machine and returns what the tier must do next.
    pub fn poll(&mut self, input: LinkInput) -> LinkAction {
        use LinkAction as A;
        use LinkInput as I;
        use LinkState as S;

        let action = match (self.state, input) {
            (S::Connected, I::NetworkUp) => {
                self.network_up = true;
                A::None
            }
            (_, I::NetworkUp | I::NetworkDown) => {
                self.network_up = false;
                A::None
            }
            (S::Suspended, I::Suspend | I::Disconnected | I::BackoffElapsed | I::Start) => A::None,
            (S::Suspended, I::Connected) => A::Disconnect,
            (S::Suspended, I::Resume) => {
                self.state = S::Connecting;
                self.backoff.reset();
                self.failed_attempts = 0;
                A::Connect
            }
            (_, I::Suspend) => {
                self.state = S::Suspended;
                A::Disconnect
            }
            (S::Idle, I::Start) | (S::Backoff, I::BackoffElapsed) => {
                self.state = S::Connecting;
                A::Connect
            }
            (S::Idle | S::Connecting | S::Backoff, I::Connected) => {
                self.state = S::Connected;
                self.connects = self.connects.wrapping_add(1);
                self.backoff.reset();
                self.failed_attempts = 0;
                A::None
            }
            (S::Connecting | S::Connected, I::Disconnected) => {
                self.state = S::Backoff;
                self.failed_attempts = self.failed_attempts.saturating_add(1);
                A::Wait {
                    ms: self.backoff.next().unwrap_or(self.max_ms),
                }
            }
            _ => A::None,
        };
        // The IP belongs to one association; it never outlives `Connected`.
        if self.state != S::Connected {
            self.network_up = false;
        }
        action
    }
}

impl Default for LinkMachine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connected() -> LinkMachine {
        let mut m = LinkMachine::new();
        m.poll(LinkInput::Start);
        m.poll(LinkInput::Connected);
        m
    }

    #[test]
    fn ready_needs_the_network_after_the_association() {
        let mut m = LinkMachine::new();
        m.poll(LinkInput::Start);
        assert_eq!(m.poll(LinkInput::NetworkUp), LinkAction::None);
        assert!(!m.is_ready(), "an IP before the association is stale");
        m.poll(LinkInput::Connected);
        assert!(!m.is_ready());
        assert_eq!(m.poll(LinkInput::NetworkUp), LinkAction::None);
        assert!(m.is_ready());
    }

    #[test]
    fn losing_the_network_keeps_the_association() {
        let mut m = connected();
        m.poll(LinkInput::NetworkUp);
        assert_eq!(m.poll(LinkInput::NetworkDown), LinkAction::None);
        assert!(!m.is_ready());
        assert_eq!(m.state(), LinkState::Connected);
        m.poll(LinkInput::NetworkUp);
        assert!(m.is_ready());
    }

    #[test]
    fn a_link_loss_or_suspend_clears_readiness() {
        let mut m = connected();
        m.poll(LinkInput::NetworkUp);
        m.poll(LinkInput::Disconnected);
        assert!(!m.is_ready());
        m.poll(LinkInput::BackoffElapsed);
        m.poll(LinkInput::Connected);
        assert!(!m.is_ready(), "a new association waits for its own IP");
        m.poll(LinkInput::NetworkUp);
        m.poll(LinkInput::Suspend);
        assert!(!m.is_ready());
        m.poll(LinkInput::NetworkUp);
        assert!(!m.is_ready(), "no readiness while suspended");
    }

    #[test]
    fn stray_inputs_while_connected_keep_readiness() {
        let mut m = connected();
        m.poll(LinkInput::NetworkUp);
        for input in [
            LinkInput::Start,
            LinkInput::BackoffElapsed,
            LinkInput::Resume,
            LinkInput::Connected,
        ] {
            assert_eq!(m.poll(input), LinkAction::None);
            assert!(m.is_ready(), "{:?} must not drop readiness", input);
        }
    }

    #[test]
    fn restart_returns_to_idle_and_keeps_the_count() {
        let mut m = connected();
        m.poll(LinkInput::NetworkUp);
        m.restart();
        assert_eq!(m.state(), LinkState::Idle);
        assert!(!m.is_ready());
        assert_eq!(m.connect_count(), 1);
        assert_eq!(m.poll(LinkInput::Start), LinkAction::Connect);
    }

    #[test]
    fn connect_count_counts_associations_but_not_while_suspended() {
        let mut m = LinkMachine::new();
        assert_eq!(m.poll(LinkInput::Connected), LinkAction::None);
        assert_eq!(m.connect_count(), 1, "an adopted association counts");
        m.poll(LinkInput::Connected);
        assert_eq!(m.connect_count(), 1, "a duplicate does not");
        m.poll(LinkInput::Disconnected);
        m.poll(LinkInput::BackoffElapsed);
        m.poll(LinkInput::Connected);
        assert_eq!(m.connect_count(), 2);
        m.poll(LinkInput::Suspend);
        assert_eq!(m.poll(LinkInput::Connected), LinkAction::Disconnect);
        assert_eq!(
            m.connect_count(),
            2,
            "a late association while suspended does not"
        );
        m.poll(LinkInput::Resume);
        m.poll(LinkInput::Connected);
        assert_eq!(m.connect_count(), 3);
    }

    fn drive_failure(m: &mut LinkMachine) -> u64 {
        match m.poll(LinkInput::Disconnected) {
            LinkAction::Wait { ms } => ms,
            other => panic!("expected Wait, got {:?}", other),
        }
    }

    #[test]
    fn start_requests_a_connect() {
        let mut m = LinkMachine::new();
        assert_eq!(m.state(), LinkState::Idle);
        assert_eq!(m.poll(LinkInput::Start), LinkAction::Connect);
        assert_eq!(m.state(), LinkState::Connecting);
    }

    #[test]
    fn connected_before_start_is_adopted() {
        let mut m = LinkMachine::new();
        assert_eq!(m.poll(LinkInput::Connected), LinkAction::None);
        assert_eq!(m.state(), LinkState::Connected);
        assert_eq!(m.poll(LinkInput::Start), LinkAction::None);
    }

    #[test]
    fn disconnect_waits_the_base_delay_then_connects() {
        let mut m = LinkMachine::new();
        m.poll(LinkInput::Start);
        m.poll(LinkInput::Connected);
        assert_eq!(
            m.poll(LinkInput::Disconnected),
            LinkAction::Wait {
                ms: RECONNECT_BACKOFF_BASE_MS
            }
        );
        assert_eq!(m.state(), LinkState::Backoff);
        assert_eq!(m.poll(LinkInput::BackoffElapsed), LinkAction::Connect);
        assert_eq!(m.state(), LinkState::Connecting);
    }

    #[test]
    fn repeated_failures_double_and_cap() {
        let mut m = LinkMachine::new();
        m.poll(LinkInput::Start);
        let mut seen = alloc::vec::Vec::new();
        for _ in 0..8 {
            seen.push(drive_failure(&mut m));
            assert_eq!(m.poll(LinkInput::BackoffElapsed), LinkAction::Connect);
        }
        assert_eq!(
            seen,
            [1_000, 2_000, 4_000, 8_000, 16_000, 30_000, 30_000, 30_000]
        );
        assert_eq!(m.failed_attempts(), 8);
    }

    #[test]
    fn connect_resets_the_backoff() {
        let mut m = LinkMachine::new();
        m.poll(LinkInput::Start);
        drive_failure(&mut m);
        m.poll(LinkInput::BackoffElapsed);
        drive_failure(&mut m);
        m.poll(LinkInput::BackoffElapsed);
        assert_eq!(m.poll(LinkInput::Connected), LinkAction::None);
        assert_eq!(m.failed_attempts(), 0);
        assert_eq!(drive_failure(&mut m), RECONNECT_BACKOFF_BASE_MS);
    }

    #[test]
    fn stale_or_duplicate_inputs_are_noops() {
        let mut m = LinkMachine::new();
        assert_eq!(m.poll(LinkInput::Disconnected), LinkAction::None);
        assert_eq!(m.poll(LinkInput::BackoffElapsed), LinkAction::None);
        assert_eq!(m.state(), LinkState::Idle);
        m.poll(LinkInput::Start);
        assert_eq!(m.poll(LinkInput::Start), LinkAction::None);
        assert_eq!(m.poll(LinkInput::BackoffElapsed), LinkAction::None);
        m.poll(LinkInput::Connected);
        assert_eq!(m.poll(LinkInput::Connected), LinkAction::None);
        drive_failure(&mut m);
        assert_eq!(m.poll(LinkInput::Disconnected), LinkAction::None);
        assert_eq!(m.poll(LinkInput::Start), LinkAction::None);
        assert_eq!(m.state(), LinkState::Backoff);
    }

    #[test]
    fn late_connect_during_backoff_is_adopted() {
        let mut m = LinkMachine::new();
        m.poll(LinkInput::Start);
        drive_failure(&mut m);
        assert_eq!(m.poll(LinkInput::Connected), LinkAction::None);
        assert_eq!(m.state(), LinkState::Connected);
        assert_eq!(m.poll(LinkInput::BackoffElapsed), LinkAction::None);
        assert_eq!(m.state(), LinkState::Connected);
    }

    /// One machine per non-suspended state, reached through public inputs only.
    fn in_every_state() -> [LinkMachine; 4] {
        let idle = LinkMachine::new();
        let mut connecting = LinkMachine::new();
        connecting.poll(LinkInput::Start);
        let mut connected = LinkMachine::new();
        connected.poll(LinkInput::Connected);
        let mut backoff = LinkMachine::new();
        backoff.poll(LinkInput::Start);
        drive_failure(&mut backoff);
        [idle, connecting, connected, backoff]
    }

    #[test]
    fn suspend_from_every_state_disconnects() {
        for mut m in in_every_state() {
            let from = m.state();
            assert_eq!(
                m.poll(LinkInput::Suspend),
                LinkAction::Disconnect,
                "from {:?}",
                from
            );
            assert_eq!(m.state(), LinkState::Suspended, "from {:?}", from);
        }
    }

    #[test]
    fn resume_connects_with_a_fresh_backoff() {
        let mut m = LinkMachine::new();
        m.poll(LinkInput::Start);
        drive_failure(&mut m);
        m.poll(LinkInput::BackoffElapsed);
        drive_failure(&mut m);
        assert_eq!(m.failed_attempts(), 2);
        m.poll(LinkInput::Suspend);
        assert_eq!(m.poll(LinkInput::Resume), LinkAction::Connect);
        assert_eq!(m.state(), LinkState::Connecting);
        assert_eq!(m.failed_attempts(), 0);
        assert_eq!(drive_failure(&mut m), RECONNECT_BACKOFF_BASE_MS);
    }

    #[test]
    fn link_events_and_timers_are_ignored_while_suspended() {
        let mut m = LinkMachine::new();
        m.poll(LinkInput::Start);
        drive_failure(&mut m);
        m.poll(LinkInput::Suspend);
        assert_eq!(m.poll(LinkInput::Disconnected), LinkAction::None);
        assert_eq!(m.poll(LinkInput::BackoffElapsed), LinkAction::None);
        assert_eq!(m.poll(LinkInput::Start), LinkAction::None);
        assert_eq!(m.state(), LinkState::Suspended);
        assert_eq!(m.failed_attempts(), 1);
    }

    #[test]
    fn connect_landing_after_suspend_is_disconnected_again() {
        let mut m = LinkMachine::new();
        m.poll(LinkInput::Start);
        m.poll(LinkInput::Suspend);
        assert_eq!(m.poll(LinkInput::Connected), LinkAction::Disconnect);
        assert_eq!(m.state(), LinkState::Suspended);
        assert_eq!(m.poll(LinkInput::Connected), LinkAction::Disconnect);
        assert_eq!(m.state(), LinkState::Suspended);
    }

    #[test]
    fn duplicate_suspend_and_resume_change_nothing() {
        let mut m = LinkMachine::new();
        m.poll(LinkInput::Connected);
        assert_eq!(m.poll(LinkInput::Resume), LinkAction::None);
        assert_eq!(m.state(), LinkState::Connected);
        assert_eq!(m.poll(LinkInput::Suspend), LinkAction::Disconnect);
        assert_eq!(m.poll(LinkInput::Suspend), LinkAction::None);
        assert_eq!(m.state(), LinkState::Suspended);
        assert_eq!(m.poll(LinkInput::Resume), LinkAction::Connect);
        assert_eq!(m.poll(LinkInput::Resume), LinkAction::None);
        assert_eq!(m.state(), LinkState::Connecting);
    }

    #[test]
    fn suspend_and_resume_within_one_attempt_issue_one_connect() {
        let mut m = LinkMachine::new();
        m.poll(LinkInput::Start);
        m.poll(LinkInput::Connected);
        assert_eq!(m.poll(LinkInput::Suspend), LinkAction::Disconnect);
        assert_eq!(m.poll(LinkInput::Resume), LinkAction::Connect);
        assert_eq!(m.poll(LinkInput::Connected), LinkAction::None);
        assert_eq!(m.state(), LinkState::Connected);
    }

    #[test]
    fn custom_backoff_is_honoured() {
        let mut m = LinkMachine::with_backoff(100, 250);
        m.poll(LinkInput::Start);
        assert_eq!(drive_failure(&mut m), 100);
        m.poll(LinkInput::BackoffElapsed);
        assert_eq!(drive_failure(&mut m), 200);
        m.poll(LinkInput::BackoffElapsed);
        assert_eq!(drive_failure(&mut m), 250);
    }
}
