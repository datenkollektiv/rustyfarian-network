//! The pure rules of the `lorawan-device` event loop (ADR 023 decision 3).
//!
//! A tier loop samples the IRQ line and the clock, asks [`LorawanTimeline::next_event`]
//! which event to deliver to `Device::handle_event`, and classifies each response with
//! [`classify_response`].
//! The loop is a single blocking tick loop, so it keeps the one piece of state itself: the
//! pending deadline, which [`requested_deadline`] reads out of a response.
//!
//! ```rust,ignore
//! let timeline = LorawanTimeline::new(60_000, 0);
//! let mut deadline = requested_deadline(&first_response);
//! let mut outcome = classify_response(&first_response);
//! while outcome == Outcome::Continue {
//!     let now_ms = start.elapsed().as_millis() as u32;
//!     let irq = device.get_radio().irq_pending();
//!     let event = match timeline.next_event(irq, now_ms, deadline) {
//!         Some(TickEvent::Phy) => Event::RadioEvent(radio::Event::Phy(())),
//!         Some(TickEvent::TimeoutFired) => {
//!             deadline = None;
//!             Event::TimeoutFired
//!         }
//!         Some(TickEvent::BudgetExceeded) => break,
//!         None => { sleep(10 ms); continue }
//!     };
//!     let response = device.handle_event(event)?;
//!     deadline = requested_deadline(&response).or(deadline);
//!     outcome = classify_response(&response);
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

/// The timeline and time budget of one join or send cycle.
///
/// Immutable: the pending deadline lives in the caller's loop, see the module docs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LorawanTimeline {
    start_ms: u32,
    budget_ms: u32,
}

impl LorawanTimeline {
    /// Create a timeline whose deadlines and budget are measured from `start_ms`.
    ///
    /// `start_ms` is the caller's clock reading when the join or send was issued;
    /// `lorawan-device` deadlines are measured from it.
    pub const fn new(budget_ms: u32, start_ms: u32) -> Self {
        Self {
            start_ms,
            budget_ms,
        }
    }

    /// Decide which event to deliver for this sample, if any.
    ///
    /// The budget is checked first so every path reaches a terminal event, then a
    /// pending IRQ, then `deadline_ms` (absolute in this timeline, from
    /// [`requested_deadline`]).
    /// The caller clears its deadline when it delivers [`TickEvent::TimeoutFired`] and stops
    /// at [`TickEvent::BudgetExceeded`], which every later sample repeats.
    pub fn next_event(
        &self,
        irq_pending: bool,
        now_ms: u32,
        deadline_ms: Option<u32>,
    ) -> Option<TickEvent> {
        let elapsed = now_ms.wrapping_sub(self.start_ms);
        if elapsed >= self.budget_ms {
            return Some(TickEvent::BudgetExceeded);
        }
        if irq_pending {
            return Some(TickEvent::Phy);
        }
        match deadline_ms {
            Some(deadline) if elapsed >= deadline => Some(TickEvent::TimeoutFired),
            _ => None,
        }
    }
}

/// The absolute deadline a response asks for, if it is a `TimeoutRequest`.
///
/// Assign it to the pending deadline, never add elapsed time (it is an absolute
/// timestamp in the timeline); a response without one leaves the deadline as it was.
pub fn requested_deadline(response: &Response) -> Option<u32> {
    match response {
        Response::TimeoutRequest(ms) => Some(*ms),
        _ => None,
    }
}

