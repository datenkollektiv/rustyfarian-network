//! MQTT client manager for ESP-IDF projects.
//!
//! Provides a persistent, auto-reconnecting MQTT client with lifecycle
//! callbacks, thread-safe publishing, and pure connection-state validation.
//!
//! # Quick start
//!
//! ```ignore
//! use rustyfarian_esp_idf_network::mqtt::{MqttBuilder, MqttConfig};
//! use esp_idf_svc::mqtt::client::QoS;
//!
//! let config = MqttConfig::new("192.168.1.100", 1883, "my-device");
//!
//! let handle = MqttBuilder::new(config)
//!     .subscribe("commands/#", QoS::AtLeastOnce)
//!     .with_startup_message()
//!     .on_disconnect(|| log::warn!("MQTT disconnected"))
//!     .on_message(|topic, data| log::info!("msg on {}: {:?}", topic, data))
//!     .build()?;
//!
//! handle.publish("status", "online")?;
//! ```
//!
//! Callbacks must not call any [`MqttHandle`] method — such a call fails fast
//! with [`PublishAckError::WrongThread`] instead of hanging. From
//! `on_connect`, use the `client` argument instead: `enqueue()` and
//! `subscribe()` are safe there, since the callback runs on a per-connect
//! helper thread that already holds the client mutex.
//!
//! ## Non-blocking publish
//!
//! For time-critical loops (e.g. ESP-NOW at 50 Hz), use [`MqttHandle::try_publish`]
//! to avoid blocking when the connect helper thread holds the client mutex
//! (running the startup publish, `on_connect`, or SUBSCRIBE enqueues) or
//! another publisher is waiting on esp-mqtt's `api_lock` during a connect
//! handshake. Messages are silently dropped on `WouldBlock` — buffer or count
//! misses at the application layer if lossless delivery matters:
//!
//! ```ignore
//! use rustyfarian_esp_idf_network::mqtt::TryPublishError;
//!
//! match handle.try_publish("sensors/temp", "22.5") {
//!     Ok(()) => log::info!("published"),
//!     Err(TryPublishError::WouldBlock) => { /* skip this tick, retry next */ }
//!     Err(TryPublishError::Other(e)) => log::warn!("publish failed: {}", e),
//! }
//! ```
//!
//! ## Acknowledged publish
//!
//! When a caller must know that a specific message was durably received — e.g.
//! clearing persistent state only after a status publish is confirmed — use
//! [`MqttHandle::publish_acked`]. It publishes at QoS 1 and blocks until the
//! broker's PUBACK arrives or a timeout elapses, returning a typed
//! [`PublishAckError`] that separates retry-eligible broker-timing outcomes from
//! local faults. It must **not** be called from any callback: from
//! `on_message`/`on_disconnect` it would deadlock the event-loop thread that
//! delivers the PUBACK, and from `on_connect` it would self-deadlock on the
//! client mutex the helper thread already holds; such misuse returns
//! [`PublishAckError::WrongThread`] rather than hanging.
//!
//! ```ignore
//! use rustyfarian_esp_idf_network::mqtt::PublishAckError;
//! use std::time::Duration;
//!
//! match handle.publish_acked("ota/status", b"rolled_back", true, Duration::from_secs(5)) {
//!     Ok(()) => { /* PUBACK received — safe to clear rollback state */ }
//!     Err(PublishAckError::Timeout | PublishAckError::Disconnected) => { /* keep state, retry */ }
//!     Err(e) => log::error!("publish_acked failed: {}", e),
//! }
//! ```
//!
//! ## Battery-optimized configuration
//!
//! On thermally constrained boards (e.g. ESP32-C3 Super Mini) where MQTT is
//! telemetry-only, increase the reconnect interval to reduce power draw when
//! the broker is offline:
//!
//! ```ignore
//! let config = MqttConfig::new("192.168.1.100", 1883, "sensor-01")
//!     .with_reconnect_timeout(60_000)  // retry every 60 s (default: 10 s)
//!     .with_keep_alive(120);           // keep-alive every 2 min
//! ```
//!
//! [`MqttManager`] is still available but deprecated — use [`MqttBuilder`] for
//! new code.

use anyhow::Context as _;
use pennant::PulseEffect;
use rgb::RGB8;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

// Re-export StatusLed and SimpleLed from pennant for convenience
pub use pennant::{SimpleLed, StatusLed};

use juggler::mqtt::{
    connection_wait_iterations, format_broker_url, in_callback, next_state, spawn_connect_thread,
    validate_broker_host, validate_broker_port, validate_client_id, validate_publish_topic,
    validate_subscribe_filter, AckOutcome, CallbackScope, ConnectionEpoch, MqttConnectionState,
    MqttEvent, PendingAcks, QoS as PureQoS, SubscribeClient,
};

/// Poll interval used while waiting for the MQTT broker connection to be confirmed.
///
/// Must stay consistent with the `poll_interval_ms` argument passed to
/// [`connection_wait_iterations`] — both express the same physical interval.
const POLL_INTERVAL_MS: u64 = 100;

/// Stack size for the `MqttManager` (legacy) event loop thread.
///
/// The default ESP-IDF pthread stack (3 KiB) is too small for
/// `EspLogger::should_log`, which walks a `BTreeMap` and overflows the stack.
/// 8 KiB provides sufficient headroom for the event loop and message callbacks.
const EVENT_LOOP_STACK_SIZE: usize = 8192;

/// Stack size for the `MqttBuilder` event loop thread.
///
/// This thread only runs `on_message` and `on_disconnect` — `on_connect` and
/// the startup/subscribe traffic run on the per-connect helper thread instead
/// (see [`CONNECT_THREAD_STACK_SIZE`]).  12 KiB provides headroom for
/// `on_message`/`on_disconnect` callback frames on top of the base event loop
/// overhead.  Measure on hardware and increase if stack overflows are
/// observed with deeply nested callback logic.
const BUILDER_EVENT_LOOP_STACK_SIZE: usize = 12 * 1024;

/// Stack size for the per-connect helper thread.
///
/// Spawned once per connect/reconnect to run, off the event-loop thread: the
/// startup-message publish (if `with_startup_message()` was used), then
/// `on_connect`, then `subscribe()` for every registered topic — see
/// `docs/project-lore.md` "MQTT Event Loop" for why none of this can run on
/// the event-loop thread itself.  12 KiB accounts for `on_connect` callback
/// frames on top of the base helper-thread overhead.
const CONNECT_THREAD_STACK_SIZE: usize = 12 * 1024;

/// Default stack size for the ESP-IDF MQTT client task.
///
/// ESP-IDF defaults to 6144 bytes, which overflows during TLS negotiation
/// error paths — corrupting the heap and crashing `pthread_exit`.
/// 8 KiB provides sufficient headroom for TLS handshakes on ESP32.
/// Override via [`MqttConfig::with_task_stack_size`] if needed.
const DEFAULT_MQTT_TASK_STACK_SIZE: usize = 8192;

/// Cyan LED colour for the MQTT connecting state.
///
/// Re-exported from [`juggler::status_colors::MQTT_CONNECTING`].
pub const MQTT_CONNECTING_COLOR: (u8, u8, u8) = juggler::status_colors::MQTT_CONNECTING;

/// Red LED colour for connection timeout/failure.
///
/// Re-exported from [`juggler::status_colors::ERROR`].
pub const MQTT_ERROR_COLOR: (u8, u8, u8) = juggler::status_colors::ERROR;

/// Green channel brightness for the "connected" LED state.
///
/// Derived from [`juggler::status_colors::CONNECTED`].
pub const CONNECTED_LED_BRIGHTNESS: u8 = juggler::status_colors::CONNECTED.1;

/// LED update interval during the connection wait loop (milliseconds).
///
/// 50 ms matches Wi-Fi's `connect_with_led()` cadence for a consistent
/// pulse animation speed across both boot phases.
pub const LED_POLL_INTERVAL_MS: u64 = 50;

/// Number of red-pulse frames shown on connection timeout.
///
/// 20 frames at 50 ms = 1 second of error indication before returning.
pub const ERROR_PULSE_FRAMES: u32 = 20;

use esp_idf_svc::mqtt::client::{
    EspMqttClient, EventPayload, LwtConfiguration, MqttClientConfiguration, QoS,
};

