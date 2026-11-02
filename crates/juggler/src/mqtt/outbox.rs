//! Outbound publishes for a client that may not exist yet (ADR 022 decision 10).
//!
//! The ESP-IDF tier creates its `esp-mqtt` client only once the station has an
//! IP (bug 010), and the esp-hal tier accepts publishes before the first
//! connect; both must therefore hold a publish until a client can take it.
//! [`Outbox`] is that holding place, shared so both tiers behave alike: a
//! publish with a client attached goes straight out, one without is queued in
//! [`Outbox::new`]'s depth slots. Two policies share that queue:
//! [`Outbox::publish_waiting`] waits for room and never evicts — the
//! [`MqttClient::publish`](super::MqttClient::publish) contract both tiers
//! implement — while the legacy synchronous [`Outbox::publish`] drops (and
//! counts) the oldest entry to make room. Order is kept: a message queued
//! before the client existed is handed over before anything published after it.
//!
//! Requires the `std` feature.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError, TryLockError};

use super::QoS;
use crate::wait_list::WaitList;

/// One publish waiting for a client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingPublish {
    topic: String,
    payload: Vec<u8>,
    qos: QoS,
    retain: bool,
}

impl PendingPublish {
    /// A publish of `payload` to `topic`.
    pub fn new(topic: impl Into<String>, payload: &[u8], qos: QoS, retain: bool) -> Self {
        Self {
            topic: topic.into(),
            payload: payload.to_vec(),
            qos,
            retain,
        }
    }

    /// Destination topic.
    pub fn topic(&self) -> &str {
        &self.topic
    }

    /// Message body.
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Quality of service the publisher asked for.
    pub fn qos(&self) -> QoS {
        self.qos
    }

    /// Whether the broker should retain the message.
    pub fn retain(&self) -> bool {
        self.retain
    }
}

struct Inner<C> {
    client: Option<C>,
    pending: VecDeque<PendingPublish>,
}

/// Outcome of [`Outbox::try_with_client_mut`].
#[derive(Debug)]
pub enum TryClient<R> {
    /// A client is attached and the closure ran on it.
    Attached(R),
    /// No client is attached.
    Detached,
    /// The outbox lock is held elsewhere, so the closure did not run.
    Busy,
}

/// Holds publishes until a client can take them (see the module docs).
///
/// Every method takes the sink as an argument instead of storing it, so the
/// tier keeps ownership of what "sending" means (one `enqueue` call, no
/// decisions).
pub struct Outbox<C> {
    inner: Mutex<Inner<C>>,
    depth: usize,
    dropped: AtomicU32,
    /// Tasks waiting for a client or for room in `pending` (see
    /// [`publish_waiting`](Self::publish_waiting)).
    space: WaitList,
}

