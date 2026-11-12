//! Library-owned station supervision for the bare-metal tier.
//!
//! The firmware declares one `static LINK: WifiLink = WifiLink::new();`, spawns
//! [`link_task`] with the [`WifiController`] and the `Stack` from
//! `AsyncWifiHandle`, and never writes a reconnect loop again. `WifiLink::run` drives the shared
//! [`LinkMachine`]: connect, wait for the link to drop, back off 1 s doubling to
//! 30 s, reconnect. A controller that is already associated when `run` starts is
//! adopted as connected and counted as an association.
//!
//! # Readiness
//!
//! While associated, `run` checks the stack's IPv4 configuration every
//! [`NETWORK_POLL_MS`] and reports it to the machine, so [`WifiLink::is_ready`]
//! and [`WifiLink::wait_connected`] mean "associated with an IP", the same as on
//! the ESP-IDF tier. The stack is polled because embassy-net keeps only one state
//! waker, which belongs to the firmware.
//! `WifiLink` implements [`juggler::wifi::NetworkLink`] (ADR 022).
//!
//! # Suspend and resume
//!
//! [`WifiLink::suspend`] and [`WifiLink::resume`] are non-blocking signals for the
//! main loop: the state flips at once and `run` reacts at whichever await it is
//! in (connect, wait for disconnect, back-off). Suspend disconnects the station
//! (bounded by [`SUSPEND_DISCONNECT_TIMEOUT_MS`]) and parks the supervisor, with
//! the radio still powered (unlike the ESP-IDF tier, which stops it); no
//! reconnect happens until `resume`, which connects with a fresh back-off.
//! [`WifiLink::is_suspended`] turns `true` once that disconnect has finished, so a
//! firmware can wait a bounded time before deep sleep:
//!
//! ```ignore
//! LINK.suspend();
//! let _ = with_timeout(
//!     Duration::from_millis(SUSPEND_DISCONNECT_TIMEOUT_MS),
//!     LINK.wait_suspended(),
//! )
//! .await;
//! ```
//!
//! # Lifecycle
//!
//! Run at most one `run` per `WifiLink` at a time; the controller is a singleton,
//! so a second concurrent supervisor cannot exist anyway.
//! `run` resets the machine on entry, so a supervisor that was cancelled (for
//! example inside a `select`) can be started again on the same `static`; a
//! suspend issued before `run` starts is kept.

use core::cell::RefCell;
use core::net::Ipv4Addr;
use core::pin::pin;
use core::task::Poll;

use embassy_futures::select::{select, select3, Either, Either3};
use embassy_net::Stack;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::signal::Signal;
use embassy_time::{with_timeout, Duration, Timer};
use esp_radio::wifi::ConnectionError;
use esp_radio::wifi::DisconnectReason;
use esp_radio::wifi::WifiController;
use juggler::waker_slots::{poll_ready, ReadyPoll, WakerSlots};
use juggler::wifi::lines::{ByDebug, Disconnected, Lifecycle, LinkUp, NetworkReady, Reconnecting};
use juggler::wifi::{
    IdleWait, JoinedAp, LinkAction, LinkHandover, LinkInput, LinkState, NetworkLink,
    CREDENTIALS_HINT,
};

/// Upper bound on the disconnect that [`WifiLink::suspend`] triggers (milliseconds);
/// [`WifiLink::is_suspended`] turns `true` when it finishes or this bound passes.
pub const SUSPEND_DISCONNECT_TIMEOUT_MS: u64 = 500;

/// How often a parked or idle supervisor re-checks the controller (milliseconds).
const RECHECK_MS: u64 = 1_000;

/// How often `run` checks the stack's IPv4 configuration while associated (milliseconds).
pub const NETWORK_POLL_MS: u64 = 500;

/// Tasks that [`WifiLink::wait_connected`] wakes directly; more waiters re-check
/// every [`NETWORK_POLL_MS`] instead.
const READY_WAITERS: usize = 4;

/// Station link supervisor; one per firmware, declared as a `static`.
pub struct WifiLink {
    /// The machine and the action `suspend`/`resume` hand to `run`, changed
    /// together under one lock so the handover always matches the latest call.
    handover: Mutex<CriticalSectionRawMutex, RefCell<LinkHandover>>,
    /// Wakes `run` to take the handover; carries no data.
    wake: Signal<CriticalSectionRawMutex, ()>,
    /// Tasks in [`wait_connected`](Self::wait_connected); a full set refuses, never mass-wakes.
    ready_wakers: Mutex<CriticalSectionRawMutex, RefCell<WakerSlots<READY_WAITERS>>>,
    /// Tasks in [`wait_suspended`](Self::wait_suspended); woken when a park ends.
    suspend_wakers: Mutex<CriticalSectionRawMutex, RefCell<WakerSlots<READY_WAITERS>>>,
}

