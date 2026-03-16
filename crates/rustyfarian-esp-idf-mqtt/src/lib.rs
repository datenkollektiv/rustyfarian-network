//! MQTT client manager for ESP-IDF projects.
//!
//! Provides a persistent, auto-reconnecting MQTT client with lifecycle
//! callbacks, thread-safe publishing, and pure connection-state validation.
//!
//! # Quick start
//!
//! ```ignore
//! use rustyfarian_esp_idf_mqtt::{MqttBuilder, MqttConfig};
//! use esp_idf_svc::mqtt::client::QoS;
//!
//! let config = MqttConfig::new("192.168.1.100", 1883, "my-device");
//!
//! let handle = MqttBuilder::new(config)
//!     .on_connect(|client, _is_clean| {
//!         client.subscribe("commands/#", QoS::AtLeastOnce)?;
//!         Ok(())
//!     })
//!     .on_disconnect(|| log::warn!("MQTT disconnected"))
//!     .on_message(|topic, data| log::info!("msg on {}: {:?}", topic, data))
//!     .build()?;
//!
//! handle.publish("status", "online")?;
//! ```
//!
//! ## Non-blocking publish
//!
//! For time-critical loops (e.g. ESP-NOW at 50 Hz), use [`MqttHandle::try_publish`]
//! to avoid blocking when the event loop holds the client mutex during reconnects.
//! Messages are silently dropped on `WouldBlock` — buffer or count misses at the
//! application layer if lossless delivery matters:
//!
//! ```ignore
//! use rustyfarian_esp_idf_mqtt::TryPublishError;
//!
//! match handle.try_publish("sensors/temp", "22.5") {
//!     Ok(()) => log::info!("published"),
//!     Err(TryPublishError::WouldBlock) => { /* skip this tick, retry next */ }
//!     Err(TryPublishError::Other(e)) => log::warn!("publish failed: {}", e),
//! }
//! ```
//!
//! [`MqttManager`] is still available but deprecated — use [`MqttBuilder`] for
//! new code.