impl<C> Outbox<C> {
    /// An outbox holding up to `depth` publishes before a client is attached.
    pub fn new(depth: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                client: None,
                pending: VecDeque::with_capacity(depth),
            }),
            depth,
            dropped: AtomicU32::new(0),
            space: WaitList::new(),
        }
    }

    /// Attaches `client` and hands it every queued publish, in order.
    ///
    /// `send` returns whether the client took the message; one it refuses is
    /// counted in [`dropped`](Self::dropped). Runs with the outbox locked, so
    /// no publish can slip in between the queued ones and the client.
    ///
    /// Wakes every [`publish_waiting`](Self::publish_waiting) waiter once the
    /// queue is drained and the lock is released.
    pub fn attach(&self, client: C, mut send: impl FnMut(&mut C, &PendingPublish) -> bool) {
        {
            let mut inner = lock(&self.inner);
            inner.client = Some(client);
            let queued = core::mem::take(&mut inner.pending);
            if let Some(client) = inner.client.as_mut() {
                for message in &queued {
                    if !send(client, message) {
                        self.count_dropped(1);
                    }
                }
            }
        }
        // After the guard: waiters take the outbox lock inside their check, so
        // waking under it would invert the check(outbox) -> wake(waitlist)
        // order.
        self.space.wake_all();
    }

    /// Sends `message`, or queues it until a client is attached.
    ///
    /// The synchronous, legacy policy: with a client attached the message goes
    /// straight out through `send`; without one it is queued, and the oldest
    /// queued message is dropped (counted) when all [`depth`](Self::new) slots
    /// are taken. It cannot wait — use
    /// [`publish_waiting`](Self::publish_waiting) for the shared
    /// [`MqttClient::publish`](super::MqttClient::publish) contract.
    pub fn publish(
        &self,
        message: PendingPublish,
        mut send: impl FnMut(&mut C, &PendingPublish) -> bool,
    ) {
        let mut inner = lock(&self.inner);
        if let Some(client) = inner.client.as_mut() {
            if !send(client, &message) {
                self.count_dropped(1);
            }
            return;
        }
        if inner.pending.len() >= self.depth {
            inner.pending.pop_front();
            self.count_dropped(1);
        }
        inner.pending.push_back(message);
    }

    /// Sends `message`, waiting for room when the queue is full.
    ///
    /// With a client attached the message goes straight out through `send`; one
    /// the client refuses is counted in [`dropped`](Self::dropped), exactly as
    /// in [`publish`](Self::publish). Without a client it is queued, and when
    /// all [`depth`](Self::new) slots are taken this waits until one frees up or
    /// a client attaches — it never evicts a queued message. This is the shared
    /// [`MqttClient::publish`](super::MqttClient::publish) contract.
    pub async fn publish_waiting(
        &self,
        message: PendingPublish,
        mut send: impl FnMut(&mut C, &PendingPublish) -> bool,
    ) {
        loop {
            {
                let mut inner = lock(&self.inner);
                if let Some(client) = inner.client.as_mut() {
                    if !send(client, &message) {
                        self.count_dropped(1);
                    }
                    return;
                }
                if inner.pending.len() < self.depth {
                    inner.pending.push_back(message);
                    return;
                }
            }
            self.space
                .wait(|| {
                    let inner = lock(&self.inner);
                    (inner.client.is_some() || inner.pending.len() < self.depth).then_some(())
                })
                .await;
        }
    }

    /// Queued publishes the outbox had to drop: no client attached and all
    /// slots taken (the synchronous [`publish`](Self::publish) only), or a send
    /// the client refused.
    pub fn dropped(&self) -> u32 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// `true` once a client is attached.
    pub fn is_attached(&self) -> bool {
        lock(&self.inner).client.is_some()
    }

    /// Runs `f` on the attached client, or reports that there is none.
    ///
    /// Takes the outbox lock for the call, so the client can be reached without
    /// an outer mutex around the whole outbox.
    pub fn with_client_mut<R>(&self, f: impl FnOnce(&mut C) -> R) -> Option<R> {
        let mut inner = lock(&self.inner);
        inner.client.as_mut().map(f)
    }

    /// Runs `f` on the attached client without waiting for the outbox lock.
    ///
    /// A contended lock reports [`TryClient::Busy`] instead of blocking, so a
    /// caller on the event-loop or connect-helper thread can fail fast; the
    /// tier maps that to its non-blocking "would block" error.
    pub fn try_with_client_mut<R>(&self, f: impl FnOnce(&mut C) -> R) -> TryClient<R> {
        let mut inner = match self.inner.try_lock() {
            Ok(inner) => inner,
            Err(TryLockError::WouldBlock) => return TryClient::Busy,
            // Poisoning is ignored here as it is in `lock` above.
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
        };
        match inner.client.as_mut() {
            Some(client) => TryClient::Attached(f(client)),
            None => TryClient::Detached,
        }
    }

    /// Detaches the client, for shutdown; anything still queued stays queued.
    pub fn take_client(&self) -> Option<C> {
        lock(&self.inner).client.take()
    }

    fn count_dropped(&self, messages: u32) {
        self.dropped.fetch_add(messages, Ordering::Relaxed);
    }
}

