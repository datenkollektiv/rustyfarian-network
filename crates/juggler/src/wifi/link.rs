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
}

/// Action the tier performs before polling again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkAction {
    /// Nothing to do.
    None,
    /// Issue a connect request to the driver.
    Connect,
    /// Sleep this long, then feed [`LinkInput::BackoffElapsed`].
    Wait {
        /// Delay in milliseconds.
        ms: u64,
    },
}

/// Where the supervised link currently is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkState {
    /// Not started; inputs other than `Start` and `Connected` are ignored.
    Idle,
    /// A connect request is in flight.
    Connecting,
    /// Associated.
    Connected,
    /// Waiting out the back-off before the next attempt.
    Backoff,
}

/// Pure reconnect policy: back-off, reset on success, ignores out-of-state inputs.
#[derive(Debug)]
pub struct LinkMachine {
    state: LinkState,
    backoff: ExponentialBackoff,
    max_ms: u64,
    failed_attempts: u32,
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

    /// Advances the machine and returns what the tier must do next.
    pub fn poll(&mut self, input: LinkInput) -> LinkAction {
        use LinkAction as A;
        use LinkInput as I;
        use LinkState as S;

        match (self.state, input) {
            (S::Idle, I::Start) | (S::Backoff, I::BackoffElapsed) => {
                self.state = S::Connecting;
                A::Connect
            }
            (S::Idle | S::Connecting | S::Backoff, I::Connected) => {
                self.state = S::Connected;
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
        }
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
