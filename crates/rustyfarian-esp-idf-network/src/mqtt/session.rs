//! Session state shared by the runner thread, the event-loop thread and every
//! [`MqttHandle`](super::MqttHandle) clone (ADR 022).
//!
//! The shared [`SessionMachine`] is the single source of state, readiness, the
//! publish gate and the connect count. Its mutex is never held across a client
//! call, so the event-loop thread may take it (ADR 017 forbids only the client
//! mutex there).

use std::collections::VecDeque;
use std::future::Future;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use juggler::mqtt::{
    MqttConnectionState, MqttMessage, Outbox, PendingPublish, QoS, SessionAction, SessionInput,
    SessionMachine, SessionToken,
};
use juggler::wait_list::WaitList;

use super::runner::Msg;
use super::{to_esp_qos, SubscribableClient};

/// Messages [`MqttHandle::receive`](super::MqttHandle::receive) can hold before
/// the oldest is dropped.
pub const INBOUND_QUEUE_DEPTH: usize = 8;

/// Largest message body [`MqttHandle::receive`](super::MqttHandle::receive)
/// reassembles from esp-mqtt's chunks; larger ones are dropped and counted.
pub const MAX_INBOUND_PAYLOAD_LEN: usize = 16 * 1024;

/// Publishes held while the client does not exist yet (same depth as esp-hal).
pub const PUBLISH_QUEUE_DEPTH: usize = 4;

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

impl InboundMessage {
    /// Topic the message was published to.
    pub fn topic(&self) -> &str {
        &self.topic
    }

    /// Message body.
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    /// Always `false`, esp-idf-svc 0.53 does not report the retain flag.
    pub fn retained(&self) -> bool {
        false
    }

    /// Always [`QoS::AtMostOnce`], esp-idf-svc 0.53 does not report the delivery QoS.
    pub fn qos(&self) -> QoS {
        QoS::AtMostOnce
    }
}

impl MqttMessage for InboundMessage {
    fn topic(&self) -> &str {
        InboundMessage::topic(self)
    }

    fn payload(&self) -> &[u8] {
        InboundMessage::payload(self)
    }

    fn retained(&self) -> bool {
        InboundMessage::retained(self)
    }

    fn qos(&self) -> QoS {
        InboundMessage::qos(self)
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Everything the runner, the event loop and the handles share.
pub(crate) struct Session {
    machine: Mutex<SessionMachine>,
    ready: WaitList,
    inbound: Mutex<VecDeque<InboundMessage>>,
    arrivals: WaitList,
    dropped_inbound: AtomicU32,
    /// The esp-mqtt client, once the station has an IP, plus the
    /// publishes that arrived before it. The outbox owns its own lock, so no
    /// outer mutex is needed to reach the client.
    outbox: Arc<Outbox<SubscribableClient>>,
    /// Session inputs for the runner thread; the event loop only reports, it
    /// never acts (ADR 017).
    inputs: Sender<Msg>,
}

impl Session {
    pub(super) fn new(retry_ms: u64, inputs: Sender<Msg>) -> Self {
        Self {
            machine: Mutex::new(SessionMachine::with_retry(retry_ms)),
            ready: WaitList::new(),
            inbound: Mutex::new(VecDeque::with_capacity(INBOUND_QUEUE_DEPTH)),
            arrivals: WaitList::new(),
            dropped_inbound: AtomicU32::new(0),
            outbox: Arc::new(Outbox::new(PUBLISH_QUEUE_DEPTH)),
            inputs,
        }
    }

    /// Hands `input` to the runner, which feeds the machine and performs the
    /// action. Never blocks; a gone runner means the handle is being torn down.
    pub(crate) fn send(&self, input: SessionInput) {
        let _ = self.inputs.send(Msg::Input(input));
    }

    /// Feeds the machine; wakes `wait_connected` when it answers
    /// [`SessionAction::Ready`].
    ///
    /// Only the runner calls this: every other action it returns is a driver call.
    pub(crate) fn poll(&self, input: SessionInput) -> SessionAction {
        let action = lock(&self.machine).poll(input);
        if action == SessionAction::Ready {
            self.ready.wake_all();
        }
        action
    }

    /// The shared outbox, for the runner's connect steps.
    pub(crate) fn outbox(&self) -> &Arc<Outbox<SubscribableClient>> {
        &self.outbox
    }

    /// Hands the client to the outbox, which then takes every queued publish.
    pub(crate) fn attach_client(&self, client: SubscribableClient) {
        self.outbox.attach(client, send_pending);
    }

    /// Takes the client back out, for shutdown.
    pub(crate) fn take_client(&self) -> Option<SubscribableClient> {
        self.outbox.take_client()
    }

    /// The synchronous publish: straight out with a client attached, queued
    /// while there is none, dropping the oldest entry when all slots are taken
    /// (see [`Outbox::publish`]).
    pub(crate) fn publish(&self, message: PendingPublish) {
        self.outbox.publish(message, send_pending);
    }

    /// The shared [`MqttClient::publish`](juggler::mqtt::MqttClient::publish)
    /// contract: waits until the outbox has room, never evicting a queued entry.
    pub(crate) async fn publish_waiting(&self, message: PendingPublish) {
        self.outbox.publish_waiting(message, send_pending).await;
    }

    /// Publishes the outbox had to drop (no client yet and all slots taken, or
    /// a publish the client refused).
    pub(crate) fn dropped_publish(&self) -> u32 {
        self.outbox.dropped()
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

    /// `true` while closing a session whose driver had already dropped its
    /// transport, so the tier reports `Disconnected` itself (see
    /// [`SessionMachine::driver_idle`]).
    pub(crate) fn driver_idle(&self) -> bool {
        lock(&self.machine).driver_idle()
    }

    /// Queues a whole received message, dropping the oldest when full.
    pub(crate) fn push_message(&self, message: InboundMessage) {
        {
            let mut inbound = lock(&self.inbound);
            if inbound.len() >= INBOUND_QUEUE_DEPTH {
                inbound.pop_front();
                self.count_dropped(1);
            }
            inbound.push_back(message);
        }
        self.arrivals.wake_all();
    }

    /// Counts received messages that never reached the queue.
    pub(crate) fn count_dropped(&self, messages: u32) {
        if messages > 0 {
            self.dropped_inbound.fetch_add(messages, Ordering::Relaxed);
        }
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

/// Hands one outbox message to esp-mqtt's outbox; `false` means it was refused.
fn send_pending(client: &mut SubscribableClient, message: &PendingPublish) -> bool {
    match client.enqueue(
        message.topic(),
        to_esp_qos(message.qos()),
        message.retain(),
        message.payload(),
    ) {
        Ok(_) => true,
        Err(e) => {
            log::warn!(
                "[mqtt] publish of a queued message to '{}' failed: {:?}",
                message.topic(),
                e
            );
            false
        }
    }
}
