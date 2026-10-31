//! Library-owned station supervision for the bare-metal tier.
//!
//! The firmware declares one `static LINK: WifiLink = WifiLink::new();`, spawns
//! [`link_task`] with the [`WifiController`] from `AsyncWifiHandle`, and never
//! writes a reconnect loop again. `WifiLink::run` drives the shared
//! [`LinkMachine`]: connect, wait for the link to drop, back off 1 s doubling to
//! 30 s, reconnect. A controller that is already associated when `run` starts is
//! adopted as connected and counted as an association.
//!
//! # Lifecycle
//!
//! Run at most one `run` per `WifiLink` at a time; the controller is a singleton,
//! so a second concurrent supervisor cannot exist anyway.
//! `run` resets the machine on entry, so a supervisor that was cancelled (for
//! example inside a `select`) can be started again on the same `static`.

use core::cell::RefCell;
use core::sync::atomic::{AtomicU32, Ordering};

use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::blocking_mutex::Mutex;
use embassy_time::Timer;
use esp_radio::wifi::WifiController;
use juggler::wifi::{LinkAction, LinkInput, LinkMachine, LinkState};

/// Station link supervisor; one per firmware, declared as a `static`.
pub struct WifiLink {
    machine: Mutex<CriticalSectionRawMutex, RefCell<LinkMachine>>,
    connects: AtomicU32,
}

impl WifiLink {
    /// A supervisor with the shared back-off defaults.
    pub const fn new() -> Self {
        Self {
            machine: Mutex::new(RefCell::new(LinkMachine::new())),
            connects: AtomicU32::new(0),
        }
    }

    /// Current link state as the supervisor last saw it.
    pub fn state(&self) -> LinkState {
        self.machine.lock(|m| m.borrow().state())
    }

    /// `true` while the station is associated.
    pub fn is_connected(&self) -> bool {
        self.state() == LinkState::Connected
    }

    /// Associations since boot, including one adopted when `run` starts.
    pub fn connect_count(&self) -> u32 {
        self.connects.load(Ordering::Relaxed)
    }

    fn poll(&self, input: LinkInput) -> LinkAction {
        self.machine.lock(|m| m.borrow_mut().poll(input))
    }

    /// Puts the machine back to `Idle` with a fresh back-off.
    fn reset(&self) {
        self.machine.lock(|m| *m.borrow_mut() = LinkMachine::new());
    }

    /// Counts and logs one association.
    fn note_link_up(&self, how: &str) {
        // load + store: riscv32imc has no fetch_add; `run` is the only writer.
        let n = self.connects.load(Ordering::Relaxed).wrapping_add(1);
        self.connects.store(n, Ordering::Relaxed);
        log::info!("[wifi] link up #{}{}", n, how);
    }

    /// Supervises the station forever: initial association and every reconnect.
    pub async fn run(&'static self, mut controller: WifiController<'static>) -> ! {
        // A cancelled earlier `run` may have left the machine mid-attempt.
        self.reset();
        let first = if controller.is_connected() {
            self.note_link_up(" (adopted)");
            LinkInput::Connected
        } else {
            LinkInput::Start
        };
        let mut action = self.poll(first);
        loop {
            action = match action {
                LinkAction::Connect => match controller.connect_async().await {
                    Ok(_) => {
                        self.note_link_up("");
                        self.poll(LinkInput::Connected)
                    }
                    Err(e) => {
                        log::warn!("[wifi] connect failed: {:?}", e);
                        self.poll(LinkInput::Disconnected)
                    }
                },
                LinkAction::Wait { ms } => {
                    log::info!("[wifi] reconnecting in {} ms", ms);
                    Timer::after_millis(ms).await;
                    self.poll(LinkInput::BackoffElapsed)
                }
                LinkAction::None => {
                    if self.state() == LinkState::Connected {
                        match controller.wait_for_disconnect_async().await {
                            Ok(info) => log::warn!("[wifi] disconnected — {:?}", info.reason),
                            Err(e) => log::warn!("[wifi] disconnected — {:?}", e),
                        }
                        self.poll(LinkInput::Disconnected)
                    } else {
                        // Not reachable while `run` owns the machine from its own reset;
                        // reset again rather than spin on an ignored `Start`.
                        self.reset();
                        self.poll(LinkInput::Start)
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
pub async fn link_task(link: &'static WifiLink, controller: WifiController<'static>) -> ! {
    link.run(controller).await
}
