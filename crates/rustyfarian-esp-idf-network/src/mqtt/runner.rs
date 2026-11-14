//! The ESP-IDF session runner: network gate, retry pacing, the connect work and
//! every esp-mqtt call in one place (ADR 022 decisions 2 and 10).
//!
//! esp-mqtt cannot be told to wait: `EspMqttClient::new` starts the client task,
//! which attempts a connection immediately and reports `Host is unreachable`
//! when the station has no IP yet. The runner therefore creates the
//! client on the first `NetworkUp` — the station is associated and holds an IP —
//! and from then on performs every [`SessionAction`] the shared
//! [`SessionMachine`] asks for:
//!
//! | Action | esp-mqtt call |
//! |:--|:--|
//! | `Connect` (first) | `EspMqttClient::new`, which starts the task |
//! | `Connect` (later) | `esp_mqtt_client_reconnect` (sets a bit) |
//! | `Close` | `esp_mqtt_client_disconnect` (sets a bit) |
//! | `Wait { ms }` | a deadline in this thread |
//! | `ConnectWork` | this thread, one step per turn ([`ConnectWorkMachine`]) |
//! | `Ready` | none; [`Session::poll`] already woke `wait_connected` |
//! | `Ignored` | none; logged, and an error after a long run of them |
//!
//! Auto-reconnect is off (`reconnect_timeout: None` in the client
//! configuration), so esp-mqtt makes no attempt of its own: an attempt happens
//! only after this thread asks for one, and none happens while the link is down
//! or suspended.
//!
//! # Threading (ADR 017)
//!
//! This is the only thread that calls `esp_mqtt_client_*`. The `reconnect`,
//! `disconnect` and `stop` calls take the raw handle and, except for `stop`,
//! only set an event bit. The connect work — SUBSCRIBE for every builder
//! topic, the startup publish, the user's `on_connect`, then the runtime
//! catch-up — runs here too, as one [`ConnectWorkMachine`] action per turn:
//! each action is one `with_client_mut` hold around one esp-mqtt call, and
//! between two actions the runner drains up to [`DRAIN_PER_STEP`] inputs, so
//! `NetworkDown`, `Disconnected`, `Shutdown`, a retry and the dropped-handle
//! check are at most one step late. The runner takes the client lock only for
//! one bounded connect step at a time, releases it and yields after every
//! step, so a publisher thread at the runner's priority or above that waits
//! for the lock (`enqueue`, `publish_acked`, the outbox drain; any
//! `std::thread`-spawned publisher) gets it before the next step, while
//! readiness (`try_publish`, `is_connected`, `wait_connected`) stays gated on
//! `WorkDone`. It never holds the lock while a callback could be
//! parking the mqtt task: the event-loop thread never takes it (ADR 017), and
//! esp-mqtt's recursive `api_lock`, which `subscribe`, `enqueue` and `stop`
//! take, is released once that thread calls `next()` again — it never waits
//! on this one, so a step cannot deadlock. The event-loop thread only reports
//! inputs; it never acts on the session.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use esp_idf_svc::handle::RawHandle as _;
use esp_idf_svc::mqtt::client::{
    EspMqttClient, EspMqttConnection, LwtConfiguration, MqttClientConfiguration,
};
use juggler::mqtt::lines::{Connecting, Reconnecting, WaitingForNetwork};
use juggler::mqtt::{
    ConnectWorkMachine, ConnectWorkPlan, NetworkGate, SessionAction, SessionInput, SessionToken,
    WorkAction, WorkInput,
};

use super::connect_port::IdfConnectPort;
use super::session::Session;
use super::task_name::spawn_named;
use super::{to_esp_qos, LwtParts, SubscribableClient, RUNNER_STACK_SIZE};

/// Ignored inputs in a row after which the runner reports that it and the
/// machine have fallen out of step (ADR 023 decisions 5 and 6).
const IGNORED_IN_A_ROW_LIMIT: u32 = 16;

/// Most inputs the runner takes between two connect steps, so a flood of
/// inputs cannot starve the work and a pending input is seen within one step.
const DRAIN_PER_STEP: usize = 16;

