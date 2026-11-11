//! Real MQTT implementation: builder, handle and the background runner.
//!
//! Compiled only with `embassy` plus a chip feature (`esp32c3`, `esp32c6`, `esp32s3`, `esp32`).
//!
//! Delivery contract: see the `mqtt` module docs — a handle `Ok(())` is queue
//! acceptance, and in-flight QoS 1 replay does not survive the clean-start reconnect
//! (minimq resets in-flight state when the broker reports no session).

use core::cell::RefCell;
use core::sync::atomic::{AtomicU32, Ordering};
use core::task::Poll;

use embassy_futures::select::{select, select3, Either, Either3};
use embassy_net::dns::DnsQueryType;
use embassy_net::tcp::TcpSocket;
use embassy_net::{IpAddress, IpEndpoint, Stack};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::channel::Channel;
use embassy_sync::signal::Signal;
use embassy_time::{with_deadline, with_timeout, Duration, Instant, Timer};
use juggler::mqtt::lines::{Connected, Connecting, Disconnected, Reconnecting, WaitingForNetwork};
use juggler::mqtt::{
    AckStatus, BrokerHost, ConnectStep, ConnectWorkMachine, ConnectWorkPlan, MqttClient,
    NetworkGate, SessionAction, SessionInput, SessionMachine, SubscribeWindow, WorkAction,
    WorkInput, SUBSCRIBE_WINDOW,
};
use juggler::waker_slots::{poll_ready, ReadyPoll, WakerSlots};
use minimq::{
    Buffers, ConfigBuilder, ConnectEvent, Connection, Error as MinimqError, InboundPublish, Op,
    PubError, Publication, ResourceError, Session, SubscriptionOptions, TopicFilter, Will,
};
use static_cell::StaticCell;