impl<C> Default for Outbox<C> {
    fn default() -> Self {
        Self::new(4)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::{pin, Pin};
    use std::task::{Context, Poll, Waker};

    use super::*;

    /// The client in these tests: a log of everything it took.
    #[derive(Default)]
    struct RecordingClient {
        sent: Vec<String>,
        refuse: bool,
    }

    fn poll_with<F: Future>(mut future: Pin<&mut F>, waker: &Waker) -> Poll<F::Output> {
        future.as_mut().poll(&mut Context::from_waker(waker))
    }

    fn sink(client: &mut RecordingClient, message: &PendingPublish) -> bool {
        if client.refuse {
            return false;
        }
        client.sent.push(message.topic().to_string());
        true
    }

    fn message(topic: &str) -> PendingPublish {
        PendingPublish::new(topic, b"body", QoS::AtLeastOnce, false)
    }

    fn sent(outbox: &Outbox<RecordingClient>) -> Vec<String> {
        outbox
            .inner
            .lock()
            .unwrap()
            .client
            .as_ref()
            .map(|c| c.sent.clone())
            .unwrap_or_default()
    }

    #[test]
    fn a_publish_without_a_client_is_queued() {
        let outbox = Outbox::new(4);
        outbox.publish(message("a"), sink);
        assert!(!outbox.is_attached());
        let inner = outbox.inner.lock().unwrap();
        assert_eq!(inner.pending.len(), 1);
        assert_eq!(inner.pending[0].topic(), "a");
        assert_eq!(outbox.dropped(), 0);
    }

    #[test]
    fn attaching_hands_the_queued_publishes_over_in_order() {
        let outbox = Outbox::new(4);
        for topic in ["a", "b", "c"] {
            outbox.publish(message(topic), sink);
        }
        outbox.attach(RecordingClient::default(), sink);
        assert_eq!(sent(&outbox), ["a", "b", "c"]);
        assert!(outbox.is_attached());
        assert!(
            outbox.inner.lock().unwrap().pending.is_empty(),
            "the queue must be drained, not replayed"
        );
    }

    #[test]
    fn a_publish_with_a_client_attached_goes_straight_out() {
        let outbox = Outbox::new(4);
        outbox.attach(RecordingClient::default(), sink);
        outbox.publish(message("a"), sink);
        outbox.publish(message("b"), sink);
        assert_eq!(sent(&outbox), ["a", "b"]);
        assert_eq!(outbox.dropped(), 0);
    }

    #[test]
    fn a_message_published_after_the_handover_follows_the_queued_ones() {
        let outbox = Outbox::new(4);
        outbox.publish(message("queued"), sink);
        outbox.attach(RecordingClient::default(), sink);
        outbox.publish(message("later"), sink);
        assert_eq!(
            sent(&outbox),
            ["queued", "later"],
            "order must survive the handover"
        );
    }

    #[test]
    fn a_full_queue_drops_the_oldest_message() {
        let outbox = Outbox::new(2);
        for topic in ["a", "b", "c"] {
            outbox.publish(message(topic), sink);
        }
        assert_eq!(outbox.dropped(), 1);
        outbox.attach(RecordingClient::default(), sink);
        assert_eq!(sent(&outbox), ["b", "c"], "the oldest entry makes room");
    }

    #[test]
    fn a_client_that_refuses_a_message_is_counted() {
        let outbox = Outbox::new(4);
        outbox.attach(
            RecordingClient {
                refuse: true,
                ..Default::default()
            },
            sink,
        );
        outbox.publish(message("a"), sink);
        assert_eq!(outbox.dropped(), 1);
        assert!(sent(&outbox).is_empty());
    }

    #[test]
    fn a_queued_message_the_client_refuses_is_counted() {
        let outbox = Outbox::new(4);
        outbox.publish(message("a"), sink);
        outbox.attach(
            RecordingClient {
                refuse: true,
                ..Default::default()
            },
            sink,
        );
        assert_eq!(outbox.dropped(), 1);
    }

    #[test]
    fn taking_the_client_leaves_the_outbox_without_one() {
        let outbox = Outbox::new(4);
        outbox.attach(RecordingClient::default(), sink);
        assert!(outbox.take_client().is_some());
        assert!(!outbox.is_attached());
        outbox.publish(message("a"), sink);
        assert_eq!(outbox.inner.lock().unwrap().pending.len(), 1);
    }

    #[test]
    fn a_pending_message_keeps_its_payload_and_flags() {
        let message = PendingPublish::new("topic/x", b"\x00\x01", QoS::ExactlyOnce, true);
        assert_eq!(message.topic(), "topic/x");
        assert_eq!(message.payload(), b"\x00\x01");
        assert_eq!(message.qos(), QoS::ExactlyOnce);
        assert!(message.retain());
    }

    #[test]
    fn a_full_queue_makes_publish_waiting_pending_without_evicting() {
        let outbox = Outbox::new(2);
        outbox.publish(message("a"), sink);
        outbox.publish(message("b"), sink);
        let mut waiting = pin!(outbox.publish_waiting(message("c"), sink));
        assert!(
            poll_with(waiting.as_mut(), Waker::noop()).is_pending(),
            "a full queue must wait instead of making room"
        );
        let inner = outbox.inner.lock().unwrap();
        assert_eq!(
            inner
                .pending
                .iter()
                .map(PendingPublish::topic)
                .collect::<Vec<_>>(),
            ["a", "b"],
            "the queued set must be unchanged"
        );
        assert_eq!(outbox.dropped(), 0);
    }

    #[test]
    fn attaching_completes_a_waiter_and_hands_every_message_over_in_order() {
        let outbox = Outbox::new(4);
        for topic in ["a", "b", "c", "d"] {
            outbox.publish(message(topic), sink);
        }
        let mut waiting = pin!(outbox.publish_waiting(message("e"), sink));
        assert!(poll_with(waiting.as_mut(), Waker::noop()).is_pending());
        outbox.attach(RecordingClient::default(), sink);
        assert!(
            poll_with(waiting.as_mut(), Waker::noop()).is_ready(),
            "attach must wake the waiter"
        );
        assert_eq!(sent(&outbox), ["a", "b", "c", "d", "e"]);
        assert_eq!(outbox.dropped(), 0, "waiting never evicts");
    }

    #[test]
    fn publish_waiting_with_a_client_attached_sends_at_once() {
        let outbox = Outbox::new(4);
        outbox.attach(RecordingClient::default(), sink);
        let mut waiting = pin!(outbox.publish_waiting(message("a"), sink));
        assert!(poll_with(waiting.as_mut(), Waker::noop()).is_ready());
        assert_eq!(sent(&outbox), ["a"]);
        assert_eq!(outbox.dropped(), 0);
    }

    #[test]
    fn a_publish_waiting_the_client_refuses_is_counted() {
        let outbox = Outbox::new(4);
        outbox.attach(
            RecordingClient {
                refuse: true,
                ..Default::default()
            },
            sink,
        );
        let mut waiting = pin!(outbox.publish_waiting(message("a"), sink));
        assert!(poll_with(waiting.as_mut(), Waker::noop()).is_ready());
        assert_eq!(outbox.dropped(), 1);
        assert!(sent(&outbox).is_empty());
    }

    #[test]
    fn with_client_mut_reaches_the_attached_client() {
        let outbox: Outbox<RecordingClient> = Outbox::new(4);
        assert_eq!(outbox.with_client_mut(|client| client.sent.len()), None);
        outbox.attach(RecordingClient::default(), sink);
        let len = outbox.with_client_mut(|client| {
            client.sent.push("x".to_string());
            client.sent.len()
        });
        assert_eq!(len, Some(1));
    }

    #[test]
    fn try_with_client_mut_reports_detached_busy_and_attached() {
        let outbox: Outbox<RecordingClient> = Outbox::new(4);
        assert!(matches!(
            outbox.try_with_client_mut(|client| client.sent.len()),
            TryClient::Detached
        ));
        outbox.attach(RecordingClient::default(), sink);
        assert!(matches!(
            outbox.try_with_client_mut(|client| client.sent.len()),
            TryClient::Attached(0)
        ));
        let _held = outbox.inner.lock().unwrap();
        assert!(matches!(
            outbox.try_with_client_mut(|_| ()),
            TryClient::Busy
        ));
    }
}