/// One message to the runner.
pub(super) enum Msg {
    /// A session input from the event loop, or one this thread reported
    /// itself through [`Session::send`].
    Input(SessionInput),
    /// The station became ready (`true`) or stopped being ready (`false`).
    Ready(bool),
}

/// Everything the runner needs to create the esp-mqtt client later.
///
/// The credential copies live only until the client exists: `build` hands over
/// a value, [`ClientConfig::create`] consumes it, and the strings are dropped
/// right after `EspMqttClient::new` copied them into the C library.
pub(super) struct ClientConfig {
    pub(super) url: String,
    pub(super) client_id: String,
    pub(super) keep_alive_secs: u64,
    pub(super) task_stack_size: usize,
    pub(super) username: Option<String>,
    pub(super) password: Option<String>,
    pub(super) lwt: Option<LwtParts>,
}

impl ClientConfig {
    /// Creates and starts the client, returning it and its event connection.
    ///
    /// esp-idf-svc starts the client task here, which is why the runner calls
    /// this only once the station has an IP.
    ///
    /// The configuration stays usable after a failure, so a retry can succeed
    /// once whatever refused it (usually heap) recovers; the credential copies
    /// are dropped as soon as the client exists.
    fn create(&mut self) -> anyhow::Result<(EspMqttClient<'static>, EspMqttConnection)> {
        let lwt = self.lwt.as_ref().map(|lwt| LwtConfiguration {
            topic: lwt.topic.as_str(),
            payload: lwt.payload.as_slice(),
            qos: to_esp_qos(lwt.qos),
            retain: lwt.retain,
        });

        let mqtt_cfg = MqttClientConfiguration {
            client_id: Some(self.client_id.as_str()),
            keep_alive_interval: Some(Duration::from_secs(self.keep_alive_secs)),
            // `None` makes esp-idf-svc set `disable_auto_reconnect`: the session
            // machine paces every attempt (ADR 022), so esp-mqtt must
            // not retry on its own; `esp_mqtt_client_reconnect` starts one.
            reconnect_timeout: None,
            task_stack: self.task_stack_size,
            lwt,
            username: self.username.as_deref(),
            password: self.password.as_deref(),
            ..Default::default()
        };

        let created =
            EspMqttClient::new(&self.url, &mqtt_cfg).context("failed to create EspMqttClient")?;
        // esp_mqtt_client_init() strdup()'d what it needs; the copies are done.
        self.username = None;
        self.password = None;
        self.lwt = None;
        Ok(created)
    }
}

/// Connect work in progress: the machine plus the input to feed it next.
pub(super) struct PendingWork {
    machine: ConnectWorkMachine,
    next: WorkInput,
}

/// Drives the session machine on its own thread; see the module docs.
pub(super) struct Runner {
    pub(super) session: Arc<Session>,
    pub(super) inputs: Receiver<Msg>,
    /// Hands the client's event connection to the event-loop thread.
    pub(super) connection: Sender<EspMqttConnection>,
    /// `None` once every handle is gone; the runner then shuts the session down.
    pub(super) alive: Weak<()>,
    pub(super) config: ClientConfig,
    /// Broker host and port for the shared `connecting to` line; separate from
    /// `config`, which also carries the credentials.
    pub(super) broker_host: String,
    pub(super) broker_port: u16,
    /// Raw `esp_mqtt_client_handle_t` of the live client, as an address because
    /// raw pointers are not `Send` (same idiom as the station netif handle).
    pub(super) handle: Option<usize>,
    /// Link readiness at build time, fed into the gate before any event.
    pub(super) seed_ready: bool,
    /// Inputs the machine ignored in a row; see [`IGNORED_IN_A_ROW_LIMIT`].
    pub(super) ignored_in_a_row: u32,
    pub(super) subs: Arc<super::Subscriptions>,
    /// The message published on every (re)connect, borrowed per step.
    pub(super) startup: Option<super::StartupMessage>,
    /// The user's connect callback; this thread is its only caller.
    pub(super) on_connect: Option<super::OnConnectCallback>,
    /// Connect work in progress: the machine plus the input to feed it next.
    pub(super) work: Option<PendingWork>,
    /// Turns readiness observations into `NetworkUp` / `NetworkDown`.
    pub(super) gate: NetworkGate,
    /// When the next connect attempt is due, after a `Wait`.
    pub(super) retry_at: Option<Instant>,
    /// `true` once the input channel closed; the runner then shuts down.
    pub(super) inputs_closed: bool,
}

