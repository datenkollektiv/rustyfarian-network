//! Async MQTT 5 client for the bare-metal tier (ADR 019).
//!
//! The client is `minimq` driven over an `embassy-net` [`TcpSocket`] from the
//! STA stack that [`WiFiManager::init_async`] returns.
//! One background runner owns the session; the application talks to it
//! through a [`MqttHandle`] backed by bounded `embassy-sync` channels, so no
//! closure ever runs inside the client and the ESP-IDF tier's callback
//! contract (ADR 017) has no counterpart here.
//!
//! Behavioural parity with the ESP-IDF tier:
//!
//! * Last Will and Testament via [`MqttConfig::with_lwt`].
//! * A startup message (typically a retained `online` flag) published on every
//!   (re)connect via [`MqttBuilder::with_startup_message`].
//! * Builder subscriptions re-issued after every reconnect.
//! * Auto-reconnect at [`MqttConfig::reconnect_timeout_ms`] (default 10 s).
//! * [`MqttHandle::try_publish`] never blocks and reports
//!   [`TryPublishError::WouldBlock`] while the session is down or the queue is full.
//!
//! Limitations in v1: the broker host must be an IPv4 literal, publishes are
//! QoS 0/1, subscriptions are fixed at build time, and payloads are capped at
//! [`MAX_PAYLOAD_LEN`] bytes. All buffers are `static` — nothing is heap-allocated.
//!
//! ```ignore
//! use rustyfarian_esp_hal_network::mqtt::{LwtConfig, MqttBuilder, MqttConfig, QoS};
//!
//! let lwt = LwtConfig::new("knob/status", b"offline", QoS::AtLeastOnce, true);
//! let config = MqttConfig::new("192.168.1.10", 1883, "knob-01").with_lwt(lwt);
//! let (mqtt, runner) = MqttBuilder::new(config)
//!     .with_startup_message("knob/status", b"online", QoS::AtLeastOnce, true)
//!     .subscribe("knob/cmd/#", QoS::AtLeastOnce)
//!     .build(stack)?;
//! spawner.spawn(mqtt_task(runner).unwrap());
//! ```
//!
//! [`TcpSocket`]: embassy_net::tcp::TcpSocket
//! [`WiFiManager::init_async`]: crate::wifi::WiFiManager::init_async

use core::fmt;
use core::net::Ipv4Addr;
use core::str::FromStr;

pub use juggler::mqtt::{
    topic_matches_filter, validate_broker_port, validate_client_id, validate_publish_topic,
    validate_subscribe_filter, LwtConfig, MqttConfig, MqttConnectionState, QoS,
};

/// Longest topic (publish or subscribe filter) the queues store, in bytes.
pub const MAX_TOPIC_LEN: usize = 64;
/// Longest publish payload the queues store, in bytes.
pub const MAX_PAYLOAD_LEN: usize = 256;
/// Subscriptions a builder accepts.
pub const MAX_SUBSCRIPTIONS: usize = 8;
/// Outbound publishes buffered between the application and the runner.
pub const PUBLISH_QUEUE_DEPTH: usize = 4;
/// Inbound messages buffered between the runner and the application.
///
/// When full, the oldest message is dropped and [`MqttHandle::dropped_inbound`] grows.
pub const INBOUND_QUEUE_DEPTH: usize = 4;
/// `minimq` inbound packet buffer: one MQTT control packet, topic + properties + payload.
pub const MQTT_RX_BUFFER: usize = 1024;
/// `minimq` outbound arena: in-flight QoS 1 packets plus encode workspace.
pub const MQTT_TX_BUFFER: usize = 2048;
/// TCP receive buffer handed to `embassy-net`.
pub const TCP_RX_BUFFER: usize = 1536;
/// TCP transmit buffer handed to `embassy-net`.
pub const TCP_TX_BUFFER: usize = 1536;
/// Keep-alive advertised to the broker when [`MqttConfig::keep_alive_secs`] is unset.
pub const DEFAULT_KEEP_ALIVE_SECS: u64 = 30;
/// TCP connect and MQTT handshake budget when [`MqttConfig::connection_timeout_ms`] is unset.
pub const DEFAULT_CONNECTION_TIMEOUT_MS: u64 = 5_000;
/// Pause between reconnect attempts when [`MqttConfig::reconnect_timeout_ms`] is unset.
pub const DEFAULT_RECONNECT_TIMEOUT_MS: u64 = 10_000;