impl WifiLink {
    /// A supervisor with the shared back-off defaults.
    pub const fn new() -> Self {
        Self {
            handover: Mutex::new(RefCell::new(LinkHandover::new())),
            wake: Signal::new(),
            ready_wakers: Mutex::new(RefCell::new(WakerSlots::new())),
            suspend_wakers: Mutex::new(RefCell::new(WakerSlots::new())),
        }
    }

    /// Current link state as the supervisor last saw it; [`LinkState::Suspended`]
    /// from the moment [`suspend`](Self::suspend) is called.
    pub fn state(&self) -> LinkState {
        self.with(|h| h.state())
    }

    /// `true` while the station is associated.
    pub fn is_connected(&self) -> bool {
        self.state() == LinkState::Connected
    }

    /// `true` once a [`suspend`](Self::suspend) has taken the station down: the
    /// disconnect finished or gave up after [`SUSPEND_DISCONNECT_TIMEOUT_MS`].
    /// Turns `false` again on [`resume`](Self::resume).
    ///
    /// `true` therefore does not prove the association ended: when the disconnect timed
    /// out, the station may still be associated (the supervisor logs a warning for that
    /// case). Use it as "the supervisor stopped reconnecting", not as "the radio is off
    /// the air".
    pub fn is_suspended(&self) -> bool {
        self.with(|h| h.is_suspended())
    }

    /// Associations since boot, including one adopted when `run` starts.
    pub fn connect_count(&self) -> u32 {
        self.with(|h| h.connect_count())
    }

    /// `true` while associated with an IPv4 configuration.
    pub fn is_ready(&self) -> bool {
        self.with(|h| h.is_ready())
    }

    /// Completes once the station is associated and has its IPv4 configuration.
    ///
    /// Up to four waiting tasks are woken directly; further ones re-check every
    /// [`NETWORK_POLL_MS`], so many waiters never wake each other in a loop.
    pub async fn wait_connected(&self) {
        loop {
            let woken = core::future::poll_fn(|cx| {
                // Register first, then check: a readiness change in between still wakes us.
                let registered = self
                    .ready_wakers
                    .lock(|w| w.borrow_mut().register(cx.waker()));
                match poll_ready(registered, self.is_ready()) {
                    ReadyPoll::Ready => Poll::Ready(true),
                    ReadyPoll::Waiting => Poll::Pending,
                    ReadyPoll::Full => Poll::Ready(false),
                }
            })
            .await;
            if woken {
                return;
            }
            Timer::after_millis(NETWORK_POLL_MS).await;
        }
    }

    /// Completes once a [`suspend`](Self::suspend) has taken the station down,
    /// that is when [`is_suspended`](Self::is_suspended) is `true`.
    ///
    /// Returning does not prove the association ended: if the disconnect timed out after
    /// [`SUSPEND_DISCONNECT_TIMEOUT_MS`] the supervisor parks anyway, with the station
    /// possibly still associated.
    ///
    /// Has no timeout of its own: the disconnect it waits for is bounded by
    /// [`SUSPEND_DISCONNECT_TIMEOUT_MS`]. Stays pending if nothing was suspended,
    /// or if [`resume`](Self::resume) overtakes the suspend before the supervisor
    /// parked. Up to four waiting tasks are woken directly; further ones re-check
    /// every [`NETWORK_POLL_MS`].
    pub async fn wait_suspended(&self) {
        loop {
            let woken = core::future::poll_fn(|cx| {
                // Register first, then check: a park that ends in between still wakes us.
                let registered = self
                    .suspend_wakers
                    .lock(|w| w.borrow_mut().register(cx.waker()));
                match poll_ready(registered, self.is_suspended()) {
                    ReadyPoll::Ready => Poll::Ready(true),
                    ReadyPoll::Waiting => Poll::Pending,
                    ReadyPoll::Full => Poll::Ready(false),
                }
            })
            .await;
            if woken {
                return;
            }
            Timer::after_millis(NETWORK_POLL_MS).await;
        }
    }

