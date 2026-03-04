//! Pure MQTT primitives — no I/O, no ESP-IDF.

/// Returns the number of 100 ms poll iterations needed to cover `timeout_ms`.
///
/// Uses ceiling division so that a timeout that is not an exact multiple of
/// 100 ms is always fully respected — e.g. 5050 ms yields 51 iterations
/// (5100 ms of polling) rather than 50 (5000 ms).
pub fn connection_wait_iterations(timeout_ms: u64) -> u64 {
    timeout_ms.div_ceil(100)
}

// ── Broker URL ───────────────────────────────────────────────────────────────

/// Formats the `mqtt://` URL used to connect to the broker.
///
/// This is the single place where the URL scheme is chosen; a future TLS
/// variant would change the prefix here.
pub fn format_broker_url(host: &str, port: u16) -> String {
    format!("mqtt://{}:{}", host, port)
}

// ── Validation ───────────────────────────────────────────────────────────────

/// Maximum client ID length for maximum MQTT 3.1.1 broker compatibility.
///
/// Section 3.1.3.1 of the MQTT 3.1.1 specification caps client IDs at
/// 23 bytes for brokers that must support all conformant clients.
pub const CLIENT_ID_MAX_LEN: usize = 23;

/// Returns `Ok(())` if `client_id` is a valid MQTT client identifier.
///
/// Rejects empty strings and strings longer than [`CLIENT_ID_MAX_LEN`] bytes.
pub fn validate_client_id(client_id: &str) -> Result<(), &'static str> {
    if client_id.is_empty() {
        return Err("MQTT client ID must not be empty");
    }
    if client_id.len() > CLIENT_ID_MAX_LEN {
        return Err("MQTT client ID exceeds the 23-byte MQTT 3.1.1 maximum");
    }
    Ok(())
}

/// Returns `Ok(())` if `topic` is a valid MQTT topic string.
///
/// Rejects empty strings, strings longer than 65535 UTF-8 bytes (the MQTT
/// maximum), and strings containing the NUL character (`\0`).
pub fn validate_topic(topic: &str) -> Result<(), &'static str> {
    if topic.is_empty() {
        return Err("MQTT topic must not be empty");
    }
    if topic.len() > 65535 {
        return Err("MQTT topic exceeds the 65535-byte maximum");
    }
    if topic.contains('\0') {
        return Err("MQTT topic must not contain the NUL character");
    }
    Ok(())
}

/// Returns `Ok(())` if `host` is a non-empty broker hostname or IP address.
pub fn validate_broker_host(host: &str) -> Result<(), &'static str> {
    if host.is_empty() {
        return Err("MQTT broker host must not be empty");
    }
    Ok(())
}

/// Returns `Ok(())` if `port` is a valid TCP port number (1–65535).
pub fn validate_broker_port(port: u16) -> Result<(), &'static str> {
    if port == 0 {
        return Err("MQTT broker port must not be 0");
    }
    Ok(())
}

// ── Connection state machine ─────────────────────────────────────────────────

/// Observable connection states for an MQTT client session.
///
/// The state machine governs when lifecycle callbacks (`on_connect`,
/// `on_disconnect`) are invoked.
/// Invalid transitions return `None` from [`next_state`], meaning the event
/// is silently ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MqttConnectionState {
    /// `build()` was called; no connection has completed yet.
    Connecting,
    /// The broker acknowledged the CONNECT packet.
    Connected,
    /// The connection was lost; the ESP-IDF layer will attempt to reconnect.
    Disconnected,
    /// Shutdown was requested; no further reconnections will be attempted.
    ShuttingDown,
}

/// Events that drive the MQTT connection state machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MqttEvent {
    /// The broker sent a CONNACK (maps to `EventPayload::Connected`).
    Connected,
    /// The connection was lost (maps to `EventPayload::Disconnected`).
    Disconnected,
    /// The application requested a clean shutdown.
    ShutdownRequested,
}