/// Errors reported by the bare-metal MQTT client.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MqttError {
    /// A [`MqttConfig`] field was rejected at build time; the payload names it.
    InvalidConfig(&'static str),
    /// [`MqttBuilder::build`] was already called once this boot.
    AlreadyBuilt,
    /// Topic longer than [`MAX_TOPIC_LEN`].
    TopicTooLong,
    /// Payload longer than [`MAX_PAYLOAD_LEN`].
    PayloadTooLarge,
    /// `juggler::mqtt` topic validation failed; the payload names the rule.
    InvalidTopic(&'static str),
    /// More than [`MAX_SUBSCRIPTIONS`] topics were passed to the builder.
    TooManySubscriptions,
}

impl fmt::Display for MqttError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfig(what) => write!(f, "invalid MQTT config: {}", what),
            Self::AlreadyBuilt => write!(f, "MQTT client already built"),
            Self::TopicTooLong => write!(f, "topic exceeds {} bytes", MAX_TOPIC_LEN),
            Self::PayloadTooLarge => write!(f, "payload exceeds {} bytes", MAX_PAYLOAD_LEN),
            Self::InvalidTopic(why) => write!(f, "invalid topic: {}", why),
            Self::TooManySubscriptions => {
                write!(f, "more than {} subscriptions", MAX_SUBSCRIPTIONS)
            }
        }
    }
}

impl core::error::Error for MqttError {}

/// Error returned by the `try_publish*` family.
///
/// `WouldBlock` means "retry next tick": the session is not connected or the
/// publish queue is full. `Other` carries a [`MqttError`] that retrying will not fix.
pub type TryPublishError = juggler::mqtt::TryPublishError<MqttError>;

/// An inbound publish, copied out of the client's packet buffer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboundMessage {
    /// Topic the broker delivered the message on.
    pub topic: heapless::String<MAX_TOPIC_LEN>,
    /// Message payload.
    pub payload: heapless::Vec<u8, MAX_PAYLOAD_LEN>,
    /// Whether the broker flagged the message as retained.
    pub retained: bool,
    /// QoS the broker delivered the message with.
    pub qos: QoS,
}

impl InboundMessage {
    /// Copies topic and payload into the fixed-capacity fields.
    ///
    /// Returns `None` when either exceeds its cap; the runner then drops the message.
    pub(crate) fn copy_from(topic: &str, payload: &[u8], retained: bool, qos: QoS) -> Option<Self> {
        let topic = heapless::String::try_from(topic).ok()?;
        let payload = heapless::Vec::from_slice(payload).ok()?;
        Some(Self {
            topic,
            payload,
            retained,
            qos,
        })
    }
}

/// One queued outbound publish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PublishRequest {
    pub(crate) topic: heapless::String<MAX_TOPIC_LEN>,
    pub(crate) payload: heapless::Vec<u8, MAX_PAYLOAD_LEN>,
    pub(crate) qos: QoS,
    pub(crate) retain: bool,
}

impl PublishRequest {
    /// Validates the topic and copies both parts into fixed-capacity storage.
    pub(crate) fn new(
        topic: &str,
        payload: &[u8],
        qos: QoS,
        retain: bool,
    ) -> Result<Self, MqttError> {
        validate_publish_topic(topic).map_err(MqttError::InvalidTopic)?;
        let topic = heapless::String::try_from(topic).map_err(|_| MqttError::TopicTooLong)?;
        let payload = heapless::Vec::from_slice(payload).map_err(|_| MqttError::PayloadTooLarge)?;
        Ok(Self {
            topic,
            payload,
            qos,
            retain,
        })
    }
}

/// Parses the broker host as an IPv4 literal (v1 has no DNS; ADR 019 §2).
pub(crate) fn parse_broker_ipv4(host: &str) -> Result<Ipv4Addr, MqttError> {
    Ipv4Addr::from_str(host.trim())
        .map_err(|_| MqttError::InvalidConfig("host must be an IPv4 literal on the esp-hal tier"))
}

/// Build-time validation shared by the runner and the host tests.
pub(crate) fn validate_config(config: &MqttConfig<'_>) -> Result<(), MqttError> {
    parse_broker_ipv4(config.host)?;
    validate_broker_port(config.port).map_err(MqttError::InvalidConfig)?;
    validate_client_id(config.client_id).map_err(MqttError::InvalidConfig)?;
    if let Some(lwt) = config.lwt() {
        validate_publish_topic(lwt.topic()).map_err(MqttError::InvalidConfig)?;
        if lwt.topic().len() > MAX_TOPIC_LEN {
            return Err(MqttError::InvalidConfig("lwt topic too long"));
        }
    }
    let keep_alive = config.keep_alive_secs.unwrap_or(DEFAULT_KEEP_ALIVE_SECS);
    if keep_alive == 0 || keep_alive > u64::from(u16::MAX) {
        return Err(MqttError::InvalidConfig(
            "keep_alive_secs must be 1..=65535",
        ));
    }
    Ok(())
}

/// Connection state as a `u8` for the runner's atomic, and back.
pub(crate) fn state_to_u8(state: MqttConnectionState) -> u8 {
    match state {
        MqttConnectionState::Connecting => 0,
        MqttConnectionState::Connected => 1,
        MqttConnectionState::Disconnected => 2,
        MqttConnectionState::ShuttingDown => 3,
    }
}

