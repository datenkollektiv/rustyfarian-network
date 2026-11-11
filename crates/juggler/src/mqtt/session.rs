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
//!   unknown one is subscribed again), then publishes the startup message, then
//!   runs its connect callback, and only then reports [`SessionInput::WorkDone`];
//!   the session is ready after that, never before its subscriptions were sent.
//!   [`ConnectWorkPlan`] yields exactly this order, so no tier spells it out.
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
//! - After [`SessionAction::ConnectWork`] it runs the steps of a
//!   [`ConnectWorkPlan`] in order and feeds [`SessionInput::WorkDone`] or, at
//!   the first failed step, [`SessionInput::WorkFailed`]; it does so by driving
//!   a [`ConnectWorkMachine`](super::ConnectWorkMachine) built from that plan,
//!   one action per turn, and feeding the input the machine reports.
//! - After [`SessionAction::Ready`] it wakes the publishers waiting for the session.
//! - A driver that retries on its own may report [`SessionInput::AttemptStarted`].
//!
//! Inputs that do not fit the current state are answered with
//! [`SessionAction::Ignored`] and leave the state unchanged (ADR 023 decision 5);
//! `Ignored` is never a request for I/O, but a tier loop that sees it on its own
//! transitions has fallen out of step with the machine.

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
    /// The input was accepted; nothing to do.
    None,
    /// The input did not fit the current state and was dropped; the state is
    /// unchanged and nothing is to be done.
    Ignored,
    /// Open the transport and send CONNECT.
    Connect,
    /// Run the steps of a [`ConnectWorkPlan`] built with `subscribe`, then feed
    /// [`SessionInput::WorkDone`] or [`SessionInput::WorkFailed`] with `token`.
    ConnectWork {
        /// Session this work belongs to.
        token: SessionToken,
        /// `false` only when the broker kept a session whose subscriptions completed.
        subscribe: bool,
        /// The broker's session-present flag of this connection, for the
        /// tier's connect callback.
        session_present: bool,
    },
    /// The connect work was accepted and the session is now publishable: wake
    /// the publishers waiting for it.
    Ready,
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
    ///
    /// `driver_idle` records whether the driver had already dropped the
    /// transport when it asked for the close: an idle driver posts no event of
    /// its own, so the tier has to report `Disconnected` itself.
    Closing {
        /// `true` when the driver was already disconnected (it was in the retry pause).
        driver_idle: bool,
    },
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
            Phase::Offline | Phase::Retrying | Phase::Closing { .. } => {
                MqttConnectionState::Disconnected
            }
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

    /// `true` while the machine is closing and the driver had already dropped
    /// its transport, so it will report no end of its own: the tier reports
    /// [`SessionInput::Disconnected`] itself after performing the `Close`.
    ///
    /// `false` at every other time, including a close of a live transport — that
    /// one ends in a driver event.
    pub fn driver_idle(&self) -> bool {
        matches!(self.phase, Phase::Closing { driver_idle: true })
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
            return A::Ignored;
        }
        match (self.phase, input) {
            (_, I::Shutdown) => {
                // `Retrying` counts: a driver that retries on its own is still active,
                // but it holds no transport, so it reports no end of its own.
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
            (_, I::NetworkUp) if self.network_up => A::Ignored,
            (_, I::NetworkUp) => {
                self.network_up = true;
                A::None
            }
            (P::Connecting | P::Working(_) | P::Ready, I::NetworkDown) => {
                self.network_up = false;
                self.phase = P::Closing { driver_idle: false };
                A::Close
            }
            (P::Retrying, I::NetworkDown) => {
                // The driver sits in its retry pause with no transport, so the
                // tier stops it and reports `Disconnected` itself.
                self.network_up = false;
                self.phase = P::Closing { driver_idle: true };
                A::Close
            }
            (_, I::NetworkDown) if !self.network_up => A::Ignored,
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
                self.phase = P::Closing { driver_idle: false };
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
                A::ConnectWork {
                    token,
                    subscribe,
                    session_present,
                }
            }
            (P::Working(current), I::WorkDone(token)) if token == current => {
                self.subscribed = true;
                self.phase = P::Ready;
                self.connects = self.connects.wrapping_add(1);
                A::Ready
            }
            (P::Working(current), I::WorkFailed(token)) if token == current => {
                self.phase = P::Closing { driver_idle: false };
                A::Close
            }
            (P::Connecting | P::Working(_) | P::Ready | P::Closing { .. }, I::Disconnected) => {
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
            _ => A::Ignored,
        }
    }
}

