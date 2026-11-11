//! Subscription lists shared by the handle and the runner (ADR 022, ADR 023).
//!
//! The builder's topics are fixed; [`MqttHandle::subscribe`](super::MqttHandle::subscribe)
//! appends runtime ones.
//! Which request is added, changed or a no-op is decided by
//! `juggler::mqtt::admit_subscription`; this file only stores the lists and
//! stores the topics; which entries the live session still owes is tracked by
//! `juggler::mqtt::SubscriptionTracker`.

use std::sync::{Mutex, MutexGuard};

use juggler::mqtt::{
    admit_subscription, QoS, SubscriptionAdmission, SubscriptionTracker, TrackedRequest,
};

/// Most runtime subscriptions one session keeps, like the esp-hal tier.
pub const MAX_RUNTIME_SUBSCRIPTIONS: usize = 8;

#[derive(Debug, Default)]
struct Inner {
    /// Runtime topics; the tracker holds their QoS and pending flags at the same index.
    topics: Vec<String>,
    tracker: SubscriptionTracker<MAX_RUNTIME_SUBSCRIPTIONS>,
}

/// What a runtime request needs after admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Admitted {
    /// Nothing to send: already subscribed, or stored for the next connect.
    Stored,
    /// Send it to the live session now.
    SendNow,
}

/// The builder's subscriptions plus the runtime ones.
///
/// Indices count the builder entries first, then the runtime entries in
/// registration order, matching `ConnectWorkPlan`.
#[derive(Debug)]
pub(crate) struct Subscriptions {
    fixed: Vec<(String, QoS)>,
    inner: Mutex<Inner>,
}

impl Subscriptions {
    pub(super) fn new(fixed: Vec<(String, QoS)>) -> Self {
        Self {
            fixed,
            inner: Mutex::new(Inner::default()),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Number of entries a connect plan walks.
    pub(super) fn len(&self) -> usize {
        self.fixed.len() + self.lock().topics.len()
    }

    /// Copy of entry `index`.
    pub(super) fn get(&self, index: usize) -> Option<(String, QoS)> {
        match self.fixed.get(index) {
            Some(entry) => Some(entry.clone()),
            None => {
                let i = index - self.fixed.len();
                let inner = self.lock();
                Some((inner.topics.get(i)?.clone(), inner.tracker.qos(i)?))
            }
        }
    }

    /// Marks the start of a connect work: the session is not live yet, and a
    /// session that needs every subscription again is owed every runtime entry.
    pub(super) fn begin_connect(&self, resubscribe_all: bool) {
        self.lock().tracker.begin_connect(resubscribe_all);
    }

    /// The session ended or the link dropped: requests wait for the next connect.
    pub(super) fn go_offline(&self) {
        self.lock().tracker.go_offline();
    }

    /// Records that `qos` was sent for entry `index`; a QoS changed since stays pending.
    pub(super) fn mark_sent(&self, index: usize, qos: QoS) {
        if let Some(i) = index.checked_sub(self.fixed.len()) {
            self.lock().tracker.mark_sent(i, qos);
        }
    }

    /// Admits a validated runtime request (`juggler::mqtt::admit_subscription`).
    pub(super) fn admit(&self, topic: &str, qos: QoS) -> anyhow::Result<Admitted> {
        let mut inner = self.lock();
        let admission = admit_subscription(
            self.fixed.iter().map(|(t, q)| (t.as_str(), *q)),
            inner
                .topics
                .iter()
                .enumerate()
                .map(|(i, t)| (t.as_str(), inner.tracker.qos(i).unwrap_or(qos))),
            topic,
            qos,
            MAX_RUNTIME_SUBSCRIPTIONS,
        );
        let added = admission == SubscriptionAdmission::Added;
        match inner.tracker.apply(admission, qos) {
            TrackedRequest::Unchanged => Ok(Admitted::Stored),
            TrackedRequest::Full => {
                anyhow::bail!("more than {MAX_RUNTIME_SUBSCRIPTIONS} runtime subscriptions")
            }
            request => {
                if added {
                    inner.topics.push(topic.to_owned());
                }
                Ok(if request == TrackedRequest::SendNow {
                    Admitted::SendNow
                } else {
                    Admitted::Stored
                })
            }
        }
    }

    /// Sends the newest QoS still owed for the runtime entry `topic`, then marks it sent.
    ///
    /// Called with the client lock held, so concurrent callers transmit one at a
    /// time and each sends what the tracker holds at that moment: a caller whose
    /// request a newer one superseded finds nothing owed, and the newest QoS
    /// always goes out last.
    /// Lock order is client lock, then this store's lock, like the runner's connect steps.
    /// Returns `false` when `send` failed; the entry stays owed.
    pub(super) fn send_owed_topic(&self, topic: &str, send: impl FnOnce(QoS) -> bool) -> bool {
        let mut inner = self.lock();
        match inner.topics.iter().position(|t| t == topic) {
            Some(i) => inner.tracker.send_owed(i, send),
            None => true,
        }
    }

    /// Sends every runtime entry the session still lacks, then goes live.
    ///
    /// Runs on the runner thread, under the client lock, as the last connect step.
    /// Requests admitted before this call are sent here; later ones see `live`
    /// and go out themselves.
    /// Returns `false` (not live) when a send fails; that entry stays pending.
    pub(super) fn catch_up_and_go_live(&self, mut send: impl FnMut(&str, QoS) -> bool) -> bool {
        let mut inner = self.lock();
        let Inner { topics, tracker } = &mut *inner;
        tracker.catch_up_and_go_live(|index, qos| send(&topics[index], qos))
    }
}
