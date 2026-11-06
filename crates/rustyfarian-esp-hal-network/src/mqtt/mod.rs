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
//! * Auto-reconnect at [`MqttConfig::reconnect_timeout_ms`] (default 10 s) after a
//!   session error. A network watch polls the stack every 500 ms and ends the session
//!   as soon as the Wi-Fi link or the IP configuration is gone; the runner then waits
//!   for the network instead of the reconnect pause (the ESP-IDF tier retries on the
//!   pause regardless of link state). While the network is up, a transport timeout of
//!   1.5 × keep-alive (45 s by default) covers a silent broker whose TCP reset can be
//!   routed, and a runner-side stall deadline of socket timeout + keep-alive (75 s by
//!   default) covers a dead LAN host whose reset cannot; either way a silent broker
//!   ends the session within roughly 2.5 × keep-alive of its last packet.
//! * [`MqttHandle::try_publish`] never blocks and reports
//!   [`TryPublishError::WouldBlock`] while the session is not `Connected` or the
//!   queue is full. `Ok(())` means accepted into the [`PUBLISH_QUEUE_DEPTH`]-slot
//!   runner queue; queue entries survive a reconnect and go out in order, messages
//!   already handed to the session but not yet acknowledged by the broker do not (one
//!   at most when the network drops, more if the broker itself dies) — QoS 1 is
//!   at-least-once within one broker session only. [`MqttHandle::publish_with`]
//!   skips the state check, so it can enqueue while disconnected; those messages go
//!   out after the next connect.
//!
//! While Wi-Fi is suspended (`WifiLink::suspend`) the client has no call of its
//! own; it sends no DISCONNECT, so the broker keeps the will armed:
//!
//! * **State:** the network watch sees the link down within 500 ms and ends the
//!   session; [`MqttHandle::state`] leaves `Connected` and the runner waits for
//!   the network, so there is no retry churn while suspended.
//! * **Publishing:** [`MqttHandle::try_publish`] returns `WouldBlock`;
//!   [`MqttHandle::publish_with`] still enqueues for the next session.
//! * **Broker view:** the will goes out after 1.5 × keep-alive; a shorter suspend
//!   ends in a client-id takeover on reconnect (will, then the startup message).
//! * **After `resume`:** the runner connects as soon as the link and DHCP are up,
//!   republishes the startup message and bumps [`MqttHandle::connect_count`].
//!
//! Subscriptions: [`MqttBuilder::subscribe`] registers the ones known at build time;
//! [`MqttHandle::subscribe`] adds more while the client runs, up to
//! [`MAX_RUNTIME_SUBSCRIPTIONS`], with the signature of the ESP-IDF handle.
//! The runner copies the filter (at most [`MAX_TOPIC_LEN`] bytes), subscribes on the live
//! session, and re-issues runtime topics after every reconnect that needs
//! subscriptions, after the builder ones, in registration order.
//!
//! The broker host is an IPv4 literal (connected directly, no DNS query) or a DNS name
//! (resolved with the embassy-net DNS client on every connect, bounded by the connection
//! timeout; a failed lookup is a connect failure that goes through the reconnect back-off).
//! IPv6 is not supported.
//!
//! Limitations in v1: publishes are
//! QoS 0/1, and payloads are capped at [`MAX_PAYLOAD_LEN`] bytes. All buffers are `static` — nothing is heap-allocated.
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

use juggler::mqtt::{
    admit_subscription, classify_broker_host, BrokerHost, SubscriptionAdmission,
    SubscriptionTracker, TrackedRequest,
};
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
/// Subscriptions [`MqttHandle::subscribe`] accepts, on top of the builder's.
pub const MAX_RUNTIME_SUBSCRIPTIONS: usize = 8;
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
    /// [`MAX_RUNTIME_SUBSCRIPTIONS`] runtime subscriptions are already registered.
    RuntimeSubscriptionsFull,
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
            Self::RuntimeSubscriptionsFull => write!(
                f,
                "more than {} runtime subscriptions",
                MAX_RUNTIME_SUBSCRIPTIONS
            ),
        }
    }
}

impl core::error::Error for MqttError {}

/// Error returned by the `try_publish*` family.
///
/// `WouldBlock` means "retry next tick": the session is not connected or the
/// publish queue is full. `Other` carries a [`MqttError`] that retrying will not fix.
/// A successful call only means the message entered the runner queue; see the module docs for the delivery rule.
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

impl juggler::mqtt::MqttMessage for InboundMessage {
    fn topic(&self) -> &str {
        self.topic.as_str()
    }

