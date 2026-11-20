//! The wake-up path of the `ota-worker` thread and the MQTT nudge hook that uses it.
//!
//! Three things wake the worker out of its sleep: a nudge (the app's `on_message` closure), the health policy (it persisted the success verdict) and an `ota-download` thread that ended.
//! Each is a flag under one mutex plus a condition variable: setting a flag never blocks, so any thread may do it, the esp-mqtt event thread included.

use std::fmt;
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Instant;

/// What woke the worker.
#[derive(Debug, Default, Clone, Copy)]
pub(super) struct Wake {
    /// The nudge topic was received.
    pub(super) nudge: bool,
    /// The health policy reached its verdict.
    pub(super) health: bool,
    /// A download thread ended, or may have.
    pub(super) download: bool,
}

impl Wake {
    fn any(self) -> bool {
        self.nudge || self.health || self.download
    }
}

/// Wakes the `ota-worker` thread out of its sleep.
///
/// A flag under a mutex and a condition variable: setting a flag never blocks, so any thread may do it.
#[derive(Debug)]
pub(super) struct Waker {
    state: Mutex<Wake>,
    cv: Condvar,
}

impl Waker {
    pub(super) fn new() -> Self {
        Self {
            state: Mutex::new(Wake::default()),
            cv: Condvar::new(),
        }
    }

    fn set(&self, f: impl FnOnce(&mut Wake)) {
        f(&mut self.state.lock().unwrap_or_else(PoisonError::into_inner));
        self.cv.notify_one();
    }

    /// The nudge topic was received: the poll is wanted sooner.
    pub(super) fn notify_nudge(&self) {
        self.set(|wake| wake.nudge = true);
    }

    /// The health policy reached its verdict; the success verdict, if any, is already persisted.
    pub(super) fn notify_health(&self) {
        self.set(|wake| wake.health = true);
    }

    /// A download thread is about to exit.
    pub(super) fn notify_download(&self) {
        self.set(|wake| wake.download = true);
    }

    /// Waits until a flag is set or `deadline` passes (forever for `None`), then returns and clears the flags.
    ///
    /// A spurious wake-up returns early with no flag set; the caller loops.
    pub(super) fn wait_until(&self, deadline: Option<Instant>) -> Wake {
        let mut guard = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if !guard.any() {
            guard = match deadline {
                Some(deadline) => {
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        guard
                    } else {
                        self.cv
                            .wait_timeout(guard, left)
                            .unwrap_or_else(PoisonError::into_inner)
                            .0
                    }
                }
                None => self.cv.wait(guard).unwrap_or_else(PoisonError::into_inner),
            };
        }
        std::mem::take(&mut *guard)
    }
}

/// Experimental: API may change before 1.0.
///
/// The MQTT nudge hook: call [`on_message`](Self::on_message) from the app's `MqttBuilder::on_message` closure.
///
/// The library does not subscribe itself: register [`topic`](Self::topic) with `MqttBuilder::subscribe`, which re-sends it on every connect.
/// `on_message` only compares the topic, sets a flag and wakes the `ota-worker` thread; it never touches the MQTT client, so it is safe inside the esp-mqtt callback (ADR 017).
/// The payload carries no authority and is ignored: a nudge schedules one authenticated poll, never sooner than `NUDGE_MIN_INTERVAL` (10 s) after the last one and never while a poll is in flight (the rules live in `PollMachine::nudge`).
///
/// The handle is cheap to clone; take it from `DdiRuntime::nudge` or `DdiHandle::nudge`.
/// The `mqtt` feature is not needed to use it.
#[derive(Clone)]
pub struct DdiNudge {
    topic: Arc<str>,
    waker: Arc<Waker>,
}

const _: fn() = || {
    fn shared<T: Clone + Send + Sync + 'static>() {}
    shared::<DdiNudge>();
};

impl DdiNudge {
    pub(super) fn new(topic: String, waker: Arc<Waker>) -> Self {
        Self {
            topic: Arc::from(topic),
            waker,
        }
    }

    /// Experimental: API may change before 1.0.
    ///
    /// The nudge topic `rustyfarian/ota/<controller-id>/nudge`, the one to pass to `MqttBuilder::subscribe`.
    pub fn topic(&self) -> &str {
        &self.topic
    }

    /// Experimental: API may change before 1.0.
    ///
    /// `true` when `topic` is the nudge topic (a poll is then requested), `false` for any other topic.
    ///
    /// Never blocks and never touches the MQTT client.
    /// The worker applies the request on its own thread, no sooner than `NUDGE_MIN_INTERVAL` (10 s) after the last poll started.
    pub fn on_message(&self, topic: &str) -> bool {
        if topic != &*self.topic {
            return false;
        }
        self.waker.notify_nudge();
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
    use std::sync::Arc;

    use super::{DdiNudge, Waker};

    #[test]
    fn the_nudge_matches_its_topic_only() {
        let waker = Arc::new(Waker::new());
        let nudge = DdiNudge::new("rustyfarian/ota/dev-1/nudge".to_owned(), Arc::clone(&waker));
        assert!(nudge.on_message("rustyfarian/ota/dev-1/nudge"));
        assert!(waker.wait_until(None).nudge);
        assert!(!nudge.on_message("rustyfarian/ota/dev-2/nudge"));
        assert!(!nudge.on_message("rustyfarian/ota/dev-1/nudge/extra"));
        assert!(!waker.wait_until(Some(std::time::Instant::now())).nudge);
    }
}

#[cfg(all(test, feature = "mqtt"))]
mod mqtt_wiring {
    use super::DdiNudge;
    use crate::mqtt::{MqttBuilder, QoS};

    /// Type-checks the app-side chain (`just clippy-ota-ddi-mqtt-idf`); IDF tests never run on the host.
    #[allow(dead_code)]
    fn wires_into_the_mqtt_builder(nudge: DdiNudge, builder: MqttBuilder<'_>) -> MqttBuilder<'_> {
        let topic = nudge.topic().to_owned();
        builder
            .subscribe(topic, QoS::AtLeastOnce)
            .on_message(move |topic, _payload| {
                let _ = nudge.on_message(topic);
            })
    }
}