/// One step of a session's connect work; see [`ConnectWorkPlan`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectStep {
    /// Subscribe to the tier's subscription with this index.
    Subscribe(usize),
    /// Publish the startup message.
    PublishStartup,
    /// Run the tier's connect callback.
    RunOnConnect,
}

/// The connect work of one session, in the shared order: every subscription
/// in index order (only when the session needs them), then the startup
/// message, then the connect callback.
///
/// A tier builds it from [`SessionAction::ConnectWork`] and what it has
/// configured, maps each [`ConnectStep`] to one driver call, and stops at the
/// first failure.
#[derive(Debug, Clone)]
pub struct ConnectWorkPlan {
    subscriptions: core::ops::Range<usize>,
    startup: bool,
    on_connect: bool,
}

impl ConnectWorkPlan {
    /// The plan for one connect work.
    ///
    /// `subscribe` is the flag of [`SessionAction::ConnectWork`]; `subscriptions`
    /// is how many the tier has; `startup` and `on_connect` say whether a startup
    /// message and a connect callback are configured.
    pub fn new(subscribe: bool, subscriptions: usize, startup: bool, on_connect: bool) -> Self {
        Self {
            subscriptions: 0..if subscribe { subscriptions } else { 0 },
            startup,
            on_connect,
        }
    }

    /// `true` when no step is left, so the tier can report
    /// [`SessionInput::WorkDone`] at once.
    pub fn is_empty(&self) -> bool {
        self.subscriptions.is_empty() && !self.startup && !self.on_connect
    }
}

impl Iterator for ConnectWorkPlan {
    type Item = ConnectStep;