use anyhow::Context as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustyfarian_network_pure::mqtt::{
    connection_wait_iterations, format_broker_url, next_state, validate_broker_host,
    validate_broker_port, validate_client_id, validate_publish_topic, validate_subscribe_filter,
    MqttConnectionState, MqttEvent,
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
/// 12 KiB accommodates the `on_connect` callback frames on top of the base
/// event loop overhead.  Measure on hardware and increase if stack overflows
/// are observed with deeply nested `on_connect` logic.
const BUILDER_EVENT_LOOP_STACK_SIZE: usize = 12 * 1024;

/// Default stack size for the ESP-IDF MQTT client task.
///
/// ESP-IDF defaults to 6144 bytes, which overflows during TLS negotiation
/// error paths — corrupting the heap and crashing `pthread_exit`.
/// 8 KiB provides sufficient headroom for TLS handshakes on ESP32.
/// Override via [`MqttConfig::with_task_stack_size`] if needed.
const DEFAULT_MQTT_TASK_STACK_SIZE: usize = 8192;

use esp_idf_svc::mqtt::client::{
    EspMqttClient, EventPayload, LwtConfiguration, MqttClientConfiguration, QoS,
};

/// Error returned by the `try_publish*` family when the publish cannot
/// complete without blocking.
#[derive(Debug)]
pub enum TryPublishError {
    /// The MQTT client mutex is held by the event loop (e.g. during reconnect).
    /// The caller should retry on the next tick.
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

    /// Configures a Last Will and Testament message.
    ///
    /// The broker publishes this message when the client disconnects
    /// unexpectedly. A clean shutdown suppresses the LWT.
    pub fn with_lwt(mut self, lwt: LwtConfig<'a>) -> Self {
        self.lwt = Some(lwt);
        self
    }

    /// Sets MQTT broker authentication credentials.
    pub fn with_auth(mut self, username: &'a str, password: &'a str) -> Self {
        self.username = Some(username);
        self.password = Some(password);
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
    #[deprecated(note = "use publish() or publish_with() for custom lifecycle messages")]
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
/// [`on_connect`](MqttBuilder::on_connect) is called on every (re)connect;
/// use it for subscriptions and retained-state publishes.
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
/// use rustyfarian_esp_idf_mqtt::{MqttBuilder, MqttConfig, LwtConfig};
/// use esp_idf_svc::mqtt::client::QoS;
///
/// let config = MqttConfig::new("192.168.1.100", 1883, "my-device");
///
/// let handle = MqttBuilder::new(config)
///     .on_connect(|client, _is_clean| {
///         client.subscribe("commands/#", QoS::AtLeastOnce)?;
///         client.enqueue("device/status", QoS::AtLeastOnce, true, b"online")?;
///         Ok(())
///     })
///     .on_disconnect(|| log::warn!("MQTT disconnected"))
///     .on_message(|topic, data| log::info!("msg on {}: {:?}", topic, data))
///     .build()?;
///
/// // `build()` returns immediately; connection happens in the background.
/// handle.publish("events/boot", "ok")?;
/// ```
pub struct MqttBuilder<'a> {
    config: MqttConfig<'a>,
    on_connect: Option<OnConnectCallback>,
    on_disconnect: Option<Box<dyn Fn() + Send + 'static>>,
    on_message: Option<OnMessageCallback>,
}

impl<'a> MqttBuilder<'a> {
    /// Creates a new builder from the given configuration.
    pub fn new(config: MqttConfig<'a>) -> Self {
        Self {
            config,
            on_connect: None,
            on_disconnect: None,
            on_message: None,
        }
    }

    /// Registers a callback invoked on every (re)connect.
    ///
    /// `is_clean_session` is `true` when the broker reports a clean session
    /// (no retained state from a previous session), and `false` when the
    /// previous session was resumed.
    ///
    /// Use the `client` parameter to call `subscribe()` and `enqueue()`.
    /// Subscriptions placed here are automatically re-established on every
    /// automatic reconnection.
    ///
    /// If the callback returns `Err`, the error is logged with `warn!` and the
    /// event loop continues.  The next automatic reconnection will invoke the
    /// callback again.
    ///
    /// # Note
    ///
    /// The callback is invoked by the event loop thread while it holds
    /// exclusive access to the MQTT client.  Do **not** call
    /// [`MqttHandle::publish`] from inside this callback — use
    /// `client.enqueue()` directly instead.
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
    pub fn on_message<F>(mut self, f: F) -> Self
    where
        F: Fn(&str, &[u8]) + Send + 'static,
    {
        self.on_message = Some(Box::new(f));
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

        // Build owned copies of all string fields.
        // esp_mqtt_client_init() calls strdup() on each of these immediately,
        // so they only need to live through the EspMqttClient::new() call below.
        // No Box::leak required.
        let url = format_broker_url(config.host, config.port);
        let client_id = config.client_id.to_string();
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

        let shared_client = Arc::new(Mutex::new(client));
        let client_for_thread = Arc::clone(&shared_client);

        let connected = Arc::new(AtomicBool::new(false));
        let connected_for_thread = Arc::clone(&connected);
        let connected_for_handle = Arc::clone(&connected);

        // Alive token: the thread holds a Weak reference; when the last
        // MqttHandle clone is dropped (taking the Arc<()> refcount to zero),
        // upgrade() returns None and the event loop exits at the next event.
        let alive = Arc::new(());
        let alive_weak = Arc::downgrade(&alive);

        let on_connect = self.on_connect;
        let on_disconnect = self.on_disconnect;
        let on_message = self.on_message;

        std::thread::Builder::new()
            .stack_size(BUILDER_EVENT_LOOP_STACK_SIZE)
            .spawn(move || {
                log::info!("[mqtt] builder event loop started");
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
                    log::info!("[mqtt] event loop: received event: {:?}", event.payload());

                    match event.payload() {
                        EventPayload::Connected(is_clean) => {
                            if let Some(next) = next_state(state, MqttEvent::Connected) {
                                state = next;
                                log::info!("[mqtt] connected (clean_session={})", is_clean);
                                if let Some(ref f) = on_connect {
                                    let mut guard = client_for_thread.lock().unwrap();
                                    if let Err(e) = f(&mut guard, is_clean) {
                                        log::warn!("[mqtt] on_connect callback failed: {:#}", e);
                                    }
                                }
                                // Set connected AFTER the on_connect callback releases the
                                // mutex.  This prevents publish_with() callers from racing
                                // for the mutex while on_connect still holds it, which could
                                // cause both threads to deadlock inside esp_mqtt_client_enqueue.
                                connected_for_thread.store(true, Ordering::Release);
                            }
                        }
                        EventPayload::Disconnected => {
                            if let Some(next) = next_state(state, MqttEvent::Disconnected) {
                                state = next;
                                connected_for_thread.store(false, Ordering::Release);
                                log::info!("[mqtt] disconnected");
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
            connected: connected_for_handle,
            _alive: alive,
        })
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
    client: Arc<Mutex<EspMqttClient<'static>>>,
    connected: Arc<AtomicBool>,
    // Keeps the event loop alive.  When the last clone is dropped the
    // Arc refcount reaches zero, and the thread's Weak::upgrade() returns
    // None, causing the event loop to exit.
    _alive: Arc<()>,
}

impl MqttHandle {
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
    pub fn publish_with(
        &self,
        topic: &str,
        payload: &[u8],
        qos: QoS,
        retain: bool,
    ) -> anyhow::Result<()> {
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

    /// Non-blocking publish with QoS 1 and no retain flag.
    ///
    /// Returns [`TryPublishError::WouldBlock`] if the MQTT client mutex is
    /// held by the event loop (e.g. during a reconnect).
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
    /// with [`TryPublishError::WouldBlock`] when the event loop thread
    /// holds the client mutex (e.g. during a WiFi-triggered reconnect).
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
    pub fn try_publish_with(
        &self,
        topic: &str,
        payload: &[u8],
        qos: QoS,
        retain: bool,
    ) -> Result<(), TryPublishError> {
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
    /// Do **not** call this from inside the `on_connect` callback — in
    /// esp-idf-svc 0.52+, `subscribe()` blocks until the broker sends
    /// SUBACK, which requires the event loop to process the response.
    /// Since the event loop is blocked inside the callback, this deadlocks.
    ///
    /// Instead, call `subscribe()` after `build()` once [`is_connected`]
    /// returns `true`.
    pub fn subscribe(&self, topic: &str, qos: QoS) -> anyhow::Result<()> {
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

    /// Returns `true` if the client is connected **and** the `on_connect`
    /// callback has completed.
    ///
    /// The flag is set only after `on_connect` releases the internal mutex,
    /// so a caller that sees `true` and immediately calls `publish_with()`
    /// is guaranteed to find the mutex free — no race with the event loop.
    ///
    /// Uses `Ordering::Acquire` to ensure visibility of any state written
    /// by the event loop thread before the flag was set.
    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }
}
