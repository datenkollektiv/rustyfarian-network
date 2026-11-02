//! [`MqttClient`] and [`MqttMessage`] — the MQTT surface both tiers implement (ADR 022).
//!
//! Firmware that is generic over `MqttClient` runs unchanged on the ESP-IDF and
//! the esp-hal `MqttHandle`; only the setup code differs.

use super::{MqttConnectionState, QoS, TryPublishError};

/// A received MQTT message; each tier keeps its own storage behind it.
pub trait MqttMessage {
    /// Topic the message was published to.
    fn topic(&self) -> &str;
    /// Message body.
    fn payload(&self) -> &[u8];
    /// `true` if the broker delivered it from its retained store.
    fn retained(&self) -> bool;
    /// Delivery QoS.
    fn qos(&self) -> QoS;
}

/// MQTT client surface shared by both tiers.
///
/// `try_publish*` never wait: they refuse with [`TryPublishError::WouldBlock`]
/// while the session is not [`Connected`](MqttConnectionState::Connected) or
/// the client cannot take the message right now.
///
/// [`publish`](Self::publish) waits until the tier's admission queue has room
/// for the message, so it never makes room by evicting one that is already
/// queued. Once admitted, delivery is best effort: a message the transport
/// refuses is dropped and counted in the tier's `dropped_publish()` — waiting
/// is for admission, never for the broker's acknowledgement. The contract is
/// implemented once, in `Outbox::publish_waiting` (`std`); the esp-hal tier's
/// channel send already behaves this way.
///
/// The returned futures are not required to be `Send`, because the esp-hal
/// ones are not.
#[allow(async_fn_in_trait)] // futures deliberately carry no `Send` bound (ADR 022)
pub trait MqttClient {
    /// Tier error, reported by `publish` and inside [`TryPublishError::Other`].
    type Error: core::fmt::Debug;
    /// Received message type.
    type Message: MqttMessage;

    /// Current session state.
    fn state(&self) -> MqttConnectionState;

    /// `true` exactly when [`state`](Self::state) is `Connected`.
    fn is_connected(&self) -> bool {
        self.state() == MqttConnectionState::Connected
    }

    /// Sessions established since boot; a change means a new broker session.
    fn connect_count(&self) -> u32;

    /// Publishes with QoS 1, not retained, without waiting.
    fn try_publish(&self, topic: &str, payload: &[u8]) -> Result<(), TryPublishError<Self::Error>> {
        self.try_publish_with(topic, payload, QoS::AtLeastOnce, false)
    }

    /// Publishes with QoS 1, retained, without waiting.
    fn try_publish_retained(
        &self,
        topic: &str,
        payload: &[u8],
    ) -> Result<(), TryPublishError<Self::Error>> {
        self.try_publish_with(topic, payload, QoS::AtLeastOnce, true)
    }

    /// Publishes with explicit QoS and retain flag, without waiting.
    fn try_publish_with(
        &self,
        topic: &str,
        payload: &[u8],
        qos: QoS,
        retain: bool,
    ) -> Result<(), TryPublishError<Self::Error>>;

    /// Publishes, waiting until the admission queue has room for the message.
    ///
    /// Never evicts a queued message to make room; a message the transport
    /// refuses afterwards is dropped and counted in the tier's
    /// `dropped_publish()`.
    async fn publish(
        &self,
        topic: &str,
        payload: &[u8],
        qos: QoS,
        retain: bool,
    ) -> Result<(), Self::Error>;

    /// Completes once the session is `Connected`.
    ///
    /// Any number of tasks may wait concurrently; the tier wakes all of them.
    async fn wait_connected(&self);

    /// Waits for the next message on a subscribed topic.
    async fn receive(&self) -> Self::Message;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mqtt::mock::MockMqttClient;
    use crate::test_support::block_on;

    #[test]
    fn try_publish_refuses_until_the_session_is_connected() {
        let mqtt = MockMqttClient::new();
        assert!(matches!(
            mqtt.try_publish("t/a", b"1"),
            Err(TryPublishError::WouldBlock)
        ));
        mqtt.connect();
        assert!(mqtt.try_publish("t/a", b"1").is_ok());
        mqtt.disconnect();
        assert!(matches!(
            mqtt.try_publish_retained("t/a", b"2"),
            Err(TryPublishError::WouldBlock)
        ));
        assert_eq!(mqtt.published().len(), 1);
    }

    #[test]
    fn the_default_try_publish_methods_pick_qos_1_and_the_retain_flag() {
        let mqtt = MockMqttClient::new();
        mqtt.connect();
        mqtt.try_publish("t/plain", b"a").unwrap();
        mqtt.try_publish_retained("t/kept", b"b").unwrap();
        let published = mqtt.published();
        assert_eq!(published[0].qos, QoS::AtLeastOnce);
        assert!(!published[0].retained);
        assert!(published[1].retained);
    }

    #[test]
    fn a_busy_client_refuses_without_waiting() {
        let mqtt = MockMqttClient::new();
        mqtt.connect();
        mqtt.busy.set(true);
        assert!(matches!(
            mqtt.try_publish("t/a", b"1"),
            Err(TryPublishError::WouldBlock)
        ));
        assert!(mqtt.published().is_empty());
    }

    #[test]
    fn publish_and_receive_complete_through_the_trait() {
        let mqtt = MockMqttClient::new();
        mqtt.deliver("t/cmd", b"hello");
        block_on(mqtt.publish("t/out", b"x", QoS::AtMostOnce, false)).unwrap();
        let message = block_on(mqtt.receive());
        assert_eq!(message.topic(), "t/cmd");
        assert_eq!(message.payload(), b"hello");
        assert_eq!(mqtt.published()[0].qos, QoS::AtMostOnce);
    }

    #[cfg(feature = "wifi")]
    mod one_app_both_tiers {
        use super::*;
        use crate::wifi::mock::MockLink;
        use crate::wifi::NetworkLink;

        /// Firmware logic written once against the shared traits (ADR 022).
        async fn announce<L: NetworkLink, M: MqttClient>(
            link: &L,
            mqtt: &M,
        ) -> Result<u32, TryPublishError<M::Error>> {
            link.wait_connected().await;
            mqtt.wait_connected().await;
            mqtt.try_publish_retained("dev/status", b"online")?;
            Ok(mqtt.connect_count())
        }

        #[test]
        fn generic_firmware_code_runs_against_the_shared_traits() {
            let link = MockLink::new();
            let mqtt = MockMqttClient::new();
            link.associate();
            mqtt.connect();
            assert_eq!(block_on(announce(&link, &mqtt)).unwrap(), 1);
            assert_eq!(mqtt.published()[0].topic, "dev/status");
        }

        #[test]
        fn suspend_and_resume_go_through_the_link_trait() {
            let link = MockLink::new();
            link.associate();
            link.suspend().unwrap();
            assert!(link.is_suspended());
            assert!(!link.is_connected());
            link.resume().unwrap();
            assert!(!link.is_suspended());
            link.fail_calls.set(true);
            assert!(link.suspend().is_err());
            assert_eq!(link.connect_count(), 1);
        }
    }
}
