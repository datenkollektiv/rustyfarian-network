//! MQTT session lifecycle shared by both tiers (ADR 022 decision 10).
//!
//! [`SessionMachine`] decides *when* to connect, what to do after the broker
//! accepted a session, and when a session is ready; the tier decides *how*.
//! Feed it a [`SessionInput`], perform the returned [`SessionAction`], repeat.
//!
//! # Rules
//!
//! - **Network gate:** no attempt starts while the link has no IP
//!   ([`SessionInput::NetworkUp`] / [`SessionInput::NetworkDown`]); losing the
//!   network closes the transport instead of letting the driver retry blindly.
//! - **Connect work order:** after CONNACK the tier subscribes (unless the broker
//!   kept a session whose subscriptions all completed earlier; an incomplete or
//!   unknown one is subscribed again), then publishes the startup message, and only then
//!   reports [`SessionInput::WorkDone`]; the session is ready after that, never
//!   before its subscriptions were sent.
//! - **Stale-session guard:** every connect work carries a [`SessionToken`];
//!   a `WorkDone` or `WorkFailed` for an older session is ignored.
//! - **Publish gate:** [`SessionMachine::can_publish`] is `true` only while ready.
//! - **Retry pacing:** a failed attempt or lost session with the network still up
//!   waits [`SessionMachine::retry_ms`] before the next attempt.
//!
//! # Adapter contract
//!
//! - After [`SessionAction::Close`] the tier tears the transport down, stops any
//!   retries the driver would make on its own, and feeds [`SessionInput::Disconnected`].
//! - After [`SessionAction::Wait`] it feeds [`SessionInput::RetryElapsed`]; it keeps
//!   at most one retry timer.
//! - A driver that retries on its own may report [`SessionInput::AttemptStarted`].
//!
//! Inputs that do not fit the current state are ignored.

use super::MqttConnectionState;

/// Pause after a failed attempt or lost session while the network is up (milliseconds).
pub const SESSION_RETRY_MS: u64 = 5_000;

/// Identifies one broker session's connect work; see the stale-session guard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionToken(u32);

/// Event fed to [`SessionMachine::poll`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionInput {
    /// The link is ready (associated with an IP).
    NetworkUp,
    /// The link lost its IP or association.
    NetworkDown,
    /// The driver began an attempt on its own (for example esp-mqtt's retry).
    AttemptStarted,
    /// The broker accepted the CONNECT.
    Connected {
        /// The broker kept the previous session, so its subscriptions still stand.
        session_present: bool,
    },
    /// The connect work of this session finished.
    WorkDone(SessionToken),
    /// The connect work of this session failed (a subscribe or the startup message).
    WorkFailed(SessionToken),
    /// The session or the attempt ended.
    Disconnected,
    /// The delay of the last [`SessionAction::Wait`] has passed.
    RetryElapsed,
    /// Stop for good; no further attempts.
    Shutdown,
}

/// Action the tier performs before polling again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionAction {
    /// Nothing to do.
    None,
    /// Open the transport and send CONNECT.
    Connect,
    /// Subscribe if `subscribe`, then publish the startup message, then feed
    /// [`SessionInput::WorkDone`] or [`SessionInput::WorkFailed`] with `token`.
    ConnectWork {
        /// Session this work belongs to.
        token: SessionToken,
        /// `false` only when the broker kept a session whose subscriptions completed.
        subscribe: bool,
    },
    /// Tear the transport down and stop the driver's own retries, then feed
    /// [`SessionInput::Disconnected`].
    Close,
    /// Sleep this long, then feed [`SessionInput::RetryElapsed`].
    Wait {
        /// Delay in milliseconds.
        ms: u64,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// No session completed yet; waiting for the network.
    Starting,
    /// A session or attempt ended; waiting for the network.
    Offline,
    /// An attempt is in flight.
    Connecting,
    /// The broker accepted; the connect work for `token` runs.
    Working(SessionToken),
    /// Connect work done; the session is usable.
    Ready,
    /// Waiting out the retry pause.
    Retrying,
    /// The transport is being closed; `Disconnected` is next.
    Closing,
    /// Shut down for good.
    Shutdown,
}

/// Pure MQTT session lifecycle: network gate, connect work order, stale guard, retry pacing.
#[derive(Debug)]
pub struct SessionMachine {
    phase: Phase,
    network_up: bool,
    /// Generation of the latest session's connect work.
    generation: u32,
    /// Sessions that became ready since boot.
    connects: u32,
    /// The last connect work that subscribed completed, so a kept session holds
    /// every subscription.
    subscribed: bool,
    retry_ms: u64,
}