pub(crate) fn state_from_u8(raw: u8) -> MqttConnectionState {
    match raw {
        1 => MqttConnectionState::Connected,
        2 => MqttConnectionState::Disconnected,
        3 => MqttConnectionState::ShuttingDown,
        _ => MqttConnectionState::Connecting,
    }
}

#[cfg(all(
    feature = "embassy",
    any(feature = "esp32c3", feature = "esp32c6", feature = "esp32s3")
))]
mod runner;
#[cfg(all(
    feature = "embassy",
    any(feature = "esp32c3", feature = "esp32c6", feature = "esp32s3")
))]
pub use runner::{MqttBuilder, MqttHandle, MqttRunner};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broker_host_must_be_an_ipv4_literal() {
        assert_eq!(
            parse_broker_ipv4("192.168.31.5"),
            Ok(Ipv4Addr::new(192, 168, 31, 5))
        );
        assert_eq!(
            parse_broker_ipv4(" 10.0.0.1 "),
            Ok(Ipv4Addr::new(10, 0, 0, 1))
        );
        assert!(matches!(
            parse_broker_ipv4("broker.local"),
            Err(MqttError::InvalidConfig(_))
        ));
        assert!(matches!(
            parse_broker_ipv4("::1"),
            Err(MqttError::InvalidConfig(_))
        ));
    }

    #[test]
    fn validate_config_accepts_a_minimal_config() {
        let config = MqttConfig::new("10.0.0.2", 1883, "knob-01");
        assert_eq!(validate_config(&config), Ok(()));
    }

    #[test]
    fn validate_config_rejects_bad_fields() {
        assert!(validate_config(&MqttConfig::new("broker", 1883, "id")).is_err());
        assert!(validate_config(&MqttConfig::new("10.0.0.2", 0, "id")).is_err());
        assert!(validate_config(&MqttConfig::new("10.0.0.2", 1883, "")).is_err());
        let zero_keep_alive = MqttConfig::new("10.0.0.2", 1883, "id").with_keep_alive(0);
        assert_eq!(
            validate_config(&zero_keep_alive),
            Err(MqttError::InvalidConfig(
                "keep_alive_secs must be 1..=65535"
            ))
        );
        let bad_lwt = MqttConfig::new("10.0.0.2", 1883, "id").with_lwt(LwtConfig::new(
            "status/#",
            b"offline",
            QoS::AtLeastOnce,
            true,
        ));
        assert!(validate_config(&bad_lwt).is_err());
    }

    #[test]
    fn publish_request_enforces_caps_and_topic_rules() {
        let ok = PublishRequest::new("knob/pos", b"42", QoS::AtMostOnce, false).unwrap();
        assert_eq!(ok.topic.as_str(), "knob/pos");
        assert_eq!(ok.payload.as_slice(), b"42");

        assert!(matches!(
            PublishRequest::new("knob/#", b"x", QoS::AtMostOnce, false),
            Err(MqttError::InvalidTopic(_))
        ));
        let long_topic = [b'a'; MAX_TOPIC_LEN + 1];
        let long_topic = core::str::from_utf8(&long_topic).unwrap();
        assert_eq!(
            PublishRequest::new(long_topic, b"x", QoS::AtMostOnce, false).map(|_| ()),
            Err(MqttError::TopicTooLong)
        );
        let big = [0u8; MAX_PAYLOAD_LEN + 1];
        assert_eq!(
            PublishRequest::new("t", &big, QoS::AtMostOnce, false).map(|_| ()),
            Err(MqttError::PayloadTooLarge)
        );
    }

    #[test]
    fn inbound_message_drops_oversized_input() {
        assert!(InboundMessage::copy_from("a/b", b"1", true, QoS::AtMostOnce).is_some());
        let big = [0u8; MAX_PAYLOAD_LEN + 1];
        assert!(InboundMessage::copy_from("a/b", &big, false, QoS::AtMostOnce).is_none());
    }

    #[test]
    fn connection_state_round_trips_through_u8() {
        for state in [
            MqttConnectionState::Connecting,
            MqttConnectionState::Connected,
            MqttConnectionState::Disconnected,
            MqttConnectionState::ShuttingDown,
        ] {
            assert_eq!(state_from_u8(state_to_u8(state)), state);
        }
    }

    #[test]
    fn try_publish_error_alias_matches_juggler_shape() {
        let e: TryPublishError = TryPublishError::Other(MqttError::PayloadTooLarge);
        assert!(matches!(
            e,
            TryPublishError::Other(MqttError::PayloadTooLarge)
        ));
        let w: TryPublishError = TryPublishError::WouldBlock;
        assert!(matches!(w, TryPublishError::WouldBlock));
    }
}