impl Runner {
    /// Spawns the runner on its own thread, named `mqtt-net` for FreeRTOS.
    pub(crate) fn spawn(runner: Runner) -> anyhow::Result<()> {
        spawn_named(c"mqtt-net", RUNNER_STACK_SIZE, move || runner.run())
            .context("failed to spawn the MQTT runner thread")?;
        Ok(())
    }

    fn run(mut self) {
        // Seeds the gate with the readiness the link had at build time; the
        // sink registered by `build` was in place first, so an event that landed
        // in between is already queued and is applied after the seed.
        if let Some(input) = self.gate.feed(self.seed_ready) {
            self.act(input);
        }

        loop {
            if retry_due(self.retry_at) {
                self.retry_at = None;
                self.act(SessionInput::RetryElapsed);
            }
            if self.alive.upgrade().is_none() {
                log::info!("[mqtt] all handles dropped, stopping the session");
                self.shutdown();
                return;
            }

            // While connect work is pending the runner never blocks: it takes
            // the inputs already queued, then performs one step.
            if self.work.is_some() {
                self.drain_inputs();
                if self.inputs_closed {
                    log::info!("[mqtt] runner inputs closed, stopping the session");
                    self.shutdown();
                    return;
                }
                self.work_turn();
                continue;
            }

            match self.inputs.recv_timeout(wait_for(self.retry_at)) {
                Ok(msg) => self.handle_msg(msg),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    log::info!("[mqtt] runner inputs closed, stopping the session");
                    self.shutdown();
                    return;
                }
            }
        }
    }

    /// Takes up to [`DRAIN_PER_STEP`] queued inputs without waiting.
    fn drain_inputs(&mut self) {
        for _ in 0..DRAIN_PER_STEP {
            match self.inputs.try_recv() {
                Ok(msg) => self.handle_msg(msg),
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    self.inputs_closed = true;
                    return;
                }
            }
        }
    }

    /// Feeds one message to the gate or the machine; shared by the blocking
    /// wait and the drain between connect steps.
    fn handle_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Input(input) => self.act(input),
            Msg::Ready(ready) => {
                if let Some(input) = self.gate.feed(ready) {
                    if input == SessionInput::NetworkDown {
                        log::info!("{}", WaitingForNetwork);
                    }
                    self.act(input);
                }
            }
        }
    }

    /// Feeds one input to the machine and performs its action.
    fn act(&mut self, input: SessionInput) {
        if matches!(
            input,
            SessionInput::Disconnected | SessionInput::NetworkDown
        ) {
            self.subs.go_offline();
        }
        let action = self.session.poll(input);
        if action == SessionAction::Ignored {
            log::debug!("[mqtt] session ignored {:?}", input);
            self.ignored_in_a_row = self.ignored_in_a_row.saturating_add(1);
            if self.ignored_in_a_row == IGNORED_IN_A_ROW_LIMIT {
                log::error!(
                    "[mqtt] the session ignored {} inputs in a row; runner and machine are out of step",
                    IGNORED_IN_A_ROW_LIMIT
                );
            }
            return;
        }
        self.ignored_in_a_row = 0;
        match action {
            // `Session::poll` already woke `wait_connected`.
            SessionAction::None | SessionAction::Ready | SessionAction::Ignored => {}
            SessionAction::Connect => self.connect(),
            SessionAction::ConnectWork {
                token,
                subscribe,
                session_present,
            } => self.connect_work(token, subscribe, session_present),
            SessionAction::Close => self.close(),
            SessionAction::Wait { ms } => {
                log::warn!("{}", Reconnecting { ms });
                self.retry_at = Some(Instant::now() + Duration::from_millis(ms));
            }
        }
    }

    /// Creates the client on the first attempt, reconnects it afterwards.
    fn connect(&mut self) {
        log::info!(
            "{}",
            Connecting {
                host: self.broker_host.as_str(),
                port: self.broker_port
            }
        );
        match self.handle {
            Some(handle) => kick(handle),
            None => self.create_client(),
        }
    }

    fn create_client(&mut self) {
        match self.config.create() {
            Ok((client, connection)) => {
                self.handle = Some(client.handle() as usize);
                if self.connection.send(connection).is_err() {
                    log::error!("[mqtt] event loop is gone; the MQTT session cannot run");
                }
                // Starts the task, which connects at once: no `reconnect` needed.
                self.session.attach_client(SubscribableClient(client));
                log::info!("[mqtt] client started");
            }
            Err(e) => {
                // Report the failed attempt so the machine paces a retry.
                log::error!("[mqtt] client creation failed: {:#}", e);
                self.session.send(SessionInput::Disconnected);
            }
        }
    }

    /// Installs one session's connect work; the turns that follow perform it
    /// one action at a time (see [`Runner::work_turn`]).
    ///
    /// An empty plan is installed too: the machine's closing step still runs
    /// the runtime catch-up and marks the tracker live, so a kept session with
    /// nothing else to send sends its stored runtime requests.
    fn connect_work(&mut self, token: SessionToken, subscribe: bool, session_present: bool) {
        if let Some(old) = self.work.take() {
            log::debug!(
                "[mqtt] connect work for an older session abandoned ({:?})",
                old.machine.token()
            );
        }
        self.subs.begin_connect(subscribe);
        let plan = ConnectWorkPlan::new(
            subscribe,
            self.subs.len(),
            self.startup.is_some(),
            self.on_connect.is_some(),
        );
        self.work = Some(PendingWork {
            machine: ConnectWorkMachine::new(token, plan, session_present),
            next: WorkInput::Start,
        });
    }

    /// Performs exactly one action of the pending connect work.
    ///
    /// The session is probed before every action: `go_offline`, this probe and
    /// the closing step all run on this thread, so the closing step can never
    /// follow a `go_offline` for the same session.
    fn work_turn(&mut self) {
        let Some(mut work) = self.work.take() else {
            return;
        };
        let token = work.machine.token();
        let input = if self.session.is_current(token) {
            work.next
        } else {
            WorkInput::SessionEnded
        };
        match work.machine.poll(input) {
            action @ (WorkAction::Step(_) | WorkAction::Finish) => {
                work.next = self.perform(action, work.machine.session_present());
                self.work = Some(work);
                // The client lock is released now, but the next turn re-takes
                // it at once: `run` never blocks while work is pending, and a
                // FreeRTOS mutex give only makes an equal-priority waiter
                // ready, it does not preempt the giver (a higher-priority
                // waiter already ran). One yield per step hands the CPU to a
                // same-priority publisher that waits for the lock
                // (`enqueue`, `publish_acked`, the outbox drain), so it
                // takes the lock before the next step.
                std::thread::yield_now();
            }
            WorkAction::Report(report) => {
                if matches!(report, SessionInput::WorkDone(_)) {
                    log::debug!(
                        "[mqtt] mqtt-net stack: {} B unused",
                        super::stack_unused_here()
                    );
                }
                // `WorkDone` -> `Ready`: `Session::poll` wakes `wait_connected`
                // here, no channel round trip; `WorkFailed` -> `Close`.
                self.act(report);
            }
            WorkAction::Abandon => {
                log::debug!("[mqtt] connect work abandoned: the session ended");
            }
            WorkAction::Ignored => {
                log::error!("[mqtt] connect work out of step; dropping it");
            }
        }
    }

    /// Performs one `Step` or `Finish` through the client: one hold of the
    /// outbox's client lock around one esp-mqtt call.
    fn perform(&self, action: WorkAction, session_present: bool) -> WorkInput {
        let subs = &*self.subs;
        let startup = self.startup.as_ref();
        let on_connect = self.on_connect.as_ref();
        let performed = self.session.outbox().with_client_mut(|client| {
            juggler::mqtt::perform(
                &mut IdfConnectPort {
                    client,
                    subs,
                    startup,
                    on_connect,
                },
                action,
                session_present,
            )
        });
        match performed {
            Some(Some(input)) => input,
            // Unreachable for `Step` and `Finish`, which always ask for a call.
            Some(None) => WorkInput::StepFailed,
            None => {
                log::error!("[mqtt] connect work: no client attached to the session");
                WorkInput::StepFailed
            }
        }
    }

    /// Performs a `Close`: tear the transport down, or report the end here.
    ///
    /// A client that still holds a transport ends its own session and posts
    /// `Disconnected`, which the event loop forwards. One that already dropped
    /// it (it sits in esp-mqtt's reconnect pause) posts nothing, so this thread
    /// reports `Disconnected` itself — otherwise the machine would wait for an
    /// input that never comes and would never reconnect. The machine says which
    /// case applies (see `SessionMachine::driver_idle`); closing an idle client
    /// also skips the `disconnect` call, which would only set a stale bit.
    fn close(&self) {
        let Some(handle) = self.handle else {
            // No client at all, so no event will report the end either.
            self.session.send(SessionInput::Disconnected);
            return;
        };
        if self.session.driver_idle() {
            log::debug!("[mqtt] closing a session with no transport");
            self.session.send(SessionInput::Disconnected);
            return;
        }
        // SAFETY: `handle` belongs to the client this runner owns; the call only
        // sets the disconnect event bit, so it cannot block on `MQTT_API_LOCK`.
        let result = unsafe { esp_idf_svc::sys::esp_mqtt_client_disconnect(handle as *mut _) };
        if result != esp_idf_svc::sys::ESP_OK {
            log::warn!("[mqtt] closing the session failed: {}", result);
        }
    }

    /// Ends the session through the machine, then stops the client; dropping
    /// it makes the event-loop thread's `next()` return, so that thread exits too.
    ///
    /// Never runs under the client lock.
    fn shutdown(&mut self) {
        // Nothing may touch the client after `stop`, so pending work goes first.
        self.work = None;
        // The machine answers `Close` for a busy session, which `act` performs;
        // nothing reads the `Disconnected` that follows, and a retry deadline
        // is never waited for again.
        self.act(SessionInput::Shutdown);
        if let Some(handle) = self.handle.take() {
            // Wake the task out of its reconnect wait first: otherwise `stop`
            // waits out the whole pause (`reconnect_timeout_ms / 2`, 5 s).
            kick(handle);
            // SAFETY: the client is still alive (the outbox owns it) and this is
            // not the mqtt task; stopping from here is the documented use.
            let result = unsafe { esp_idf_svc::sys::esp_mqtt_client_stop(handle as *mut _) };
            if result != esp_idf_svc::sys::ESP_OK {
                log::warn!("[mqtt] stopping the client failed: {}", result);
            }
        }
        // Drops the client (and with it the C client), unblocking the event loop.
        drop(self.session.take_client());
    }
}

