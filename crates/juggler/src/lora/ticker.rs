//! [`LorawanTicker`] — the rung-2 event machine that drives `lorawan-device`'s
//! `nb_device::Device` (ADR 023).
//!
//! A tier loop samples the IRQ line and the clock, asks [`LorawanTicker::update`]
//! which event to deliver, hands it to `Device::handle_event`, and feeds the
//! response back through [`LorawanTicker::on_response`]:
//!
//! ```rust,ignore
//! let mut ticker = LorawanTicker::new(60_000, 0);
//! let mut outcome = ticker.on_response(&device.join(mode)?);
//! while outcome == Outcome::Continue {
//!     let now_ms = start.elapsed().as_millis() as u32;
//!     let irq = device.get_radio().irq_pending();
//!     let event = match ticker.update(irq, now_ms) {
//!         Some(TickEvent::Phy) => Event::RadioEvent(radio::Event::Phy(())),
//!         Some(TickEvent::TimeoutFired) => Event::TimeoutFired,
//!         Some(TickEvent::BudgetExceeded) => break,
//!         None => { sleep(10 ms); continue }
//!     };
//!     outcome = ticker.on_response(&device.handle_event(event)?);
//!     sleep(10 ms);
//! }
//! ```
//!
//! It encodes the two `lorawan-device` 0.12 rules that the hand-written loops
//! each had to remember:
//!
//! - `Response::TimeoutRequest(ms)` is an ABSOLUTE timestamp in the timeline that
//!   starts at `start_ms`, not a relative delay — assign it, never add elapsed time.
//! - `Response::DownlinkReceived` is terminal for a send cycle: RX2 is cancelled
//!   and no further events fire.
//!
//! Time is an input (ADR 023 decision 4); every comparison is wrap-safe.

use lorawan_device::nb_device::Response;

/// The event the tier loop delivers to `lorawan-device` next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TickEvent {
    /// A radio IRQ is pending: deliver `Event::RadioEvent(Phy(()))`.
    Phy,
    /// The requested deadline has passed: deliver `Event::TimeoutFired`.
    TimeoutFired,
    /// The time budget is spent: stop driving the device (terminal).
    BudgetExceeded,
}

/// What a `lorawan-device` response means for the loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Keep ticking.
    Continue,
    /// The OTAA join was accepted (terminal).
    Joined,
    /// No join-accept arrived (terminal).
    JoinFailed,
    /// The uplink cycle finished without a downlink (terminal).
    UplinkDone,
    /// A downlink arrived; the send cycle is over (terminal).
    Downlink,
    /// The session expired; a re-join is required (terminal).
    SessionExpired,
}

impl Outcome {
    /// Return `true` for every outcome other than [`Outcome::Continue`].
    pub fn is_terminal(self) -> bool {
        self != Self::Continue
    }
}

/// Decodes IRQ and clock samples into `lorawan-device` events, with a time budget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LorawanTicker {
    start_ms: u32,
    budget_ms: u32,
    /// Absolute deadline in the timeline that starts at `start_ms`.
    deadline_ms: Option<u32>,
    budget_spent: bool,
}

impl LorawanTicker {
    /// Create a ticker whose timeline and budget start at `start_ms`.
    ///
    /// `start_ms` is the caller's clock reading when the join or send was issued;
    /// `lorawan-device` deadlines are measured from it.
    pub fn new(budget_ms: u32, start_ms: u32) -> Self {
        Self {
            start_ms,
            budget_ms,
            deadline_ms: None,
            budget_spent: false,
        }
    }

    /// Decide which event to deliver for this sample, if any.
    ///
    /// The budget is checked first so every path reaches a terminal event, then a
    /// pending IRQ, then the deadline. `TimeoutFired` is returned once per deadline;
    /// `BudgetExceeded` is latched and returned on every later call.
    pub fn update(&mut self, irq_pending: bool, now_ms: u32) -> Option<TickEvent> {
        let elapsed = now_ms.wrapping_sub(self.start_ms);
        if self.budget_spent || elapsed >= self.budget_ms {
            self.budget_spent = true;
            return Some(TickEvent::BudgetExceeded);
        }
        if irq_pending {
            return Some(TickEvent::Phy);
        }
        match self.deadline_ms {
            Some(deadline) if elapsed >= deadline => {
                self.deadline_ms = None;
                Some(TickEvent::TimeoutFired)
            }
            _ => None,
        }
    }

    /// Record a `lorawan-device` response and classify it.
    pub fn on_response(&mut self, response: &Response) -> Outcome {
        match response {
            Response::TimeoutRequest(ms) => {
                // Absolute timestamp in the device timeline — assign, never add.
                self.deadline_ms = Some(*ms);
                Outcome::Continue
            }
            Response::JoinSuccess => Outcome::Joined,
            Response::NoJoinAccept => Outcome::JoinFailed,
            Response::DownlinkReceived(_) => Outcome::Downlink,
            Response::RxComplete | Response::ReadyToSend | Response::NoAck => Outcome::UplinkDone,
            Response::SessionExpired => Outcome::SessionExpired,
            Response::NoUpdate | Response::JoinRequestSending | Response::UplinkSending(_) => {
                Outcome::Continue
            }
        }
    }