use super::{
    clamp_qos, parse_broker_host, socket_timeout_ms, stall_timeout_ms, validate_config,
    InboundMessage, MqttConfig, MqttConnectionState, MqttError, PublishRequest, QoS,
    RuntimeSubscriptions, TryPublishError, DEFAULT_CONNECTION_TIMEOUT_MS, DEFAULT_KEEP_ALIVE_SECS,
    DEFAULT_RECONNECT_TIMEOUT_MS, INBOUND_QUEUE_DEPTH, MAX_SUBSCRIPTIONS, MQTT_RX_BUFFER,
    MQTT_TX_BUFFER, PUBLISH_QUEUE_DEPTH, TCP_RX_BUFFER, TCP_TX_BUFFER,
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
/// Outcome string for a session await that outlived the stall deadline.
const TRANSPORT_STALLED: &str = "transport stalled";
/// Tasks that [`MqttHandle::wait_connected`] wakes directly; more waiters
/// re-check every [`CONNECTED_WAITER_POLL`] instead.
const CONNECTED_WAITERS: usize = 4;
/// How often a waiter beyond [`CONNECTED_WAITERS`] re-checks the session.
const CONNECTED_WAITER_POLL: Duration = Duration::from_millis(500);

type Socket = TcpSocket<'static>;
type Conn<'a> = Connection<'a, 'static, &'a mut Socket>;
type TransportError = embassy_net::tcp::Error;
/// SUBSCRIBE packets sent and not yet acknowledged; the handle is the `minimq`
/// operation and the QoS that went out.
type SubWindow = SubscribeWindow<(Op, QoS), SUBSCRIBE_WINDOW>;

/// State shared between the handle and the runner; lives in a `StaticCell`.
struct Shared {
    publish_q: Channel<CriticalSectionRawMutex, PublishRequest, PUBLISH_QUEUE_DEPTH>,
    inbound_q: Channel<CriticalSectionRawMutex, InboundMessage, INBOUND_QUEUE_DEPTH>,
    /// The shared session lifecycle (ADR 022 decision 10); the runner feeds it,
    /// the handle reads state, publish gate and connect count from it.
    machine: Mutex<CriticalSectionRawMutex, RefCell<SessionMachine>>,
    /// Tasks in [`MqttHandle::wait_connected`]; a full set refuses, never mass-wakes.
    connected: Mutex<CriticalSectionRawMutex, RefCell<WakerSlots<CONNECTED_WAITERS>>>,
    /// The builder's subscriptions, read by [`MqttHandle::subscribe`] for de-duplication.
    fixed_subscriptions: heapless::Vec<(&'static str, QoS), MAX_SUBSCRIPTIONS>,
    /// Subscriptions added at runtime; see [`RuntimeSubscriptions`].
    runtime_subscriptions: Mutex<CriticalSectionRawMutex, RefCell<RuntimeSubscriptions>>,
    /// Raised when a runtime subscription became pending; the runner's live loop awaits it.
    subscriptions_changed: Signal<CriticalSectionRawMutex, ()>,
    dropped_inbound: AtomicU32,
    dropped_publish: AtomicU32,
}

impl Shared {
    fn new(
        retry_ms: u64,
        fixed_subscriptions: heapless::Vec<(&'static str, QoS), MAX_SUBSCRIPTIONS>,
    ) -> Self {
        Self {
            publish_q: Channel::new(),
            inbound_q: Channel::new(),
            machine: Mutex::new(RefCell::new(SessionMachine::with_retry(retry_ms))),
            connected: Mutex::new(RefCell::new(WakerSlots::new())),
            fixed_subscriptions,
            runtime_subscriptions: Mutex::new(RefCell::new(RuntimeSubscriptions::new())),
            subscriptions_changed: Signal::new(),
            dropped_inbound: AtomicU32::new(0),
            dropped_publish: AtomicU32::new(0),
        }
    }

    /// Feeds the machine; wakes `wait_connected` when it answers [`SessionAction::Ready`].
    fn poll(&self, input: SessionInput) -> SessionAction {
        let action = self.machine.lock(|m| m.borrow_mut().poll(input));
        if action == SessionAction::Ready {
            self.connected.lock(|w| w.borrow_mut().wake_all());
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
    /// Any number of tasks may wait: up to four are woken directly, and further
    /// ones re-check every 500 ms.
    pub async fn wait_connected(&self) {
        loop {
            let woken = core::future::poll_fn(|cx| {
                // Register first, then check: a readiness change in between still wakes us.
                let registered = self
                    .shared
                    .connected
                    .lock(|w| w.borrow_mut().register(cx.waker()));
                match poll_ready(registered, self.shared.can_publish()) {
                    ReadyPoll::Ready => Poll::Ready(true),
                    ReadyPoll::Waiting => Poll::Pending,
                    ReadyPoll::Full => Poll::Ready(false),
                }
            })
            .await;
            if woken {
                return;
            }
            Timer::after(CONNECTED_WAITER_POLL).await;
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
    /// This is the admission contract of `juggler::mqtt::MqttClient::publish`,
    /// defined once in `juggler` and shared with the ESP-IDF tier: the
    /// `send().await` on the publish channel is the wait, and a message already
    /// in the channel is never evicted to make room for a newer one.
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

    /// Subscribes to a topic filter while the client runs.
    ///
    /// Same name, arguments and return shape as the ESP-IDF `MqttHandle::subscribe`;
    /// only the error type differs. Never blocks.
    /// `Ok(())` means the request was accepted: the runner subscribes on the live
    /// session, or right after the next connect when there is none, and re-issues
    /// the topic after every reconnect that needs subscriptions, after the builder's.
    /// A filter already subscribed at the same QoS is a no-op, and QoS 2 is downgraded to QoS 1.
    ///
    /// The filter is copied into a [`MAX_TOPIC_LEN`](super::MAX_TOPIC_LEN)-byte
    /// buffer, so the argument need not be `'static`.
    /// Errors: [`MqttError::InvalidTopic`] for a malformed filter,
    /// [`MqttError::TopicTooLong`] above the cap, and
    /// [`MqttError::RuntimeSubscriptionsFull`] once
    /// [`MAX_RUNTIME_SUBSCRIPTIONS`](super::MAX_RUNTIME_SUBSCRIPTIONS) topics are registered.
    /// A broker refusal surfaces as a session restart, like a failed builder subscription.
    pub fn subscribe(&self, topic: &str, qos: QoS) -> Result<(), MqttError> {
        let wake = self.shared.runtime_subscriptions.lock(|subs| {
            subs.borrow_mut()
                .admit(&self.shared.fixed_subscriptions, topic, qos)
        })?;
        if wake {
            self.shared.subscriptions_changed.signal(());
        }
        Ok(())
    }

    /// Waits for the next inbound message on any subscription.
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

    fn subscribe(&self, topic: &str, qos: QoS) -> Result<(), MqttError> {
        MqttHandle::subscribe(self, topic, qos)
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
        let broker = match parse_broker_host(self.config.host)? {
            BrokerHost::Ipv4(ip) => {
                BrokerAddr::Literal(IpEndpoint::new(IpAddress::Ipv4(ip), self.config.port))
            }
            BrokerHost::Name(name) => BrokerAddr::Name {
                host: name,
                port: self.config.port,
            },
        };

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
            .try_init(Shared::new(reconnect_ms, self.subscriptions.clone()))
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
            gate: NetworkGate::new(),
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
    /// Turns the polled link samples into the machine's network edges; every
    /// sample goes through it, so no edge is fed twice.
    gate: NetworkGate,
    socket: Socket,
    session: Session<'static>,
    broker: BrokerAddr,
    subscriptions: heapless::Vec<(&'static str, QoS), MAX_SUBSCRIPTIONS>,
    startup: Option<StartupMessage>,
    connection_timeout: Duration,
    stall: Duration,
}

impl MqttRunner {
    /// Drives connect, serve and reconnect cycles forever.
    ///
    /// The shared [`SessionMachine`] decides when to connect, what to do after
    /// CONNACK and how long to pause; the [`NetworkGate`] decides when the
    /// network came or went; this loop performs the machine's actions.
    pub async fn run(mut self) -> ! {
        let edge = self.next_gate_edge().await;
        let mut action = self.feed_edge(edge);
        loop {
            action = match action {
                // Nothing to perform: the next input is a network edge.
                SessionAction::None => {
                    let edge = self.next_gate_edge().await;
                    self.feed_edge(edge)
                }
                SessionAction::Connect => {
                    match self.broker {
                        BrokerAddr::Literal(endpoint) => log::info!(
                            "{}",
                            Connecting {
                                host: endpoint.addr,
                                port: endpoint.port
                            }
                        ),
                        BrokerAddr::Name { host, port } => {
                            log::info!("{}", Connecting { host, port });
                        }
                    }
                    match select(
                        serve(
                            &mut self.session,
                            &mut self.socket,
                            self.shared,
                            &self.stack,
                            self.broker,
                            &self.subscriptions,
                            self.startup,
                            self.connection_timeout,
                            self.stall,
                        ),
                        next_gate_edge(&mut self.gate, &self.stack),
                    )
                    .await
                    {
                        Either::First(outcome) => {
                            // `serve` ended on its own (transport or broker). Report the
                            // end before awaiting the cleanup, so `is_connected()` and
                            // `try_publish` stop accepting at once; the teardown still
                            // finishes before the next action runs.
                            let next = self.shared.poll(SessionInput::Disconnected);
                            self.close().await;
                            log::warn!(
                                "{}",
                                Disconnected {
                                    cause: Some(outcome)
                                }
                            );
                            if let SessionAction::Wait { ms } = next {
                                log::warn!("{}", Reconnecting { ms });
                            }
                            next
                        }
                        // The network went: perform what the machine answers (`Close`).
                        Either::Second(edge) => self.feed_edge(edge),
                    }
                }
                SessionAction::Wait { ms } => {
                    match select(
                        Timer::after_millis(ms),
                        next_gate_edge(&mut self.gate, &self.stack),
                    )
                    .await
                    {
                        Either::First(()) => self.shared.poll(SessionInput::RetryElapsed),
                        Either::Second(edge) => self.feed_edge(edge),
                    }
                }
                SessionAction::Close => {
                    self.close().await;
                    log::warn!(
                        "{}",
                        Disconnected {
                            cause: Some("network lost")
                        }
                    );
                    self.shared.poll(SessionInput::Disconnected)
                }
                // Connect work runs inside `serve`, which also consumes its `Ready`;
                // seeing either here, or an ignored input, means the machine and the
                // runner disagree, so tear down and start over.
                SessionAction::ConnectWork { .. } | SessionAction::Ready => {
                    log::warn!("[mqtt] {:?} outside a connection, restarting", action);
                    self.restart().await
                }
                SessionAction::Ignored => {
                    log::warn!("[mqtt] the session ignored an input, restarting");
                    self.restart().await
                }
            };
        }
    }

    /// Recovers from a runner/machine disagreement: closes the socket and reports
    /// `Disconnected`. A second disagreement waits for the next network edge
    /// instead of looping, so it can never spin.
    async fn restart(&mut self) -> SessionAction {
        self.close().await;
        match self.shared.poll(SessionInput::Disconnected) {
            SessionAction::Ignored | SessionAction::Ready => SessionAction::None,
            next => next,
        }
    }

    /// Waits for the next network edge; see [`next_gate_edge`].
    async fn next_gate_edge(&mut self) -> SessionInput {
        next_gate_edge(&mut self.gate, &self.stack).await
    }

    /// Feeds one network edge to the machine, logging the wait once per outage.
    fn feed_edge(&self, edge: SessionInput) -> SessionAction {
        if edge == SessionInput::NetworkDown {
            log::info!("{}", WaitingForNetwork);
        }
        self.shared.poll(edge)
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

/// Where the runner connects: an IPv4 literal is used as is, a name is resolved on
/// every connect (the address may change between sessions).
#[derive(Clone, Copy)]
enum BrokerAddr {
    Literal(IpEndpoint),
    Name { host: &'static str, port: u16 },
}

/// One connection lifetime: DNS lookup (names only), TCP connect, MQTT handshake, startup work, then the
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
    stack: &Stack<'static>,
    broker: BrokerAddr,
    subscriptions: &[(&'static str, QoS)],
    startup: Option<StartupMessage>,
    connection_timeout: Duration,
    stall: Duration,
) -> &'static str {
    let broker = match broker {
        BrokerAddr::Literal(endpoint) => endpoint,
        BrokerAddr::Name { host, port } => {
            match with_timeout(connection_timeout, stack.dns_query(host, DnsQueryType::A)).await {
                Ok(Ok(addrs)) => match addrs.first() {
                    Some(addr) => IpEndpoint::new(*addr, port),
                    None => return "dns lookup returned no address",
                },
                Ok(Err(e)) => {
                    log::warn!("[mqtt] dns lookup of {} failed: {:?}", host, e);
                    return "dns lookup failed";
                }
                Err(_) => return "dns lookup timed out",
            }
        }
    };

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
        SessionAction::ConnectWork {
            token, subscribe, ..
        } => (token, subscribe),
        other => {
            log::warn!("[mqtt] CONNACK not expected by the session ({:?})", other);
            return "unexpected connack";
        }
    };
    // This tier has no connect callback, so the plan ends with the startup message.
    // Builder subscriptions come first, then the runtime ones in registration order;
    // a runtime entry added after this count is picked up by the live loop below.
    let fixed = subscriptions.len();
    let total = fixed
        + shared.runtime_subscriptions.lock(|s| {
            let mut subs = s.borrow_mut();
            subs.begin_connect(subscribe);
            subs.len()
        });
    // The shared machine holds the step order and the stop-at-first-failure rule
    // (ADR 023 rung 3); this loop performs one action per loop turn (the startup
    // step drains the SUBACK window before it publishes). It never feeds
    // `SessionEnded`: on this tier a session that ends mid-work is the `select` in
    // `run` dropping this future, so no action runs after the end.
    let mut work = ConnectWorkMachine::new(
        token,
        ConnectWorkPlan::new(subscribe, total, startup.is_some(), false),
        event != ConnectEvent::Connected,
    );
    let mut window = SubWindow::new();
    let mut input = WorkInput::Start;
    let mut failure: Option<&'static str> = None;
    loop {
        input = match work.poll(input) {
            WorkAction::Step(ConnectStep::Subscribe(index)) => outcome(
                subscribe_entry(
                    &mut conn,
                    shared,
                    &mut window,
                    subscriptions,
                    index,
                    deadline,
                )
                .await,
                &mut failure,
            ),
            WorkAction::Step(ConnectStep::PublishStartup) => match startup {
                Some(msg) => {
                    let sent =
                        match drain_subscriptions(&mut conn, shared, &mut window, fixed, deadline)
                            .await
                        {
                            Err(reason) => Err(reason),
                            Ok(()) => {
                                let request = PublishRequest {
                                    topic: heapless::String::try_from(msg.topic)
                                        .unwrap_or_default(),
                                    payload: heapless::Vec::from_slice(msg.payload)
                                        .unwrap_or_default(),
                                    qos: msg.qos,
                                    retain: msg.retain,
                                };
                                publish_queued(&mut conn, shared, &request, deadline).await
                            }
                        };
                    outcome(sent, &mut failure)
                }
                None => WorkInput::StepDone,
            },
            // Unreachable: the plan is built with `on_connect = false` on this tier.
            WorkAction::Step(ConnectStep::RunOnConnect) => WorkInput::StepDone,
            // Ready only once the broker has answered every SUBSCRIBE.
            WorkAction::Finish => outcome(
                drain_subscriptions(&mut conn, shared, &mut window, fixed, deadline).await,
                &mut failure,
            ),
            WorkAction::Report(report) => {
                if shared.poll(report) != SessionAction::Ready {
                    return failure.unwrap_or("session ended during connect work");
                }
                break;
            }
            WorkAction::Abandon | WorkAction::Ignored => return "connect work out of step",
        };
    }
    log::info!(
        "{}",
        Connected {
            session_present: event != ConnectEvent::Connected,
            subscriptions: subscriptions.len()
        }
    );

    // A request whose send failed in an earlier session is still owed, and a kept
    // session's plan skips it; send it before waiting for a new request.
    if let Err(reason) = send_pending_subscriptions(&mut conn, shared, stall).await {
        return reason;
    }

    loop {
        deadline = Instant::now() + stall;
        match select3(
            with_deadline(deadline, conn.poll()),
            shared.publish_q.receive(),
            shared.subscriptions_changed.wait(),
        )
        .await
        {
            Either3::First(Ok(Ok(Some(inbound)))) => forward_inbound(shared, &inbound),
            Either3::First(Ok(Ok(None))) => {}
            Either3::First(Ok(Err(e))) => {
                log::warn!("[mqtt] session error: {:?}", e);
                return "session error";
            }
            Either3::First(Err(_)) => {
                log::warn!("[mqtt] session stalled for {} ms", stall.as_millis());
                return TRANSPORT_STALLED;
            }
            Either3::Second(request) => {
                if let Err(reason) = publish_queued(&mut conn, shared, &request, deadline).await {
                    return reason;
                }
            }
            Either3::Third(()) => {
                if let Err(reason) = send_pending_subscriptions(&mut conn, shared, stall).await {
                    return reason;
                }
            }
        }
    }
}

/// Why one SUBSCRIBE was not sent.
enum SubscribeError {
    /// `minimq` has no room for another unacknowledged packet right now.
    NoRoom,
    /// The session must end, for this reason.
    Failed(&'static str),
}

/// Copies one inbound publish into the receive channel, counting an oversized one.
fn forward_inbound(shared: &Shared, inbound: &InboundPublish<'_>) {
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

/// Maps the result of one connect step to the machine's input; a failure reason is
/// kept so `serve` returns it once the machine reports.
fn outcome(result: Result<(), &'static str>, failure: &mut Option<&'static str>) -> WorkInput {
    match result {
        Ok(()) => WorkInput::StepDone,
        Err(reason) => {
            *failure = Some(reason);
            WorkInput::StepFailed
        }
    }
}

/// Sends the SUBSCRIBE of plan entry `index`: a builder topic below the builder count,
/// otherwise the runtime entry at `index - fixed`; a runtime entry that is gone is done.
async fn subscribe_entry(
    conn: &mut Conn<'_>,
    shared: &Shared,
    window: &mut SubWindow,
    subscriptions: &[(&'static str, QoS)],
    index: usize,
    deadline: Instant,
) -> Result<(), &'static str> {
    let fixed = subscriptions.len();
    if index < fixed {
        let (topic, qos) = subscriptions[index];
        return subscribe_paced(conn, shared, window, fixed, index, topic, qos, deadline).await;
    }
    let entry = shared
        .runtime_subscriptions
        .lock(|s| s.borrow().get(index - fixed));
    match entry {
        Some((topic, qos)) => {
            subscribe_paced(
                conn,
                shared,
                window,
                fixed,
                index,
                topic.as_str(),
                qos,
                deadline,
            )
            .await
        }
        None => Ok(()),
    }
}

/// Sends every runtime subscription the session still owes, paced by the SUBACKs.
///
/// An entry stays pending until its SUBACK arrived, so a failed session sends it again.
async fn send_pending_subscriptions(
    conn: &mut Conn<'_>,
    shared: &Shared,
    stall: Duration,
) -> Result<(), &'static str> {
    let mut window = SubWindow::new();
    loop {
        let deadline = Instant::now() + stall;
        let mut cursor = 0;
        let mut sent_any = false;
        loop {
            let next = shared
                .runtime_subscriptions
                .lock(|s| s.borrow().next_pending_from(cursor));
            let Some((index, topic, qos)) = next else {
                break;
            };
            subscribe_paced(
                conn,
                shared,
                &mut window,
                0,
                index,
                topic.as_str(),
                qos,
                deadline,
            )
            .await?;
            cursor = index + 1;
            sent_any = true;
        }
        drain_subscriptions(conn, shared, &mut window, 0, deadline).await?;
        if !sent_any {
            return Ok(());
        }
    }
}

/// Sends one SUBSCRIBE once the window has room and tracks it under `tag`.
///
/// `runtime_base` is the tag of the first runtime entry (the count of builder
/// entries); a runtime entry is marked sent when its SUBACK arrives.
/// While the window is full, or `minimq` reports no room, the connection is driven
/// and inbound publishes keep flowing to the receive channel.
#[allow(clippy::too_many_arguments)]
async fn subscribe_paced(
    conn: &mut Conn<'_>,
    shared: &Shared,
    window: &mut SubWindow,
    runtime_base: usize,
    tag: usize,
    topic: &str,
    qos: QoS,
    deadline: Instant,
) -> Result<(), &'static str> {
    loop {
        while !window.has_room() {
            subscription_progress(conn, shared, window, runtime_base, deadline).await?;
        }
        match subscribe_filter(conn, topic, qos, deadline).await {
            Ok(op) => {
                window.track(tag, (op, qos));
                return Ok(());
            }
            Err(SubscribeError::NoRoom) => {
                subscription_progress(conn, shared, window, runtime_base, deadline).await?;
            }
            Err(SubscribeError::Failed(reason)) => return Err(reason),
        }
    }
}

/// Drives the connection until every sent SUBSCRIBE is acknowledged.
async fn drain_subscriptions(
    conn: &mut Conn<'_>,
    shared: &Shared,
    window: &mut SubWindow,
    runtime_base: usize,
    deadline: Instant,
) -> Result<(), &'static str> {
    while !window.is_empty() {
        subscription_progress(conn, shared, window, runtime_base, deadline).await?;
    }
    Ok(())
}

/// One bounded step of session progress while SUBACKs are outstanding.
///
/// A refused subscription surfaces as the session error of `poll()`, which ends the
/// session like a failed send. Inbound publishes that arrive meanwhile are forwarded.
async fn subscription_progress(
    conn: &mut Conn<'_>,
    shared: &Shared,
    window: &mut SubWindow,
    runtime_base: usize,
    deadline: Instant,
) -> Result<(), &'static str> {
    match with_deadline(deadline, conn.poll()).await {
        Ok(Ok(Some(inbound))) => forward_inbound(shared, &inbound),
        Ok(Ok(None)) => {}
        Ok(Err(e)) => {
            log::warn!("[mqtt] subscribe not acknowledged: {:?}", e);
            return Err("subscribe failed");
        }
        Err(_) => {
            log::warn!("[mqtt] subscribe acknowledgement stalled");
            return Err(TRANSPORT_STALLED);
        }
    }
    window
        .settle(
            |(op, _)| {
                if conn.is_complete(op) {
                    AckStatus::Complete
                } else if conn.is_invalidated(op) {
                    AckStatus::Lost
                } else {
                    AckStatus::Pending
                }
            },
            |tag, (_, qos)| {
                if let Some(index) = tag.checked_sub(runtime_base) {
                    shared
                        .runtime_subscriptions
                        .lock(|s| s.borrow_mut().mark_sent(index, qos));
                }
            },
        )
        .map_err(|_| "subscribe failed")
}

/// Sends one SUBSCRIBE and returns its operation handle, bounded by `deadline`.
///
/// It does **not** wait for the SUBACK: `minimq` only enqueues and flushes the packet,
/// which then occupies one of its in-flight slots until the broker answers. The caller
/// tracks the handle in a [`SubWindow`] and drives the connection until it completes;
/// a refusal shows up there as a session error.
async fn subscribe_filter(
    conn: &mut Conn<'_>,
    topic: &str,
    qos: QoS,
    deadline: Instant,
) -> Result<Op, SubscribeError> {
    let filter = TopicFilter::new(topic)
        .options(SubscriptionOptions::default().maximum_qos(to_minimq_qos(qos)));
    match with_deadline(deadline, conn.subscribe(&[filter], &[])).await {
        Ok(Ok(op)) => Ok(op),
        Ok(Err(MinimqError::Resource(ResourceError::InflightExhausted))) => {
            Err(SubscribeError::NoRoom)
        }
        Ok(Err(e)) => {
            log::warn!("[mqtt] subscribe {} failed: {:?}", topic, e);
            Err(SubscribeError::Failed("subscribe failed"))
        }
        Err(_) => {
            log::warn!("[mqtt] subscribe {} stalled", topic);
            Err(SubscribeError::Failed(TRANSPORT_STALLED))
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

/// Samples the link every [`NETWORK_POLL_INTERVAL`] and returns the first edge the
/// gate reports: `NetworkUp` once the Wi-Fi link and the IP configuration are both up,
/// `NetworkDown` once either is gone.
///
/// Cancel-safe: the gate is updated synchronously with each sample, so dropping the
/// future between samples loses nothing. Dropping the racing `serve()` future drops
/// the `minimq` connection; the transport is aborted next and `Session::connect`
/// resets the outbound state, so a publish cancelled here is lost (documented) but
/// the session stays reusable.
async fn next_gate_edge(gate: &mut NetworkGate, stack: &Stack<'static>) -> SessionInput {
    loop {
        if let Some(edge) = gate.feed(network_up(stack)) {
            return edge;
        }
        Timer::after(NETWORK_POLL_INTERVAL).await;
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
