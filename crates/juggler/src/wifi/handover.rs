//! Suspend bookkeeping for a supervisor whose radio calls run apart from the
//! callers of `suspend`/`resume`.
//!
//! The esp-hal `WiFiManager` runs the radio on one task while `suspend`/`resume`
//! arrive from other tasks. The adapter keeps a [`LinkHandover`] under one lock
//! and only *wakes* the runner, so a state transition and the action it hands
//! over are published together: however `suspend` and `resume` interleave,
//! [`LinkHandover::take`] returns the action that matches the latest call.
//!
//! Both tiers also take the runner's entry ([`LinkHandover::start`]), its wait
//! between actions ([`LinkHandover::idle_wait`]) and the readiness edges they
//! report ([`LinkHandover::take_ready_change`]) from here, so neither adapter
//! decides them (ADR 023 decision 7).

use super::link::{LinkAction, LinkInput, LinkMachine, LinkState};

/// What a runner waits for after an action that needs no I/O.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdleWait {
    /// Associated: wait for the link to drop, a handover, or the next network check.
    Link,
    /// Suspended: wait for a handover, re-checking the driver for a late association.
    Park,
    /// No action is in flight in a state that needs one: restart supervision
    /// with [`LinkHandover::start`] unless a handover arrives first.
    Restart,
}

/// A [`LinkMachine`] plus the action handed to the runner, the park epoch and
/// the readiness last reported.
#[derive(Debug, Clone)]
pub struct LinkHandover {
    machine: LinkMachine,
    pending: Option<LinkAction>,
    /// Bumped by every effective suspend.
    suspends: u32,
    /// Value of `suspends` when the runner last finished taking the station down.
    parked: u32,
    /// Readiness as last returned by [`take_ready_change`](Self::take_ready_change).
    ready_reported: bool,
}

impl LinkHandover {
    /// A fresh handover around a machine with the shared defaults.
    pub const fn new() -> Self {
        Self::with_machine(LinkMachine::new())
    }

    /// A fresh handover around `machine`.
    pub(crate) const fn with_machine(machine: LinkMachine) -> Self {
        Self {
            machine,
            pending: None,
            suspends: 0,
            parked: 0,
            ready_reported: false,
        }
    }

    /// Current state; [`LinkState::Suspended`] from the moment of [`suspend`](Self::suspend).
    pub fn state(&self) -> LinkState {
        self.machine.state()
    }

    /// Applies a suspend; `true` when the runner must be woken to [`take`](Self::take) it.
    pub fn suspend(&mut self) -> bool {
        let action = self.machine.poll(LinkInput::Suspend);
        if matches!(action, LinkAction::None | LinkAction::Ignored) {
            return false;
        }
        self.suspends = self.suspends.wrapping_add(1);
        self.pending = Some(action);
        true
    }

    /// Applies a resume; `true` when the runner must be woken to [`take`](Self::take) it.
    pub fn resume(&mut self) -> bool {
        let action = self.machine.poll(LinkInput::Resume);
        if matches!(action, LinkAction::None | LinkAction::Ignored) {
            return false;
        }
        self.pending = Some(action);
        true
    }

    /// The action handed over by the latest `suspend`/`resume`;
    /// [`LinkAction::Ignored`] if the runner's own inputs have since moved the
    /// state on, and [`LinkAction::None`] if nothing was handed over.
    pub fn take(&mut self) -> LinkAction {
        match (self.pending.take(), self.machine.state()) {
            (Some(LinkAction::Disconnect), LinkState::Suspended) => LinkAction::Disconnect,
            (Some(LinkAction::Connect), LinkState::Connecting) => LinkAction::Connect,
            (Some(_), _) => LinkAction::Ignored,
            (None, _) => LinkAction::None,
        }
    }

    /// Feeds an input observed by the runner itself.
    pub fn poll(&mut self, input: LinkInput) -> LinkAction {
        self.machine.poll(input)
    }

    /// Restarts supervision with a fresh machine and nothing pending, keeping a
    /// suspend that is in force; returns `true` if it is.
    pub fn restart(&mut self) -> bool {
        let suspended = self.machine.state() == LinkState::Suspended;
        self.machine.restart();
        self.pending = None;
        if suspended {
            self.machine.poll(LinkInput::Suspend);
        }
        suspended
    }

