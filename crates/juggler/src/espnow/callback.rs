//! Sans-IO marker for the frame whose send callback is awaited.
//!
//! `esp_now_send` returns when the frame is queued; the MAC-layer verdict arrives later in a
//! global send callback that carries only the destination MAC and no sequence number.
//! [`InFlightMarker`] is the state behind that callback on a worker-based driver: the worker
//! marks the frame it hands to the radio, the callback records the verdict, and the worker takes
//! it.
//! A callback that arrives while nothing is awaited, or for another MAC, is ignored.
//! At most one verdict is kept per marked frame (the first), so a repeated callback cannot
//! overwrite it.

use super::{MacAddress, TxId};

/// The frame in flight and the callback's verdict for it; see the [module
/// documentation](self).
#[derive(Debug, Default)]
pub struct InFlightMarker {
    awaiting: Option<(TxId, MacAddress)>,
    outcome: Option<bool>,
}

impl InFlightMarker {
    /// Nothing awaited.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Marks frame `id` to `mac` as handed to the radio and forgets any earlier verdict.
    pub fn begin(&mut self, id: TxId, mac: MacAddress) {
        self.awaiting = Some((id, mac));
        self.outcome = None;
    }

    /// The id of the frame awaited, if any.
    #[must_use]
    pub fn id(&self) -> Option<TxId> {
        self.awaiting.map(|(id, _)| id)
    }

    /// The send callback: records `acked` for the frame awaited when `mac` is its destination.
    ///
    /// Returns true when the verdict was recorded (the worker must be woken), false when the
    /// callback was ignored.
    pub fn on_callback(&mut self, mac: &[u8], acked: bool) -> bool {
        match self.awaiting {
            Some((_, expected)) if expected[..] == *mac && self.outcome.is_none() => {
                self.outcome = Some(acked);
                true
            }
            _ => false,
        }
    }

    /// Takes the verdict (`true` = MAC ACK) once the callback has arrived.
    pub fn take_outcome(&mut self) -> Option<bool> {
        self.outcome.take()
    }

    /// The frame is finished (or never started): awaits nothing.
    pub fn clear(&mut self) {
        self.awaiting = None;
        self.outcome = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::espnow::{SendKind, TxCore};
    use core::time::Duration;

    const MAC: MacAddress = [1, 2, 3, 4, 5, 6];
    const OTHER: MacAddress = [9, 9, 9, 9, 9, 9];

    fn id() -> TxId {
        TxCore::<u8, 2>::new()
            .submit(Duration::ZERO, MAC, b"x", None, SendKind::App)
            .unwrap()
    }

    #[test]
    fn a_callback_while_idle_is_ignored() {
        let mut marker = InFlightMarker::new();
        assert!(!marker.on_callback(&MAC, true));
        assert_eq!(marker.take_outcome(), None);
    }

    #[test]
    fn the_callback_of_the_marked_frame_is_recorded_once() {
        let mut marker = InFlightMarker::new();
        marker.begin(id(), MAC);
        assert_eq!(marker.id(), Some(id()));
        assert_eq!(marker.take_outcome(), None);
        assert!(marker.on_callback(&MAC, true));
        assert!(!marker.on_callback(&MAC, false));
        assert_eq!(marker.take_outcome(), Some(true));
        assert_eq!(marker.take_outcome(), None);
    }

    #[test]
    fn a_callback_for_another_mac_is_ignored() {
        let mut marker = InFlightMarker::new();
        marker.begin(id(), MAC);
        assert!(!marker.on_callback(&OTHER, true));
        assert!(!marker.on_callback(&MAC[..3], true));
        assert_eq!(marker.take_outcome(), None);
        assert!(marker.on_callback(&MAC, false));
        assert_eq!(marker.take_outcome(), Some(false));
    }

    #[test]
    fn begin_forgets_a_stale_verdict_and_clear_ends_the_wait() {
        let mut marker = InFlightMarker::new();
        marker.begin(id(), MAC);
        assert!(marker.on_callback(&MAC, true));
        marker.begin(id(), MAC);
        assert_eq!(marker.take_outcome(), None);
        marker.clear();
        assert_eq!(marker.id(), None);
        assert!(!marker.on_callback(&MAC, true));
    }
}
