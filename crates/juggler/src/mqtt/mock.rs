//! [`MockMqttClient`] — an [`MqttClient`] test double for firmware host tests.

use alloc::collections::VecDeque;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::task::Poll;

use super::{MqttClient, MqttConnectionState, MqttMessage, QoS, TryPublishError};

/// Error returned by [`MockMqttClient::publish`] when a failure was requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MockMqttError;

/// A message delivered to, or published through, the mock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MockMessage {
    /// Topic.
    pub topic: String,
    /// Body.
    pub payload: Vec<u8>,
    /// QoS.
    pub qos: QoS,
    /// Retain flag (published) or retained delivery (received).
    pub retained: bool,
}

impl MqttMessage for MockMessage {
    fn topic(&self) -> &str {
        &self.topic
    }

    fn payload(&self) -> &[u8] {
        &self.payload
    }

    fn retained(&self) -> bool {
        self.retained
    }

    fn qos(&self) -> QoS {
        self.qos
    }
}

/// An [`MqttClient`] whose session the test drives by hand.
///
/// Published messages are recorded in order; `wait_connected` and `receive`
/// wake themselves on every poll, so a test drives them with a polling `block_on`.
#[derive(Debug)]
pub struct MockMqttClient {
    state: Cell<MqttConnectionState>,
    connects: Cell<u32>,
    published: RefCell<Vec<MockMessage>>,
    inbound: RefCell<VecDeque<MockMessage>>,
    subscribed: RefCell<Vec<(String, QoS)>>,
    /// When `true`, `try_publish*` refuse with `WouldBlock` even while connected.
    pub busy: Cell<bool>,
    /// When `true`, `publish` fails with [`MockMqttError`].
    pub fail_publish: Cell<bool>,
}

impl MockMqttClient {
    /// A client whose first session has not come up yet.
    pub fn new() -> Self {
        Self {
            state: Cell::new(MqttConnectionState::Connecting),
            connects: Cell::new(0),
            published: RefCell::new(Vec::new()),
            inbound: RefCell::new(VecDeque::new()),
            subscribed: RefCell::new(Vec::new()),
            busy: Cell::new(false),
            fail_publish: Cell::new(false),
        }
    }

    /// Simulates a new broker session.
    pub fn connect(&self) {
        self.state.set(MqttConnectionState::Connected);
        self.connects.set(self.connects.get() + 1);
    }

    /// Simulates a lost session.
    pub fn disconnect(&self) {
        self.state.set(MqttConnectionState::Disconnected);
    }

    /// Queues a message for `receive`.
    pub fn deliver(&self, topic: &str, payload: &[u8]) {
        self.inbound.borrow_mut().push_back(MockMessage {
            topic: topic.to_string(),
            payload: payload.to_vec(),
            qos: QoS::AtLeastOnce,
            retained: false,
        });
    }

    /// Everything published so far, oldest first.
    pub fn published(&self) -> Vec<MockMessage> {
        self.published.borrow().clone()
    }

    /// Every `subscribe` the mock accepted, oldest first, one entry per filter.
    pub fn subscribed(&self) -> Vec<(String, QoS)> {
        self.subscribed.borrow().clone()
    }

    fn record(&self, topic: &str, payload: &[u8], qos: QoS, retain: bool) {
        self.published.borrow_mut().push(MockMessage {
            topic: topic.to_string(),
            payload: payload.to_vec(),
            qos,
            retained: retain,
        });
    }
}

impl Default for MockMqttClient {
    fn default() -> Self {
        Self::new()
    }
}

impl MqttClient for MockMqttClient {
    type Error = MockMqttError;
    type Message = MockMessage;

    fn state(&self) -> MqttConnectionState {
        self.state.get()
    }

    fn connect_count(&self) -> u32 {
        self.connects.get()
    }

    fn try_publish_with(
        &self,
        topic: &str,
        payload: &[u8],
        qos: QoS,
        retain: bool,
    ) -> Result<(), TryPublishError<Self::Error>> {
        if !self.is_connected() || self.busy.get() {
            return Err(TryPublishError::WouldBlock);
        }
        self.record(topic, payload, qos, retain);
        Ok(())
    }

    async fn publish(
        &self,
        topic: &str,
        payload: &[u8],
        qos: QoS,
        retain: bool,
    ) -> Result<(), Self::Error> {
        if self.fail_publish.get() {
            return Err(MockMqttError);
        }
        self.record(topic, payload, qos, retain);
        Ok(())
    }

    fn subscribe(&self, topic: &str, qos: QoS) -> Result<(), Self::Error> {
        let mut subscribed = self.subscribed.borrow_mut();
        match subscribed.iter_mut().find(|(filter, _)| filter == topic) {
            Some(entry) => entry.1 = qos,
            None => subscribed.push((topic.to_string(), qos)),
        }
        Ok(())
    }

    async fn wait_connected(&self) {
        core::future::poll_fn(|cx| {
            if self.is_connected() {
                Poll::Ready(())
            } else {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
        .await
    }

    async fn receive(&self) -> Self::Message {
        core::future::poll_fn(|cx| match self.inbound.borrow_mut().pop_front() {
            Some(message) => Poll::Ready(message),
            None => {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
        .await
    }
}