    /// Disconnects the station and pauses reconnects until [`resume`](Self::resume).
    ///
    /// The radio stays powered: esp-radio has no stop, so unlike the ESP-IDF
    /// tier this does not power the radio down; only the reset of deep sleep does.
    ///
    /// Non-blocking and idempotent; callable from any task. The MQTT client is not
    /// told: it sends no DISCONNECT, so the broker publishes its will after
    /// 1.5 × keep-alive. Before deep sleep, wait (bounded) for
    /// [`is_suspended`](Self::is_suspended); the reset after sleep re-initialises
    /// the radio.
    pub fn suspend(&self) {
        // Readiness falls before the station goes down, as on the ESP-IDF tier.
        let (woken, change) = self.with(|h| (h.suspend(), h.take_ready_change()));
        self.report_ready(change, None);
        if woken {
            log::info!("{}", Lifecycle::SuspendRequested);
            self.wake.signal(());
        }
    }

    /// Leaves suspend and connects with a fresh back-off.
    ///
    /// Non-blocking and idempotent; a no-op unless suspended. Safe right after
    /// [`suspend`](Self::suspend), even before the disconnect ran: the supervisor
    /// keeps an association that is still up, or the attempt still in flight.
    pub fn resume(&self) {
        let (woken, change) = self.with(|h| (h.resume(), h.take_ready_change()));
        self.report_ready(change, None);
        if woken {
            log::info!("{}", Lifecycle::ResumeRequested);
            self.wake.signal(());
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut LinkHandover) -> R) -> R {
        self.handover.lock(|h| f(&mut h.borrow_mut()))
    }

    /// Feeds an input observed by `run` and reports the readiness edge it caused.
    fn poll(&self, input: LinkInput) -> LinkAction {
        self.poll_with_ip(input, None)
    }

    /// [`poll`](Self::poll), naming the station's address in the readiness line.
    fn poll_with_ip(&self, input: LinkInput, ip: Option<Ipv4Addr>) -> LinkAction {
        let (action, change) = self.with(|h| (h.poll(input), h.take_ready_change()));
        self.report_ready(change, ip);
        action
    }

    /// Hands a readiness edge from [`LinkHandover::take_ready_change`] to the waiters.
    fn report_ready(&self, change: Option<bool>, ip: Option<Ipv4Addr>) {
        if change == Some(true) {
            log::info!("{}", NetworkReady { ip });
            self.ready_wakers.lock(|w| w.borrow_mut().wake_all());
        }
    }

    /// Waits for `suspend`/`resume` and takes the action they handed over.
    async fn handed_over(&self) -> LinkAction {
        self.wake.wait().await;
        let action = self.with(|h| h.take());
        if action == LinkAction::Ignored {
            log::debug!("[wifi] handover overtaken by the supervisor, dropped");
        }
        action
    }

    /// Reports the stack's IPv4 configuration to the machine.
    fn report_network(&self, stack: Stack<'static>) {
        let up = stack.is_config_up();
        if up == self.is_ready() {
            return;
        }
        let ip = stack.config_v4().map(|config| config.address.address());
        self.poll_with_ip(
            if up {
                LinkInput::NetworkUp
            } else {
                LinkInput::NetworkDown
            },
            ip,
        );
    }

    /// Feeds an association to the machine and logs it with the joined AP.
    fn link_up(&self, adopted: bool, ap: Option<JoinedAp>) -> LinkAction {
        let action = self.poll(LinkInput::Connected);
        self.log_association(adopted, ap);
        action
    }

    /// Restarts supervision through [`LinkHandover::start`] and returns the first action.
    fn start(&self, associated: bool, ap: Option<JoinedAp>) -> LinkAction {
        let (action, change) = self.with(|h| (h.start(associated), h.take_ready_change()));
        self.report_ready(change, None);
        if associated {
            self.log_association(true, ap);
        }
        action
    }

    /// Logs an association the machine was just told about, with the AP when the driver reports it.
    fn log_association(&self, adopted: bool, ap: Option<JoinedAp>) {
        if self.state() == LinkState::Connected {
            log::info!(
                "{}",
                LinkUp {
                    count: self.connect_count(),
                    adopted,
                    ap
                }
            );
        } else {
            log::info!("[wifi] associated while suspended, disconnecting again");
        }
    }

