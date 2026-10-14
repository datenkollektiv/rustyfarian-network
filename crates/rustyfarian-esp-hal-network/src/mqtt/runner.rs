//! Real MQTT implementation: builder, handle and the background runner.
//!
//! Compiled only with `embassy` plus a chip feature (`esp32c3`, `esp32c6`, `esp32s3`).

use core::sync::atomic::{AtomicU32, AtomicU8, Ordering};

use embassy_futures::select::{select, Either};
use embassy_net::tcp::TcpSocket;
use embassy_net::{IpAddress, IpEndpoint, Stack};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::{with_timeout, Duration, Timer};
use minimq::{
    Buffers, ConfigBuilder, ConnectEvent, Connection, Error as MinimqError, PubError, Publication,
    Session, SubscriptionOptions, TopicFilter, Will,
};
use static_cell::StaticCell;

use super::{
    parse_broker_ipv4, state_from_u8, state_to_u8, validate_config, InboundMessage, MqttConfig,
    MqttConnectionState, MqttError, PublishRequest, QoS, TryPublishError,
    DEFAULT_CONNECTION_TIMEOUT_MS, DEFAULT_KEEP_ALIVE_SECS, DEFAULT_RECONNECT_TIMEOUT_MS,
    INBOUND_QUEUE_DEPTH, MAX_SUBSCRIPTIONS, MQTT_RX_BUFFER, MQTT_TX_BUFFER, PUBLISH_QUEUE_DEPTH,
    TCP_RX_BUFFER, TCP_TX_BUFFER,
};

/// Attempts to publish one queued message before dropping it when the TX arena is full.
const PUBLISH_ATTEMPTS: usize = 3;

type Socket = TcpSocket<'static>;
type Conn<'a> = Connection<'a, 'static, &'a mut Socket>;
type TransportError = embassy_net::tcp::Error;

/// State shared between the handle and the runner; lives in a `StaticCell`.
struct Shared {
    publish_q: Channel<CriticalSectionRawMutex, PublishRequest, PUBLISH_QUEUE_DEPTH>,
    inbound_q: Channel<CriticalSectionRawMutex, InboundMessage, INBOUND_QUEUE_DEPTH>,
    state: AtomicU8,
    connected: Signal<CriticalSectionRawMutex, ()>,
    dropped_inbound: AtomicU32,
    dropped_publish: AtomicU32,
    connect_count: AtomicU32,
}

impl Shared {
    const fn new() -> Self {
        Self {
            publish_q: Channel::new(),
            inbound_q: Channel::new(),
            state: AtomicU8::new(0),
            connected: Signal::new(),
            dropped_inbound: AtomicU32::new(0),
            dropped_publish: AtomicU32::new(0),
            connect_count: AtomicU32::new(0),
        }
    }

    fn set_state(&self, state: MqttConnectionState) {
        self.state.store(state_to_u8(state), Ordering::Release);
        if state == MqttConnectionState::Connected {
            self.connected.signal(());
        }
    }

    fn state(&self) -> MqttConnectionState {
        state_from_u8(self.state.load(Ordering::Acquire))
    }

    fn push_inbound(&self, msg: InboundMessage) {
        if self.inbound_q.try_send(msg.clone()).is_err() {
            // Drop the oldest so a stalled consumer still sees fresh state.
            let _ = self.inbound_q.try_receive();
            bump(&self.dropped_inbound);
            let _ = self.inbound_q.try_send(msg);
        }
    }
}

/// Experimental: API may change before 1.0.
///
/// Handle for publishing and receiving; `Copy`, so hand it to any task.
///
/// Every method is non-blocking except the `async` ones, and none runs inside the client.
#[derive(Clone, Copy)]
pub struct MqttHandle {
    shared: &'static Shared,
}

impl MqttHandle {
    /// Current connection state, as the runner last reported it.
    pub fn state(&self) -> MqttConnectionState {
        self.shared.state()
    }

    /// `true` once the broker acknowledged the CONNECT and the startup work is done.
    pub fn is_connected(&self) -> bool {
        self.state() == MqttConnectionState::Connected
    }

