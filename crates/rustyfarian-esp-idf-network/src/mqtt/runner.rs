//! The ESP-IDF session runner: network gate, retry pacing and every esp-mqtt
//! call in one place (ADR 022 decisions 2 and 10, bug 010).
//!
//! esp-mqtt cannot be told to wait: `EspMqttClient::new` starts the client task,
//! which attempts a connection immediately and reports `Host is unreachable`
//! when the station has no IP yet (bug 010). The runner therefore creates the
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
//! | `ConnectWork` | the per-connect helper thread |
//!
//! Auto-reconnect is off (`reconnect_timeout: None` in the client
//! configuration), so esp-mqtt makes no attempt of its own: an attempt happens
//! only after this thread asks for one, and none happens while the link is down
//! or suspended.
//!
//! # Threading (ADR 017)
//!
//! This is the only thread that calls `esp_mqtt_client_*`. It never takes the
//! client mutex — the `reconnect`, `disconnect` and `stop` calls take the raw
//! handle and, except for `stop`, only set an event bit, so the runner cannot
//! wait on esp-mqtt's `api_lock` while the mqtt task is parked in an event
//! dispatch. The event-loop thread only reports inputs, and the connect helper
//! only runs the connect work; both hand their results to this thread.

use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use esp_idf_svc::handle::RawHandle as _;
use esp_idf_svc::mqtt::client::{
    EspMqttClient, EspMqttConnection, LwtConfiguration, MqttClientConfiguration,
};
use juggler::mqtt::{NetworkGate, SessionAction, SessionInput, SessionToken};

use super::session::Session;
use super::{to_esp_qos, LwtParts, SubscribableClient, CONNECT_THREAD_STACK_SIZE};

/// Stack of the runner thread: it only logs, reads a channel and calls the
/// handful of esp-mqtt entry points above (no recursion, no payloads).
pub(super) const RUNNER_STACK_SIZE: usize = 4096;

/// One message to the runner.
pub(super) enum Msg {
    /// A session input from the event loop or the connect helper.
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
    /// this only once the station has an IP (bug 010).
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
            // machine paces every attempt (ADR 022, bug 010), so esp-mqtt must
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

/// Drives the session machine on its own thread; see the module docs.
pub(super) struct Runner {
    pub(super) session: Arc<Session>,
    pub(super) inputs: Receiver<Msg>,
    /// Hands the client's event connection to the event-loop thread.
    pub(super) connection: Sender<EspMqttConnection>,
    /// `None` once every handle is gone; the runner then shuts the session down.
    pub(super) alive: Weak<()>,
    pub(super) config: ClientConfig,
    /// Raw `esp_mqtt_client_handle_t` of the live client, as an address because
    /// raw pointers are not `Send` (same idiom as the station netif handle).
    pub(super) handle: Option<usize>,
    /// Link readiness at build time, fed into the gate before any event.
    pub(super) seed_ready: bool,
    /// The broker's session-present flag of the current connection; the machine
    /// does not carry it in `ConnectWork`, but `on_connect` reports it.
    pub(super) session_present: bool,
    pub(super) subscribe_topics: Vec<(String, juggler::mqtt::QoS)>,
    pub(super) startup_topic: Option<String>,
    pub(super) on_connect: Option<Arc<std::sync::Mutex<super::OnConnectCallback>>>,
}

impl Runner {
    /// Spawns the runner on its own thread.
    pub(crate) fn spawn(runner: Runner) -> anyhow::Result<()> {
        std::thread::Builder::new()
            .name("mqtt-net".into())
            .stack_size(RUNNER_STACK_SIZE)
            .spawn(move || runner.run())
            .context("failed to spawn the MQTT runner thread")?;
        Ok(())
    }