impl SessionMachine {
    /// A machine with the shared retry pause.
    pub const fn new() -> Self {
        Self::with_retry(SESSION_RETRY_MS)
    }

    /// A machine with a custom retry pause.
    pub const fn with_retry(retry_ms: u64) -> Self {
        Self {
            phase: Phase::Starting,
            network_up: false,
            generation: 0,
            connects: 0,
            subscribed: false,
            retry_ms,
        }
    }

    /// Observable session state.
    pub fn state(&self) -> MqttConnectionState {
        match self.phase {
            Phase::Starting | Phase::Connecting | Phase::Working(_) => {
                MqttConnectionState::Connecting
            }
            Phase::Ready => MqttConnectionState::Connected,
            Phase::Offline | Phase::Retrying | Phase::Closing => MqttConnectionState::Disconnected,
            Phase::Shutdown => MqttConnectionState::ShuttingDown,
        }
    }

    /// `true` only while the session is ready: the publish gate of `try_publish`.
    pub fn can_publish(&self) -> bool {
        self.phase == Phase::Ready
    }

    /// Sessions that became ready since boot.
    pub fn connect_count(&self) -> u32 {
        self.connects
    }

    /// `true` while the connect work of `token` is still the current one, so a
    /// worker can skip work for a session that has meanwhile ended.
    pub fn is_current(&self, token: SessionToken) -> bool {
        self.phase == Phase::Working(token)
    }

    /// Pause before the next attempt while the network is up (milliseconds).
    pub fn retry_ms(&self) -> u64 {
        self.retry_ms
    }

    /// Advances the machine and returns what the tier must do next.
    pub fn poll(&mut self, input: SessionInput) -> SessionAction {
        use Phase as P;
        use SessionAction as A;
        use SessionInput as I;

        if self.phase == P::Shutdown {
            return A::None;
        }
        match (self.phase, input) {
            (_, I::Shutdown) => {
                // `Retrying` counts: a driver that retries on its own is still active.
                let busy = matches!(
                    self.phase,
                    P::Connecting | P::Working(_) | P::Ready | P::Retrying
                );
                self.phase = P::Shutdown;
                if busy {
                    A::Close
                } else {
                    A::None
                }
            }
            (P::Starting | P::Offline, I::NetworkUp) => {
                self.network_up = true;
                self.phase = P::Connecting;
                A::Connect
            }
            (_, I::NetworkUp) => {
                self.network_up = true;
                A::None
            }
            (P::Connecting | P::Working(_) | P::Ready, I::NetworkDown) => {
                self.network_up = false;
                self.phase = P::Closing;
                A::Close
            }
            (P::Retrying, I::NetworkDown) => {
                // Stop a driver that would keep retrying on its own; `Disconnected` follows.
                self.network_up = false;
                self.phase = P::Closing;
                A::Close
            }
            (_, I::NetworkDown) => {
                self.network_up = false;
                A::None
            }
            (P::Retrying, I::AttemptStarted) => {
                self.phase = P::Connecting;
                A::None
            }
            (P::Starting | P::Offline, I::AttemptStarted) => {
                // The driver retried although the gate is closed: stop it.
                self.phase = P::Closing;
                A::Close
            }
            (P::Connecting, I::Connected { session_present }) => {
                self.generation = self.generation.wrapping_add(1);
                let token = SessionToken(self.generation);
                self.phase = P::Working(token);
                // A kept session is trusted only if its subscriptions completed;
                // until this work is done they count as incomplete.
                let subscribe = !(session_present && self.subscribed);
                if subscribe {
                    self.subscribed = false;
                }
                A::ConnectWork { token, subscribe }
            }
            (P::Working(current), I::WorkDone(token)) if token == current => {
                self.subscribed = true;
                self.phase = P::Ready;
                self.connects = self.connects.wrapping_add(1);
                A::None
            }
            (P::Working(current), I::WorkFailed(token)) if token == current => {
                self.phase = P::Closing;
                A::Close
            }
            (P::Connecting | P::Working(_) | P::Ready | P::Closing, I::Disconnected) => {
                if self.network_up {
                    self.phase = P::Retrying;
                    A::Wait { ms: self.retry_ms }
                } else {
                    self.phase = P::Offline;
                    A::None
                }
            }
            (P::Retrying, I::RetryElapsed) => {
                self.phase = P::Connecting;
                A::Connect
            }
            _ => A::None,
        }
    }
}