    /// Waits until the session is connected.
    ///
    /// Intended for one waiter at a time (a `Signal` wakes one task per connect).
    pub async fn wait_connected(&self) {
        while !self.is_connected() {
            self.shared.connected.wait().await;
        }
    }

    /// Number of broker sessions established since boot.
    pub fn connect_count(&self) -> u32 {
        self.shared.connect_count.load(Ordering::Relaxed)
    }

    /// Inbound messages dropped because nobody drained the queue in time.
    pub fn dropped_inbound(&self) -> u32 {
        self.shared.dropped_inbound.load(Ordering::Relaxed)
    }

    /// Queued publishes the runner had to drop (TX arena full after retries, or oversized).
    pub fn dropped_publish(&self) -> u32 {
        self.shared.dropped_publish.load(Ordering::Relaxed)
    }

    /// Publishes with QoS 1 and no retain flag, never blocking.
    ///
    /// Returns [`TryPublishError::WouldBlock`] while disconnected or when the
    /// publish queue is full; the caller skips this tick and retries later.
    pub fn try_publish(&self, topic: &str, payload: &[u8]) -> Result<(), TryPublishError> {
        self.try_publish_with(topic, payload, QoS::AtLeastOnce, false)
    }

    /// Publishes a retained message with QoS 1, never blocking.
    pub fn try_publish_retained(&self, topic: &str, payload: &[u8]) -> Result<(), TryPublishError> {
        self.try_publish_with(topic, payload, QoS::AtLeastOnce, true)
    }

    /// Publishes with explicit QoS and retain flag, never blocking.
    ///
    /// QoS 2 is downgraded to QoS 1 on this tier.
    pub fn try_publish_with(
        &self,
        topic: &str,
        payload: &[u8],
        qos: QoS,
        retain: bool,
    ) -> Result<(), TryPublishError> {
        let request = PublishRequest::new(topic, payload, clamp_qos(qos), retain)
            .map_err(TryPublishError::Other)?;
        if !self.is_connected() {
            return Err(TryPublishError::WouldBlock);
        }
        self.shared
            .publish_q
            .try_send(request)
            .map_err(|_| TryPublishError::WouldBlock)
    }

    /// Queues a publish, waiting for queue space.
    ///
    /// Unlike [`try_publish_with`](Self::try_publish_with) this does not check
    /// the connection: a message queued while disconnected goes out after the
    /// next successful connect.
    pub async fn publish_with(
        &self,
        topic: &str,
        payload: &[u8],
        qos: QoS,
        retain: bool,
    ) -> Result<(), MqttError> {
        let request = PublishRequest::new(topic, payload, clamp_qos(qos), retain)?;
        self.shared.publish_q.send(request).await;
        Ok(())
    }

    /// Waits for the next inbound message on any builder subscription.
    pub async fn receive(&self) -> InboundMessage {
        self.shared.inbound_q.receive().await
    }

    /// Takes the next inbound message if one is queued.
    pub fn try_receive(&self) -> Option<InboundMessage> {
        self.shared.inbound_q.try_receive().ok()
    }
}

/// Message the runner publishes right after every successful connect.
#[derive(Clone, Copy)]
struct StartupMessage {
    topic: &'static str,
    payload: &'static [u8],
    qos: QoS,
    retain: bool,
}

/// Experimental: API may change before 1.0.
///
/// Configures and starts the bare-metal MQTT client.
///
/// Everything the runner keeps must be `'static`: credentials from
/// `option_env!` qualify directly, provisioned ones go through a `StaticCell`.
pub struct MqttBuilder {
    config: MqttConfig<'static>,
    subscriptions: heapless::Vec<(&'static str, QoS), MAX_SUBSCRIPTIONS>,
    startup: Option<StartupMessage>,
    overflow: bool,
}

impl MqttBuilder {
    /// Starts a builder from a connection configuration.
    pub fn new(config: MqttConfig<'static>) -> Self {
        Self {
            config,
            subscriptions: heapless::Vec::new(),
            startup: None,
            overflow: false,
        }
    }