/// Error returned by the `try_publish*` family when the publish cannot
/// complete without blocking.
#[derive(Debug)]
pub enum TryPublishError {
    /// The MQTT client mutex is held by the connect helper thread (running
    /// the startup publish, `on_connect`, or SUBSCRIBE enqueues) or by
    /// another publisher blocked on esp-mqtt's `api_lock` during a connect
    /// handshake. The caller should retry on the next tick.
    WouldBlock,
    /// Any other publish failure (invalid topic, enqueue error, poisoned mutex).
    Other(anyhow::Error),
}

impl std::fmt::Display for TryPublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WouldBlock => write!(f, "MQTT client busy (would block)"),
            Self::Other(e) => write!(f, "{:#}", e),
        }
    }
}

/// Error returned by [`MqttHandle::publish_acked`].
///
/// The variants separate **broker-timing outcomes** (retry-eligible) from
/// **local/programming faults**, so a caller — e.g. the OTA rollback-evidence
/// publish that must clear NVS only after a confirmed PUBACK — knows whether to
/// keep its state and retry, or to treat the failure as a bug to fix.
#[derive(Debug)]
pub enum PublishAckError {
    /// No PUBACK arrived within the timeout. A broker-timing outcome: the caller
    /// should keep its state (e.g. leave NVS untouched) and retry later.
    Timeout,
    /// The MQTT session dropped before the PUBACK. Also broker-timing and
    /// retry-eligible; never reported as a false `Ok`.
    Disconnected,
    /// Called from inside an MQTT callback — `on_connect`, `on_message`, or
    /// `on_disconnect` — where the call would deadlock: `on_message` and
    /// `on_disconnect` run on the event-loop thread, which would wait on
    /// esp-mqtt's recursive `api_lock` (held by the mqtt task delivering the
    /// event that is running the callback); `on_connect` runs on the
    /// per-connect helper thread while it holds the client mutex, so the call
    /// would self-deadlock on that mutex instead. Detected via a callback
    /// scope entered around each callback, not by comparing thread identity.
    /// Also returned, wrapped in [`anyhow::Error`], by [`MqttHandle::publish`],
    /// [`publish_with`], [`publish_retained`], the `try_publish*` family, and
    /// [`subscribe`](MqttHandle::subscribe). A programming error; fix the call
    /// site.
    ///
    /// [`publish_with`]: MqttHandle::publish_with
    /// [`publish_retained`]: MqttHandle::publish_retained
    WrongThread,
    /// A local, non-broker failure: topic validation rejected the publish, the
    /// underlying `enqueue` call failed, or the client mutex was poisoned. Says
    /// nothing about broker reachability and is not resolved by retrying.
    Other(anyhow::Error),
}

impl std::fmt::Display for PublishAckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout => write!(f, "publish not acknowledged within timeout"),
            Self::Disconnected => write!(f, "MQTT session dropped before acknowledgment"),
            Self::WrongThread => write!(
                f,
                "called from inside an MQTT callback (on_connect / on_message / \
                 on_disconnect) — would deadlock"
            ),
            Self::Other(e) => write!(f, "{:#}", e),
        }
    }
}

impl std::error::Error for PublishAckError {}

/// Last Will and Testament configuration.
///
/// The broker publishes this message on behalf of the client when it
/// disconnects unexpectedly (e.g. network loss, crash). A clean
/// `DISCONNECT` (via [`MqttManager::shutdown`]) suppresses the LWT.
#[derive(Debug, Clone)]
pub struct LwtConfig<'a> {
    topic: &'a str,
    payload: &'a [u8],
    qos: QoS,
    retain: bool,
}

impl<'a> LwtConfig<'a> {
    /// Creates a new LWT configuration.
    ///
    /// # Arguments
    ///
    /// * `topic` - Topic the broker publishes to on unexpected disconnect
    /// * `payload` - Message payload
    /// * `qos` - Quality of Service level
    /// * `retain` - Whether the broker retains the LWT message
    pub fn new(topic: &'a str, payload: &'a [u8], qos: QoS, retain: bool) -> Self {
        Self {
            topic,
            payload,
            qos,
            retain,
        }
    }
}

/// MQTT broker connection configuration.
///
/// Credentials (`username`, `password`) are redacted in the `Debug` output
/// (`Some("<redacted>")`) to prevent them from appearing in log files.
#[derive(Clone)]
pub struct MqttConfig<'a> {
    /// MQTT broker hostname or IP address
    pub host: &'a str,
    /// MQTT broker port (typically 1883 for unencrypted)
    pub port: u16,
    /// Unique client identifier
    pub client_id: &'a str,
    /// Keep-alive interval in seconds (default: 30)
    pub keep_alive_secs: Option<u64>,
    /// Connection timeout in milliseconds (default: 5000)
    pub connection_timeout_ms: Option<u64>,
    /// Interval between automatic reconnection attempts in milliseconds.
    ///
    /// When `None` (the default), the ESP-IDF default of 10 000 ms is used.
    /// Set via [`with_reconnect_timeout`](Self::with_reconnect_timeout).
    pub reconnect_timeout_ms: Option<u64>,
    /// Stack size for the ESP-IDF MQTT client task (default: 8192).
    ///
    /// Set via [`with_task_stack_size`](Self::with_task_stack_size).
    pub task_stack_size: usize,
    lwt: Option<LwtConfig<'a>>,
    username: Option<&'a str>,
    password: Option<&'a str>,
}

impl<'a> std::fmt::Debug for MqttConfig<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let redacted_username: Option<&'static str> = if self.username.is_some() {
            Some("<redacted>")
        } else {
            None
        };
        let redacted_password: Option<&'static str> = if self.password.is_some() {
            Some("<redacted>")
        } else {
            None
        };
        f.debug_struct("MqttConfig")
            .field("host", &self.host)
            .field("port", &self.port)
            .field("client_id", &self.client_id)
            .field("keep_alive_secs", &self.keep_alive_secs)
            .field("connection_timeout_ms", &self.connection_timeout_ms)
            .field("reconnect_timeout_ms", &self.reconnect_timeout_ms)
            .field("task_stack_size", &self.task_stack_size)
            .field("lwt", &self.lwt)
            .field("username", &redacted_username)
            .field("password", &redacted_password)
            .finish()
    }
}

impl<'a> MqttConfig<'a> {
    /// Creates a new configuration with the required fields.
    pub fn new(host: &'a str, port: u16, client_id: &'a str) -> Self {
        Self {
            host,
            port,
            client_id,
            keep_alive_secs: None,
            connection_timeout_ms: None,
            reconnect_timeout_ms: None,
            task_stack_size: DEFAULT_MQTT_TASK_STACK_SIZE,
            lwt: None,
            username: None,
            password: None,
        }
    }

    /// Sets the keep-alive interval.
    pub fn with_keep_alive(mut self, secs: u64) -> Self {
        self.keep_alive_secs = Some(secs);
        self
    }

    /// Sets the connection timeout.
    pub fn with_timeout(mut self, ms: u64) -> Self {
        self.connection_timeout_ms = Some(ms);
        self
    }

    /// Sets the interval between automatic reconnection attempts.
    ///
    /// When the broker is unreachable, the ESP-IDF MQTT client retries at
    /// this interval.  The default is 10 000 ms.  Battery-powered or
    /// thermally constrained devices may want 30 000–60 000 ms to reduce
    /// power draw during prolonged broker outages.
    ///
    /// This does not affect the initial connection wait controlled by
    /// [`with_timeout`](Self::with_timeout).
    ///
    /// ```ignore
    /// let config = MqttConfig::new("192.168.1.100", 1883, "sensor-01")
    ///     .with_reconnect_timeout(30_000); // retry every 30 s instead of 10 s
    /// ```
    pub fn with_reconnect_timeout(mut self, ms: u64) -> Self {
        self.reconnect_timeout_ms = Some(ms);
        self
    }

    /// Configures a Last Will and Testament message.
    ///
    /// The broker publishes this message when the client disconnects
    /// unexpectedly. A clean shutdown suppresses the LWT.
    pub fn with_lwt(mut self, lwt: LwtConfig<'a>) -> Self {
        self.lwt = Some(lwt);
        self
    }