impl Default for SessionMachine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use SessionAction as A;
    use SessionInput as I;

    fn connect_work(m: &mut SessionMachine, session_present: bool) -> (SessionToken, bool) {
        match m.poll(I::Connected { session_present }) {
            A::ConnectWork { token, subscribe } => (token, subscribe),
            other => panic!("expected ConnectWork, got {:?}", other),
        }
    }

    fn working(m: &mut SessionMachine, session_present: bool) -> SessionToken {
        connect_work(m, session_present).0
    }

    fn reconnect(m: &mut SessionMachine) {
        assert_eq!(
            m.poll(I::Disconnected),
            A::Wait {
                ms: SESSION_RETRY_MS
            }
        );
        assert_eq!(m.poll(I::RetryElapsed), A::Connect);
    }

    fn ready() -> SessionMachine {
        let mut m = SessionMachine::new();
        m.poll(I::NetworkUp);
        let token = working(&mut m, false);
        m.poll(I::WorkDone(token));
        m
    }

    #[test]
    fn no_attempt_starts_before_the_network_is_up() {
        let mut m = SessionMachine::new();
        assert_eq!(m.state(), MqttConnectionState::Connecting);
        assert_eq!(m.poll(I::RetryElapsed), A::None);
        assert_eq!(
            m.poll(I::Connected {
                session_present: false
            }),
            A::None
        );
        assert_eq!(m.poll(I::NetworkUp), A::Connect);
    }

    #[test]
    fn the_session_is_ready_only_after_its_connect_work() {
        let mut m = SessionMachine::new();
        m.poll(I::NetworkUp);
        let token = working(&mut m, false);
        assert_eq!(m.state(), MqttConnectionState::Connecting);
        assert!(
            !m.can_publish(),
            "no publish before the subscriptions were sent"
        );
        assert_eq!(m.poll(I::WorkDone(token)), A::None);
        assert_eq!(m.state(), MqttConnectionState::Connected);
        assert!(m.can_publish());
        assert_eq!(m.connect_count(), 1);
    }

    #[test]
    fn a_kept_session_skips_the_subscriptions_once_they_completed() {
        let mut m = ready();
        reconnect(&mut m);
        assert!(!connect_work(&mut m, true).1);
    }

    #[test]
    fn a_new_session_always_subscribes() {
        let mut m = ready();
        reconnect(&mut m);
        assert!(connect_work(&mut m, false).1);
    }

    #[test]
    fn a_kept_session_from_before_boot_is_subscribed_again() {
        let mut m = SessionMachine::new();
        m.poll(I::NetworkUp);
        assert!(
            connect_work(&mut m, true).1,
            "nothing is known about an older session"
        );
    }

    #[test]
    fn a_kept_session_is_subscribed_again_after_failed_connect_work() {
        let mut m = SessionMachine::new();
        m.poll(I::NetworkUp);
        let (token, subscribe) = connect_work(&mut m, false);
        assert!(subscribe);
        assert_eq!(m.poll(I::WorkFailed(token)), A::Close);
        reconnect(&mut m);
        assert!(
            connect_work(&mut m, true).1,
            "the broker kept a session with incomplete subscriptions"
        );
    }

    #[test]
    fn a_kept_session_is_subscribed_again_after_a_drop_during_connect_work() {
        let mut m = ready();
        reconnect(&mut m);
        let (_, subscribe) = connect_work(&mut m, false);
        assert!(subscribe);
        reconnect(&mut m);
        assert!(connect_work(&mut m, true).1);
    }

    #[test]
    fn work_for_an_older_session_is_ignored() {
        let mut m = SessionMachine::new();
        m.poll(I::NetworkUp);
        let old = working(&mut m, false);
        m.poll(I::Disconnected);
        m.poll(I::RetryElapsed);
        let current = working(&mut m, false);
        assert_eq!(m.poll(I::WorkDone(old)), A::None);
        assert!(
            !m.can_publish(),
            "a stale WorkDone must not mark the new session ready"
        );
        assert_eq!(m.poll(I::WorkFailed(old)), A::None);
        assert_eq!(m.poll(I::WorkDone(current)), A::None);
        assert!(m.can_publish());
    }

    #[test]
    fn only_the_running_connect_work_is_current() {
        let mut m = SessionMachine::new();
        m.poll(I::NetworkUp);
        let old = working(&mut m, false);
        assert!(m.is_current(old));
        m.poll(I::Disconnected);
        assert!(!m.is_current(old), "a dropped session's work is stale");
        m.poll(I::RetryElapsed);
        let current = working(&mut m, false);
        assert!(!m.is_current(old));
        assert!(m.is_current(current));
        m.poll(I::WorkDone(current));
        assert!(!m.is_current(current), "finished work is no longer running");
    }

    #[test]
    fn work_done_after_the_session_dropped_is_ignored() {
        let mut m = SessionMachine::new();
        m.poll(I::NetworkUp);
        let token = working(&mut m, false);
        m.poll(I::Disconnected);
        assert_eq!(m.poll(I::WorkDone(token)), A::None);
        assert!(!m.can_publish());
        assert_eq!(m.connect_count(), 0);
    }

    #[test]
    fn failed_connect_work_closes_and_retries() {
        let mut m = SessionMachine::new();
        m.poll(I::NetworkUp);
        let token = working(&mut m, false);
        assert_eq!(m.poll(I::WorkFailed(token)), A::Close);
        assert_eq!(
            m.poll(I::Disconnected),
            A::Wait {
                ms: SESSION_RETRY_MS
            }
        );
        assert_eq!(m.poll(I::RetryElapsed), A::Connect);
    }

    #[test]
    fn a_lost_session_with_the_network_up_waits_then_reconnects() {
        let mut m = ready();
        assert_eq!(
            m.poll(I::Disconnected),
            A::Wait {
                ms: SESSION_RETRY_MS
            }
        );
        assert_eq!(m.state(), MqttConnectionState::Disconnected);
        assert!(!m.can_publish());
        assert_eq!(m.poll(I::RetryElapsed), A::Connect);
        assert_eq!(m.state(), MqttConnectionState::Connecting);
    }

    #[test]
    fn losing_the_network_closes_and_waits_for_it() {
        let mut m = ready();
        assert_eq!(m.poll(I::NetworkDown), A::Close);
        assert_eq!(
            m.poll(I::Disconnected),
            A::None,
            "no retry without a network"
        );
        assert_eq!(m.state(), MqttConnectionState::Disconnected);
        assert_eq!(m.poll(I::RetryElapsed), A::None);
        assert_eq!(
            m.poll(I::NetworkUp),
            A::Connect,
            "the network's return starts at once"
        );
    }

    #[test]
    fn losing_the_network_during_the_retry_pause_stops_the_driver() {
        let mut m = ready();
        m.poll(I::Disconnected);
        assert_eq!(
            m.poll(I::NetworkDown),
            A::Close,
            "a driver retrying on its own must be stopped"
        );
        assert_eq!(m.poll(I::Disconnected), A::None);
        assert_eq!(m.state(), MqttConnectionState::Disconnected);
        assert_eq!(m.poll(I::RetryElapsed), A::None);
        assert_eq!(m.poll(I::NetworkUp), A::Connect);
    }

    #[test]
    fn a_driver_attempt_while_offline_is_stopped() {
        let mut m = ready();
        m.poll(I::NetworkDown);
        m.poll(I::Disconnected);
        assert_eq!(m.poll(I::AttemptStarted), A::Close);
        assert_eq!(
            m.poll(I::Disconnected),
            A::None,
            "still no network, no retry"
        );
        let mut fresh = SessionMachine::new();
        assert_eq!(
            fresh.poll(I::AttemptStarted),
            A::Close,
            "nor before the first NetworkUp"
        );
    }

    #[test]
    fn shutdown_during_the_retry_pause_stops_the_driver() {
        let mut m = ready();
        m.poll(I::Disconnected);
        assert_eq!(m.poll(I::Shutdown), A::Close);
        assert_eq!(m.poll(I::AttemptStarted), A::None);
    }

    #[test]
    fn a_driver_retry_during_the_pause_counts_as_an_attempt() {
        let mut m = ready();
        m.poll(I::Disconnected);
        assert_eq!(m.poll(I::AttemptStarted), A::None);
        assert_eq!(m.state(), MqttConnectionState::Connecting);
        let token = working(&mut m, false);
        m.poll(I::WorkDone(token));
        assert_eq!(m.connect_count(), 2);
    }

    #[test]
    fn shutdown_closes_a_live_session_and_ignores_everything_after() {
        let mut m = ready();
        assert_eq!(m.poll(I::Shutdown), A::Close);
        assert_eq!(m.state(), MqttConnectionState::ShuttingDown);
        for input in [I::Disconnected, I::NetworkUp, I::RetryElapsed, I::Shutdown] {
            assert_eq!(m.poll(input), A::None);
        }
        assert!(!m.can_publish());
    }

    #[test]
    fn shutdown_while_offline_needs_no_close() {
        let mut m = SessionMachine::new();
        assert_eq!(m.poll(I::Shutdown), A::None);
        assert_eq!(m.poll(I::NetworkUp), A::None);
    }

    #[test]
    fn duplicate_network_up_while_connected_changes_nothing() {
        let mut m = ready();
        assert_eq!(m.poll(I::NetworkUp), A::None);
        assert!(m.can_publish());
        assert_eq!(m.connect_count(), 1);
    }
}