    fn payload(&self) -> &[u8] {
        self.payload.as_slice()
    }

    fn retained(&self) -> bool {
        self.retained
    }

    fn qos(&self) -> QoS {
        self.qos
    }
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

/// Subscriptions added through [`MqttHandle::subscribe`]; the runner reads them.
///
/// Entries are never removed, so an index stays valid.
/// This type stores the topics; which entries the current session still owes is
/// decided by `juggler::mqtt::SubscriptionTracker` (ADR 023).
#[derive(Debug, Default)]
pub(crate) struct RuntimeSubscriptions {
    topics: heapless::Vec<heapless::String<MAX_TOPIC_LEN>, MAX_RUNTIME_SUBSCRIPTIONS>,
    tracker: SubscriptionTracker<MAX_RUNTIME_SUBSCRIPTIONS>,
}

impl RuntimeSubscriptions {
    pub(crate) const fn new() -> Self {
        Self {
            topics: heapless::Vec::new(),
            tracker: SubscriptionTracker::new(),
        }
    }

    /// Validates and admits one request (`juggler::mqtt::admit_subscription`).
    ///
    /// `Ok(true)` means an entry is pending and the runner must be woken.
    pub(crate) fn admit(
        &mut self,
        fixed: &[(&'static str, QoS)],
        topic: &str,
        qos: QoS,
    ) -> Result<bool, MqttError> {
        validate_subscribe_filter(topic).map_err(MqttError::InvalidTopic)?;
        let filter = heapless::String::<MAX_TOPIC_LEN>::try_from(topic)
            .map_err(|_| MqttError::TopicTooLong)?;
        let qos = clamp_qos(qos);
        let admission = admit_subscription(
            fixed.iter().copied(),
            self.topics
                .iter()
                .enumerate()
                .map(|(i, t)| (t.as_str(), self.tracker.qos(i).unwrap_or(qos))),
            topic,
            qos,
            MAX_RUNTIME_SUBSCRIPTIONS,
        );
        let added = admission == SubscriptionAdmission::Added;
        match self.tracker.apply(admission, qos) {
            TrackedRequest::Unchanged => Ok(false),
            TrackedRequest::Full => Err(MqttError::RuntimeSubscriptionsFull),
            TrackedRequest::Stored | TrackedRequest::SendNow => {
                if added && self.topics.push(filter).is_err() {
                    return Err(MqttError::RuntimeSubscriptionsFull);
                }
                Ok(true)
            }
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.topics.len()
    }

    /// Copy of entry `index`, for use across an await.
    pub(crate) fn get(&self, index: usize) -> Option<(heapless::String<MAX_TOPIC_LEN>, QoS)> {
        let topic = self.topics.get(index)?;
        Some((topic.clone(), self.tracker.qos(index)?))
    }

    /// First entry at or after `start` the session still has to receive.
    pub(crate) fn next_pending_from(
        &self,
        start: usize,
    ) -> Option<(usize, heapless::String<MAX_TOPIC_LEN>, QoS)> {
        let (index, qos) = self.tracker.next_pending_from(start)?;
        Some((index, self.topics.get(index)?.clone(), qos))
    }

    /// First entry the session still has to receive (host tests only).
    #[cfg(test)]
    pub(crate) fn next_pending(&self) -> Option<(usize, heapless::String<MAX_TOPIC_LEN>, QoS)> {
        self.next_pending_from(0)
    }

    /// A connect work starts; a session that needs every subscription is owed every entry.
    pub(crate) fn begin_connect(&mut self, resubscribe_all: bool) {
        self.tracker.begin_connect(resubscribe_all);
    }

    /// Records that `qos` was sent for entry `index`; a QoS changed since stays pending.
    pub(crate) fn mark_sent(&mut self, index: usize, qos: QoS) {
        self.tracker.mark_sent(index, qos);
    }
}

/// QoS 2 is not offered on this tier; requests are downgraded to QoS 1.
pub(crate) fn clamp_qos(qos: QoS) -> QoS {
    match qos {
        QoS::ExactlyOnce => QoS::AtLeastOnce,
        other => other,
    }
}

/// Classifies the broker host as an IPv4 literal (no DNS query) or a DNS name
/// (resolved by the runner on every connect); the rule is shared in `juggler`.
pub(crate) fn parse_broker_host(host: &str) -> Result<BrokerHost<'_>, MqttError> {
    classify_broker_host(host).map_err(MqttError::InvalidConfig)
}

/// Build-time validation shared by the runner and the host tests.
pub(crate) fn validate_config(config: &MqttConfig<'_>) -> Result<(), MqttError> {
    parse_broker_host(config.host)?;
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

/// Transport timeout handed to `TcpSocket::set_timeout`, derived from the keep-alive.
///
/// 1.5 × keep-alive mirrors the broker's LWT rule, floored at keep-alive + 5 s so a
/// short keep-alive still leaves `minimq` its own 5 s PINGRESP tolerance.
/// smoltcp counts it from the peer's last packet, or from the first send into an
/// empty TX buffer; TCP ACKs and PINGRESPs refresh it, so a live path never trips it.
pub(crate) const fn socket_timeout_ms(keep_alive_secs: u64) -> u64 {
    let scaled = keep_alive_secs.saturating_mul(1_500);
    let floor = keep_alive_secs.saturating_mul(1_000).saturating_add(5_000);
    if scaled > floor {
        scaled
    } else {
        floor
    }
}

/// Runner-side deadline for one session await (`poll`, `publish`, `subscribe`).
///
/// The socket timeout alone cannot end a session whose TCP RST has no route (a dead
/// LAN host whose ARP entry expired): smoltcp keeps the socket tuple until the RST is
/// emitted, and `embassy-net`'s `flush()` pends on it, so `minimq` never returns.
/// A healthy session returns from `poll()` at least every keep-alive (a queued
/// PINGREQ counts as progress), so socket timeout + keep-alive never fires on a live path.
///
/// This assumes the broker does not assign a CONNACK Server Keep Alive above the
/// configured keep-alive (or 0, which disables the ping): `minimq` adopts that value
/// and the runner cannot read it, so such a broker causes periodic `transport stalled`
/// reconnects with an LWT on a working link; mosquitto only ever lowers it.
pub(crate) const fn stall_timeout_ms(keep_alive_secs: u64) -> u64 {
    socket_timeout_ms(keep_alive_secs).saturating_add(keep_alive_secs.saturating_mul(1_000))
}

#[cfg(all(
    feature = "embassy",
    any(
        feature = "esp32c3",
        feature = "esp32c6",
        feature = "esp32s3",
        feature = "esp32"
    )
))]
mod runner;
#[cfg(all(
    feature = "embassy",
    any(
        feature = "esp32c3",
        feature = "esp32c6",
        feature = "esp32s3",
        feature = "esp32"
    )
))]
pub use runner::{MqttBuilder, MqttHandle, MqttRunner};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broker_host_is_a_literal_or_a_name() {
        assert_eq!(
            parse_broker_host("192.168.31.5"),
            Ok(BrokerHost::Ipv4(core::net::Ipv4Addr::new(192, 168, 31, 5)))
        );
        assert_eq!(
            parse_broker_host("broker.local"),
            Ok(BrokerHost::Name("broker.local"))
        );
        assert!(matches!(
            parse_broker_host("::1"),
            Err(MqttError::InvalidConfig(_))
        ));
        assert!(matches!(
            parse_broker_host("999.1.1.1"),
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
        assert!(validate_config(&MqttConfig::new("", 1883, "id")).is_err());
        assert_eq!(
            validate_config(&MqttConfig::new("broker", 1883, "id")),
            Ok(())
        );
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

    fn filter_of_len(len: usize) -> heapless::String<{ MAX_TOPIC_LEN + 1 }> {
        let mut s = heapless::String::new();
        for _ in 0..len {
            s.push('a').unwrap();
        }
        s
    }

    fn numbered(i: usize) -> heapless::String<8> {
        let mut s = heapless::String::new();
        core::fmt::write(&mut s, format_args!("t/{i}")).unwrap();
        s
    }

    const Q0: QoS = QoS::AtMostOnce;
    const Q1: QoS = QoS::AtLeastOnce;

    #[test]
    fn runtime_subscribe_copies_the_filter_and_marks_it_pending() {
        let mut subs = RuntimeSubscriptions::new();
        assert_eq!(subs.admit(&[], "knob/cmd/#", Q1), Ok(true));
        let (index, topic, qos) = subs.next_pending().unwrap();
        assert_eq!((index, topic.as_str(), qos), (0, "knob/cmd/#", Q1));
        subs.mark_sent(index, qos);
        assert!(subs.next_pending().is_none());
        assert_eq!(subs.len(), 1);
    }

    #[test]
    fn runtime_subscribe_refuses_long_and_invalid_filters() {
        let mut subs = RuntimeSubscriptions::new();
        let long = filter_of_len(MAX_TOPIC_LEN + 1);
        assert_eq!(subs.admit(&[], &long, Q1), Err(MqttError::TopicTooLong));
        assert!(matches!(
            subs.admit(&[], "a/#/b", Q1),
            Err(MqttError::InvalidTopic(_))
        ));
        assert!(matches!(
            subs.admit(&[], "", Q1),
            Err(MqttError::InvalidTopic(_))
        ));
        let exact = filter_of_len(MAX_TOPIC_LEN);
        assert_eq!(subs.admit(&[], &exact, Q1), Ok(true));
        assert_eq!(subs.len(), 1);
    }

    #[test]
    fn runtime_subscribe_is_bounded_and_keeps_registration_order() {
        let mut subs = RuntimeSubscriptions::new();
        for i in 0..MAX_RUNTIME_SUBSCRIPTIONS {
            assert_eq!(subs.admit(&[], &numbered(i), Q1), Ok(true));
        }
        assert_eq!(
            subs.admit(&[], "t/extra", Q1),
            Err(MqttError::RuntimeSubscriptionsFull)
        );
        for i in 0..MAX_RUNTIME_SUBSCRIPTIONS {
            assert_eq!(subs.get(i).unwrap().0.as_str(), numbered(i).as_str());
        }
        assert_eq!(subs.admit(&[], "t/3", Q1), Ok(false));
    }

    #[test]
    fn runtime_subscribe_dedupes_against_the_builder_list_and_clamps_qos() {
        let fixed = [("cmd/#", Q1)];
        let mut subs = RuntimeSubscriptions::new();
        assert_eq!(subs.admit(&fixed, "cmd/#", Q1), Ok(false));
        assert_eq!(subs.admit(&fixed, "cmd/#", Q0), Ok(true));
        assert_eq!(subs.admit(&[], "x", QoS::ExactlyOnce), Ok(true));
        assert_eq!(subs.get(1).unwrap().1, Q1);
    }

    #[test]
    fn a_changed_qos_is_pending_again_and_a_stale_ack_does_not_clear_it() {
        let mut subs = RuntimeSubscriptions::new();
        subs.admit(&[], "a", Q0).unwrap();
        subs.mark_sent(0, Q0);
        assert_eq!(subs.admit(&[], "a", Q1), Ok(true));
        subs.mark_sent(0, Q0);
        assert_eq!(subs.next_pending().map(|(i, _, q)| (i, q)), Some((0, Q1)));
        subs.mark_sent(0, Q1);
        assert!(subs.next_pending().is_none());
    }

    #[test]
    fn a_failed_live_send_is_still_owed_after_a_kept_session_reconnect() {
        let mut subs = RuntimeSubscriptions::new();
        subs.begin_connect(true);
        assert_eq!(subs.admit(&[], "a", Q1), Ok(true));
        assert_eq!(subs.next_pending().map(|(i, _, q)| (i, q)), Some((0, Q1)));
        subs.begin_connect(false);
        let (index, topic, qos) = subs.next_pending().unwrap();
        assert_eq!((index, topic.as_str(), qos), (0, "a", Q1));
        subs.mark_sent(index, qos);
        assert!(subs.next_pending().is_none());
    }

    #[test]
    fn inbound_message_drops_oversized_input() {
        assert!(InboundMessage::copy_from("a/b", b"1", true, QoS::AtMostOnce).is_some());
        let big = [0u8; MAX_PAYLOAD_LEN + 1];
        assert!(InboundMessage::copy_from("a/b", &big, false, QoS::AtMostOnce).is_none());
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

    #[test]
    fn socket_timeout_is_one_and_a_half_keep_alive() {
        assert_eq!(socket_timeout_ms(30), 45_000);
        assert_eq!(socket_timeout_ms(60), 90_000);
        assert_eq!(socket_timeout_ms(DEFAULT_KEEP_ALIVE_SECS), 45_000);
    }

    #[test]
    fn socket_timeout_floors_at_keep_alive_plus_ping_tolerance() {
        assert_eq!(socket_timeout_ms(1), 6_000);
        assert_eq!(socket_timeout_ms(9), 14_000);
        assert_eq!(socket_timeout_ms(10), 15_000);
        assert_eq!(socket_timeout_ms(u64::MAX), u64::MAX);
    }

    #[test]
    fn stall_timeout_is_socket_timeout_plus_keep_alive() {
        assert_eq!(stall_timeout_ms(30), 75_000);
        assert_eq!(stall_timeout_ms(60), 150_000);
        assert_eq!(stall_timeout_ms(1), 7_000);
        assert_eq!(stall_timeout_ms(DEFAULT_KEEP_ALIVE_SECS), 75_000);
        assert_eq!(stall_timeout_ms(u64::MAX), u64::MAX);
    }
}