    /// Sets MQTT broker authentication credentials.
    ///
    /// For brokers whose ACL keys off a username alone (no password), use
    /// [`with_username_only`](Self::with_username_only) instead — it omits the
    /// CONNECT packet's password field rather than transmitting an empty
    /// string, matching what those brokers expect.
    pub fn with_auth(mut self, username: &'a str, password: &'a str) -> Self {
        self.username = Some(username);
        self.password = Some(password);
        self
    }

    /// Sets a username with no password, for brokers that authorise by
    /// username alone.
    ///
    /// The CONNECT packet carries the username and *omits* the password field
    /// (rather than carrying an empty one). This is semantically distinct on
    /// the wire from [`with_auth(user, "")`](Self::with_auth) and is what
    /// broker-side username-only ACL implementations typically expect.
    pub fn with_username_only(mut self, username: &'a str) -> Self {
        self.username = Some(username);
        self.password = None;
        self
    }

    /// Overrides the ESP-IDF MQTT client task stack size.
    ///
    /// Defaults to 8192 bytes (8 KiB), which provides sufficient headroom
    /// for TLS handshakes on ESP32.
    /// Increase to 16384 (16 KiB) if stack overflows are observed during
    /// TLS negotiation on resource-constrained targets.
    pub fn with_task_stack_size(mut self, bytes: usize) -> Self {
        self.task_stack_size = bytes;
        self
    }
}

/// MQTT client manager with automatic connection and event handling.
///
/// The manager spawns a background thread to process MQTT events,
/// which is required for the ESP-IDF MQTT client to function.
///
/// When dropped, the manager signals the background thread to shut down.
/// Use [`publish`](Self::publish) or [`publish_with`](Self::publish_with)
/// for lifecycle messages instead of the deprecated startup/shutdown helpers.
pub struct MqttManager<'a, F>
where
    F: Fn(&str, &[u8]) + Send + 'static,
{
    client: EspMqttClient<'a>,
    client_id: String,
    shutdown: Arc<AtomicBool>,
    _phantom: std::marker::PhantomData<F>,
}