    /// The pending deadline, in the timeline that starts at `start_ms`.
    pub fn deadline_ms(&self) -> Option<u32> {
        self.deadline_ms
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_request_is_absolute_not_relative() {
        let mut t = LorawanTicker::new(60_000, 0);
        // The request arrives at t=400 for an absolute deadline of 1000.
        assert_eq!(t.update(false, 400), None);
        assert_eq!(
            t.on_response(&Response::TimeoutRequest(1000)),
            Outcome::Continue
        );
        assert_eq!(t.deadline_ms(), Some(1000));
        assert_eq!(t.update(false, 999), None);
        // Fires at 1000, not at 400 + 1000.
        assert_eq!(t.update(false, 1000), Some(TickEvent::TimeoutFired));
    }

    #[test]
    fn deadline_is_measured_from_start() {
        let mut t = LorawanTicker::new(60_000, 5_000);
        t.on_response(&Response::TimeoutRequest(1000));
        assert_eq!(t.update(false, 5_999), None);
        assert_eq!(t.update(false, 6_000), Some(TickEvent::TimeoutFired));
    }

    #[test]
    fn downlink_received_is_terminal() {
        let mut t = LorawanTicker::new(30_000, 0);
        let o = t.on_response(&Response::DownlinkReceived(3));
        assert_eq!(o, Outcome::Downlink);
        assert!(o.is_terminal());
    }

    #[test]
    fn no_join_accept_is_terminal() {
        let mut t = LorawanTicker::new(60_000, 0);
        assert_eq!(t.on_response(&Response::NoJoinAccept), Outcome::JoinFailed);
        assert_eq!(t.on_response(&Response::JoinSuccess), Outcome::Joined);
    }

    #[test]
    fn uplink_terminals_and_progress_responses() {
        let mut t = LorawanTicker::new(30_000, 0);
        assert_eq!(t.on_response(&Response::RxComplete), Outcome::UplinkDone);
        assert_eq!(t.on_response(&Response::ReadyToSend), Outcome::UplinkDone);
        assert_eq!(t.on_response(&Response::NoAck), Outcome::UplinkDone);
        assert_eq!(
            t.on_response(&Response::SessionExpired),
            Outcome::SessionExpired
        );
        assert_eq!(
            t.on_response(&Response::UplinkSending(1)),
            Outcome::Continue
        );
        assert_eq!(
            t.on_response(&Response::JoinRequestSending),
            Outcome::Continue
        );
        assert_eq!(t.on_response(&Response::NoUpdate), Outcome::Continue);
        assert!(!Outcome::Continue.is_terminal());
    }

    #[test]
    fn irq_pending_yields_phy_before_timeout() {
        let mut t = LorawanTicker::new(60_000, 0);
        t.on_response(&Response::TimeoutRequest(100));
        assert_eq!(t.update(true, 200), Some(TickEvent::Phy));
        // The deadline is still pending and fires on the next IRQ-free sample.
        assert_eq!(t.update(false, 210), Some(TickEvent::TimeoutFired));
    }

    #[test]
    fn timeout_fires_once() {
        let mut t = LorawanTicker::new(60_000, 0);
        t.on_response(&Response::TimeoutRequest(100));
        assert_eq!(t.update(false, 150), Some(TickEvent::TimeoutFired));
        assert_eq!(t.update(false, 160), None);
        assert_eq!(t.update(false, 10_000), None);
    }

    #[test]
    fn no_deadline_means_no_timeout() {
        let mut t = LorawanTicker::new(60_000, 0);
        assert_eq!(t.update(false, 0), None);
        assert_eq!(t.update(false, 59_999), None);
    }

    #[test]
    fn budget_exceeded_is_terminal_and_reached_on_every_path() {
        // Idle path.
        let mut t = LorawanTicker::new(1_000, 0);
        assert_eq!(t.update(false, 999), None);
        assert_eq!(t.update(false, 1_000), Some(TickEvent::BudgetExceeded));
        // Latched: later samples keep reporting it, even with an IRQ.
        assert_eq!(t.update(true, 1_001), Some(TickEvent::BudgetExceeded));

        // A stuck IRQ cannot starve the budget.
        let mut t = LorawanTicker::new(1_000, 0);
        assert_eq!(t.update(true, 500), Some(TickEvent::Phy));
        assert_eq!(t.update(true, 1_000), Some(TickEvent::BudgetExceeded));

        // A deadline due at the same sample loses to the budget.
        let mut t = LorawanTicker::new(1_000, 0);
        t.on_response(&Response::TimeoutRequest(1_000));
        assert_eq!(t.update(false, 1_000), Some(TickEvent::BudgetExceeded));
    }

    #[test]
    fn time_wraparound_safe() {
        let start = u32::MAX - 100;
        let mut t = LorawanTicker::new(1_000, start);
        t.on_response(&Response::TimeoutRequest(200));
        // now has wrapped past zero: elapsed is 150, deadline 200.
        assert_eq!(t.update(false, start.wrapping_add(150)), None);
        assert_eq!(
            t.update(false, start.wrapping_add(200)),
            Some(TickEvent::TimeoutFired)
        );
        assert_eq!(t.update(false, start.wrapping_add(999)), None);
        assert_eq!(
            t.update(false, start.wrapping_add(1_000)),
            Some(TickEvent::BudgetExceeded)
        );
    }
}