    /// Supervises the station forever: initial association, every reconnect,
    /// suspend and resume.
    pub async fn run(
        &'static self,
        mut controller: WifiController<'static>,
        stack: Stack<'static>,
    ) -> ! {
        // A cancelled earlier `run` may have left the machine mid-attempt; keep a
        // suspend the firmware already asked for.
        self.wake.reset();
        let mut action = self.start(controller.is_connected(), joined_ap(&controller));
        loop {
            action = match action {
                LinkAction::Connect if controller.is_connected() => {
                    // Resumed before the suspend's disconnect ran: the link never dropped.
                    log::info!("[wifi] resumed, still associated");
                    self.poll(LinkInput::Connected)
                }
                LinkAction::Connect => {
                    // `Ok` once associated; the AP is read after the attempt has
                    // released the controller.
                    let outcome = {
                        let mut connect = pin!(controller.connect_async());
                        loop {
                            match select(self.handed_over(), connect.as_mut()).await {
                                Either::First(handed) => match handed {
                                    LinkAction::Disconnect => break Err(LinkAction::Disconnect),
                                    // A resume right after a suspend, or nothing (`None`,
                                    // `Ignored`): keep the attempt in flight.
                                    _ => continue,
                                },
                                Either::Second(Ok(_)) => break Ok(()),
                                Either::Second(Err(e)) => {
                                    // A refused association arrives here, not as a
                                    // disconnect event, so the hint belongs on this
                                    // line too.
                                    log::warn!(
                                        "[wifi] connect failed: {:?}{}",
                                        e,
                                        connect_failure_hint(&e)
                                    );
                                    break Err(self.poll(LinkInput::ConnectFailed));
                                }
                            }
                        }
                    };
                    match outcome {
                        Ok(()) => self.link_up(false, joined_ap(&controller)),
                        Err(action) => action,
                    }
                }
                LinkAction::Disconnect => {
                    let epoch = self.with(|h| h.begin_park());
                    match with_timeout(
                        Duration::from_millis(SUSPEND_DISCONNECT_TIMEOUT_MS),
                        controller.disconnect_async(),
                    )
                    .await
                    {
                        Ok(Ok(info)) => {
                            log::debug!("[wifi] disconnect reason: {:?}", info.reason);
                            log::info!("{}", Lifecycle::Suspended);
                        }
                        Ok(Err(e)) => {
                            log::debug!("[wifi] disconnect error: {:?}", e);
                            log::info!("{}", Lifecycle::Suspended);
                        }
                        Err(_) => {
                            log::warn!(
                                "[wifi] disconnect gave up after {} ms; \
                                 reporting suspended although the station may still be associated",
                                SUSPEND_DISCONNECT_TIMEOUT_MS
                            );
                            log::info!("{}", Lifecycle::Suspended);
                        }
                    }
                    self.with(|h| h.end_park(epoch));
                    self.suspend_wakers.lock(|w| w.borrow_mut().wake_all());
                    LinkAction::None
                }
                LinkAction::Wait { ms } => {
                    log::info!(
                        "{}",
                        Reconnecting {
                            ms,
                            attempt: self.with(|h| h.failed_attempts())
                        }
                    );
                    let mut timer = pin!(Timer::after_millis(ms));
                    loop {
                        match select(self.handed_over(), timer.as_mut()).await {
                            Either::First(handed) => match handed {
                                LinkAction::None | LinkAction::Ignored => continue,
                                other => break other,
                            },
                            Either::Second(()) => break self.poll(LinkInput::BackoffElapsed),
                        }
                    }
                }
                LinkAction::None | LinkAction::Ignored => {
                    if action == LinkAction::Ignored {
                        log::debug!("[wifi] input ignored in {:?}", self.state());
                    }
                    match self.with(|h| h.idle_wait()) {
                        IdleWait::Link => {
                            self.report_network(stack);
                            let mut gone = pin!(controller.wait_for_disconnect_async());
                            loop {
                                match select3(
                                    self.handed_over(),
                                    gone.as_mut(),
                                    Timer::after_millis(NETWORK_POLL_MS),
                                )
                                .await
                                {
                                    Either3::First(handed) => match handed {
                                        LinkAction::None | LinkAction::Ignored => continue,
                                        other => break other,
                                    },
                                    Either3::Third(()) => {
                                        self.report_network(stack);
                                        continue;
                                    }
                                    Either3::Second(result) => {
                                        match result {
                                            // esp-radio reports a named reason, not the
                                            // ESP-IDF code, so the name is its `Debug`
                                            // form; the credentials hint is the one both
                                            // tiers share.
                                            Ok(info) => log::warn!(
                                                "{}",
                                                Disconnected {
                                                    reason: ByDebug(&info.reason),
                                                    hint: hal_disconnect_hint(&info.reason)
                                                }
                                            ),
                                            Err(e) => log::warn!(
                                                "{}",
                                                Disconnected {
                                                    reason: ByDebug(&e),
                                                    hint: ""
                                                }
                                            ),
                                        }
                                        break self.poll(LinkInput::Disconnected);
                                    }
                                }
                            }
                        }
                        IdleWait::Park => {
                            match select(self.handed_over(), Timer::after_millis(RECHECK_MS)).await
                            {
                                Either::First(handed) => handed,
                                // Catch an association that slipped past the disconnect;
                                // the machine answers it with another `Disconnect`.
                                Either::Second(()) if controller.is_connected() => {
                                    log::info!("[wifi] still associated while suspended");
                                    self.poll(LinkInput::Connected)
                                }
                                Either::Second(()) => LinkAction::None,
                            }
                        }
                        IdleWait::Restart => {
                            match select(self.handed_over(), Timer::after_millis(RECHECK_MS)).await
                            {
                                Either::First(handed) => handed,
                                // Not reachable while `run` owns the machine from its own
                                // start; start again rather than stall.
                                Either::Second(()) => {
                                    self.start(controller.is_connected(), joined_ap(&controller))
                                }
                            }
                        }
                    }
                }
            };
        }
    }
}

