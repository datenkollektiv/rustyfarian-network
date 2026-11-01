//! Suspend bookkeeping for an async supervisor (the esp-hal `WifiLink`).
//!
//! One task runs the radio; `suspend`/`resume` arrive from other tasks. The
//! adapter keeps a [`LinkHandover`] under one lock and only *wakes* the runner,
//! so a state transition and the action it hands over are published together:
//! however `suspend` and `resume` interleave, [`LinkHandover::take`] returns the
//! action that matches the latest call.

use super::link::{LinkAction, LinkInput, LinkMachine, LinkState};

/// A [`LinkMachine`] plus the action handed to the runner and the park epoch.
#[derive(Debug)]
pub struct LinkHandover {
    machine: LinkMachine,
    pending: Option<LinkAction>,
    /// Bumped by every effective suspend.
    suspends: u32,
    /// Value of `suspends` when the runner last finished taking the station down.
    parked: u32,
}

impl LinkHandover {
    /// A fresh handover around a machine with the shared defaults.
    pub const fn new() -> Self {
        Self {
            machine: LinkMachine::new(),
            pending: None,
            suspends: 0,
            parked: 0,
        }
    }

    /// Current state; [`LinkState::Suspended`] from the moment of [`suspend`](Self::suspend).
    pub fn state(&self) -> LinkState {
        self.machine.state()
    }

    /// Applies a suspend; `true` when the runner must be woken to [`take`](Self::take) it.
    pub fn suspend(&mut self) -> bool {
        let action = self.machine.poll(LinkInput::Suspend);
        if action == LinkAction::None {
            return false;
        }
        self.suspends = self.suspends.wrapping_add(1);
        self.pending = Some(action);
        true
    }

    /// Applies a resume; `true` when the runner must be woken to [`take`](Self::take) it.
    pub fn resume(&mut self) -> bool {
        let action = self.machine.poll(LinkInput::Resume);
        if action == LinkAction::None {
            return false;
        }
        self.pending = Some(action);
        true
    }

    /// The action handed over by the latest `suspend`/`resume`, or
    /// [`LinkAction::None`] if there is none or the runner's own inputs have
    /// since moved the state on.
    pub fn take(&mut self) -> LinkAction {
        match (self.pending.take(), self.machine.state()) {
            (Some(LinkAction::Disconnect), LinkState::Suspended) => LinkAction::Disconnect,
            (Some(LinkAction::Connect), LinkState::Connecting) => LinkAction::Connect,
            _ => LinkAction::None,
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
        assert_eq!(h.take(), LinkAction::None);
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
