//! Session state shared by the event-loop thread, the connect helper and every
//! [`MqttHandle`](super::MqttHandle) clone (ADR 022, tier-parity Step 8b).
//!
//! The shared [`SessionMachine`] is the single source of state, readiness, the
//! publish gate and the connect count. Its mutex is never held across a client
//! call, so the event-loop thread may take it (ADR 017 forbids only the client
//! mutex there).

use std::collections::VecDeque;
use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard, PoisonError};

use juggler::mqtt::{
    MqttConnectionState, MqttMessage, QoS, SessionAction, SessionInput, SessionMachine,
    SessionToken,
};
use juggler::wait_list::WaitList;

/// Messages [`MqttHandle::receive`](super::MqttHandle::receive) can hold before
/// the oldest is dropped.
pub const INBOUND_QUEUE_DEPTH: usize = 8;

/// A message received on a builder subscription, for
/// [`MqttHandle::receive`](super::MqttHandle::receive).
///
/// esp-idf-svc 0.53 reports neither the retain flag nor the delivery QoS of a
/// received message, so [`retained`](MqttMessage::retained) is always `false`
/// and [`qos`](MqttMessage::qos) always [`QoS::AtMostOnce`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundMessage {
    /// Topic the message was published to.
    pub topic: String,
    /// Message body.
    pub payload: Vec<u8>,
}

impl MqttMessage for InboundMessage {
    fn topic(&self) -> &str {
        &self.topic
    }

    fn payload(&self) -> &[u8] {
        &self.payload
    }

    fn retained(&self) -> bool {
        false
    }

    fn qos(&self) -> QoS {
        QoS::AtMostOnce
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Everything the event loop, the connect helper and the handles share.
pub(crate) struct Session {
    machine: Mutex<SessionMachine>,
    ready: WaitList,
    inbound: Mutex<VecDeque<InboundMessage>>,
    arrivals: WaitList,
    dropped_inbound: AtomicU32,
}

impl Session {
    pub(crate) fn new() -> Self {
        Self {
            machine: Mutex::new(SessionMachine::new()),
            ready: WaitList::new(),
            inbound: Mutex::new(VecDeque::with_capacity(INBOUND_QUEUE_DEPTH)),
            arrivals: WaitList::new(),
            dropped_inbound: AtomicU32::new(0),
        }
    }

    /// Feeds the machine; wakes `wait_connected` once connect work made the session ready.
    pub(crate) fn poll(&self, input: SessionInput) -> SessionAction {
        let (action, ready) = {
            let mut machine = lock(&self.machine);
            let action = machine.poll(input);
            (action, machine.can_publish())
        };
        if ready && matches!(input, SessionInput::WorkDone(_)) {
            self.ready.wake_all();
        }
        action
    }

    pub(crate) fn state(&self) -> MqttConnectionState {
        lock(&self.machine).state()
    }

    pub(crate) fn can_publish(&self) -> bool {
        lock(&self.machine).can_publish()
    }

    pub(crate) fn connect_count(&self) -> u32 {
        lock(&self.machine).connect_count()
    }

    pub(crate) fn is_current(&self, token: SessionToken) -> bool {
        lock(&self.machine).is_current(token)
    }

    /// Queues a received message, dropping the oldest when full.
    pub(crate) fn push_inbound(&self, topic: &str, payload: &[u8]) {
        {
            let mut inbound = lock(&self.inbound);
            if inbound.len() >= INBOUND_QUEUE_DEPTH {
                inbound.pop_front();
                self.dropped_inbound.fetch_add(1, Ordering::Relaxed);
            }
            inbound.push_back(InboundMessage {
                topic: topic.to_string(),
                payload: payload.to_vec(),
            });
        }
        self.arrivals.wake_all();
    }

    pub(crate) fn try_receive(&self) -> Option<InboundMessage> {
        lock(&self.inbound).pop_front()
    }

    pub(crate) fn dropped_inbound(&self) -> u32 {
        self.dropped_inbound.load(Ordering::Relaxed)
    }

    pub(crate) fn wait_connected(&self) -> impl Future<Output = ()> + '_ {
        self.ready.wait(move || self.can_publish().then_some(()))
    }

    pub(crate) fn receive(&self) -> impl Future<Output = InboundMessage> + '_ {
        self.arrivals.wait(move || self.try_receive())
    }
}