impl<'a, F> MqttManager<'a, F>
where
    F: Fn(&str, &[u8]) + Send + 'static,
{
    /// Creates a new MQTT manager and connects to the broker.
    ///
    /// # Arguments
    ///
    /// * `config` - Connection configuration
    /// * `incoming_topics` - Topics to subscribe to for incoming messages
    /// * `on_message` - Callback invoked with `(topic, payload)` when a message
    ///   is received on any subscribed topic
    ///
    /// # Returns
    ///
    /// A connected MQTT manager, or an error if the connection fails.
    ///
    /// # Deprecation
    ///
    /// This constructor does not expose reconnect lifecycle callbacks, making it
    /// impossible to re-subscribe after an automatic broker reconnect.
    /// Use [`MqttBuilder`] instead: it handles reconnection transparently and
    /// avoids the heap-corruption risk that existed in earlier versions of this
    /// method.
    #[deprecated(
        since = "0.2.0",
        note = "use MqttBuilder (via MqttBuilder::new) instead; \
                MqttManager::new does not re-subscribe after auto-reconnect"
    )]
    pub fn new(
        config: MqttConfig<'_>,
        incoming_topics: &[&str],
        on_message: F,
    ) -> anyhow::Result<Self> {
        let topics: Vec<String> = incoming_topics.iter().map(|t| t.to_string()).collect();
        let client_id = config.client_id.to_string();

        log::info!(
            "Connecting to MQTT broker at {}:{}",
            config.host,
            config.port
        );

        let lwt_cfg = config.lwt.as_ref().map(|lwt| LwtConfiguration {
            topic: lwt.topic,
            payload: lwt.payload,
            qos: lwt.qos,
            retain: lwt.retain,
        });

        let mqtt_cfg = MqttClientConfiguration {
            client_id: Some(config.client_id),
            keep_alive_interval: Some(Duration::from_secs(config.keep_alive_secs.unwrap_or(30))),
            reconnect_timeout: config.reconnect_timeout_ms.map(Duration::from_millis),
            task_stack: config.task_stack_size,
            lwt: lwt_cfg,
            username: config.username,
            password: config.password,
            ..Default::default()
        };

        let mqtt_url = format!("mqtt://{}:{}", config.host, config.port);
        let (client, mut connection) = EspMqttClient::new(&mqtt_url, &mqtt_cfg)?;

        let connected = Arc::new(AtomicBool::new(false));
        let connected_clone = Arc::clone(&connected);
        let connection_error = Arc::new(AtomicBool::new(false));
        let connection_error_clone = Arc::clone(&connection_error);
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = Arc::clone(&shutdown);

        // Spawn a background thread for MQTT event processing.
        std::thread::Builder::new()
            .stack_size(EVENT_LOOP_STACK_SIZE)
            .spawn(move || {
                log::info!("MQTT event loop started");
                while let Ok(event) = connection.next() {
                    if shutdown_clone.load(Ordering::Acquire) {
                        log::info!("MQTT shutdown signal received");
                        break;
                    }
                    match event.payload() {
                        EventPayload::Connected(_) => {
                            log::info!("MQTT connected");
                            connected_clone.store(true, Ordering::Release);
                        }
                        EventPayload::Subscribed(id) => {
                            log::info!("Subscription confirmed (id: {})", id);
                        }
                        EventPayload::Received {
                            data,
                            topic: Some(topic_str),
                            ..
                        } => {
                            log::debug!("Received on '{}': {:?}", topic_str, data);
                            on_message(topic_str, data);
                        }
                        EventPayload::Error(e) => {
                            log::error!("MQTT error: {:?}", e);
                            connection_error_clone.store(true, Ordering::Release);
                        }
                        EventPayload::Disconnected => {
                            log::info!("MQTT disconnected");
                            connected_clone.store(false, Ordering::Release);
                            if shutdown_clone.load(Ordering::Acquire) {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
                log::info!("MQTT event loop exited");
            })
            .context("failed to spawn MQTT event loop thread")?;

        let shutdown_for_err = Arc::clone(&shutdown);
        let mut manager = Self {
            client,
            client_id,
            shutdown,
            _phantom: std::marker::PhantomData,
        };

        // Wait for connection
        let timeout_ms = config.connection_timeout_ms.unwrap_or(5000);
        let iterations = connection_wait_iterations(timeout_ms);
        log::info!("Waiting for MQTT connection...");

        let mut connected_within_timeout = false;
        for _ in 0..iterations {
            if connected.load(Ordering::Acquire) {
                log::info!("MQTT connection confirmed");
                connected_within_timeout = true;
                break;
            }
            if connection_error.load(Ordering::Acquire) {
                break;
            }
            std::thread::sleep(Duration::from_millis(POLL_INTERVAL_MS));
        }

        if connected_within_timeout {
            // Subscribe to all topics only when connected — calling subscribe() on an
            // unconnected EspMqttClient corrupts the ESP-IDF heap.
            for topic in &topics {
                validate_subscribe_filter(topic.as_str())
                    .map_err(|e| anyhow::anyhow!("invalid subscribe filter '{}': {}", topic, e))?;
                manager.client.subscribe(topic.as_str(), QoS::AtLeastOnce)?;
                log::info!("Subscribed to '{}'", topic);
            }
        } else {
            log::warn!(
                "[mqtt] connection failed — skipping subscribe to avoid heap corruption; \
                 caller should retry after a delay"
            );
            shutdown_for_err.store(true, Ordering::Release);
            return Err(anyhow::anyhow!("MQTT broker unreachable within timeout"));
        }

        Ok(manager)
    }

    /// Publishes a message to a topic with QoS 1 and no retain flag.
    ///
    /// For full control over QoS and retain, use [`publish_with`](Self::publish_with).
    pub fn publish(&mut self, topic: &str, payload: &str) -> anyhow::Result<()> {
        self.publish_with(topic, payload.as_bytes(), QoS::AtLeastOnce, false)
    }

    /// Publishes a retained message with QoS 1.
    ///
    /// Convenience wrapper around [`publish_with`](Self::publish_with) for
    /// messages that should be retained by the broker (e.g. state, online status).
    pub fn publish_retained(&mut self, topic: &str, payload: &str) -> anyhow::Result<()> {
        self.publish_with(topic, payload.as_bytes(), QoS::AtLeastOnce, true)
    }

    /// Publishes a message with explicit QoS and retain control.
    ///
    /// # Arguments
    ///
    /// * `topic` - The topic to publish to
    /// * `payload` - The message payload
    /// * `qos` - Quality of Service level
    /// * `retain` - Whether the broker should retain this message
    pub fn publish_with(
        &mut self,
        topic: &str,
        payload: &[u8],
        qos: QoS,
        retain: bool,
    ) -> anyhow::Result<()> {
        validate_publish_topic(topic)
            .map_err(|e| anyhow::anyhow!("invalid publish topic: {}", e))?;
        log::debug!("Publishing to '{}': {:?}", topic, payload);
        self.client.enqueue(topic, qos, retain, payload)?;
        Ok(())
    }

    /// Sends a startup notification message.
    ///
    /// Publishes "1" to `iot/{client_id}/startup`.
    #[deprecated(
        note = "use MqttBuilder::with_startup_message() for automatic startup notifications on every (re)connect, or publish() / publish_with() for custom lifecycle messages"
    )]
    pub fn send_startup_message(&mut self) -> anyhow::Result<()> {
        let topic = format!("iot/{}/startup", self.client_id);
        self.publish(&topic, "1")
    }

    /// Returns the client ID.
    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// Sends a shutdown notification message.
    ///
    /// Publishes "1" to `iot/{client_id}/shutdown`.
    #[deprecated(note = "use publish() or publish_with() for custom lifecycle messages")]
    pub fn send_shutdown_message(&mut self) -> anyhow::Result<()> {
        let topic = format!("iot/{}/shutdown", self.client_id);
        self.publish(&topic, "1")
    }

    /// Signals the background thread to shut down.
    ///
    /// This is called automatically when the manager is dropped.
    /// Sends a shutdown notification while still connected, then signals
    /// the background thread to exit.
    #[allow(deprecated)]
    pub fn shutdown(&mut self) {
        log::info!("Initiating MQTT shutdown");
        if let Err(e) = self.send_shutdown_message() {
            log::warn!("Failed to send shutdown message: {:?}", e);
        }
        // Then signal the background thread to stop
        self.shutdown.store(true, Ordering::Release);
    }
}

impl<'a, F> Drop for MqttManager<'a, F>
where
    F: Fn(&str, &[u8]) + Send + 'static,
{
    fn drop(&mut self) {
        self.shutdown();
    }
}

// ── Builder API ───────────────────────────────────────────────────────────────

/// Newtype wrapper around `EspMqttClient<'static>`.
///
/// Exists solely to satisfy the orphan rule: `SubscribeClient` is defined in
/// `juggler` and `EspMqttClient` is defined in `esp-idf-svc`,
/// so neither crate can implement the trait for the other.  Wrapping the client
/// in a local type makes the `impl` legal without changing runtime behaviour.
/// `Deref`/`DerefMut` forward all other method calls transparently.
struct SubscribableClient(EspMqttClient<'static>);

impl std::ops::Deref for SubscribableClient {
    type Target = EspMqttClient<'static>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for SubscribableClient {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl SubscribeClient for SubscribableClient {
    fn subscribe_topic(&mut self, topic: &str, qos: PureQoS) -> anyhow::Result<()> {
        self.0
            .subscribe(topic, pure_to_idf_qos(qos))
            .map(|_| ())
            .map_err(Into::into)
    }
}

fn pure_to_idf_qos(q: PureQoS) -> QoS {
    match q {
        PureQoS::AtMostOnce => QoS::AtMostOnce,
        PureQoS::AtLeastOnce => QoS::AtLeastOnce,
        PureQoS::ExactlyOnce => QoS::ExactlyOnce,
    }
}

fn idf_to_pure_qos(q: QoS) -> PureQoS {
    match q {
        QoS::AtMostOnce => PureQoS::AtMostOnce,
        QoS::AtLeastOnce => PureQoS::AtLeastOnce,
        QoS::ExactlyOnce => PureQoS::ExactlyOnce,
    }
}

/// Callback invoked on every (re)connect.
///
/// Receives `&mut EspMqttClient<'_>` for subscriptions and retained publishes,
/// and a `bool` that is `true` for a clean session.
type OnConnectCallback =
    Box<dyn Fn(&mut EspMqttClient<'_>, bool) -> anyhow::Result<()> + Send + 'static>;

/// Callback invoked for each incoming message with `(topic, payload)`.
type OnMessageCallback = Box<dyn Fn(&str, &[u8]) + Send + 'static>;

/// Builder for a persistent, auto-reconnecting MQTT manager.
///
/// Use [`MqttBuilder::new`] to obtain a builder, configure callbacks, then
/// call [`build`](MqttBuilder::build) to start the background event loop and
/// receive an [`MqttHandle`].
///
/// # Reconnection
///
/// The underlying `EspMqttClient` reconnects automatically.
/// [`on_connect`](MqttBuilder::on_connect) fires on every (re)connect, on a
/// per-connect helper thread, after the startup message (if enabled) and
/// before the builder's [`subscribe`](MqttBuilder::subscribe) topics are
/// sent. Its `client` parameter may be used for `enqueue()` — e.g. a retained
/// "online" status, as in the example below — and `subscribe()`; calling an
/// [`MqttHandle`] method from it instead fails fast with
/// [`PublishAckError::WrongThread`].
///
/// # Thread safety
///
/// The [`MqttHandle`] returned by `build` is cheaply cloneable and safe to
/// use from any thread.
/// When the last clone is dropped the event loop exits at the next MQTT
/// event boundary.
///
/// # Example
///
/// ```ignore
/// use rustyfarian_esp_idf_network::mqtt::{MqttBuilder, MqttConfig, LwtConfig};
/// use esp_idf_svc::mqtt::client::QoS;
///
/// let config = MqttConfig::new("192.168.1.100", 1883, "my-device");
///
/// let handle = MqttBuilder::new(config)
///     .subscribe("commands/#", QoS::AtLeastOnce)
///     .on_connect(|client, is_clean| {
///         log::info!("connected (clean={})", is_clean);
///         client.enqueue("device/status", QoS::AtLeastOnce, true, b"online")?;
///         Ok(())
///     })
///     .on_disconnect(|| log::warn!("MQTT disconnected"))
///     .on_message(|topic, data| log::info!("msg on {}: {:?}", topic, data))
///     .build()?;
///
/// handle.publish("events/boot", "ok")?;
/// ```
pub struct MqttBuilder<'a> {
    config: MqttConfig<'a>,
    on_connect: Option<OnConnectCallback>,
    on_disconnect: Option<Box<dyn Fn() + Send + 'static>>,
    on_message: Option<OnMessageCallback>,
    subscribe_topics: Vec<(String, PureQoS)>,
    with_startup_message: bool,
}

impl<'a> MqttBuilder<'a> {
    /// Creates a new builder from the given configuration.
    pub fn new(config: MqttConfig<'a>) -> Self {
        Self {
            config,
            on_connect: None,
            on_disconnect: None,
            on_message: None,
            subscribe_topics: Vec::new(),
            with_startup_message: false,
        }
    }

    /// Registers a topic to subscribe to on every (re)connect.
    ///
    /// Subscriptions are sent from the same per-connect helper thread that
    /// runs [`on_connect`](Self::on_connect) — after it returns — outside the
    /// event loop thread, avoiding the deadlock that would result from
    /// calling `subscribe()` on the event-loop thread while esp-mqtt holds
    /// its recursive `api_lock`.
    ///
    /// # Semantics
    ///
    /// - **Lifecycle**: a helper thread is spawned on every `Connected` event
    ///   (initial connect and every automatic reconnect) when at least one
    ///   topic is registered, [`with_startup_message()`](Self::with_startup_message)
    ///   is enabled, or [`on_connect`](Self::on_connect) is set.
    /// - **`is_connected()`**: flips to `true` immediately when `on_connect`
    ///   returns, which is *before* the helper thread has sent the SUBSCRIBE
    ///   packets.  Do not assume subscriptions are active the instant
    ///   `is_connected()` returns `true`.
    /// - **Failures**: subscribe errors are logged as warnings and not
    ///   propagated.  Failed subscriptions are retried automatically on the
    ///   next reconnect.
    /// - **Duplicates**: registering the same `(topic, qos)` pair more than
    ///   once is intentionally preserved — duplicate SUBSCRIBE packets are
    ///   handled safely by brokers per MQTT §3.8.
    ///
    /// Call this method once per topic; it can be chained:
    ///
    /// ```ignore
    /// MqttBuilder::new(config)
    ///     .subscribe("commands/#", QoS::AtLeastOnce)
    ///     .subscribe("ota/manifest", QoS::AtLeastOnce)
    ///     .build()?;
    /// ```
    pub fn subscribe(mut self, topic: impl Into<String>, qos: QoS) -> Self {
        self.subscribe_topics
            .push((topic.into(), idf_to_pure_qos(qos)));
        self
    }

    /// Registers a callback invoked on every (re)connect.
    ///
    /// Runs on a per-connect helper thread, after the startup message (if
    /// [`with_startup_message()`](Self::with_startup_message) is enabled) and
    /// before the builder's [`subscribe`](Self::subscribe) topics are sent.
    /// The `client` parameter may be used for `enqueue()` — e.g. a retained
    /// "online" status — and `subscribe()`; do **not** call any
    /// [`MqttHandle`] method from it, such a call returns
    /// [`PublishAckError::WrongThread`] instead of hanging.
    /// [`is_connected()`](MqttHandle::is_connected) flips to `true` only
    /// after the callback returns.
    ///
    /// For a resumed session (`is_clean_session == false`), the broker may
    /// redeliver queued messages while `on_connect` is still running; those
    /// trigger [`on_message`](Self::on_message) on the event-loop thread
    /// concurrently with the helper thread running `on_connect`.
    ///
    /// The `is_clean_session` parameter is `true` when the broker reports a clean
    /// session (no retained state from a previous session), and `false` when the
    /// previous session was resumed.
    ///
    /// If the callback returns `Err`, the error is logged with `warn!` and the event
    /// loop continues. The next automatic reconnection will invoke the callback
    /// again.
    ///
    /// Must not wait for a thread whose job is to publish via this MQTT
    /// client: that thread would block trying to acquire the client mutex
    /// this callback already holds, deadlocking both.
    pub fn on_connect<F>(mut self, f: F) -> Self
    where
        F: Fn(&mut EspMqttClient<'_>, bool) -> anyhow::Result<()> + Send + 'static,
    {
        self.on_connect = Some(Box::new(f));
        self
    }

    /// Registers a callback invoked immediately when the connection drops.
    ///
    /// The callback is invoked by the event loop thread and must return
    /// quickly.  The ESP-IDF layer will attempt to reconnect automatically.
    /// It runs on the event-loop thread while esp-mqtt holds its recursive
    /// `api_lock`; do not call any [`MqttHandle`] method from it (such a call
    /// returns [`PublishAckError::WrongThread`]).
    pub fn on_disconnect<F>(mut self, f: F) -> Self
    where
        F: Fn() + Send + 'static,
    {
        self.on_disconnect = Some(Box::new(f));
        self
    }

    /// Registers a callback invoked for each incoming message.
    ///
    /// Called with `(topic, payload)` for every `Received` event.
    /// Do not call any [`MqttHandle`] method from it — such calls return an error
    /// carrying [`PublishAckError::WrongThread`]; hand the message to another thread
    /// (via a channel) if processing must trigger a publish.
    ///
    /// Do not block on a bounded channel whose consumer is itself blocked in
    /// an [`MqttHandle`] call — that closes a cycle back through `api_lock`.
    pub fn on_message<F>(mut self, f: F) -> Self
    where
        F: Fn(&str, &[u8]) + Send + 'static,
    {
        self.on_message = Some(Box::new(f));
        self
    }

    /// Opts in to publishing a startup notification on every MQTT (re)connect.
    ///
    /// **Fires on every broker `Connected` transition — both the initial
    /// connection and every automatic reconnection — not only at device boot.**
    /// Despite the "startup" name, the publish repeats on each reconnect by
    /// design, so a downstream broker (or its consumers) always sees a fresh
    /// liveness ping after a network blip without the host needing to wire
    /// reconnect bookkeeping itself.
    ///
    /// When enabled, the builder publishes `"1"` to `iot/{client_id}/startup`
    /// with [`QoS::AtLeastOnce`] (not retained) from the per-connect helper
    /// thread, before [`on_connect`](Self::on_connect) runs and before the
    /// SUBSCRIBE packets sent for [`MqttBuilder::subscribe`] topics; it is
    /// best-effort (not load-bearing). Because it never runs on the
    /// event-loop thread, it cannot hit the esp-mqtt `api_lock` deadlock
    /// described in `docs/project-lore.md` "MQTT Event Loop".
    ///
    /// Replaces the deprecated [`MqttManager::send_startup_message`]: the
    /// builder handles the (re)connect lifecycle automatically, so the host
    /// no longer needs to call it manually.
    ///
    /// A failed startup publish is logged at `warn` and does not abort the
    /// connection — the message is best-effort, not load-bearing.
    pub fn with_startup_message(mut self) -> Self {
        self.with_startup_message = true;
        self
    }

    /// Starts the background event loop and returns an [`MqttHandle`].
    ///
    /// Returns immediately — the initial broker connection happens in the
    /// background.  The caller can start its main loop before the broker is
    /// reachable.
    ///
    /// # Errors
    ///
    /// Returns an error if the configuration is invalid or if the ESP-IDF
    /// MQTT client cannot be initialised.
    pub fn build(self) -> anyhow::Result<MqttHandle> {
        let config = self.config;

        // Validate configuration fields eagerly so callers get clear errors.
        validate_broker_host(config.host)
            .map_err(|e| anyhow::anyhow!("invalid MQTT host: {}", e))?;
        validate_broker_port(config.port)
            .map_err(|e| anyhow::anyhow!("invalid MQTT port: {}", e))?;
        validate_client_id(config.client_id)
            .map_err(|e| anyhow::anyhow!("invalid MQTT client_id: {}", e))?;
        for (topic, _) in &self.subscribe_topics {
            validate_subscribe_filter(topic.as_str())
                .map_err(|e| anyhow::anyhow!("invalid subscribe filter '{}': {}", topic, e))?;
        }

        // Build owned copies of all string fields.
        // esp_mqtt_client_init() calls strdup() on each of these immediately,
        // so they only need to live through the EspMqttClient::new() call below.
        // No Box::leak required.
        let url = format_broker_url(config.host, config.port);
        let client_id = config.client_id.to_string();
        log::info!("[mqtt] broker {} (client_id={})", url, client_id);
        // codeql[rust/cleartext-logging] - credentials are passed to the MQTT
        // broker via EspMqttClient::new(); this is required for authentication
        // and is not a logging operation.  esp_mqtt_client_init() strdup()'s
        // these values immediately; they are never written to any log sink here.
        let username = config.username.map(|s| s.to_string());
        // codeql[rust/cleartext-logging]
        let password = config.password.map(|s| s.to_string());
        let lwt_topic = config.lwt.as_ref().map(|l| l.topic.to_string());
        let lwt_payload = config.lwt.as_ref().map(|l| l.payload.to_vec());

        let lwt_cfg = lwt_topic
            .as_ref()
            .zip(config.lwt.as_ref())
            .map(|(topic, lwt)| LwtConfiguration {
                topic: topic.as_str(),
                payload: lwt_payload.as_deref().unwrap_or(&[]),
                qos: lwt.qos,
                retain: lwt.retain,
            });

        let mqtt_cfg = MqttClientConfiguration {
            client_id: Some(client_id.as_str()),
            keep_alive_interval: Some(Duration::from_secs(config.keep_alive_secs.unwrap_or(30))),
            reconnect_timeout: config.reconnect_timeout_ms.map(Duration::from_millis),
            task_stack: config.task_stack_size,
            lwt: lwt_cfg,
            username: username.as_deref(),
            password: password.as_deref(),
            ..Default::default()
        };

        let (client, mut connection) =
            EspMqttClient::new(&url, &mqtt_cfg).context("failed to create EspMqttClient")?;
        // url, client_id, username, password, lwt_topic, lwt_payload and mqtt_cfg
        // are all dropped here — the C library has already strdup'd what it needs.

        let shared_client = Arc::new(Mutex::new(SubscribableClient(client)));
        let client_for_thread = Arc::clone(&shared_client);

        let epoch = Arc::new(ConnectionEpoch::new());
        let epoch_for_thread = Arc::clone(&epoch);
        let epoch_for_handle = Arc::clone(&epoch);

        // Acknowledged-publish correlation, shared between the event loop (which
        // resolves PUBACKs) and the handle (which registers and waits).
        let pending = PendingAcks::new();
        let pending_for_thread = pending.clone();

        // Alive token: the thread holds a Weak reference; when the last
        // MqttHandle clone is dropped (taking the Arc<()> refcount to zero),
        // upgrade() returns None and the event loop exits at the next event.
        let alive = Arc::new(());
        let alive_weak = Arc::downgrade(&alive);

        // Wrapped in Arc<Mutex<_>> so the per-connect helper thread can share
        // the callback without changing the public `Fn + Send` (non-`Sync`)
        // bound on `on_connect<F>`; `Mutex<T: Send>` is `Sync`.
        let on_connect: Option<Arc<Mutex<OnConnectCallback>>> =
            self.on_connect.map(|f| Arc::new(Mutex::new(f)));
        let on_disconnect = self.on_disconnect;
        let on_message = self.on_message;
        let subscribe_topics = self.subscribe_topics;
        let startup_topic: Option<String> = self
            .with_startup_message
            .then(|| format!("iot/{}/startup", client_id));

        std::thread::Builder::new()
            .stack_size(BUILDER_EVENT_LOOP_STACK_SIZE)
            .spawn(move || {
                log::info!("[mqtt] builder event loop started");
                // The whole event-loop thread body counts as callback context:
                // on_message and on_disconnect run inline here, and MqttHandle
                // methods must refuse calls made from this thread.
                let _callback_scope = CallbackScope::enter();
                let mut state = MqttConnectionState::Connecting;

                loop {
                    // Exit when all MqttHandle clones have been dropped.
                    if alive_weak.upgrade().is_none() {
                        log::info!("[mqtt] all handles dropped, exiting event loop");
                        break;
                    }

                    log::debug!("[mqtt] event loop: waiting for next event...");
                    let event = match connection.next() {
                        Ok(e) => e,
                        Err(_) => {
                            log::info!("[mqtt] builder event loop: connection closed, exiting");
                            break;
                        }
                    };
                    // Per-event trace at DEBUG so steady-state operation stays quiet
                    // at the default INFO level (lifecycle events below log at INFO).
                    // Never dump a `Received` payload's bytes — that spams a decimal
                    // byte array on every message; log the topic and length instead.
                    match event.payload() {
                        EventPayload::Received { topic, data, .. } => log::debug!(
                            "[mqtt] event loop: received message (topic={:?}, {} bytes)",
                            topic,
                            data.len()
                        ),
                        payload => {
                            log::debug!("[mqtt] event loop: received event: {payload:?}")
                        }
                    }

                    match event.payload() {
                        EventPayload::Connected(is_clean) => {
                            if let Some(next) = next_state(state, MqttEvent::Connected) {
                                state = next;
                                log::info!("[mqtt] connected (clean_session={})", is_clean);
                                // The event-loop thread must never take the client mutex: esp-mqtt
                                // holds its recursive api_lock for the whole connect handshake, and
                                // esp-idf-svc parks the mqtt task until this thread calls next()
                                // again — a mutex held here while another thread blocks on api_lock
                                // closes a three-way cycle. The helper thread spawned below runs the
                                // startup publish, then on_connect, then the subscriptions, all under
                                // one lock of the client; confirm(token) guards against a stale
                                // helper (from a since-superseded connection) marking us connected.
                                let token = epoch_for_thread.advance();
                                if startup_topic.is_none()
                                    && on_connect.is_none()
                                    && subscribe_topics.is_empty()
                                {
                                    // Nothing to run off-thread — confirm immediately. No
                                    // advance() can have raced between the one above and
                                    // here (there is no thread hand-off on this path), so
                                    // this must succeed.
                                    let confirmed = epoch_for_thread.confirm(token);
                                    debug_assert!(
                                        confirmed,
                                        "fast path: confirm(token) must succeed right after advance()"
                                    );
                                } else {
                                    let epoch = Arc::clone(&epoch_for_thread);
                                    let startup_topic = startup_topic.clone();
                                    let on_connect = on_connect.clone();
                                    let prelude = move |client: &mut SubscribableClient| {
                                        if !epoch.is_current(token) {
                                            // A newer Connected/Disconnected already advanced
                                            // the epoch before this helper even started. The
                                            // subscribes that still follow in
                                            // spawn_connect_thread are harmless duplicates, so
                                            // there is nothing else to guard here.
                                            log::debug!(
                                                "[mqtt] connect helper: stale connection, \
                                                 skipping startup publish and on_connect"
                                            );
                                            return;
                                        }
                                        if let Some(topic) = &startup_topic {
                                            if let Err(e) = client.enqueue(
                                                topic,
                                                QoS::AtLeastOnce,
                                                false,
                                                b"1",
                                            ) {
                                                log::warn!(
                                                    "[mqtt] startup-message publish to '{}' failed: {:?}",
                                                    topic, e
                                                );
                                            }
                                        }
                                        if let Some(cb) = &on_connect {
                                            let _scope = CallbackScope::enter();
                                            match cb.lock() {
                                                Ok(f) => {
                                                    if let Err(e) = f(&mut *client, is_clean) {
                                                        log::warn!(
                                                            "[mqtt] on_connect callback failed: {:#}",
                                                            e
                                                        );
                                                    }
                                                }
                                                Err(_) => log::warn!(
                                                    "[mqtt] on_connect callback mutex poisoned"
                                                ),
                                            }
                                        }
                                        if !epoch.confirm(token) {
                                            log::debug!(
                                                "[mqtt] connect helper: connection state changed \
                                                 before on_connect finished; not marking connected"
                                            );
                                        }
                                    };
                                    let spawned = spawn_connect_thread(
                                        Arc::clone(&client_for_thread),
                                        Some(prelude),
                                        subscribe_topics.clone(),
                                        CONNECT_THREAD_STACK_SIZE,
                                    );
                                    if !spawned {
                                        // The connect helper thread could not be started.
                                        // Running the prelude inline here, on the
                                        // event-loop thread, would reintroduce the
                                        // deadlock this module exists to avoid (see the
                                        // module docs): esp-mqtt holds its recursive
                                        // api_lock for the whole connect handshake, and
                                        // this thread must return to next() promptly.
                                        // Give up on on_connect/subscriptions for this
                                        // connection instead.
                                        epoch_for_thread.confirm(token);
                                        log::error!(
                                            "[mqtt] could not spawn the connect helper \
                                             thread; on_connect and subscriptions skipped \
                                             for this connection"
                                        );
                                    }
                                }
                            }
                        }
                        EventPayload::Disconnected => {
                            if let Some(next) = next_state(state, MqttEvent::Disconnected) {
                                state = next;
                                epoch_for_thread.advance();
                                log::info!("[mqtt] disconnected");
                                // Fail every in-flight acked publish: a dropped
                                // session must never masquerade as a PUBACK, and a
                                // waiter should not spin to its full timeout.
                                pending_for_thread.fail_all(AckOutcome::Disconnected);
                                if let Some(ref f) = on_disconnect {
                                    f();
                                }
                            }
                        }
                        EventPayload::Received {
                            data,
                            topic: Some(topic_str),
                            ..
                        } => {
                            if let Some(ref f) = on_message {
                                f(topic_str, data);
                            }
                        }
                        EventPayload::Subscribed(id) => {
                            log::info!("[mqtt] subscription confirmed (id: {})", id);
                        }
                        EventPayload::Published(msg_id) => {
                            // Broker acknowledged a QoS 1 publish. Wake any
                            // publish_acked caller waiting on this message id;
                            // fire-and-forget publishes register no waiter, so
                            // this is a cheap no-op for them.
                            pending_for_thread.resolve(msg_id, AckOutcome::Acked);
                        }
                        EventPayload::Error(e) => {
                            log::error!("[mqtt] error: {:?}", e);
                        }
                        _ => {}
                    }
                }

                log::info!("[mqtt] builder event loop exited");
            })
            .context("failed to spawn MQTT builder event loop thread")?;

        Ok(MqttHandle {
            client: shared_client,
            epoch: epoch_for_handle,
            pending,
            _alive: alive,
        })
    }

    /// Starts the background event loop, waits for the initial broker
    /// connection with LED feedback, and returns an [`MqttHandle`].
    ///
    /// This is the blocking counterpart to [`build`](Self::build).
    /// While waiting, the LED pulses cyan; on success it turns steady green;
    /// on timeout it flashes red for one second before returning an error.
    ///
    /// Uses [`MqttConfig::connection_timeout_ms`] (default: 5000 ms) as the
    /// connection deadline.
    /// The LED borrow is released when this method returns.
    ///
    /// # Example
    ///
    /// ```ignore
    /// use rustyfarian_esp_idf_network::mqtt::{MqttBuilder, MqttConfig, StatusLed};
    ///
    /// let handle = MqttBuilder::new(config)
    ///     .subscribe("commands/#", QoS::AtLeastOnce)
    ///     .build_and_wait(&mut status_led)?;
    /// ```
    pub fn build_and_wait<L>(self, led: &mut L) -> anyhow::Result<MqttHandle>
    where
        L: StatusLed,
        L::Error: std::fmt::Debug,
    {
        let timeout_ms = self.config.connection_timeout_ms.unwrap_or(5000);
        let handle = self.build()?;

        let mut pulse = PulseEffect::new();
        let start = std::time::Instant::now();
        let timeout = Duration::from_millis(timeout_ms);

        log::info!(
            "[mqtt] waiting for connection (timeout: {} ms)...",
            timeout_ms
        );

        loop {
            if handle.is_connected() {
                log::info!("[mqtt] connection confirmed");
                if let Err(e) = led.set_color(RGB8::new(0, CONNECTED_LED_BRIGHTNESS, 0)) {
                    log::warn!("[mqtt] LED error (non-fatal): {:?}", e);
                }
                return Ok(handle);
            }

            if start.elapsed() >= timeout {
                log::error!("[mqtt] connection timeout after {} ms", timeout_ms);
                for _ in 0..ERROR_PULSE_FRAMES {
                    if let Err(e) = led.set_color(pulse.update(MQTT_ERROR_COLOR)) {
                        log::warn!("[mqtt] LED error (non-fatal): {:?}", e);
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(LED_POLL_INTERVAL_MS));
                }
                return Err(anyhow::anyhow!(
                    "MQTT broker unreachable within {} ms",
                    timeout_ms
                ));
            }

            if let Err(e) = led.set_color(pulse.update(MQTT_CONNECTING_COLOR)) {
                log::warn!("[mqtt] LED error (non-fatal): {:?}", e);
            }
            std::thread::sleep(Duration::from_millis(LED_POLL_INTERVAL_MS));
        }
    }
}

/// Cheaply cloneable MQTT handle returned by [`MqttBuilder::build`].
///
/// Publish from any thread using `&self`.
/// When the last clone is dropped the background event loop exits at the
/// next MQTT event boundary (keepalive pings ensure this happens promptly).
///
/// # Example
///
/// ```ignore
/// let handle2 = handle.clone();
/// std::thread::spawn(move || {
///     handle2.publish("sensors/temp", "22.5").unwrap();
/// });
/// ```
#[derive(Clone)]
pub struct MqttHandle {
    client: Arc<Mutex<SubscribableClient>>,
    epoch: Arc<ConnectionEpoch>,
    // Registry correlating in-flight acked publishes to their PUBACK; shared
    // with the event-loop thread, which resolves entries on `Published`.
    pending: PendingAcks,
    // Keeps the event loop alive.  When the last clone is dropped the
    // Arc refcount reaches zero, and the thread's Weak::upgrade() returns
    // None, causing the event loop to exit.
    _alive: Arc<()>,
}

impl MqttHandle {
    /// Rejects a call made from inside an MQTT callback.
    ///
    /// `on_message` and `on_disconnect` run inline on the event-loop thread,
    /// where esp-mqtt holds its recursive `api_lock` for the duration of the
    /// event dispatch and esp-idf-svc's zerocopy channel keeps the mqtt task
    /// parked until that thread calls `next()` again — any client call made
    /// from there would wait on `api_lock` forever. `on_connect` runs on the
    /// per-connect helper thread while it holds the client mutex, so a
    /// `MqttHandle` call from inside it would self-deadlock on that mutex.
    /// Both are detected the same way: via a [`CallbackScope`] entered around
    /// each callback invocation, not by comparing thread identity, so it
    /// works regardless of which thread runs the callback. Every
    /// publish/subscribe method calls this first and returns
    /// [`PublishAckError::WrongThread`] instead of hanging.
    ///
    /// No new public type is introduced: callers that want to detect this
    /// specifically can `e.downcast_ref::<PublishAckError>()` on the returned
    /// `anyhow::Error` — `anyhow`'s downcast searches through `.context()`
    /// layers, so this works even though the error is wrapped with additional
    /// context before it reaches the caller.
    fn ensure_not_in_callback(&self) -> Result<(), PublishAckError> {
        if in_callback() {
            return Err(PublishAckError::WrongThread);
        }
        Ok(())
    }

    /// Publishes a message with QoS 1 and no retain flag.
    pub fn publish(&self, topic: &str, payload: &str) -> anyhow::Result<()> {
        self.publish_with(topic, payload.as_bytes(), QoS::AtLeastOnce, false)
    }

    /// Publishes a retained message with QoS 1.
    ///
    /// Retained messages are stored by the broker and delivered to new
    /// subscribers immediately.  Use for persistent device state (e.g.
    /// online/offline status).
    pub fn publish_retained(&self, topic: &str, payload: &str) -> anyhow::Result<()> {
        self.publish_with(topic, payload.as_bytes(), QoS::AtLeastOnce, true)
    }

    /// Publishes a message with explicit QoS and retain control.
    ///
    /// # Arguments
    ///
    /// * `topic`   - The topic to publish to
    /// * `payload` - The message payload
    /// * `qos`     - Quality of Service level
    /// * `retain`  - Whether the broker should retain this message
    ///
    /// # Threading
    ///
    /// Must not be called from inside any callback (`on_connect`,
    /// `on_message`, `on_disconnect`). Such a call returns an error carrying
    /// [`PublishAckError::WrongThread`] instead of hanging. Safe to call from
    /// any other thread at any time, including before the first connect.
    pub fn publish_with(
        &self,
        topic: &str,
        payload: &[u8],
        qos: QoS,
        retain: bool,
    ) -> anyhow::Result<()> {
        self.ensure_not_in_callback()
            .context("publish_with rejected")?;
        validate_publish_topic(topic)
            .map_err(|e| anyhow::anyhow!("invalid publish topic: {}", e))?;
        log::debug!("[mqtt] publishing to '{}': {} bytes", topic, payload.len());
        let mut guard = self
            .client
            .lock()
            .map_err(|_| anyhow::anyhow!("MQTT client mutex poisoned"))?;
        guard.enqueue(topic, qos, retain, payload)?;
        Ok(())
    }

    /// Publishes with QoS 1 and blocks until the broker acknowledges the message
    /// (PUBACK) or `timeout` elapses.
    ///
    /// Unlike [`publish`](Self::publish) and friends — which enqueue and return
    /// before any acknowledgment — this reports, with a bounded wait, whether a
    /// specific message was durably received. It exists for the one ack-gated
    /// action in the OTA contract: clearing NVS rollback state only after the
    /// `rolled_back` status publish is confirmed, so an unreachable broker
    /// retries next boot instead of losing the evidence.
    ///
    /// # QoS
    ///
    /// The publish is always QoS 1 — the only level that yields a PUBACK to wait
    /// on. There is deliberately no `qos` parameter: QoS 0 produces no
    /// acknowledgment, and QoS 2's exactly-once handshake is out of scope.
    ///
    /// # Threading
    ///
    /// **Must not be called from inside any callback**
    /// (`on_connect`/`on_message`/`on_disconnect`): from
    /// [`on_connect`](MqttBuilder::on_connect) the helper thread already
    /// holds the client mutex, so enqueueing here would self-deadlock; from
    /// [`on_message`](MqttBuilder::on_message)/`on_disconnect` it would
    /// block the event-loop thread that must return to `next()` to receive
    /// the PUBACK. Such a call returns [`PublishAckError::WrongThread`]
    /// instead of hanging. Call it from a normal task/thread (e.g. the main
    /// loop).
    ///
    /// The client mutex is held only long enough to enqueue and read the
    /// message id; it is released before the wait begins. `timeout` bounds
    /// only that PUBACK wait, not the earlier wait for the client mutex — a
    /// slow `on_connect` still holding the mutex delays the call before the
    /// timer even starts.
    ///
    /// # Errors
    ///
    /// - [`PublishAckError::Timeout`] — no PUBACK within `timeout` (retry-eligible).
    /// - [`PublishAckError::Disconnected`] — the session dropped before the ack
    ///   (retry-eligible; never a false `Ok`).
    /// - [`PublishAckError::WrongThread`] — called from inside any callback.
    /// - [`PublishAckError::Other`] — topic validation, `enqueue`, or a poisoned
    ///   mutex failed (a local fault, not a broker outcome).
    pub fn publish_acked(
        &self,
        topic: &str,
        payload: &[u8],
        retained: bool,
        timeout: Duration,
    ) -> Result<(), PublishAckError> {
        // Refuse to block on our own PUBACK from inside a callback.
        self.ensure_not_in_callback()?;

        validate_publish_topic(topic)
            .map_err(|e| PublishAckError::Other(anyhow::anyhow!("invalid publish topic: {}", e)))?;

        // Enqueue under the client mutex to obtain the message id, then release
        // the mutex *before* waiting so the event loop can deliver the PUBACK.
        let msg_id = {
            let mut guard = self.client.lock().map_err(|_| {
                PublishAckError::Other(anyhow::anyhow!("MQTT client mutex poisoned"))
            })?;
            guard
                .enqueue(topic, QoS::AtLeastOnce, retained, payload)
                .map_err(|e| PublishAckError::Other(e.into()))?
        };

        // Register after enqueue; the pure registry's early-outcome buffer makes
        // the (practically impossible) enqueue→register race correct regardless.
        let waiter = self.pending.register(msg_id);
        log::debug!(
            "[mqtt] publish_acked to '{}' (msg_id={}), awaiting PUBACK",
            topic,
            msg_id
        );
        match waiter.wait(timeout) {
            Some(AckOutcome::Acked) => Ok(()),
            Some(AckOutcome::Disconnected) => Err(PublishAckError::Disconnected),
            None => Err(PublishAckError::Timeout),
        }
    }

    /// Non-blocking publish with QoS 1 and no retain flag.
    ///
    /// Returns [`TryPublishError::WouldBlock`] if the MQTT client mutex is
    /// held by the connect helper thread or another blocked publisher (see
    /// [`TryPublishError::WouldBlock`]).
    pub fn try_publish(&self, topic: &str, payload: &str) -> Result<(), TryPublishError> {
        self.try_publish_with(topic, payload.as_bytes(), QoS::AtLeastOnce, false)
    }

    /// Non-blocking retained publish with QoS 1.
    ///
    /// Returns [`TryPublishError::WouldBlock`] if the mutex is held.
    pub fn try_publish_retained(&self, topic: &str, payload: &str) -> Result<(), TryPublishError> {
        self.try_publish_with(topic, payload.as_bytes(), QoS::AtLeastOnce, true)
    }

    /// Non-blocking publish with explicit QoS and retain control.
    ///
    /// Uses `Mutex::try_lock()` instead of `lock()`, returning immediately
    /// with [`TryPublishError::WouldBlock`] when the connect helper thread
    /// holds the client mutex (running the startup publish, `on_connect`, or
    /// SUBSCRIBE enqueues) or another publisher is blocked on `api_lock`
    /// during the connect handshake.
    ///
    /// # Message loss
    ///
    /// Messages are **silently dropped** when `WouldBlock` is returned.
    /// During prolonged reconnects (tens of seconds on poor WiFi) every
    /// call will return `WouldBlock`, so the caller must decide whether to
    /// discard, buffer, or count missed publishes at the application layer.
    ///
    /// # Tight-loop usage
    ///
    /// Calling this in a busy loop without any yield or sleep will spin the
    /// CPU.  In a fixed-rate loop (e.g. 50 Hz game tick) the natural tick
    /// interval provides sufficient back-off.  Free-running loops should
    /// add a short delay or yield between retries.
    ///
    /// # Arguments
    ///
    /// * `topic`   - The topic to publish to
    /// * `payload` - The message payload
    /// * `qos`     - Quality of Service level
    /// * `retain`  - Whether the broker should retain this message
    ///
    /// # Threading
    ///
    /// Must not be called from inside any callback (`on_connect`,
    /// `on_message`, `on_disconnect`). Such a call returns
    /// [`TryPublishError::Other`] wrapping [`PublishAckError::WrongThread`]
    /// instead of hanging. Safe to call from any other thread at any time,
    /// including before the first connect.
    pub fn try_publish_with(
        &self,
        topic: &str,
        payload: &[u8],
        qos: QoS,
        retain: bool,
    ) -> Result<(), TryPublishError> {
        self.ensure_not_in_callback()
            .context("try_publish_with rejected")
            .map_err(TryPublishError::Other)?;
        validate_publish_topic(topic)
            .map_err(|e| TryPublishError::Other(anyhow::anyhow!("invalid publish topic: {}", e)))?;
        log::debug!("[mqtt] try_publish to '{}': {} bytes", topic, payload.len());
        let mut guard = self.client.try_lock().map_err(|e| match e {
            std::sync::TryLockError::WouldBlock => TryPublishError::WouldBlock,
            std::sync::TryLockError::Poisoned(_) => {
                TryPublishError::Other(anyhow::anyhow!("MQTT client mutex poisoned"))
            }
        })?;
        guard
            .enqueue(topic, qos, retain, payload)
            .map_err(|e| TryPublishError::Other(e.into()))?;
        Ok(())
    }

    /// Subscribes to a topic.
    ///
    /// # Important
    ///
    /// Do **not** call this from inside any callback (`on_connect`,
    /// `on_message`, `on_disconnect`): `on_message`/`on_disconnect` run on the
    /// event-loop thread (an `api_lock` hazard), and `on_connect` runs on the
    /// helper thread while it already holds the client mutex (a self-deadlock
    /// hazard). Such a call returns an error carrying
    /// [`PublishAckError::WrongThread`] instead of hanging. Instead, call
    /// `subscribe()` after `build()` once [`is_connected`] returns `true`, or
    /// register the topic on the builder via [`MqttBuilder::subscribe`].
    pub fn subscribe(&self, topic: &str, qos: QoS) -> anyhow::Result<()> {
        self.ensure_not_in_callback()
            .context("subscribe rejected")?;
        validate_subscribe_filter(topic)
            .map_err(|e| anyhow::anyhow!("invalid subscribe filter: {}", e))?;
        log::debug!("[mqtt] subscribing to '{}'", topic);
        let mut guard = self
            .client
            .lock()
            .map_err(|_| anyhow::anyhow!("MQTT client mutex poisoned"))?;
        guard.subscribe(topic, qos)?;
        Ok(())
    }

    /// Returns `true` if the MQTT transport is connected and the `on_connect`
    /// callback has completed.
    ///
    /// The flag is set only after `on_connect` returns, so a consumer never
    /// observes `true` while the callback is still running.
    ///
    /// # Mutex contention during subscription handshake
    ///
    /// The startup message (if enabled), `on_connect`, and every topic
    /// registered via [`MqttBuilder::subscribe`] are all run, in that order,
    /// by a single per-connect helper thread — the same thread whose
    /// completion flips this flag to `true`.  That thread holds the client
    /// mutex for the whole sequence (it does not wait for a SUBACK), so for
    /// the brief window while it holds the mutex:
    ///
    /// - [`publish_with`](MqttHandle::publish_with) may block briefly waiting
    ///   for the helper thread to release the mutex
    /// - [`try_publish_with`](MqttHandle::try_publish_with) may return
    ///   `WouldBlock`
    ///
    /// Retained messages on subscribed topics will be delivered once the broker
    /// processes the SUBSCRIBE packets; no application action is needed.
    ///
    /// A stale helper thread — left over from a connection that a fast
    /// disconnect/reconnect has since superseded — cannot flip this flag for
    /// the newer connection: the underlying [`ConnectionEpoch`] rejects its
    /// `confirm` once the generation has moved on.
    pub fn is_connected(&self) -> bool {
        self.epoch.is_connected()
    }
}