/// Returns the next [`MqttConnectionState`] given the current state and an
/// incoming event, or `None` if the transition is invalid.
///
/// `None` encodes important safety invariants:
/// - `Connected → Connected` returns `None`: `on_connect` is never fired
///   while already connected.
/// - `Connecting → Disconnected` returns `None`: `on_disconnect` is never
///   fired before the first successful connection.
/// - Any event from `ShuttingDown` returns `None`: no callbacks fire after
///   shutdown is initiated.
pub fn next_state(current: MqttConnectionState, event: MqttEvent) -> Option<MqttConnectionState> {
    use MqttConnectionState as S;
    use MqttEvent as E;
    match (current, event) {
        (S::Connecting, E::Connected) => Some(S::Connected),
        (S::Connecting, E::Disconnected) => None,
        (S::Connecting, E::ShutdownRequested) => Some(S::ShuttingDown),
        (S::Connected, E::Disconnected) => Some(S::Disconnected),
        (S::Connected, E::Connected) => None,
        (S::Connected, E::ShutdownRequested) => Some(S::ShuttingDown),
        (S::Disconnected, E::Connected) => Some(S::Connected),
        (S::Disconnected, E::Disconnected) => None,
        (S::Disconnected, E::ShutdownRequested) => Some(S::ShuttingDown),
        (S::ShuttingDown, _) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        connection_wait_iterations, format_broker_url, next_state, validate_broker_host,
        validate_broker_port, validate_client_id, validate_topic, MqttConnectionState, MqttEvent,
        CLIENT_ID_MAX_LEN,
    };

    // ── connection_wait_iterations ───────────────────────────────────────────

    #[test]
    fn zero_timeout_yields_zero_iterations() {
        assert_eq!(connection_wait_iterations(0), 0);
    }

    #[test]
    fn exact_multiple_is_not_rounded_up() {
        assert_eq!(connection_wait_iterations(100), 1);
        assert_eq!(connection_wait_iterations(5000), 50);
    }

    #[test]
    fn non_multiple_is_rounded_up() {
        // The edge case from the review: 5050 ms must not be truncated to 50
        assert_eq!(connection_wait_iterations(5050), 51);
        assert_eq!(connection_wait_iterations(5001), 51);
        assert_eq!(connection_wait_iterations(4999), 50);
    }

    #[test]
    fn sub_100ms_timeout_yields_one_iteration() {
        assert_eq!(connection_wait_iterations(1), 1);
        assert_eq!(connection_wait_iterations(99), 1);
    }

    // ── format_broker_url ───────────────────────────────────────────────────

    #[test]
    fn broker_url_ip_and_standard_port() {
        assert_eq!(
            format_broker_url("192.168.1.100", 1883),
            "mqtt://192.168.1.100:1883"
        );
    }

    #[test]
    fn broker_url_hostname_and_tls_port() {
        assert_eq!(
            format_broker_url("broker.example.com", 8883),
            "mqtt://broker.example.com:8883"
        );
    }

    // ── validate_client_id ──────────────────────────────────────────────────

    #[test]
    fn empty_client_id_is_rejected() {
        assert!(validate_client_id("").is_err());
    }

    #[test]
    fn single_char_client_id_is_accepted() {
        assert!(validate_client_id("a").is_ok());
    }

    #[test]
    fn client_id_at_max_len_is_accepted() {
        let id = "x".repeat(CLIENT_ID_MAX_LEN);
        assert!(validate_client_id(&id).is_ok());
    }

    #[test]
    fn client_id_over_max_len_is_rejected() {
        let id = "x".repeat(CLIENT_ID_MAX_LEN + 1);
        assert!(validate_client_id(&id).is_err());
    }

    // ── validate_topic ───────────────────────────────────────────────────────

    #[test]
    fn empty_topic_is_rejected() {
        assert!(validate_topic("").is_err());
    }

    #[test]
    fn typical_topic_is_accepted() {
        assert!(validate_topic("sensors/temperature").is_ok());
    }

    #[test]
    fn topic_with_nul_is_rejected() {
        assert!(validate_topic("topic\0name").is_err());
    }

    #[test]
    fn topic_at_max_len_is_accepted() {
        let t = "t".repeat(65535);
        assert!(validate_topic(&t).is_ok());
    }

    #[test]
    fn topic_over_max_len_is_rejected() {
        let t = "t".repeat(65536);
        assert!(validate_topic(&t).is_err());
    }

    // ── validate_broker_host ─────────────────────────────────────────────────

    #[test]
    fn empty_host_is_rejected() {
        assert!(validate_broker_host("").is_err());
    }

    #[test]
    fn ip_address_host_is_accepted() {
        assert!(validate_broker_host("192.168.1.1").is_ok());
    }

    #[test]
    fn hostname_is_accepted() {
        assert!(validate_broker_host("broker.example.com").is_ok());
    }

    // ── validate_broker_port ─────────────────────────────────────────────────

    #[test]
    fn port_zero_is_rejected() {
        assert!(validate_broker_port(0).is_err());
    }

    #[test]
    fn mqtt_standard_port_is_accepted() {
        assert!(validate_broker_port(1883).is_ok());
    }

    #[test]
    fn mqtt_tls_port_is_accepted() {
        assert!(validate_broker_port(8883).is_ok());
    }

    #[test]
    fn max_port_is_accepted() {
        assert!(validate_broker_port(u16::MAX).is_ok());
    }

    // ── next_state ───────────────────────────────────────────────────────────

    use MqttConnectionState as S;
    use MqttEvent as E;

    fn assert_transition(
        current: MqttConnectionState,
        event: MqttEvent,
        expected: Option<MqttConnectionState>,
    ) {
        assert_eq!(
            next_state(current, event),
            expected,
            "unexpected transition: {current:?} + {event:?}"
        );
    }

    #[test]
    fn connecting_on_connected_transitions_to_connected() {
        assert_transition(S::Connecting, E::Connected, Some(S::Connected));
    }

    #[test]
    fn connecting_on_disconnected_is_ignored() {
        assert_transition(S::Connecting, E::Disconnected, None);
    }

    #[test]
    fn connecting_on_shutdown_transitions_to_shutting_down() {
        assert_transition(S::Connecting, E::ShutdownRequested, Some(S::ShuttingDown));
    }

    #[test]
    fn connected_on_disconnected_transitions_to_disconnected() {
        assert_transition(S::Connected, E::Disconnected, Some(S::Disconnected));
    }

    #[test]
    fn connected_on_connected_is_ignored() {
        assert_transition(S::Connected, E::Connected, None);
    }

    #[test]
    fn connected_on_shutdown_transitions_to_shutting_down() {
        assert_transition(S::Connected, E::ShutdownRequested, Some(S::ShuttingDown));
    }

    #[test]
    fn disconnected_on_connected_transitions_to_connected() {
        assert_transition(S::Disconnected, E::Connected, Some(S::Connected));
    }

    #[test]
    fn disconnected_on_disconnected_is_ignored() {
        assert_transition(S::Disconnected, E::Disconnected, None);
    }

    #[test]
    fn disconnected_on_shutdown_transitions_to_shutting_down() {
        assert_transition(S::Disconnected, E::ShutdownRequested, Some(S::ShuttingDown));
    }

    #[test]
    fn shutting_down_ignores_connected() {
        assert_transition(S::ShuttingDown, E::Connected, None);
    }

    #[test]
    fn shutting_down_ignores_disconnected() {
        assert_transition(S::ShuttingDown, E::Disconnected, None);
    }

    #[test]
    fn shutting_down_ignores_shutdown_requested() {
        assert_transition(S::ShuttingDown, E::ShutdownRequested, None);
    }
}