/// What a `lorawan-device` response means for the loop.
pub fn classify_response(response: &Response) -> Outcome {
    match response {
        Response::JoinSuccess => Outcome::Joined,
        Response::NoJoinAccept => Outcome::JoinFailed,
        Response::DownlinkReceived(_) => Outcome::Downlink,
        Response::RxComplete | Response::ReadyToSend | Response::NoAck => Outcome::UplinkDone,
        Response::SessionExpired => Outcome::SessionExpired,
        Response::TimeoutRequest(_)
        | Response::NoUpdate
        | Response::JoinRequestSending
        | Response::UplinkSending(_) => Outcome::Continue,
    }
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_request_is_absolute_not_relative() {
        let t = LorawanTimeline::new(60_000, 0);
        // The request arrives at t=400 for an absolute deadline of 1000.
        assert_eq!(t.next_event(false, 400, None), None);
        let response = Response::TimeoutRequest(1000);
        assert_eq!(classify_response(&response), Outcome::Continue);
        let deadline = requested_deadline(&response);
        assert_eq!(deadline, Some(1000));
        assert_eq!(t.next_event(false, 999, deadline), None);
        // Fires at 1000, not at 400 + 1000.
        assert_eq!(
            t.next_event(false, 1000, deadline),
            Some(TickEvent::TimeoutFired)
        );
    }

    #[test]
    fn only_a_timeout_request_carries_a_deadline() {
        assert_eq!(requested_deadline(&Response::NoUpdate), None);
        assert_eq!(requested_deadline(&Response::RxComplete), None);
        assert_eq!(requested_deadline(&Response::DownlinkReceived(1)), None);
    }

    #[test]
    fn deadline_is_measured_from_start() {
        let t = LorawanTimeline::new(60_000, 5_000);
        assert_eq!(t.next_event(false, 5_999, Some(1000)), None);
        assert_eq!(
            t.next_event(false, 6_000, Some(1000)),
            Some(TickEvent::TimeoutFired)
        );
    }

    #[test]
    fn downlink_received_is_terminal() {
        let o = classify_response(&Response::DownlinkReceived(3));
        assert_eq!(o, Outcome::Downlink);
        assert!(o.is_terminal());
    }

    #[test]
    fn no_join_accept_is_terminal() {
        assert_eq!(
            classify_response(&Response::NoJoinAccept),
            Outcome::JoinFailed
        );
        assert_eq!(classify_response(&Response::JoinSuccess), Outcome::Joined);
    }

    #[test]
    fn uplink_terminals_and_progress_responses() {
        assert_eq!(
            classify_response(&Response::RxComplete),
            Outcome::UplinkDone
        );
        assert_eq!(
            classify_response(&Response::ReadyToSend),
            Outcome::UplinkDone
        );
        assert_eq!(classify_response(&Response::NoAck), Outcome::UplinkDone);
        assert_eq!(
            classify_response(&Response::SessionExpired),
            Outcome::SessionExpired
        );
        assert_eq!(
            classify_response(&Response::UplinkSending(1)),
            Outcome::Continue
        );
        assert_eq!(
            classify_response(&Response::JoinRequestSending),
            Outcome::Continue
        );
        assert_eq!(classify_response(&Response::NoUpdate), Outcome::Continue);
        assert!(!Outcome::Continue.is_terminal());
    }

    #[test]
    fn irq_pending_yields_phy_before_timeout() {
        let t = LorawanTimeline::new(60_000, 0);
        assert_eq!(t.next_event(true, 200, Some(100)), Some(TickEvent::Phy));
        // The deadline is still pending and fires on the next IRQ-free sample.
        assert_eq!(
            t.next_event(false, 210, Some(100)),
            Some(TickEvent::TimeoutFired)
        );
    }

    #[test]
    fn no_deadline_means_no_timeout() {
        let t = LorawanTimeline::new(60_000, 0);
        assert_eq!(t.next_event(false, 0, None), None);
        assert_eq!(t.next_event(false, 59_999, None), None);
    }

    #[test]
    fn budget_exceeded_is_terminal_and_reached_on_every_path() {
        // Idle path, repeated by every later sample, even with an IRQ.
        let t = LorawanTimeline::new(1_000, 0);
        assert_eq!(t.next_event(false, 999, None), None);
        assert_eq!(
            t.next_event(false, 1_000, None),
            Some(TickEvent::BudgetExceeded)
        );
        assert_eq!(
            t.next_event(true, 1_001, None),
            Some(TickEvent::BudgetExceeded)
        );

        // A stuck IRQ cannot starve the budget.
        assert_eq!(t.next_event(true, 500, None), Some(TickEvent::Phy));
        assert_eq!(
            t.next_event(true, 1_000, None),
            Some(TickEvent::BudgetExceeded)
        );

        // A deadline due at the same sample loses to the budget.
        assert_eq!(
            t.next_event(false, 1_000, Some(1_000)),
            Some(TickEvent::BudgetExceeded)
        );
    }

    #[test]
    fn time_wraparound_safe() {
        let start = u32::MAX - 100;
        let t = LorawanTimeline::new(1_000, start);
        // now has wrapped past zero: elapsed is 150, deadline 200.
        assert_eq!(
            t.next_event(false, start.wrapping_add(150), Some(200)),
            None
        );
        assert_eq!(
            t.next_event(false, start.wrapping_add(200), Some(200)),
            Some(TickEvent::TimeoutFired)
        );
        assert_eq!(t.next_event(false, start.wrapping_add(999), None), None);
        assert_eq!(
            t.next_event(false, start.wrapping_add(1_000), None),
            Some(TickEvent::BudgetExceeded)
        );
    }
}