    /// Registers a subscription the runner issues after every connect.
    ///
    /// More than [`MAX_SUBSCRIPTIONS`] topics make [`build`](Self::build) fail.
    pub fn subscribe(mut self, topic: &'static str, qos: QoS) -> Self {
        if self.subscriptions.push((topic, clamp_qos(qos))).is_err() {
            self.overflow = true;
        }
        self
    }

    /// Message published on every (re)connect — the retained "online" flag
    /// that pairs with an `offline` LWT.
    pub fn with_startup_message(
        mut self,
        topic: &'static str,
        payload: &'static [u8],
        qos: QoS,
        retain: bool,
    ) -> Self {
        self.startup = Some(StartupMessage {
            topic,
            payload,
            qos: clamp_qos(qos),
            retain,
        });
        self
    }

    /// Validates the configuration, claims the static buffers and returns the
    /// handle plus the runner the caller must drive from a task.
    ///
    /// Call at most once per boot; a second call returns [`MqttError::AlreadyBuilt`].
    pub fn build(self, stack: Stack<'static>) -> Result<(MqttHandle, MqttRunner), MqttError> {
        validate_config(&self.config)?;
        if self.overflow {
            return Err(MqttError::TooManySubscriptions);
        }
        for (topic, _) in &self.subscriptions {
            super::validate_subscribe_filter(topic).map_err(MqttError::InvalidTopic)?;
        }
        if let Some(startup) = &self.startup {
            super::validate_publish_topic(startup.topic).map_err(MqttError::InvalidTopic)?;
        }
        let broker = IpEndpoint::new(
            IpAddress::Ipv4(parse_broker_ipv4(self.config.host)?),
            self.config.port,
        );

        static SHARED: StaticCell<Shared> = StaticCell::new();
        static TCP_RX: StaticCell<[u8; TCP_RX_BUFFER]> = StaticCell::new();
        static TCP_TX: StaticCell<[u8; TCP_TX_BUFFER]> = StaticCell::new();
        static MQTT_RX: StaticCell<[u8; MQTT_RX_BUFFER]> = StaticCell::new();
        static MQTT_TX: StaticCell<[u8; MQTT_TX_BUFFER]> = StaticCell::new();

        let shared: &'static Shared = SHARED
            .try_init(Shared::new())
            .ok_or(MqttError::AlreadyBuilt)?;
        let tcp_rx = TCP_RX.init([0u8; TCP_RX_BUFFER]);
        let tcp_tx = TCP_TX.init([0u8; TCP_TX_BUFFER]);
        let mqtt_rx = MQTT_RX.init([0u8; MQTT_RX_BUFFER]);
        let mqtt_tx = MQTT_TX.init([0u8; MQTT_TX_BUFFER]);

        let session = build_session(&self.config, Buffers::new(mqtt_rx, mqtt_tx))?;
        let socket = TcpSocket::new(stack, tcp_rx, tcp_tx);

        log::info!(
            "[mqtt] configured broker {}:{} client_id={} keep_alive={}s lwt={}",
            self.config.host,
            self.config.port,
            self.config.client_id,
            self.config
                .keep_alive_secs
                .unwrap_or(DEFAULT_KEEP_ALIVE_SECS),
            self.config.lwt().map_or("none", |l| l.topic())
        );

        let runner = MqttRunner {
            shared,
            stack,
            socket,
            session,
            broker,
            subscriptions: self.subscriptions,
            startup: self.startup,
            connection_timeout: Duration::from_millis(
                self.config
                    .connection_timeout_ms
                    .unwrap_or(DEFAULT_CONNECTION_TIMEOUT_MS),
            ),
            reconnect_timeout: Duration::from_millis(
                self.config
                    .reconnect_timeout_ms
                    .unwrap_or(DEFAULT_RECONNECT_TIMEOUT_MS),
            ),
        };
        Ok((MqttHandle { shared }, runner))
    }
}

fn build_session(
    config: &MqttConfig<'static>,
    buffers: Buffers<'static>,
) -> Result<Session<'static>, MqttError> {
    let keep_alive = config.keep_alive_secs.unwrap_or(DEFAULT_KEEP_ALIVE_SECS);
    let keep_alive = u16::try_from(keep_alive)
        .map_err(|_| MqttError::InvalidConfig("keep_alive_secs must be 1..=65535"))?;
    let mut builder = ConfigBuilder::new(buffers)
        .client_id(config.client_id)
        .map_err(|_| MqttError::InvalidConfig("client_id too long for minimq"))?
        .keepalive_interval(keep_alive)
        .autodowngrade_qos();
    if let Some(username) = config.username() {
        let password = config.password().unwrap_or("").as_bytes();
        builder = builder
            .auth(username, password)
            .map_err(|_| MqttError::InvalidConfig("duplicate auth"))?;
    }
    if let Some(lwt) = config.lwt() {
        let will = Will::new(lwt.topic(), lwt.payload(), &[])
            .map_err(|_| MqttError::InvalidConfig("lwt topic too long for minimq"))?
            .qos(to_minimq_qos(lwt.qos()));
        let will = if lwt.retain() { will.retained() } else { will };
        builder = builder
            .will(will)
            .map_err(|_| MqttError::InvalidConfig("duplicate lwt"))?;
    }
    Ok(Session::new(builder))
}

