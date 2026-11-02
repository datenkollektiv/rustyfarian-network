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
//! let deadline = Instant::now() + Duration::from_millis(SUSPEND_DISCONNECT_TIMEOUT_MS);
//! while !LINK.is_suspended() && Instant::now() < deadline {
//!     Timer::after_millis(10).await;
//! }
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
use juggler::wifi::{
    LinkAction, LinkHandover, LinkInput, LinkState, NetworkLink, CREDENTIALS_HINT,
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
}

impl WifiLink {
    /// A supervisor with the shared back-off defaults.
    pub const fn new() -> Self {
        Self {
            handover: Mutex::new(RefCell::new(LinkHandover::new())),
            wake: Signal::new(),
            ready_wakers: Mutex::new(RefCell::new(WakerSlots::new())),
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
        if self.with(|h| h.suspend()) {
            log::info!("[wifi] suspend requested");
            self.wake.signal(());
        }
    }

    /// Leaves suspend and connects with a fresh back-off.
    ///
    /// Non-blocking and idempotent; a no-op unless suspended. Safe right after
    /// [`suspend`](Self::suspend), even before the disconnect ran: the supervisor
    /// keeps an association that is still up, or the attempt still in flight.
    pub fn resume(&self) {
        if self.with(|h| h.resume()) {
            log::info!("[wifi] resume requested");
            self.wake.signal(());
        }
    }

    fn with<R>(&self, f: impl FnOnce(&mut LinkHandover) -> R) -> R {
        self.handover.lock(|h| f(&mut h.borrow_mut()))
    }

    fn poll(&self, input: LinkInput) -> LinkAction {
        self.with(|h| h.poll(input))
    }

    /// Waits for `suspend`/`resume` and takes the action they handed over.
    async fn handed_over(&self) -> LinkAction {
        self.wake.wait().await;
        self.with(|h| h.take())
    }

    /// Reports the stack's IPv4 configuration to the machine and wakes waiters once ready.
    fn report_network(&self, stack: Stack<'static>) {
        let up = stack.is_config_up();
        if up == self.is_ready() {
            return;
        }
        self.poll(if up {
            LinkInput::NetworkUp
        } else {
            LinkInput::NetworkDown
        });
        if self.is_ready() {
            log::info!("[wifi] network ready");
            self.ready_wakers.lock(|w| w.borrow_mut().wake_all());
        }
    }

    /// Feeds an association to the machine; logs it unless suspended.
    fn link_up(&self, how: &str) -> LinkAction {
        let action = self.poll(LinkInput::Connected);
        if self.state() == LinkState::Connected {
            log::info!("[wifi] link up #{}{}", self.connect_count(), how);
        } else {
            log::info!("[wifi] associated while suspended, disconnecting again");
        }
        action
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
        let suspended = self.with(|h| h.restart());
        let mut action = if suspended {
            LinkAction::Disconnect
        } else if controller.is_connected() {
            self.link_up(" (adopted)")
        } else {
            self.poll(LinkInput::Start)
        };
        loop {
            action = match action {
                LinkAction::Connect if controller.is_connected() => {
                    // Resumed before the suspend's disconnect ran: the link never dropped.
                    log::info!("[wifi] resumed, still associated");
                    self.poll(LinkInput::Connected)
                }
                LinkAction::Connect => {
                    let mut connect = pin!(controller.connect_async());
                    loop {
                        match select(self.handed_over(), connect.as_mut()).await {
                            Either::First(handed) => match handed {
                                LinkAction::Disconnect => break LinkAction::Disconnect,
                                // A resume right after a suspend: keep the attempt in flight.
                                _ => continue,
                            },
                            Either::Second(Ok(_)) => break self.link_up(""),
                            Either::Second(Err(e)) => {
                                // A refused association arrives here, not as a
                                // disconnect event, so the hint belongs on this
                                // line too (bug 011).
                                log::warn!(
                                    "[wifi] connect failed: {:?}{}",
                                    e,
                                    connect_failure_hint(&e)
                                );
                                break self.poll(LinkInput::Disconnected);
                            }
                        }
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
                        Ok(Ok(info)) => log::info!("[wifi] suspended — {:?}", info.reason),
                        Ok(Err(e)) => log::info!("[wifi] suspended ({:?})", e),
                        Err(_) => log::warn!(
                            "[wifi] suspended — disconnect gave up after {} ms",
                            SUSPEND_DISCONNECT_TIMEOUT_MS
                        ),
                    }
                    self.with(|h| h.end_park(epoch));
                    LinkAction::None
                }
                LinkAction::Wait { ms } => {
                    log::info!("[wifi] reconnecting in {} ms", ms);
                    let mut timer = pin!(Timer::after_millis(ms));
                    loop {
                        match select(self.handed_over(), timer.as_mut()).await {
                            Either::First(handed) => match handed {
                                LinkAction::None => continue,
                                other => break other,
                            },
                            Either::Second(()) => break self.poll(LinkInput::BackoffElapsed),
                        }
                    }
                }
                LinkAction::None => match self.state() {
                    LinkState::Connected => {
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
                                    LinkAction::None => continue,
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
                                        // tiers share (bug 011).
                                        Ok(info) => log::warn!(
                                            "[wifi] disconnected — {:?}{}",
                                            info.reason,
                                            hal_disconnect_hint(&info.reason)
                                        ),
                                        Err(e) => log::warn!("[wifi] disconnected — {:?}", e),
                                    }
                                    break self.poll(LinkInput::Disconnected);
                                }
                            }
                        }
                    }
                    state => {
                        match select(self.handed_over(), Timer::after_millis(RECHECK_MS)).await {
                            Either::First(handed) => handed,
                            // Parked: catch an association that slipped past the disconnect.
                            Either::Second(()) if state == LinkState::Suspended => {
                                if controller.is_connected() {
                                    self.link_up("")
                                } else {
                                    LinkAction::None
                                }
                            }
                            // Not reachable while `run` owns the machine from its own reset;
                            // reset again rather than stall.
                            Either::Second(()) => {
                                self.with(|h| h.restart());
                                self.poll(LinkInput::Start)
                            }
                        }
                    }
                },
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
/// because the numeric code is not exposed (bug 011).
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
/// event would have, and a wrong password is the common cause (bug 011).
fn connect_failure_hint(error: &ConnectionError) -> &'static str {
    match error {
        ConnectionError::Failed(info) => hal_disconnect_hint(&info.reason),
        _ => "",
    }
}