    fn next(&mut self) -> Option<ConnectStep> {
        if let Some(index) = self.subscriptions.next() {
            return Some(ConnectStep::Subscribe(index));
        }
        if core::mem::take(&mut self.startup) {
            return Some(ConnectStep::PublishStartup);
        }
        if core::mem::take(&mut self.on_connect) {
            return Some(ConnectStep::RunOnConnect);
        }
        None
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
            A::ConnectWork {
                token, subscribe, ..
            } => (token, subscribe),
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
        assert_eq!(m.poll(I::RetryElapsed), A::Ignored);
        assert_eq!(
            m.poll(I::Connected {
                session_present: false
            }),
            A::Ignored
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
        assert_eq!(m.poll(I::WorkDone(token)), A::Ready);
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
        assert_eq!(m.poll(I::WorkDone(old)), A::Ignored);
        assert!(
            !m.can_publish(),
            "a stale WorkDone must not mark the new session ready"
        );
        assert_eq!(m.poll(I::WorkFailed(old)), A::Ignored);
        assert_eq!(m.poll(I::WorkDone(current)), A::Ready);
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
        assert_eq!(m.poll(I::WorkDone(token)), A::Ignored);
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
        assert_eq!(m.poll(I::RetryElapsed), A::Ignored);
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
        assert_eq!(m.poll(I::RetryElapsed), A::Ignored);
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
        assert_eq!(m.poll(I::AttemptStarted), A::Ignored);
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
    fn a_close_of_a_live_transport_waits_for_the_driver_to_report_it() {
        // Ready, Working, Connecting and a failed connect work all sit on a live
        // transport: the driver ends it and posts `Disconnected`.
        let mut ready = ready();
        assert_eq!(ready.poll(I::NetworkDown), A::Close);
        assert!(
            !ready.driver_idle(),
            "a connected client reports its own disconnect"
        );

        let mut connecting = SessionMachine::new();
        connecting.poll(I::NetworkUp);
        assert_eq!(connecting.poll(I::NetworkDown), A::Close);
        assert!(!connecting.driver_idle());

        let mut working_state = SessionMachine::new();
        working_state.poll(I::NetworkUp);
        let token = working(&mut working_state, false);
        assert_eq!(working_state.poll(I::WorkFailed(token)), A::Close);
        assert!(!working_state.driver_idle());
    }

    #[test]
    fn a_close_from_the_retry_pause_is_reported_by_the_tier() {
        // In `Retrying` the driver holds no transport, so no driver event will
        // follow: the tier has to feed `Disconnected` itself, and needs to know.
        let mut m = ready();
        m.poll(I::Disconnected);
        assert_eq!(m.poll(I::NetworkDown), A::Close);
        assert!(
            m.driver_idle(),
            "an idle driver reports nothing, so the tier must"
        );
        assert_eq!(m.poll(I::Disconnected), A::None);
        assert_eq!(m.state(), MqttConnectionState::Disconnected);
        assert_eq!(m.poll(I::NetworkUp), A::Connect);
    }

    #[test]
    fn the_idle_flag_lasts_only_while_closing() {
        let mut m = ready();
        m.poll(I::Disconnected);
        assert!(!m.driver_idle(), "a retry pause is not a close yet");
        m.poll(I::NetworkDown);
        assert!(m.driver_idle());
        m.poll(I::Disconnected);
        assert!(!m.driver_idle(), "the close is over");
    }

    #[test]
    fn a_close_that_is_never_answered_leaves_the_machine_stuck() {
        // Why `driver_idle` exists: the contract is that the tier always feeds
        // `Disconnected` after `Close`, from the driver or by itself.
        let mut m = ready();
        m.poll(I::NetworkDown);
        assert_eq!(m.poll(I::RetryElapsed), A::Ignored);
        assert_eq!(
            m.poll(I::NetworkUp),
            A::None,
            "an unanswered close blocks the next network up"
        );
    }

    #[test]
    fn shutdown_closes_a_live_session_and_ignores_everything_after() {
        let mut m = ready();
        assert_eq!(m.poll(I::Shutdown), A::Close);
        assert_eq!(m.state(), MqttConnectionState::ShuttingDown);
        for input in [I::Disconnected, I::NetworkUp, I::RetryElapsed, I::Shutdown] {
            assert_eq!(m.poll(input), A::Ignored);
        }
        assert!(!m.can_publish());
    }

    #[test]
    fn shutdown_while_offline_needs_no_close() {
        let mut m = SessionMachine::new();
        assert_eq!(m.poll(I::Shutdown), A::None);
        assert_eq!(m.poll(I::NetworkUp), A::Ignored);
    }

    #[test]
    fn duplicate_network_up_while_connected_changes_nothing() {
        let mut m = ready();
        assert_eq!(m.poll(I::NetworkUp), A::Ignored);
        assert!(m.can_publish());
        assert_eq!(m.connect_count(), 1);
    }

    #[test]
    fn work_done_returns_ready() {
        let mut m = SessionMachine::new();
        m.poll(I::NetworkUp);
        let token = working(&mut m, false);
        assert_eq!(m.poll(I::WorkDone(token)), A::Ready);
        assert!(m.can_publish());
    }

    #[test]
    fn duplicate_network_up_is_ignored() {
        let mut m = SessionMachine::new();
        assert_eq!(m.poll(I::NetworkUp), A::Connect);
        assert_eq!(m.poll(I::NetworkUp), A::Ignored);
        assert_eq!(m.state(), MqttConnectionState::Connecting);
        m.poll(I::Disconnected);
        assert_eq!(
            m.poll(I::NetworkUp),
            A::Ignored,
            "the retry pause already knows the network is up"
        );
    }

    #[test]
    fn network_up_that_changes_the_flag_is_accepted() {
        // Closing after a NetworkDown: the flag flips back, nothing to do yet.
        let mut m = ready();
        m.poll(I::NetworkDown);
        assert_eq!(m.poll(I::NetworkUp), A::None);
        assert_eq!(
            m.poll(I::Disconnected),
            A::Wait {
                ms: SESSION_RETRY_MS
            },
            "the close ends with the network up again, so a retry follows"
        );
    }

    #[test]
    fn duplicate_network_down_is_ignored() {
        let mut fresh = SessionMachine::new();
        assert_eq!(
            fresh.poll(I::NetworkDown),
            A::Ignored,
            "the network was never up"
        );
        let mut m = ready();
        assert_eq!(m.poll(I::NetworkDown), A::Close);
        assert_eq!(m.poll(I::NetworkDown), A::Ignored);
        assert_eq!(m.poll(I::Disconnected), A::None);
        assert_eq!(m.poll(I::NetworkDown), A::Ignored);
        assert_eq!(m.state(), MqttConnectionState::Disconnected);
    }

    #[test]
    fn network_down_that_changes_the_flag_is_accepted() {
        // Closing after a failed connect work keeps the network up until told.
        let mut m = SessionMachine::new();
        m.poll(I::NetworkUp);
        let token = working(&mut m, false);
        assert_eq!(m.poll(I::WorkFailed(token)), A::Close);
        assert_eq!(m.poll(I::NetworkDown), A::None);
        assert_eq!(m.poll(I::Disconnected), A::None, "offline, no retry");
    }

    fn all_inputs(token: SessionToken) -> [SessionInput; 10] {
        [
            I::NetworkUp,
            I::NetworkDown,
            I::AttemptStarted,
            I::Connected {
                session_present: false,
            },
            I::Connected {
                session_present: true,
            },
            I::WorkDone(token),
            I::WorkFailed(token),
            I::Disconnected,
            I::RetryElapsed,
            I::Shutdown,
        ]
    }

    #[test]
    fn every_input_after_shutdown_is_ignored() {
        let mut m = SessionMachine::new();
        m.poll(I::NetworkUp);
        let token = working(&mut m, false);
        assert_eq!(m.poll(I::Shutdown), A::Close);
        for input in all_inputs(token) {
            assert_eq!(m.poll(input), A::Ignored, "{:?}", input);
            assert_eq!(m.state(), MqttConnectionState::ShuttingDown);
        }
    }

    #[test]
    fn stale_token_is_ignored() {
        let mut m = SessionMachine::new();
        m.poll(I::NetworkUp);
        let old = working(&mut m, false);
        reconnect(&mut m);
        let current = working(&mut m, false);
        assert_eq!(m.poll(I::WorkDone(old)), A::Ignored);
        assert_eq!(m.poll(I::WorkFailed(old)), A::Ignored);
        assert!(m.is_current(current), "the stale input changed nothing");
        assert_eq!(m.poll(I::WorkDone(current)), A::Ready);
    }

    #[test]
    fn connected_carries_session_present_into_connect_work() {
        for session_present in [false, true] {
            let mut m = SessionMachine::new();
            m.poll(I::NetworkUp);
            match m.poll(I::Connected { session_present }) {
                A::ConnectWork {
                    session_present: carried,
                    ..
                } => assert_eq!(carried, session_present),
                other => panic!("expected ConnectWork, got {:?}", other),
            }
        }
    }

    /// The documented answer of a well-behaved tier to `action`, or `None`
    /// when the action asks for nothing and the loop waits for the outside.
    ///
    /// `ConnectWork` is answered the way a tier answers it: a
    /// `ConnectWorkMachine` over the plan, run to its report through a port
    /// whose every call succeeds.
    fn answer(action: SessionAction) -> Option<SessionInput> {
        use crate::mqtt::connect_work::scripted::ScriptedPort;
        use crate::mqtt::{perform, ConnectWorkMachine, WorkAction, WorkInput};

        match action {
            A::Connect => Some(I::Connected {
                session_present: false,
            }),
            A::ConnectWork {
                token,
                subscribe,
                session_present,
            } => {
                let plan = ConnectWorkPlan::new(subscribe, 2, true, true);
                let mut work = ConnectWorkMachine::new(token, plan, session_present);
                let mut port = ScriptedPort::accepting();
                let mut input = WorkInput::Start;
                loop {
                    let action = work.poll(input);
                    match perform(&mut port, action, session_present) {
                        Some(next) => input = next,
                        None => match action {
                            WorkAction::Report(report) => return Some(report),
                            other => panic!("connect work ended in {:?}", other),
                        },
                    }
                }
            }
            A::Close => Some(I::Disconnected),
            A::Wait { .. } => Some(I::RetryElapsed),
            A::None | A::Ready | A::Ignored => None,
        }
    }

    #[test]
    fn a_scripted_tier_loop_settles_within_a_bounded_number_of_polls() {
        // Prefixes that reach every phase a tier loop can sit in, each ending
        // with the input whose answer the loop then performs.
        const MAX_POLLS: usize = 8;
        let prefixes: &[&[SessionInput]] = &[
            &[I::NetworkUp],
            &[I::NetworkUp, I::Disconnected],
            &[I::NetworkUp, I::NetworkDown],
            &[I::NetworkUp, I::Disconnected, I::NetworkDown],
            &[I::NetworkUp, I::Disconnected, I::AttemptStarted],
            &[I::AttemptStarted],
            &[I::NetworkUp, I::NetworkDown, I::Disconnected, I::NetworkUp],
        ];
        for prefix in prefixes {
            let mut m = SessionMachine::new();
            let mut action = A::None;
            for input in prefix.iter() {
                action = m.poll(*input);
                assert_ne!(action, A::Ignored, "prefix {:?} at {:?}", prefix, input);
            }
            let mut polls = 0;
            // A driver attempt the machine accepted (`AttemptStarted` while
            // retrying) answers with the driver's own CONNACK.
            let driver_attempt = |m: &SessionMachine| {
                (m.state() == MqttConnectionState::Connecting).then_some(I::Connected {
                    session_present: false,
                })
            };
            while let Some(input) = answer(action).or_else(|| driver_attempt(&m)) {
                polls += 1;
                assert!(polls <= MAX_POLLS, "prefix {:?} did not settle", prefix);
                action = m.poll(input);
                assert_ne!(action, A::Ignored, "prefix {:?} after {:?}", prefix, input);
            }
            let settled = m.can_publish() || m.state() == MqttConnectionState::Disconnected;
            assert!(settled, "prefix {:?} ended in {:?}", prefix, m.state());
        }
    }

    #[test]
    fn connect_work_plan_order() {
        let steps: alloc::vec::Vec<ConnectStep> =
            ConnectWorkPlan::new(true, 3, true, true).collect();
        assert_eq!(
            steps,
            [
                ConnectStep::Subscribe(0),
                ConnectStep::Subscribe(1),
                ConnectStep::Subscribe(2),
                ConnectStep::PublishStartup,
                ConnectStep::RunOnConnect,
            ]
        );
    }

    #[test]
    fn kept_session_skips_subscriptions() {
        let steps: alloc::vec::Vec<ConnectStep> =
            ConnectWorkPlan::new(false, 3, true, false).collect();
        assert_eq!(steps, [ConnectStep::PublishStartup]);
    }

    #[test]
    fn empty_plan_when_nothing_to_do() {
        assert!(ConnectWorkPlan::new(false, 3, false, false).is_empty());
        assert!(ConnectWorkPlan::new(true, 0, false, false).is_empty());
        assert_eq!(ConnectWorkPlan::new(true, 0, false, false).next(), None);
        assert!(!ConnectWorkPlan::new(true, 1, false, false).is_empty());
        assert!(!ConnectWorkPlan::new(false, 0, false, true).is_empty());
    }
}
