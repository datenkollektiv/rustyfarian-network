//! MQTT client manager for ESP-IDF projects.
//!
//! Provides a simplified wrapper around the ESP-IDF MQTT client with:
//! - Automatic connection handling
//! - Background event loop
//! - Topic subscription with callback support
//!
//! # Example
//!
//! ```ignore
//! use rustyfarian_esp_idf_mqtt::{MqttManager, MqttConfig};
//!
//! let config = MqttConfig {
//!     host: "192.168.1.100",
//!     port: 1883,
//!     client_id: "my-device",
//! };
//!
//! let mqtt = MqttManager::new(config, "commands", |data| {
//!     println!("Received: {:?}", data);
//! })?;
//!
//! mqtt.publish("status", "online")?;
//! ```

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use esp_idf_svc::mqtt::client::{EspMqttClient, EventPayload, MqttClientConfiguration, QoS};

/// MQTT broker connection configuration.
#[derive(Debug, Clone)]
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
}

/// MQTT client manager with automatic connection and event handling.
///
/// The manager spawns a background thread to process MQTT events,
/// which is required for the ESP-IDF MQTT client to function.
///
/// When dropped, the manager signals the background thread to shut down
/// and sends a shutdown notification to `iot/{client_id}/shutdown`.
pub struct MqttManager<'a, F>
where
    F: Fn(&[u8]) + Send + 'static,
{
    client: EspMqttClient<'a>,
    client_id: String,
    shutdown: Arc<AtomicBool>,
    _phantom: std::marker::PhantomData<F>,
}

impl<'a, F> MqttManager<'a, F>
where
    F: Fn(&[u8]) + Send + 'static,
{
    /// Creates a new MQTT manager and connects to the broker.
    ///
    /// # Arguments
    ///
    /// * `config` - Connection configuration
    /// * `incoming_topic` - Topic to subscribe to for incoming messages
    /// * `on_message` - Callback invoked when a message is received on the subscribed topic
    ///
    /// # Returns
    ///
    /// A connected MQTT manager, or an error if connection fails.
    pub fn new(
        config: MqttConfig<'_>,
        incoming_topic: impl Into<String>,
        on_message: F,
    ) -> anyhow::Result<Self> {
        let incoming_topic = incoming_topic.into();
        let client_id = config.client_id.to_string();

        log::info!(
            "Connecting to MQTT broker at {}:{}",
            config.host,
            config.port
        );

        let mqtt_cfg = MqttClientConfiguration {
            client_id: Some(config.client_id),
            keep_alive_interval: Some(Duration::from_secs(config.keep_alive_secs.unwrap_or(30))),
            ..Default::default()
        };

        let mqtt_url = format!("mqtt://{}:{}", config.host, config.port);
        let (client, mut connection) = EspMqttClient::new(&mqtt_url, &mqtt_cfg)?;

        let connected = Arc::new(AtomicBool::new(false));
        let connected_clone = Arc::clone(&connected);
        let shutdown = Arc::new(AtomicBool::new(false));
        let shutdown_clone = Arc::clone(&shutdown);
        let topic_filter = incoming_topic.clone();

        // Spawn background thread for MQTT event processing
        std::thread::spawn(move || {
            log::info!("MQTT event loop started");
            while let Ok(event) = connection.next() {
                // Prepare for a shutdown signal
                if shutdown_clone.load(Ordering::Relaxed) {
                    log::info!("MQTT shutdown signal received");
                    break;
                }
                match event.payload() {
                    EventPayload::Connected(_) => {
                        log::info!("MQTT connected");
                        connected_clone.store(true, Ordering::Relaxed);
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
                        if topic_str == topic_filter.as_str() {
                            on_message(data);
                        }
                    }
                    EventPayload::Error(e) => {
                        log::error!("MQTT error: {:?}", e);
                    }
                    EventPayload::Disconnected => {
                        log::info!("MQTT disconnected");
                        if shutdown_clone.load(Ordering::Relaxed) {
                            break;
                        }
                    }
                    _ => {}
                }
            }
            log::info!("MQTT event loop exited");
        });

        let mut manager = Self {
            client,
            client_id,
            shutdown,
            _phantom: std::marker::PhantomData,
        };

        // Wait for connection
        let timeout_ms = config.connection_timeout_ms.unwrap_or(5000);
        let iterations = timeout_ms / 100;
        log::info!("Waiting for MQTT connection...");

        for i in 0..iterations {
            if connected.load(Ordering::Relaxed) {
                log::info!("MQTT connection confirmed");
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
            if i == iterations - 1 {
                log::warn!("MQTT connection timeout, attempting subscribe anyway");
            }
        }

        // Subscribe to a topic
        manager
            .client
            .subscribe(incoming_topic.as_str(), QoS::AtLeastOnce)?;
        log::info!("Subscribed to '{}'", incoming_topic);

        Ok(manager)
    }

    /// Publishes a message to a topic.
    ///
    /// # Arguments
    ///
    /// * `topic` - The topic to publish to
    /// * `payload` - The message payload as a string
    pub fn publish(&mut self, topic: &str, payload: &str) -> anyhow::Result<()> {
        log::debug!("Publishing to '{}': {}", topic, payload);
        self.client
            .enqueue(topic, QoS::AtLeastOnce, false, payload.as_bytes())?;
        Ok(())
    }

    /// Sends a startup notification message.
    ///
    /// Publishes "1" to `iot/{client_id}/startup`.
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
    pub fn send_shutdown_message(&mut self) -> anyhow::Result<()> {
        let topic = format!("iot/{}/shutdown", self.client_id);
        self.publish(&topic, "1")
    }

    /// Signals the background thread to shut down.
    ///
    /// This is called automatically when the manager is dropped.
    /// Sends a shutdown notification while still connected, then signals
    /// the background thread to exit.
    pub fn shutdown(&mut self) {
        log::info!("Initiating MQTT shutdown");
        // Send shutdown message while still connected
        if let Err(e) = self.send_shutdown_message() {
            log::warn!("Failed to send shutdown message: {:?}", e);
        }
        // Then signal the background thread to stop
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

impl<'a, F> Drop for MqttManager<'a, F>
where
    F: Fn(&[u8]) + Send + 'static,
{
    fn drop(&mut self) {
        self.shutdown();
    }
}