    /// Entry of a runner: [`restart`](Self::restart)s, then returns the first action.
    ///
    /// [`LinkAction::Disconnect`] if a suspend is in force; otherwise, when the
    /// driver reports the station already `associated`, the association is
    /// adopted (and counted) without a connect; otherwise [`LinkAction::Connect`].
    pub fn start(&mut self, associated: bool) -> LinkAction {
        if self.restart() {
            LinkAction::Disconnect
        } else if associated {
            self.machine.poll(LinkInput::Connected)
        } else {
            self.machine.poll(LinkInput::Start)
        }
    }

    /// What the runner waits for after an action that needs no I/O
    /// ([`LinkAction::None`] or [`LinkAction::Ignored`]).
    pub fn idle_wait(&self) -> IdleWait {
        match self.machine.state() {
            LinkState::Connected => IdleWait::Link,
            LinkState::Suspended => IdleWait::Park,
            LinkState::Idle | LinkState::Connecting | LinkState::Backoff => IdleWait::Restart,
        }
    }

    /// The new readiness, once per transition since the last call; `None` while
    /// it is unchanged. Call it after every applied input, and on a suspend
    /// before the station goes down.
    pub fn take_ready_change(&mut self) -> Option<bool> {
        let ready = self.machine.is_ready();
        if ready == self.ready_reported {
            return None;
        }
        self.ready_reported = ready;
        Some(ready)
    }

    /// Epoch to pass to [`end_park`](Self::end_park) when the runner starts a disconnect.
    pub fn begin_park(&self) -> u32 {
        self.suspends
    }

    /// Records that the disconnect started at `epoch` has finished or given up.
    pub fn end_park(&mut self, epoch: u32) {
        self.parked = epoch;
    }

    /// `true` once the disconnect for the latest suspend has finished.
    pub fn is_suspended(&self) -> bool {
        self.machine.state() == LinkState::Suspended && self.parked == self.suspends
    }

    /// `true` while associated with the IP configuration reported (see [`LinkMachine::is_ready`]).
    pub fn is_ready(&self) -> bool {
        self.machine.is_ready()
    }

    /// Associations since boot (see [`LinkMachine::connect_count`]); survives [`restart`](Self::restart).
    pub fn connect_count(&self) -> u32 {
        self.machine.connect_count()
    }

    /// Consecutive failed attempts (see [`LinkMachine::failed_attempts`]).
    pub fn failed_attempts(&self) -> u32 {
        self.machine.failed_attempts()
    }
}

impl Default for LinkHandover {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restart_keeps_the_connect_count_and_drops_readiness() {
        let mut h = connected();
        h.poll(LinkInput::NetworkUp);
        assert!(h.is_ready());
        assert_eq!(h.connect_count(), 1);
        assert!(!h.restart());
        assert!(!h.is_ready());
        assert_eq!(h.connect_count(), 1);
    }

    fn connected() -> LinkHandover {
        let mut h = LinkHandover::new();
        h.poll(LinkInput::Start);
        h.poll(LinkInput::Connected);
        h
    }

    #[test]
    fn suspend_hands_over_a_disconnect() {
        let mut h = connected();
        assert!(h.suspend());
        assert_eq!(h.state(), LinkState::Suspended);
        assert_eq!(h.take(), LinkAction::Disconnect);
        assert_eq!(h.take(), LinkAction::None);
    }

    #[test]
    fn suspend_then_resume_before_the_runner_wakes_hands_over_the_connect() {
        let mut h = connected();
        assert!(h.suspend());
        assert!(h.resume());
        assert_eq!(h.state(), LinkState::Connecting);
        assert_eq!(h.take(), LinkAction::Connect);
    }

    #[test]
    fn resume_then_suspend_before_the_runner_wakes_hands_over_the_disconnect() {
        let mut h = connected();
        h.suspend();
        h.take();
        assert!(h.resume());
        assert!(h.suspend());
        assert_eq!(h.take(), LinkAction::Disconnect);
    }

    #[test]
    fn rapid_cycles_always_hand_over_the_latest_call() {
        let mut h = connected();
        for i in 0..50 {
            h.suspend();
            h.resume();
            if i % 3 == 0 {
                h.suspend();
                assert_eq!(h.take(), LinkAction::Disconnect);
            } else {
                assert_eq!(h.take(), LinkAction::Connect);
            }
        }
    }

