//! Real MQTT implementation: builder, handle and the background runner.
//!
//! Compiled only with `embassy` plus a chip feature (`esp32c3`, `esp32c6`, `esp32s3`).
//!
//! Delivery contract: see the `mqtt` module docs — a handle `Ok(())` is queue
//! acceptance, and in-flight QoS 1 replay does not survive the clean-start reconnect
//! (minimq resets in-flight state when the broker reports no session).

use core::cell::RefCell;
use core::sync::atomic::{AtomicU32, Ordering};

use embassy_futures::select::{select, Either};
use embassy_net::tcp::TcpSocket;
use embassy_net::{IpAddress, IpEndpoint, Stack};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::{with_deadline, with_timeout, Duration, Instant, Timer};
use juggler::mqtt::{MqttClient, SessionAction, SessionInput, SessionMachine};
use minimq::{
    Buffers, ConfigBuilder, ConnectEvent, Connection, Error as MinimqError, PubError, Publication,
    Session, SubscriptionOptions, TopicFilter, Will,
};
use static_cell::StaticCell;

use super::{
    parse_broker_ipv4, socket_timeout_ms, stall_timeout_ms, validate_config, InboundMessage,
    MqttConfig, MqttConnectionState, MqttError, PublishRequest, QoS, TryPublishError,
    DEFAULT_CONNECTION_TIMEOUT_MS, DEFAULT_KEEP_ALIVE_SECS, DEFAULT_RECONNECT_TIMEOUT_MS,
    INBOUND_QUEUE_DEPTH, MAX_SUBSCRIPTIONS, MQTT_RX_BUFFER, MQTT_TX_BUFFER, PUBLISH_QUEUE_DEPTH,
    TCP_RX_BUFFER, TCP_TX_BUFFER,
};

/// Attempts to publish one queued message before dropping it when the TX arena is full.
const PUBLISH_ATTEMPTS: usize = 3;
/// How often the runner re-checks that the Wi-Fi link and the IP configuration are up.
///
/// Polled on purpose: `embassy-net` keeps one state waker, and a consumer task may
/// await `wait_for_ip()` / `wait_config_up()` concurrently; two waiters would
/// displace each other's waker and spin. The runner never awaits `Stack::wait_*`.
const NETWORK_POLL_INTERVAL: Duration = Duration::from_millis(500);
/// Upper bound for pushing the RST out after `abort()` while the network is up.
const RST_FLUSH_TIMEOUT: Duration = Duration::from_secs(1);
/// Outcome string for a session ended by the network watch.
const NETWORK_LOST: &str = "network lost";
/// Outcome string for a session await that outlived the stall deadline.
const TRANSPORT_STALLED: &str = "transport stalled";

type Socket = TcpSocket<'static>;
type Conn<'a> = Connection<'a, 'static, &'a mut Socket>;
type TransportError = embassy_net::tcp::Error;

/// State shared between the handle and the runner; lives in a `StaticCell`.
struct Shared {
    publish_q: Channel<CriticalSectionRawMutex, PublishRequest, PUBLISH_QUEUE_DEPTH>,
    inbound_q: Channel<CriticalSectionRawMutex, InboundMessage, INBOUND_QUEUE_DEPTH>,
    /// The shared session lifecycle (ADR 022 decision 10); the runner feeds it,
    /// the handle reads state, publish gate and connect count from it.
    machine: Mutex<CriticalSectionRawMutex, RefCell<SessionMachine>>,
    connected: Signal<CriticalSectionRawMutex, ()>,
    dropped_inbound: AtomicU32,
    dropped_publish: AtomicU32,
}

impl Shared {
    fn new(retry_ms: u64) -> Self {
        Self {
            publish_q: Channel::new(),
            inbound_q: Channel::new(),
            machine: Mutex::new(RefCell::new(SessionMachine::with_retry(retry_ms))),
            connected: Signal::new(),
            dropped_inbound: AtomicU32::new(0),
            dropped_publish: AtomicU32::new(0),
        }
    }

    /// Feeds the machine; wakes `wait_connected` when connect work made the session ready.
    fn poll(&self, input: SessionInput) -> SessionAction {
        let action = self.machine.lock(|m| m.borrow_mut().poll(input));
        if matches!(input, SessionInput::WorkDone(_)) && self.can_publish() {
            self.connected.signal(());
        }
        action
    }

    fn state(&self) -> MqttConnectionState {
        self.machine.lock(|m| m.borrow().state())
    }

    fn can_publish(&self) -> bool {
        self.machine.lock(|m| m.borrow().can_publish())
    }

