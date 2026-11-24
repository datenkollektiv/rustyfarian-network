//! The wake-up path of the DDI worker and the MQTT nudge hook that uses it.
//!
//! Three things wake the worker out of its sleep: a nudge (the app's MQTT receive loop), the health policy (it persisted the success verdict) and a download that ended.
//! Each sets its part of a [`Wake`] inside a critical section and rings one `Signal`; neither ever waits, so any task may do it.

use core::cell::Cell;
use core::fmt;

use critical_section::Mutex;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::signal::Signal;
use juggler::ota::ddi::Wake;

/// Wakes the DDI worker out of its sleep: the kept wake-ups and the bell that ends the sleep.
pub(crate) struct Waker {
    kept: Mutex<Cell<Wake>>,
    bell: Signal<CriticalSectionRawMutex, ()>,
}

impl Waker {
    pub(crate) const fn new() -> Self {
        Self {
            kept: Mutex::new(Cell::new(Wake {
                nudge: false,
                health: None,
                download_ended: false,
            })),
            bell: Signal::new(),
        }
    }

    /// Adds `wake` to the kept wake-ups and rings the bell; never waits.
    pub(crate) fn notify(&self, wake: Wake) {
        critical_section::with(|cs| {
            let cell = self.kept.borrow(cs);
            let mut kept = cell.get();
            kept.merge(wake);
            cell.set(kept);
        });
        self.bell.signal(());
    }

    /// Takes the kept wake-ups, leaving none.
    pub(crate) fn take(&self) -> Wake {
        critical_section::with(|cs| self.kept.borrow(cs).take())
    }

    /// The bell rung by every [`notify`](Self::notify).
    #[cfg_attr(
        not(all(
            feature = "ota-ddi",
            any(
                feature = "esp32c3",
                feature = "esp32c6",
                feature = "esp32",
                feature = "esp32s3"
            )
        )),
        allow(dead_code, reason = "the chip build's worker sleeps on it")
    )]
    pub(crate) fn bell(&self) -> &Signal<CriticalSectionRawMutex, ()> {
        &self.bell
    }
}

/// Experimental: API may change before 1.0.
///
/// The MQTT nudge hook: hand every inbound topic to [`on_message`](Self::on_message) from the app's `MqttHandle::receive` loop.
///
/// The library does not subscribe itself: register [`topic`](Self::topic) with `MqttHandle::subscribe`, which the MQTT runner re-issues after every reconnect.
/// `on_message` only compares the topic, records the wake-up and rings the worker's bell; it never waits and never touches the MQTT client.
/// The payload carries no authority and is ignored: a nudge schedules one authenticated poll, never sooner than `NUDGE_MIN_INTERVAL` (10 s) after the last one and never while a poll is in flight (the rules live in `PollMachine::nudge`).
///
/// The handle is `Copy` and refers to state the runtime keeps for the whole program; take it from `DdiRuntime::nudge` or `DdiHandle::nudge`.
/// The ESP-IDF tier has the same type, fed from its `MqttBuilder::on_message` callback (this tier's MQTT client has no callbacks).
#[derive(Clone, Copy)]
pub struct DdiNudge {
    topic: &'static str,
    waker: &'static Waker,
}

const _: fn() = || {
    fn shared<T: Clone + Send + Sync + 'static>() {}
    shared::<DdiNudge>();
};

impl DdiNudge {
    pub(crate) const fn new(topic: &'static str, waker: &'static Waker) -> Self {
        Self { topic, waker }
    }

    /// Experimental: API may change before 1.0.
    ///
    /// The nudge topic `rustyfarian/ota/<controller-id>/nudge`, the one to pass to `MqttHandle::subscribe`.
    pub fn topic(&self) -> &str {
        self.topic
    }

    /// Experimental: API may change before 1.0.
    ///
    /// `true` when `topic` is the nudge topic (a poll is then requested), `false` for any other topic.
    ///
    /// Never waits and never touches the MQTT client.
    /// The worker applies the request in its own task, no sooner than `NUDGE_MIN_INTERVAL` (10 s) after the last poll started.
    pub fn on_message(&self, topic: &str) -> bool {
        if topic != self.topic {
            return false;
        }
        self.waker.notify(Wake {
            nudge: true,
            ..Wake::default()
        });
        true
    }
}

impl fmt::Debug for DdiNudge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DdiNudge")
            .field("topic", &self.topic)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::{DdiNudge, Waker};
    use juggler::ota::ddi::Wake;
    use juggler::ota::HealthVerdict;

    #[test]
    fn the_nudge_matches_its_topic_only() {
        static WAKER: Waker = Waker::new();
        let nudge = DdiNudge::new("rustyfarian/ota/dev-1/nudge", &WAKER);
        assert_eq!(nudge.topic(), "rustyfarian/ota/dev-1/nudge");
        assert!(nudge.on_message("rustyfarian/ota/dev-1/nudge"));
        assert!(WAKER.bell().signaled());
        assert!(WAKER.take().nudge);
        WAKER.bell().reset();
        assert!(!nudge.on_message("rustyfarian/ota/dev-2/nudge"));
        assert!(!nudge.on_message("rustyfarian/ota/dev-1/nudge/extra"));
        assert!(!nudge.on_message("rustyfarian/ota/dev-1"));
        assert!(!WAKER.bell().signaled());
        assert_eq!(WAKER.take(), Wake::default());
    }

    #[test]
    fn wake_ups_are_kept_and_merged_until_taken() {
        static WAKER: Waker = Waker::new();
        WAKER.notify(Wake {
            download_ended: true,
            ..Wake::default()
        });
        WAKER.notify(Wake {
            health: Some(HealthVerdict::Applied),
            ..Wake::default()
        });
        let taken = WAKER.take();
        assert!(taken.download_ended);
        assert_eq!(taken.health, Some(HealthVerdict::Applied));
        assert!(!taken.nudge);
        assert_eq!(WAKER.take(), Wake::default());
    }
}
