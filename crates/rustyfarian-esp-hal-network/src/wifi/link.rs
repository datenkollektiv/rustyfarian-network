//! Library-owned station supervision for the bare-metal tier.
//!
//! The firmware declares one `static LINK: WifiLink = WifiLink::new();`, spawns
//! [`link_task`] with the [`WifiController`] from `AsyncWifiHandle`, and never
//! writes a reconnect loop again. `WifiLink::run` drives the shared
//! [`LinkMachine`]: connect, wait for the link to drop, back off 1 s doubling to
//! 30 s, reconnect. A controller that is already associated when `run` starts is
//! adopted as connected and counted as an association.
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
use core::sync::atomic::{AtomicU32, Ordering};

use embassy_futures::select::{select, Either};
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::signal::Signal;
use embassy_time::{with_timeout, Duration, Timer};
use esp_radio::wifi::WifiController;
use juggler::wifi::{LinkAction, LinkHandover, LinkInput, LinkState};

/// Upper bound on the disconnect that [`WifiLink::suspend`] triggers (milliseconds);
/// [`WifiLink::is_suspended`] turns `true` when it finishes or this bound passes.
pub const SUSPEND_DISCONNECT_TIMEOUT_MS: u64 = 500;

/// How often a parked or idle supervisor re-checks the controller (milliseconds).
const RECHECK_MS: u64 = 1_000;

/// Station link supervisor; one per firmware, declared as a `static`.
pub struct WifiLink {
    /// The machine and the action `suspend`/`resume` hand to `run`, changed
    /// together under one lock so the handover always matches the latest call.
    handover: Mutex<CriticalSectionRawMutex, RefCell<LinkHandover>>,
    /// Wakes `run` to take the handover; carries no data.
    wake: Signal<CriticalSectionRawMutex, ()>,
    connects: AtomicU32,
}

impl WifiLink {
    /// A supervisor with the shared back-off defaults.
    pub const fn new() -> Self {
        Self {
            handover: Mutex::new(RefCell::new(LinkHandover::new())),
            wake: Signal::new(),
            connects: AtomicU32::new(0),
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
        self.connects.load(Ordering::Relaxed)
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

    /// Feeds an association to the machine; counts and logs it unless suspended.
    fn link_up(&self, how: &str) -> LinkAction {
        let action = self.poll(LinkInput::Connected);
        if self.state() == LinkState::Connected {
            // load + store: riscv32imc has no fetch_add; `run` is the only writer.
            let n = self.connects.load(Ordering::Relaxed).wrapping_add(1);
            self.connects.store(n, Ordering::Relaxed);
            log::info!("[wifi] link up #{}{}", n, how);
        } else {
            log::info!("[wifi] associated while suspended, disconnecting again");
        }
        action
    }

    /// Supervises the station forever: initial association, every reconnect,
    /// suspend and resume.
    pub async fn run(&'static self, mut controller: WifiController<'static>) -> ! {
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
                                log::warn!("[wifi] connect failed: {:?}", e);
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
                        let mut gone = pin!(controller.wait_for_disconnect_async());
                        loop {
                            match select(self.handed_over(), gone.as_mut()).await {
                                Either::First(handed) => match handed {
                                    LinkAction::None => continue,
                                    other => break other,
                                },
                                Either::Second(result) => {
                                    match result {
                                        Ok(info) => {
                                            log::warn!("[wifi] disconnected — {:?}", info.reason)
                                        }
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
pub async fn link_task(link: &'static WifiLink, controller: WifiController<'static>) -> ! {
    link.run(controller).await
}