/// Experimental: API may change before 1.0.
///
/// The background session driver; call [`run`](Self::run) from a dedicated task.
pub struct MqttRunner {
    shared: &'static Shared,
    stack: Stack<'static>,
    socket: Socket,
    session: Session<'static>,
    broker: IpEndpoint,
    subscriptions: heapless::Vec<(&'static str, QoS), MAX_SUBSCRIPTIONS>,
    startup: Option<StartupMessage>,
    connection_timeout: Duration,
    reconnect_timeout: Duration,
}

impl MqttRunner {
    /// Drives connect, serve and reconnect cycles forever.
    pub async fn run(mut self) -> ! {
        loop {
            self.shared.set_state(MqttConnectionState::Connecting);
            if !self.stack.is_config_up() {
                log::info!("[mqtt] waiting for network");
                self.stack.wait_config_up().await;
            }

            let outcome = serve(
                &mut self.session,
                &mut self.socket,
                self.shared,
                self.broker,
                &self.subscriptions,
                self.startup,
                self.connection_timeout,
            )
            .await;

            // Return the socket to CLOSED and push the RST out before reusing it.
            self.socket.abort();
            let _ = self.socket.flush().await;
            self.shared.set_state(MqttConnectionState::Disconnected);
            log::warn!(
                "[mqtt] disconnected ({}), reconnecting in {} ms",
                outcome,
                self.reconnect_timeout.as_millis()
            );
            Timer::after(self.reconnect_timeout).await;
        }
    }
}

/// One connection lifetime: TCP connect, MQTT handshake, startup work, then the
/// poll/publish loop until the session drops. Returns the reason it ended.
async fn serve(
    session: &mut Session<'static>,
    socket: &mut Socket,
    shared: &'static Shared,
    broker: IpEndpoint,
    subscriptions: &[(&'static str, QoS)],
    startup: Option<StartupMessage>,
    connection_timeout: Duration,
) -> &'static str {
    match with_timeout(connection_timeout, socket.connect(broker)).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            log::warn!("[mqtt] tcp connect failed: {:?}", e);
            return "tcp connect failed";
        }
        Err(_) => return "tcp connect timed out",
    }