    fn connect_count(&self) -> u32 {
        self.machine.lock(|m| m.borrow().connect_count())
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
    ///
    /// `Connected` means the broker acknowledged the CONNECT and the session has not
    /// ended since. The session ends within about 0.5 s of esp-radio reporting the
    /// station down or the IP configuration gone (a silent AP loss adds the driver's
    /// beacon timeout first); while the network is up, the socket timeout
    /// (1.5 × keep-alive, 45 s by default) and the runner's stall deadline (socket
    /// timeout + keep-alive, 75 s by default) end it within roughly 2.5 × keep-alive of
    /// the broker's last packet, whether or not the TCP reset can be routed.
    /// `Connected` therefore says the runner still considers the session healthy, not
    /// that the last publish was acknowledged or that the broker is reachable right now;
    /// `is_connected()` is this cached flag, not a probe.
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
        self.shared.connect_count()
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
    /// `Ok(())` means accepted into the [`PUBLISH_QUEUE_DEPTH`]-slot runner queue;
    /// queue entries survive a reconnect and go out in order, messages already handed
    /// to the session but not yet acknowledged by the broker do not (one at most when
    /// the network drops, more if the broker itself dies) — QoS 1 is at-least-once
    /// within one broker session only.
    ///
    /// Returns [`TryPublishError::WouldBlock`] while [`state`](Self::state) is not
    /// `Connected` or the queue is full; the caller skips this tick and retries later.
    pub fn try_publish(&self, topic: &str, payload: &[u8]) -> Result<(), TryPublishError> {
        self.try_publish_with(topic, payload, QoS::AtLeastOnce, false)
    }

    /// Publishes a retained message with QoS 1, never blocking.
    ///
    /// Acceptance and delivery rules are those of [`try_publish`](Self::try_publish).
    pub fn try_publish_retained(&self, topic: &str, payload: &[u8]) -> Result<(), TryPublishError> {
        self.try_publish_with(topic, payload, QoS::AtLeastOnce, true)
    }

    /// Publishes with explicit QoS and retain flag, never blocking.
    ///
    /// QoS 2 is downgraded to QoS 1 on this tier.
    /// Acceptance and delivery rules are those of [`try_publish`](Self::try_publish).
    pub fn try_publish_with(
        &self,
        topic: &str,
        payload: &[u8],
        qos: QoS,
        retain: bool,
    ) -> Result<(), TryPublishError> {
        let request = PublishRequest::new(topic, payload, clamp_qos(qos), retain)
            .map_err(TryPublishError::Other)?;
        if !self.shared.can_publish() {
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
    /// `Ok(())` means accepted into the queue, not delivered; the per-session rule of
    /// [`try_publish`](Self::try_publish) applies once the runner has handed the
    /// message to the session.
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

impl MqttClient for MqttHandle {
    type Error = MqttError;
    type Message = InboundMessage;

    fn state(&self) -> MqttConnectionState {
        MqttHandle::state(self)
    }

    fn connect_count(&self) -> u32 {
        MqttHandle::connect_count(self)
    }

    fn try_publish_with(
        &self,
        topic: &str,
        payload: &[u8],
        qos: QoS,
        retain: bool,
    ) -> Result<(), TryPublishError> {
        MqttHandle::try_publish_with(self, topic, payload, qos, retain)
    }

    async fn publish(
        &self,
        topic: &str,
        payload: &[u8],
        qos: QoS,
        retain: bool,
    ) -> Result<(), MqttError> {
        self.publish_with(topic, payload, qos, retain).await
    }

    async fn wait_connected(&self) {
        MqttHandle::wait_connected(self).await
    }

    async fn receive(&self) -> InboundMessage {
        MqttHandle::receive(self).await
    }
}

/// Tier parity check (ADR 022): fails to compile if `MqttHandle` stops implementing `MqttClient`.
const _: fn() = || {
    fn implements<T: MqttClient>() {}
    implements::<MqttHandle>();
};

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

        let reconnect_ms = self
            .config
            .reconnect_timeout_ms
            .unwrap_or(DEFAULT_RECONNECT_TIMEOUT_MS);
        let shared: &'static Shared = SHARED
            .try_init(Shared::new(reconnect_ms))
            .ok_or(MqttError::AlreadyBuilt)?;
        let tcp_rx = TCP_RX.init([0u8; TCP_RX_BUFFER]);
        let tcp_tx = TCP_TX.init([0u8; TCP_TX_BUFFER]);
        let mqtt_rx = MQTT_RX.init([0u8; MQTT_RX_BUFFER]);
        let mqtt_tx = MQTT_TX.init([0u8; MQTT_TX_BUFFER]);

        let session = build_session(&self.config, Buffers::new(mqtt_rx, mqtt_tx))?;
        let keep_alive = self
            .config
            .keep_alive_secs
            .unwrap_or(DEFAULT_KEEP_ALIVE_SECS);
        let mut socket = TcpSocket::new(stack, tcp_rx, tcp_tx);
        // Bounds a flush/read on a dead path while the network is up; smoltcp keeps the
        // value across abort()/connect(), so once is enough. MQTT keep-alive traffic
        // (PINGRESP, PUBACK, TCP ACKs) refreshes it, so no TCP keep-alive is needed.
        socket.set_timeout(Some(Duration::from_millis(socket_timeout_ms(keep_alive))));

        log::info!(
            "[mqtt] configured broker {}:{} client_id={} keep_alive={}s lwt={}",
            self.config.host,
            self.config.port,
            self.config.client_id,
            keep_alive,
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
            stall: Duration::from_millis(stall_timeout_ms(keep_alive)),
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
    stall: Duration,
}

impl MqttRunner {
    /// Drives connect, serve and reconnect cycles forever.
    ///
    /// The shared [`SessionMachine`] decides when to connect, what to do after
    /// CONNACK and how long to pause; this loop performs its actions.
    pub async fn run(mut self) -> ! {
        let mut action = SessionAction::None;
        // Log "waiting for network" once per wait; a disconnect line may already say it.
        let mut announce_wait = true;
        loop {
            action = match action {
                // Starting or offline: no attempt until the network is up (gate).
                SessionAction::None => {
                    if !network_up(&self.stack) {
                        if announce_wait {
                            log::info!("[mqtt] waiting for network");
                        }
                        while !network_up(&self.stack) {
                            Timer::after(NETWORK_POLL_INTERVAL).await;
                        }
                    }
                    announce_wait = true;
                    let next = self.shared.poll(SessionInput::NetworkUp);
                    if next == SessionAction::None {
                        // Not reachable from this loop's own transitions; never spin.
                        Timer::after(NETWORK_POLL_INTERVAL).await;
                    }
                    next
                }
                SessionAction::Connect => {
                    let (outcome, lost) = match select(
                        serve(
                            &mut self.session,
                            &mut self.socket,
                            self.shared,
                            self.broker,
                            &self.subscriptions,
                            self.startup,
                            self.connection_timeout,
                            self.stall,
                        ),
                        network_lost(self.stack),
                    )
                    .await
                    {
                        Either::First(outcome) => (outcome, false),
                        Either::Second(()) => (NETWORK_LOST, true),
                    };
                    if lost {
                        // The machine answers with `Close`, which the teardown below performs.
                        self.shared.poll(SessionInput::NetworkDown);
                    }
                    // Report the end before awaiting the cleanup, so `is_connected()` and
                    // `try_publish` stop accepting at once; the teardown still finishes
                    // before the next action runs.
                    let next = self.shared.poll(SessionInput::Disconnected);
                    self.close().await;
                    match next {
                        SessionAction::Wait { ms } => log::warn!(
                            "[mqtt] disconnected ({}), reconnecting in {} ms",
                            outcome,
                            ms
                        ),
                        _ => {
                            log::warn!("[mqtt] disconnected ({}), waiting for network", outcome);
                            announce_wait = false;
                        }
                    }
                    next
                }
                SessionAction::Wait { ms } => {
                    match select(Timer::after_millis(ms), network_lost(self.stack)).await {
                        Either::First(()) => self.shared.poll(SessionInput::RetryElapsed),
                        Either::Second(()) => self.shared.poll(SessionInput::NetworkDown),
                    }
                }
                SessionAction::Close => {
                    self.close().await;
                    self.shared.poll(SessionInput::Disconnected)
                }
                // Connect work runs inside `serve`; seeing it here means the machine
                // and the runner disagree, so tear down and start over.
                SessionAction::ConnectWork { .. } => {
                    log::warn!("[mqtt] connect work outside a connection, restarting");
                    self.close().await;
                    self.shared.poll(SessionInput::Disconnected)
                }
            };
        }
    }

    /// Returns the socket to CLOSED. The RST can only leave while the network is up
    /// (smoltcp clears the tuple after emitting it); the next `connect()` resets the
    /// socket either way, so the flush is skipped or bounded.
    async fn close(&mut self) {
        self.socket.abort();
        if network_up(&self.stack) {
            let _ = with_timeout(RST_FLUSH_TIMEOUT, self.socket.flush()).await;
        }
    }
}

/// One connection lifetime: TCP connect, MQTT handshake, startup work, then the
/// poll/publish loop until the session drops. Returns the reason it ended.
///
/// Every session await is bounded by a stall deadline, re-armed before each
/// `poll`/`publish` of the session loop (the post-connect subscribes and the startup
/// message share one); it is the only way out of a `flush()` whose RST has no route
/// (see `stall_timeout_ms`). `minimq` documents `poll()` and QoS 1 `publish()`
/// as cancel-safe, and `Session::connect` resets the outbound state regardless of how
/// the previous connection ended.
#[allow(clippy::too_many_arguments)]
async fn serve(
    session: &mut Session<'static>,
    socket: &mut Socket,
    shared: &'static Shared,
    broker: IpEndpoint,
    subscriptions: &[(&'static str, QoS)],
    startup: Option<StartupMessage>,
    connection_timeout: Duration,
    stall: Duration,
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

    let mut deadline = Instant::now() + stall;
    let event = conn.connect_event();
    let (token, subscribe) = match shared.poll(SessionInput::Connected {
        session_present: event != ConnectEvent::Connected,
    }) {
        SessionAction::ConnectWork { token, subscribe } => (token, subscribe),
        other => {
            log::warn!("[mqtt] CONNACK not expected by the session ({:?})", other);
            return "unexpected connack";
        }
    };
    // Connect work order shared with ESP-IDF: subscriptions, startup message, then ready.
    if subscribe {
        for (topic, qos) in subscriptions {
            let filter = TopicFilter::new(topic)
                .options(SubscriptionOptions::default().maximum_qos(to_minimq_qos(*qos)));
            match with_deadline(deadline, conn.subscribe(&[filter], &[])).await {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    log::warn!("[mqtt] subscribe {} failed: {:?}", topic, e);
                    shared.poll(SessionInput::WorkFailed(token));
                    return "subscribe failed";
                }
                Err(_) => {
                    log::warn!("[mqtt] subscribe {} stalled", topic);
                    shared.poll(SessionInput::WorkFailed(token));
                    return TRANSPORT_STALLED;
                }
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
        if let Err(reason) = publish_queued(&mut conn, shared, &request, deadline).await {
            shared.poll(SessionInput::WorkFailed(token));
            return reason;
        }
    }

    shared.poll(SessionInput::WorkDone(token));
    log::info!(
        "[mqtt] connected ({:?}, {} subscriptions{})",
        event,
        subscriptions.len(),
        if subscribe { "" } else { " kept" }
    );

    loop {
        deadline = Instant::now() + stall;
        match select(
            with_deadline(deadline, conn.poll()),
            shared.publish_q.receive(),
        )
        .await
        {
            Either::First(Ok(Ok(Some(inbound)))) => {
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
            Either::First(Ok(Ok(None))) => {}
            Either::First(Ok(Err(e))) => {
                log::warn!("[mqtt] session error: {:?}", e);
                return "session error";
            }
            Either::First(Err(_)) => {
                log::warn!("[mqtt] session stalled for {} ms", stall.as_millis());
                return TRANSPORT_STALLED;
            }
            Either::Second(request) => {
                if let Err(reason) = publish_queued(&mut conn, shared, &request, deadline).await {
                    return reason;
                }
            }
        }
    }
}

/// Publishes one queued request, retrying a full TX arena a few times.
///
/// `Ok(())` also covers a dropped message (counted); `Err` means the session is dead
/// or the transport stalled past `deadline`.
async fn publish_queued(
    conn: &mut Conn<'_>,
    shared: &Shared,
    request: &PublishRequest,
    deadline: Instant,
) -> Result<(), &'static str> {
    for attempt in 1..=PUBLISH_ATTEMPTS {
        let mut publication =
            Publication::bytes(request.topic.as_str(), request.payload.as_slice())
                .qos(to_minimq_qos(request.qos));
        if request.retain {
            publication = publication.retain();
        }
        let Ok(published) = with_deadline(deadline, conn.publish(publication)).await else {
            log::warn!("[mqtt] publish on {} stalled", request.topic.as_str());
            return Err(TRANSPORT_STALLED);
        };
        match published {
            Ok(_) => return Ok(()),
            Err(PubError::Session(MinimqError::NotReady)) if attempt < PUBLISH_ATTEMPTS => {
                // Let acknowledgements drain, then retry.
                match with_deadline(deadline, conn.drive()).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(e)) => {
                        log::warn!("[mqtt] drive failed: {:?}", e);
                        return Err("session error");
                    }
                    Err(_) => {
                        log::warn!("[mqtt] drive on {} stalled", request.topic.as_str());
                        return Err(TRANSPORT_STALLED);
                    }
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

/// `true` while the Wi-Fi link and the IP configuration are both up.
fn network_up(stack: &Stack<'static>) -> bool {
    stack.is_link_up() && stack.is_config_up()
}

/// Resolves once the Wi-Fi link or the IP configuration is gone.
///
/// Dropping the racing `serve()` future drops the `minimq` connection; the transport is
/// aborted next and `Session::connect` resets the outbound state, so a publish cancelled
/// here is lost (documented) but the session stays reusable.
async fn network_lost(stack: Stack<'static>) {
    while network_up(&stack) {
        Timer::after(NETWORK_POLL_INTERVAL).await;
    }
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