impl Default for WifiLink {
    fn default() -> Self {
        Self::new()
    }
}

/// Embassy task wrapper around [`WifiLink::run`] so a firmware spawns the
/// supervisor in one line.
#[embassy_executor::task]
pub async fn link_task(
    link: &'static WifiLink,
    controller: WifiController<'static>,
    stack: Stack<'static>,
) -> ! {
    link.run(controller, stack).await
}

impl NetworkLink for WifiLink {
    type Error = core::convert::Infallible;

    fn state(&self) -> LinkState {
        WifiLink::state(self)
    }

    fn is_suspended(&self) -> bool {
        WifiLink::is_suspended(self)
    }

    fn connect_count(&self) -> u32 {
        WifiLink::connect_count(self)
    }

    fn suspend(&self) -> Result<(), Self::Error> {
        WifiLink::suspend(self);
        Ok(())
    }

    fn resume(&self) -> Result<(), Self::Error> {
        WifiLink::resume(self);
        Ok(())
    }

    async fn wait_connected(&self) {
        WifiLink::wait_connected(self).await
    }

    async fn wait_suspended(&self) {
        WifiLink::wait_suspended(self).await
    }
}

/// Tier parity check (ADR 022): fails to compile if `WifiLink` stops implementing `NetworkLink`.
const _: fn() = || {
    fn implements<T: NetworkLink>() {}
    implements::<WifiLink>();
};

/// The credentials hint for an esp-radio disconnect reason.
///
/// esp-radio reports a named reason rather than the ESP-IDF code, so the four
/// reasons the shared [`reason_suggests_credentials`] lists are matched on their
/// variants here: `4WAY_HANDSHAKE_TIMEOUT`, `NO_AP_FOUND`, `AUTH_FAIL` and
/// `HANDSHAKE_TIMEOUT`. `reason_suggests_credentials` itself cannot be used
/// because the numeric code is not exposed.
fn hal_disconnect_hint(reason: &DisconnectReason) -> &'static str {
    match reason {
        DisconnectReason::FourWayHandshakeTimeout
        | DisconnectReason::HandshakeTimeout
        | DisconnectReason::NoAccessPointFound
        | DisconnectReason::AuthenticationFailed => CREDENTIALS_HINT,
        _ => "",
    }
}

/// The same hint for a connect that never associated: esp-radio reports the
/// rejection as [`ConnectionError::Failed`] carrying the reason a disconnect
/// event would have, and a wrong password is the common cause.
fn connect_failure_hint(error: &ConnectionError) -> &'static str {
    match error {
        ConnectionError::Failed(info) => hal_disconnect_hint(&info.reason),
        _ => "",
    }
}

/// The AP the station is associated with, for the link-up line; `None` when not associated.
fn joined_ap(controller: &WifiController<'static>) -> Option<JoinedAp> {
    let info = controller.ap_info().ok()?;
    Some(JoinedAp {
        bssid: info.bssid,
        channel: info.channel,
        rssi: info.signal_strength,
    })
}