    #[test]
    fn duplicates_neither_wake_nor_change_the_pending_action() {
        let mut h = connected();
        assert!(!h.resume());
        assert!(h.suspend());
        assert!(!h.suspend());
        assert_eq!(h.take(), LinkAction::Disconnect);
        assert!(h.resume());
        assert!(!h.resume());
        assert_eq!(h.take(), LinkAction::Connect);
    }

    #[test]
    fn a_handover_the_runner_has_overtaken_is_dropped() {
        let mut h = connected();
        h.suspend();
        h.take();
        h.resume();
        assert_eq!(
            h.poll(LinkInput::Disconnected),
            LinkAction::Wait { ms: 1_000 }
        );
        assert_eq!(h.take(), LinkAction::Ignored);
        assert_eq!(h.take(), LinkAction::None, "nothing is pending any more");
    }

    #[test]
    fn take_with_nothing_pending_is_none() {
        let mut h = connected();
        assert_eq!(h.take(), LinkAction::None);
        h.suspend();
        h.take();
        assert_eq!(h.take(), LinkAction::None);
    }

    #[test]
    fn start_while_suspended_disconnects() {
        let mut h = connected();
        h.suspend();
        assert_eq!(h.start(true), LinkAction::Disconnect);
        assert_eq!(h.state(), LinkState::Suspended);
        assert_eq!(h.take(), LinkAction::None, "the handover was dropped");
    }

    #[test]
    fn start_associated_adopts_without_connect() {
        let mut h = LinkHandover::new();
        assert_eq!(h.start(true), LinkAction::None);
        assert_eq!(h.state(), LinkState::Connected);
        assert_eq!(h.connect_count(), 1, "an adopted association counts");
    }

    #[test]
    fn start_fresh_connects() {
        let mut h = connected();
        assert_eq!(h.start(false), LinkAction::Connect);
        assert_eq!(h.state(), LinkState::Connecting);
        assert_eq!(h.connect_count(), 1);
    }

    #[test]
    fn idle_wait_matches_state() {
        let mut idle = LinkHandover::new();
        assert_eq!(idle.state(), LinkState::Idle);
        assert_eq!(idle.idle_wait(), IdleWait::Restart);
        idle.poll(LinkInput::Start);
        assert_eq!(idle.state(), LinkState::Connecting);
        assert_eq!(idle.idle_wait(), IdleWait::Restart);
        idle.poll(LinkInput::Disconnected);
        assert_eq!(idle.state(), LinkState::Backoff);
        assert_eq!(idle.idle_wait(), IdleWait::Restart);
        let mut h = connected();
        assert_eq!(h.idle_wait(), IdleWait::Link);
        h.suspend();
        assert_eq!(h.idle_wait(), IdleWait::Park);
    }

    #[test]
    fn take_ready_change_fires_once_per_transition() {
        let mut h = connected();
        assert_eq!(h.take_ready_change(), None, "associated without an IP");
        h.poll(LinkInput::NetworkUp);
        assert_eq!(h.take_ready_change(), Some(true));
        h.poll(LinkInput::NetworkUp);
        assert_eq!(h.take_ready_change(), None, "ready to ready is no edge");
        h.suspend();
        assert_eq!(h.take_ready_change(), Some(false));
        assert_eq!(h.take_ready_change(), None);
        h.resume();
        assert_eq!(h.take_ready_change(), None);
    }

    #[test]
    fn is_suspended_waits_for_the_park_of_the_latest_suspend() {
        let mut h = connected();
        assert!(!h.is_suspended());
        h.suspend();
        let first = h.begin_park();
        assert!(!h.is_suspended());
        h.resume();
        h.suspend();
        h.end_park(first);
        assert!(!h.is_suspended());
        let second = h.begin_park();
        h.end_park(second);
        assert!(h.is_suspended());
        h.resume();
        assert!(!h.is_suspended());
    }

    #[test]
    fn restart_keeps_a_suspend_and_drops_stale_handovers() {
        let mut h = connected();
        h.suspend();
        assert!(h.restart());
        assert_eq!(h.state(), LinkState::Suspended);
        assert_eq!(h.take(), LinkAction::None);
        h.resume();
        assert!(!h.restart());
        assert_eq!(h.state(), LinkState::Idle);
        assert_eq!(h.take(), LinkAction::None);
    }
}