    let mut conn: Conn<'_> = match with_timeout(connection_timeout, session.connect(socket)).await {
        Ok(Ok(conn)) => conn,
        Ok(Err(e)) => {
            log::warn!("[mqtt] CONNECT rejected: {:?}", e);
            return "mqtt connect failed";
        }
        Err(_) => return "mqtt connect timed out",
    };

    let event = conn.connect_event();
    if event == ConnectEvent::Connected {
        for (topic, qos) in subscriptions {
            let filter = TopicFilter::new(topic)
                .options(SubscriptionOptions::default().maximum_qos(to_minimq_qos(*qos)));
            if let Err(e) = conn.subscribe(&[filter], &[]).await {
                log::warn!("[mqtt] subscribe {} failed: {:?}", topic, e);
                return "subscribe failed";
            }
        }
    }
    if let Some(msg) = startup {
        let request = PublishRequest {
            topic: heapless::String::try_from(msg.topic).unwrap_or_default(),
            payload: heapless::Vec::from_slice(msg.payload).unwrap_or_default(),
            qos: msg.qos,
            retain: msg.retain,
        };
        if let Err(reason) = publish_queued(&mut conn, shared, &request).await {
            return reason;
        }
    }

    bump(&shared.connect_count);
    shared.set_state(MqttConnectionState::Connected);
    log::info!(
        "[mqtt] connected ({:?}, {} subscriptions)",
        event,
        subscriptions.len()
    );

    loop {
        match select(conn.poll(), shared.publish_q.receive()).await {
            Either::First(Ok(Some(inbound))) => {
                match InboundMessage::copy_from(
                    inbound.topic(),
                    inbound.payload(),
                    inbound.retained(),
                    from_minimq_qos(inbound.qos()),
                ) {
                    Some(msg) => shared.push_inbound(msg),
                    None => {
                        bump(&shared.dropped_inbound);
                        log::warn!("[mqtt] dropped oversized inbound on {}", inbound.topic());
                    }
                }
            }
            Either::First(Ok(None)) => {}
            Either::First(Err(e)) => {
                log::warn!("[mqtt] session error: {:?}", e);
                return "session error";
            }
            Either::Second(request) => {
                if let Err(reason) = publish_queued(&mut conn, shared, &request).await {
                    return reason;
                }
            }
        }
    }
}

/// Publishes one queued request, retrying a full TX arena a few times.
///
/// `Ok(())` also covers a dropped message (counted); `Err` means the session is dead.
async fn publish_queued(
    conn: &mut Conn<'_>,
    shared: &Shared,
    request: &PublishRequest,
) -> Result<(), &'static str> {
    for attempt in 1..=PUBLISH_ATTEMPTS {
        let mut publication =
            Publication::bytes(request.topic.as_str(), request.payload.as_slice())
                .qos(to_minimq_qos(request.qos));
        if request.retain {
            publication = publication.retain();
        }
        match conn.publish(publication).await {
            Ok(_) => return Ok(()),
            Err(PubError::Session(MinimqError::NotReady)) if attempt < PUBLISH_ATTEMPTS => {
                // Let acknowledgements drain, then retry.
                if let Err(e) = conn.drive().await {
                    log::warn!("[mqtt] drive failed: {:?}", e);
                    return Err("session error");
                }
            }
            Err(PubError::Session(MinimqError::NotReady))
            | Err(PubError::Session(MinimqError::Resource(_)))
            | Err(PubError::Session(MinimqError::InvalidRequest)) => {
                bump(&shared.dropped_publish);
                log::warn!("[mqtt] dropped publish on {}", request.topic.as_str());
                return Ok(());
            }
            Err(e) => {
                log::warn!("[mqtt] publish failed: {:?}", e);
                return Err("publish failed");
            }
        }
    }
    Ok(())
}

/// Increments a diagnostic counter.
///
/// `riscv32imc` (ESP32-C3) has no atomic read-modify-write, so this is a plain
/// load + store; the counters are advisory and the runner is the only writer.
fn bump(counter: &AtomicU32) {
    counter.store(
        counter.load(Ordering::Relaxed).wrapping_add(1),
        Ordering::Relaxed,
    );
}

fn clamp_qos(qos: QoS) -> QoS {
    match qos {
        QoS::ExactlyOnce => QoS::AtLeastOnce,
        other => other,
    }
}

fn to_minimq_qos(qos: QoS) -> minimq::QoS {
    match qos {
        QoS::AtMostOnce => minimq::QoS::AtMostOnce,
        QoS::AtLeastOnce => minimq::QoS::AtLeastOnce,
        QoS::ExactlyOnce => minimq::QoS::ExactlyOnce,
    }
}

fn from_minimq_qos(qos: minimq::QoS) -> QoS {
    match qos {
        minimq::QoS::AtMostOnce => QoS::AtMostOnce,
        minimq::QoS::AtLeastOnce => QoS::AtLeastOnce,
        minimq::QoS::ExactlyOnce => QoS::ExactlyOnce,
    }
}

#[allow(dead_code)]
fn _transport_error_is_debug(e: TransportError) -> impl core::fmt::Debug {
    e
}