    fn run(mut self) {
        // Seeds the gate with the readiness the link had at build time; the
        // sink registered by `build` was in place first, so an event that landed
        // in between is already queued and is applied after the seed.
        let mut gate = NetworkGate::new();
        let mut retry_at: Option<Instant> = None;
        let mut announced_wait = false;

        if let Some(input) = gate.feed(self.seed_ready) {
            self.act(input, &mut retry_at);
        }

        loop {
            if retry_due(retry_at) {
                retry_at = None;
                self.act(SessionInput::RetryElapsed, &mut retry_at);
            }
            if self.alive.upgrade().is_none() {
                log::info!("[mqtt] all handles dropped, stopping the session");
                self.shutdown();
                return;
            }

            match self.inputs.recv_timeout(wait_for(retry_at)) {
                Ok(Msg::Input(input)) => self.act(input, &mut retry_at),
                Ok(Msg::Ready(ready)) => {
                    if let Some(input) = gate.feed(ready) {
                        if input == SessionInput::NetworkDown {
                            log::info!("[mqtt] waiting for network");
                            announced_wait = true;
                        } else if announced_wait {
                            announced_wait = false;
                            log::info!("[mqtt] network up, connecting");
                        }
                        self.act(input, &mut retry_at);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    log::info!("[mqtt] runner inputs closed, stopping the session");
                    self.shutdown();
                    return;
                }
            }
        }
    }

    /// Feeds one input to the machine and performs its action.
    fn act(&mut self, input: SessionInput, retry_at: &mut Option<Instant>) {
        if let SessionInput::Connected { session_present } = input {
            self.session_present = session_present;
        }
        match self.session.poll(input) {
            SessionAction::None => {}
            SessionAction::Connect => self.connect(),
            SessionAction::ConnectWork { token, subscribe } => self.connect_work(token, subscribe),
            SessionAction::Close => self.close(),
            SessionAction::Wait { ms } => {
                log::warn!("[mqtt] reconnecting in {} ms", ms);
                *retry_at = Some(Instant::now() + Duration::from_millis(ms));
            }
        }
    }

    /// Creates the client on the first attempt, reconnects it afterwards.
    fn connect(&mut self) {
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

    /// Runs one session's connect work on the helper thread (ADR 017).
    fn connect_work(&mut self, token: SessionToken, subscribe: bool) {
        let subscribe = subscribe && !self.subscribe_topics.is_empty();
        if !subscribe && self.startup_topic.is_none() && self.on_connect.is_none() {
            self.session.send(SessionInput::WorkDone(token));
            return;
        }

        let session = Arc::clone(&self.session);
        let outbox = Arc::clone(self.session.outbox());
        let topics = self.subscribe_topics.clone();
        let startup_topic = self.startup_topic.clone();
        let on_connect = self.on_connect.clone();
        let session_present = self.session_present;

        // The helper does the connect work under one lock of the client, off this
        // thread and off the event-loop thread: esp-mqtt holds its recursive
        // `api_lock` for the whole connect handshake, and esp-idf-svc parks the
        // mqtt task until the event loop calls `next()` again, so neither thread
        // may take the client mutex (ADR 017).
        let spawned = std::thread::Builder::new()
            .name("mqtt-connect".into())
            .stack_size(CONNECT_THREAD_STACK_SIZE)
            .spawn(move || {
                let mut guard = match outbox.lock() {
                    Ok(guard) => guard,
                    Err(e) => {
                        log::warn!("[mqtt] connect thread: client mutex poisoned: {}", e);
                        session.send(SessionInput::WorkFailed(token));
                        return;
                    }
                };
                super::connect_work(
                    &mut guard,
                    &session,
                    token,
                    subscribe.then_some(topics.as_slice()),
                    startup_topic.as_deref(),
                    on_connect.as_deref(),
                    session_present,
                );
            })
            .is_ok();
        if !spawned {
            log::error!("[mqtt] could not spawn the connect helper thread");
            self.session.send(SessionInput::WorkFailed(token));
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

    /// Stops the client; dropping it makes the event-loop thread's `next()`
    /// return, so that thread exits too.
    fn shutdown(&mut self) {
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