/// Asks esp-mqtt for an immediate reconnect attempt.
///
/// It only sets a bit and returns; esp-mqtt ignores the request when the client
/// is not waiting to reconnect (it is already connecting, for example), which
/// is not an error here.
fn kick(handle: usize) {
    // SAFETY: `handle` belongs to the client the runner owns; the call takes no
    // `MQTT_API_LOCK` and only sets the reconnect event bit.
    let result = unsafe { esp_idf_svc::sys::esp_mqtt_client_reconnect(handle as *mut _) };
    if result != esp_idf_svc::sys::ESP_OK {
        log::debug!("[mqtt] reconnect request ignored: {}", result);
    }
}

/// `true` once the retry pause has elapsed.
fn retry_due(retry_at: Option<Instant>) -> bool {
    retry_at.is_some_and(|at| Instant::now() >= at)
}

/// How long the runner waits for the next input, capped at one poll tick.
///
/// The cap bounds three things in one place: how late a retry can start, how
/// late a dropped handle stops the session, and how late a link change is seen
/// if its notification were ever lost. A retry pause is 5 s or more, so the cap
/// costs only a wake-up, never an extra attempt (the deadline is checked before
/// the wait).
fn wait_for(retry_at: Option<Instant>) -> Duration {
    const POLL_TICK: Duration = Duration::from_millis(500);
    match retry_at {
        Some(at) => at.saturating_duration_since(Instant::now()).min(POLL_TICK),
        None => POLL_TICK,
    }
}
