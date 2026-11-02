//! Thread-based station supervisor for std tiers (the ESP-IDF `WiFiManager`).
//!
//! A worker thread owns the reconnect timer and feeds driver events to the
//! shared [`LinkMachine`]; [`LinkSupervisor::suspend`] and
//! [`LinkSupervisor::resume`] run on the caller's thread. Every transition and
//! the radio call it implies happen under one lock, so a connect never
//! interleaves with a stop, whichever threads call in.
//!
//! # Stop fence
//!
//! Driver events arrive asynchronously, so the `Disconnected` caused by a
//! suspend can still be queued when a quick resume has already connected again.
//! Every successful stop raises a fence that only the matching
//! [`LinkEvent::Stopped`] lowers; link events the worker sees while a fence is up
//! predate the stop and are dropped, however late they arrive. `resume` never
//! waits for the fence, and nothing but `Stopped` lowers it: event order is the
//! only thing that tells a stale event from a fresh one, so no timeout does.
//!
//! # Event contract
//!
//! The tier delivers every driver event, in the order the driver posted it. The
//! reconnect policy already relies on this (a lost `Disconnected` would leave
//! `Connecting` waiting for good), and a lost `Stopped` would equally leave
//! link events fenced off. ESP-IDF meets it: the Wi-Fi driver posts through a
//! wrapper that blocks while the event queue is full (`OSI_FUNCS_TIME_BLOCKING`
//! → `portMAX_DELAY` in `esp_adapter.c`), and the event loop dispatches in order.

use std::fmt::Debug;
use std::io;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Poll, Waker};
use std::thread;
use std::time::{Duration, Instant};

use super::link::{LinkAction, LinkInput, LinkMachine, LinkState};

/// The station calls a [`LinkSupervisor`] makes; each must return without blocking for long.
pub trait LinkRadio: Send + 'static {
    /// Driver error.
    type Error: Debug + Send + 'static;
    /// Starts the radio after a suspend stopped it.
    fn start(&mut self) -> Result<(), Self::Error>;
    /// Requests an association; the outcome arrives as a [`LinkEvent`].
    fn connect(&mut self) -> Result<(), Self::Error>;
    /// Drops the association or aborts an attempt.
    fn disconnect(&mut self) -> Result<(), Self::Error>;
    /// Stops the radio; the driver then posts [`LinkEvent::Stopped`].
    fn stop(&mut self) -> Result<(), Self::Error>;
}

/// A driver event, delivered through [`LinkEvents`] in the order the driver posted it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkEvent {
    /// The station associated.
    Connected,
    /// The station lost its association or an attempt failed.
    Disconnected,
    /// The radio stopped.
    Stopped,
    /// The station got its IP configuration.
    NetworkUp,
    /// The station lost its IP configuration while still associated.
    NetworkDown,
}

enum Msg {
    Event(LinkEvent),
    Input(LinkInput),
    /// Re-read the back-off deadline in the core.
    Wake,
    Shutdown,
}

/// Cloneable sender for driver events, for the tier's event subscription.
#[derive(Clone)]
pub struct LinkEvents(Sender<Msg>);

impl LinkEvents {
    /// Queues one event for the worker; a no-op once the supervisor is gone.
    pub fn send(&self, event: LinkEvent) {
        let _ = self.0.send(Msg::Event(event));
    }
}

struct Core<R> {
    machine: LinkMachine,
    radio: R,
    /// `false` between a successful stop and the next start.
    radio_started: bool,
    /// Successful stops so far.
    stops: u32,
    /// `Stopped` events seen so far, never ahead of `stops`.
    fenced: u32,
    /// When the back-off ends; set and cleared only together with the state, so
    /// no timer outlives the `Backoff` it belongs to.
    backoff_at: Option<Instant>,
    /// Pending [`ReadyWait`] futures by id, woken (and removed) when the link becomes ready.
    ready_waiters: Vec<(u64, Waker)>,
    /// Id for the next [`ReadyWait`] that registers.
    next_waiter: u64,
    /// Sinks told once per readiness transition (see [`LinkSupervisor::subscribe_ready`]).
    ready_sinks: Vec<Box<dyn Fn(bool) + Send + 'static>>,
    /// Readiness as last handed to `ready_sinks`.
    ready_reported: bool,
}

impl<R: LinkRadio> Core<R> {
    fn fence_up(&self) -> bool {
        self.fenced != self.stops
    }

    /// Starts the radio if a suspend stopped it, then requests an association.
    fn connect(&mut self) -> Result<(), R::Error> {
        if !self.radio_started {
            self.radio.start()?;
            self.radio_started = true;
        }
        self.radio.connect()
    }

    /// Disconnects and stops a started radio, raising the fence.
    fn radio_down(&mut self) -> Result<(), R::Error> {
        if !self.radio_started {
            return Ok(());
        }
        // Not connected (yet) is fine: the stop aborts an attempt as well.
        if let Err(e) = self.radio.disconnect() {
            log::debug!("WiFi disconnect before stop: {:?}", e);
        }
        self.radio.stop()?;
        self.radio_started = false;
        self.stops = self.stops.wrapping_add(1);
        Ok(())
    }

    /// Hands a readiness change to the sinks, once per transition.
    fn report_ready(&mut self) {
        let ready = self.machine.is_ready();
        if ready == self.ready_reported {
            return;
        }
        self.ready_reported = ready;
        for sink in &self.ready_sinks {
            sink(ready);
        }
    }
}

/// Station supervisor: a worker thread plus thread-safe `suspend`/`resume`.
///
/// Dropping it stops and joins the worker, so no radio call runs afterwards.
pub struct LinkSupervisor<R: LinkRadio> {
    core: Arc<Mutex<Core<R>>>,
    tx: Sender<Msg>,
    handle: Option<thread::JoinHandle<()>>,
}

impl<R: LinkRadio> LinkSupervisor<R> {
    /// Spawns the worker on `builder` around an already started `radio`.
    ///
    /// Supervision begins with [`start`](Self::start); feed driver events
    /// through [`events`](Self::events), honouring the module's event contract.
    pub fn spawn(radio: R, machine: LinkMachine, builder: thread::Builder) -> io::Result<Self> {
        let core = Arc::new(Mutex::new(Core {
            machine,
            radio,
            radio_started: true,
            stops: 0,
            fenced: 0,
            backoff_at: None,
            ready_waiters: Vec::new(),
            next_waiter: 0,
            ready_sinks: Vec::new(),
            ready_reported: false,
        }));
        let (tx, rx) = mpsc::channel();
        let worker = Arc::clone(&core);
        let handle = builder.spawn(move || run_worker(&rx, &worker))?;
        Ok(Self {
            core,
            tx,
            handle: Some(handle),
        })
    }

    /// Sender for the tier's driver-event subscription.
    pub fn events(&self) -> LinkEvents {
        LinkEvents(self.tx.clone())
    }

    /// Issues the first connect; `false` if the worker is gone.
    pub fn start(&self) -> bool {
        self.tx.send(Msg::Input(LinkInput::Start)).is_ok()
    }

    /// Current link state, including [`LinkState::Suspended`].
    pub fn state(&self) -> LinkState {
        self.lock().machine.state()
    }

    /// `true` once a suspend has taken effect: the state is
    /// [`LinkState::Suspended`] and the radio is actually stopped. A failed stop
    /// leaves it `false` until a later [`suspend`](Self::suspend) succeeds.
    pub fn is_suspended(&self) -> bool {
        let core = self.lock();
        core.machine.state() == LinkState::Suspended && !core.radio_started
    }

    /// `true` while associated with the IP configuration reported (see [`LinkMachine::is_ready`]).
    pub fn is_ready(&self) -> bool {
        self.lock().machine.is_ready()
    }

    /// Associations since boot (see [`LinkMachine::connect_count`]).
    pub fn connect_count(&self) -> u32 {
        self.lock().machine.connect_count()
    }

    /// Completes once the link is ready; the worker wakes it, nothing polls.
    ///
    /// Dropping the future before it completes removes its registration, so
    /// cancelled waits (timeouts, `select`) leave nothing behind.
    pub fn wait_ready(&self) -> ReadyWait<'_, R> {
        ReadyWait {
            core: &self.core,
            id: None,
        }
    }

    /// Registers a sink told once whenever the link becomes ready and once
    /// whenever it stops being ready, and returns the readiness at the moment
    /// of registration.
    ///
    /// Readiness is [`is_ready`](Self::is_ready), the same value the
    /// `wait_ready` futures complete on, so a consumer that must gate work on
    /// the link (the MQTT session, bug 010) needs neither a second event source
    /// nor a poll.
    ///
    /// The seed and the registration happen under one lock, so no transition can
    /// slip between them: every change is either already in the returned value or
    /// delivered to the sink. A transition that happened before the call is
    /// therefore covered exactly once, by the seed, and never replayed.
    ///
    /// The sink runs on the reconnect worker, or on the caller of
    /// [`suspend`](Self::suspend), while the supervisor's lock is held. It must
    /// return at once: hand the value to a channel, and never block or call back
    /// into the supervisor.
    pub fn subscribe_ready(&self, sink: impl Fn(bool) + Send + 'static) -> bool {
        let mut core = self.lock();
        core.ready_sinks.push(Box::new(sink));
        core.machine.is_ready()
    }

    #[cfg(test)]
    fn ready_waiter_count(&self) -> usize {
        self.lock().ready_waiters.len()
    }

    /// Disconnects and stops the radio on the caller's thread and pauses
    /// reconnects; the radio is stopped when this returns `Ok`.
    ///
    /// Readiness is reported before the stop, so consumers learn the link is down
    /// even when the stop fails.
    /// That report is a notification, not a close handshake: a consumer that moves
    /// the change to another thread can still be overtaken by the stop.
    ///
    /// Idempotent. On error reconnects stay paused and the next call retries the stop.
    pub fn suspend(&self) -> Result<(), R::Error> {
        let mut core = self.lock();
        let action = core.machine.poll(LinkInput::Suspend);
        core.backoff_at = None;
        // A repeated call only has work left if an earlier stop failed.
        if action == LinkAction::None && !core.radio_started {
            return Ok(());
        }
        log::info!("WiFi suspending");
        // Report before the stop: the machine is already `Suspended`, so consumers
        // must hear that the link is gone before the driver tears it down, including
        // when the stop then fails. This is a notification, not a handshake — a
        // consumer that hands the change to another thread (the MQTT gate) can lose
        // the race, which is why the transport may log one error as it dies
        // (runbook R3). Guaranteeing a clean close would need an acknowledgement.
        core.report_ready();
        core.radio_down()?;
        log::info!("WiFi suspended");
        Ok(())
    }

    /// Starts the radio and requests one association with a fresh back-off.
    ///
    /// A no-op unless suspended; never waits for driver events. Events of the
    /// last stop that are still queued are dropped by the worker, so a resume
    /// right after a suspend leaves one connect in flight. On error the worker
    /// retries with its back-off.
    pub fn resume(&self) -> Result<(), R::Error> {
        let mut core = self.lock();
        if core.machine.poll(LinkInput::Resume) != LinkAction::Connect {
            return Ok(());
        }
        core.backoff_at = None;
        match core.connect() {
            Ok(()) => {
                log::info!("WiFi resumed, connect requested");
                core.report_ready();
                Ok(())
            }
            Err(e) => {
                log::warn!("WiFi resume: connect request failed: {:?}", e);
                // Schedule the retry under the same lock, then let the worker
                // pick up the new deadline.
                step(&mut core, LinkInput::Disconnected);
                core.report_ready();
                let _ = self.tx.send(Msg::Wake);
                Err(e)
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, Core<R>> {
        lock(&self.core)
    }
}

/// Future returned by [`LinkSupervisor::wait_ready`].
pub struct ReadyWait<'a, R: LinkRadio> {
    core: &'a Mutex<Core<R>>,
    /// Registration id, assigned on the first pending poll.
    id: Option<u64>,
}

impl<R: LinkRadio> std::future::Future for ReadyWait<'_, R> {
    type Output = ();

    fn poll(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<()> {
        let core_ref = self.core;
        let mut core = lock(core_ref);
        if core.machine.is_ready() {
            if let Some(id) = self.id.take() {
                core.ready_waiters.retain(|(waiter, _)| *waiter != id);
            }
            return Poll::Ready(());
        }
        let id = match self.id {
            Some(id) => id,
            None => {
                let id = core.next_waiter;
                core.next_waiter = core.next_waiter.wrapping_add(1);
                self.id = Some(id);
                id
            }
        };
        match core
            .ready_waiters
            .iter_mut()
            .find(|(waiter, _)| *waiter == id)
        {
            Some((_, waker)) => {
                if !waker.will_wake(cx.waker()) {
                    waker.clone_from(cx.waker());
                }
            }
            // First poll, or the worker woke and removed this entry.
            None => core.ready_waiters.push((id, cx.waker().clone())),
        }
        Poll::Pending
    }
}

impl<R: LinkRadio> Drop for ReadyWait<'_, R> {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            lock(self.core)
                .ready_waiters
                .retain(|(waiter, _)| *waiter != id);
        }
    }
}

impl<R: LinkRadio> Drop for LinkSupervisor<R> {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Shutdown);
        if let Some(handle) = self.handle.take() {
            if handle.join().is_err() {
                log::warn!("WiFi link worker panicked");
            }
        }
    }
}

fn lock<R>(core: &Mutex<Core<R>>) -> MutexGuard<'_, Core<R>> {
    core.lock().unwrap_or_else(PoisonError::into_inner)
}

fn run_worker<R: LinkRadio>(rx: &Receiver<Msg>, core: &Mutex<Core<R>>) {
    loop {
        let backoff_at = lock(core).backoff_at;
        let msg = match backoff_at {
            Some(at) => match rx.recv_timeout(at.saturating_duration_since(Instant::now())) {
                Ok(msg) => Some(msg),
                Err(RecvTimeoutError::Timeout) => None,
                Err(RecvTimeoutError::Disconnected) => break,
            },
            None => match rx.recv() {
                Ok(msg) => Some(msg),
                Err(_) => break,
            },
        };
        let mut core = lock(core);
        let input = match msg {
            None | Some(Msg::Wake) => match core.backoff_at {
                Some(at) if Instant::now() >= at => {
                    core.backoff_at = None;
                    LinkInput::BackoffElapsed
                }
                _ => continue,
            },
            Some(Msg::Shutdown) => break,
            Some(Msg::Event(LinkEvent::Stopped)) => {
                if core.fence_up() {
                    core.fenced = core.fenced.wrapping_add(1);
                }
                continue;
            }
            Some(Msg::Event(event)) if core.fence_up() => {
                log::debug!("WiFi {:?} predates the last stop, dropped", event);
                continue;
            }
            Some(Msg::Event(LinkEvent::Connected)) => LinkInput::Connected,
            Some(Msg::Event(LinkEvent::Disconnected)) => LinkInput::Disconnected,
            Some(Msg::Event(LinkEvent::NetworkUp)) => LinkInput::NetworkUp,
            Some(Msg::Event(LinkEvent::NetworkDown)) => LinkInput::NetworkDown,
            Some(Msg::Input(input)) => input,
        };
        step(&mut core, input);
        if core.machine.is_ready() {
            for (_, waker) in core.ready_waiters.drain(..) {
                waker.wake();
            }
        }
        core.report_ready();
    }
    log::debug!("WiFi link worker exited");
}

/// Feeds one input to the machine and performs the resulting actions.
fn step<R: LinkRadio>(core: &mut Core<R>, input: LinkInput) {
    let mut action = core.machine.poll(input);
    loop {
        match action {
            LinkAction::None => break,
            LinkAction::Connect => match core.connect() {
                Ok(()) => {
                    log::info!("WiFi connect requested");
                    break;
                }
                Err(e) => {
                    log::warn!("WiFi connect request failed: {:?}", e);
                    action = core.machine.poll(LinkInput::Disconnected);
                }
            },
            LinkAction::Disconnect => {
                // An association that landed after `suspend`.
                if let Err(e) = core.radio_down() {
                    log::warn!("WiFi stop while suspended failed: {:?}", e);
                }
                break;
            }
            LinkAction::Wait { ms } => {
                log::info!(
                    "WiFi reconnect in {} ms (attempt {})",
                    ms,
                    core.machine.failed_attempts()
                );
                core.backoff_at = Some(Instant::now() + Duration::from_millis(ms));
                break;
            }
        }
    }
    if core.machine.state() != LinkState::Backoff {
        core.backoff_at = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::vec::Vec;

    #[derive(Default)]
    struct Log {
        started: bool,
        /// A connect was requested and neither a stop nor a link loss ended it.
        attempt_live: bool,
        connects: u32,
        stops: u32,
        violations: Vec<&'static str>,
        fail_stop: bool,
        /// Every n-th connect / stop request fails (0 = never).
        fail_every_connect: u32,
        fail_every_stop: u32,
        connect_calls: u32,
        stop_calls: u32,
        /// Readiness reports and radio calls in the order they happened.
        order: Vec<&'static str>,
    }

    fn nth(every: u32, call: u32) -> bool {
        every != 0 && call.is_multiple_of(every)
    }

    /// Records calls, flags overlapping calls, connects on a stopped radio and
    /// double connects; optionally posts the driver events through an in-order emitter.
    struct MockRadio {
        log: Arc<Mutex<Log>>,
        busy: Arc<AtomicBool>,
        post: Option<Sender<LinkEvent>>,
    }

    impl MockRadio {
        fn enter(&self) {
            if self.busy.swap(true, Ordering::SeqCst) {
                self.log
                    .lock()
                    .unwrap()
                    .violations
                    .push("overlapping radio calls");
            }
            thread::sleep(Duration::from_micros(200));
        }
        fn leave(&self) {
            self.busy.store(false, Ordering::SeqCst);
        }
        fn post(&self, event: LinkEvent) {
            if let Some(tx) = &self.post {
                let _ = tx.send(event);
            }
        }
    }

    impl LinkRadio for MockRadio {
        type Error = &'static str;
        fn start(&mut self) -> Result<(), Self::Error> {
            self.enter();
            self.log.lock().unwrap().started = true;
            self.leave();
            Ok(())
        }
        fn connect(&mut self) -> Result<(), Self::Error> {
            self.enter();
            {
                let mut log = self.log.lock().unwrap();
                if !log.started {
                    log.violations.push("connect on a stopped radio");
                }
                if log.attempt_live {
                    log.violations.push("two connects in flight");
                }
                log.connect_calls += 1;
                if nth(log.fail_every_connect, log.connect_calls) {
                    drop(log);
                    self.leave();
                    return Err("connect failed");
                }
                log.attempt_live = true;
                log.connects += 1;
            }
            self.post(LinkEvent::Connected);
            self.post(LinkEvent::NetworkUp);
            self.leave();
            Ok(())
        }
        fn disconnect(&mut self) -> Result<(), Self::Error> {
            self.enter();
            {
                let mut log = self.log.lock().unwrap();
                log.attempt_live = false;
                log.order.push("disconnect");
            }
            self.leave();
            Ok(())
        }
        fn stop(&mut self) -> Result<(), Self::Error> {
            self.enter();
            let result = {
                let mut log = self.log.lock().unwrap();
                log.order.push("stop");
                log.stop_calls += 1;
                if log.fail_stop || nth(log.fail_every_stop, log.stop_calls) {
                    Err("stop failed")
                } else {
                    log.started = false;
                    log.attempt_live = false;
                    log.stops += 1;
                    Ok(())
                }
            };
            if result.is_ok() {
                self.post(LinkEvent::Disconnected);
                self.post(LinkEvent::Stopped);
            }
            self.leave();
            result
        }
    }

    struct Rig {
        sup: LinkSupervisor<MockRadio>,
        log: Arc<Mutex<Log>>,
    }

    impl Rig {
        fn connects(&self) -> u32 {
            self.log.lock().unwrap().connects
        }
        fn violations(&self) -> Vec<&'static str> {
            self.log.lock().unwrap().violations.clone()
        }
    }

    /// `auto_events`: the mock posts its driver events, each delivered 1 ms later.
    fn rig(auto_events: bool) -> Rig {
        let log = Arc::new(Mutex::new(Log {
            started: true,
            ..Log::default()
        }));
        let slot: Arc<Mutex<Option<LinkEvents>>> = Arc::new(Mutex::new(None));
        let post = auto_events.then(|| {
            let (tx, rx) = mpsc::channel::<LinkEvent>();
            let slot = Arc::clone(&slot);
            thread::spawn(move || {
                for event in rx {
                    thread::sleep(Duration::from_millis(1));
                    if let Some(events) = slot.lock().unwrap().as_ref() {
                        events.send(event);
                    }
                }
            });
            tx
        });
        let radio = MockRadio {
            log: Arc::clone(&log),
            busy: Arc::new(AtomicBool::new(false)),
            post,
        };
        let sup = LinkSupervisor::spawn(
            radio,
            LinkMachine::with_backoff(10, 20),
            thread::Builder::new(),
        )
        .unwrap();
        *slot.lock().unwrap() = Some(sup.events());
        Rig { sup, log }
    }

    fn settle() {
        thread::sleep(Duration::from_millis(60));
    }

    /// Waits up to 2 s for the in-order emitter to deliver everything queued.
    fn wait_for_state(sup: &LinkSupervisor<MockRadio>, want: LinkState) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while sup.state() != want && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(sup.state(), want);
    }

    /// A waker that records whether the worker woke it.
    struct Flag(AtomicBool);

    impl std::task::Wake for Flag {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    fn eventually(cond: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(2);
        while !cond() {
            if Instant::now() >= deadline {
                return false;
            }
            thread::sleep(Duration::from_millis(2));
        }
        true
    }

    #[test]
    fn wait_ready_is_woken_by_the_worker_once_the_network_is_up() {
        use std::future::Future;
        let rig = rig(true);
        let flag = Arc::new(Flag(AtomicBool::new(false)));
        let waker = Waker::from(Arc::clone(&flag));
        let mut cx = std::task::Context::from_waker(&waker);
        let mut ready = std::pin::pin!(rig.sup.wait_ready());
        assert!(ready.as_mut().poll(&mut cx).is_pending());

        assert!(rig.sup.start());
        assert!(
            eventually(|| flag.0.load(Ordering::SeqCst)),
            "the worker must wake the waiter"
        );
        assert!(ready.as_mut().poll(&mut cx).is_ready());
        assert!(rig.sup.is_ready());
        assert_eq!(rig.sup.connect_count(), 1);
    }

    #[test]
    fn a_ready_sink_sees_each_transition_once() {
        let rig = rig(true);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        rig.sup
            .subscribe_ready(move |ready| sink.lock().unwrap().push(ready));

        rig.sup.start();
        assert!(eventually(|| rig.sup.is_ready()));
        rig.sup.suspend().unwrap();
        rig.sup.resume().unwrap();
        wait_for_state(&rig.sup, LinkState::Connected);
        settle();
        assert_eq!(
            *seen.lock().unwrap(),
            [true, false, true],
            "one notification per transition, in order"
        );
    }

    #[test]
    fn a_repeated_state_is_not_reported_again() {
        let rig = rig(true);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        rig.sup
            .subscribe_ready(move |ready| sink.lock().unwrap().push(ready));

        rig.sup.start();
        assert!(eventually(|| rig.sup.is_ready()));
        settle();
        rig.sup.suspend().unwrap();
        rig.sup.suspend().unwrap();
        assert_eq!(*seen.lock().unwrap(), [true, false]);
    }

    /// Subscribes a sink that appends every readiness report to the mock's log,
    /// so the notifications and the radio calls share one order.
    fn rig_recording_readiness(auto_events: bool) -> Rig {
        let rig = rig(auto_events);
        let log = Arc::clone(&rig.log);
        rig.sup.subscribe_ready(move |ready| {
            log.lock()
                .unwrap()
                .order
                .push(if ready { "ready(true)" } else { "ready(false)" });
        });
        rig
    }

    #[test]
    fn readiness_falls_before_the_radio_is_disconnected_and_stopped() {
        let rig = rig_recording_readiness(true);
        rig.sup.start();
        assert!(eventually(|| rig.sup.is_ready()));
        settle();
        rig.log.lock().unwrap().order.clear();

        rig.sup.suspend().unwrap();

        assert_eq!(
            rig.log.lock().unwrap().order,
            ["ready(false)", "disconnect", "stop"],
            "consumers hear about the suspend before the radio is torn down"
        );
    }

    #[test]
    fn a_failed_stop_reports_the_suspend_and_a_retry_does_not_repeat_it() {
        let rig = rig_recording_readiness(true);
        rig.sup.start();
        assert!(eventually(|| rig.sup.is_ready()));
        settle();
        {
            let mut log = rig.log.lock().unwrap();
            log.order.clear();
            log.fail_stop = true;
        }

        assert!(rig.sup.suspend().is_err(), "the stop fails");

        assert_eq!(
            rig.log.lock().unwrap().order,
            ["ready(false)", "disconnect", "stop"],
            "readiness falls even though the stop failed"
        );

        rig.log.lock().unwrap().fail_stop = false;
        rig.sup.suspend().unwrap();

        assert_eq!(
            rig.log.lock().unwrap().order,
            ["ready(false)", "disconnect", "stop", "disconnect", "stop"],
            "the retry finishes the stop without a second transition"
        );
    }

    #[test]
    fn every_ready_sink_is_told() {
        let rig = rig(false);
        let first = Arc::new(Mutex::new(Vec::new()));
        let second = Arc::new(Mutex::new(Vec::new()));
        let (first_sink, second_sink) = (Arc::clone(&first), Arc::clone(&second));
        rig.sup
            .subscribe_ready(move |ready| first_sink.lock().unwrap().push(ready));
        rig.sup
            .subscribe_ready(move |ready| second_sink.lock().unwrap().push(ready));

        rig.sup.events().send(LinkEvent::Connected);
        rig.sup.events().send(LinkEvent::NetworkUp);
        assert!(eventually(|| rig.sup.is_ready()));
        settle();
        assert_eq!(*first.lock().unwrap(), [true]);
        assert_eq!(*second.lock().unwrap(), [true]);
    }

    #[test]
    fn a_transition_before_the_subscription_is_not_replayed() {
        let rig = rig(true);
        rig.sup.start();
        assert!(eventually(|| rig.sup.is_ready()));

        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let seed = rig
            .sup
            .subscribe_ready(move |ready| sink.lock().unwrap().push(ready));
        assert!(seed, "the seed is the readiness at registration");
        rig.sup.suspend().unwrap();
        assert_eq!(
            *seen.lock().unwrap(),
            [false],
            "the seed covers the earlier transition; nothing is replayed"
        );
        assert!(!rig.sup.is_ready());
    }

    #[test]
    fn the_seed_and_the_sink_together_cover_every_transition() {
        let rig = rig(true);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&seen);
        let seed = rig
            .sup
            .subscribe_ready(move |ready| sink.lock().unwrap().push(ready));
        assert!(
            !seed,
            "the link has not associated yet, so the session starts closed"
        );

        rig.sup.start();
        assert!(eventually(|| rig.sup.is_ready()));
        rig.sup.suspend().unwrap();
        rig.sup.resume().unwrap();
        wait_for_state(&rig.sup, LinkState::Connected);
        settle();
        assert_eq!(
            *seen.lock().unwrap(),
            [true, false, true],
            "starting from the seed, the sink sees every later edge once"
        );
    }

    #[test]
    fn a_dropped_wait_unregisters_itself() {
        use std::future::Future;
        let rig = rig(false);
        let mut cx = std::task::Context::from_waker(Waker::noop());
        for _ in 0..3 {
            let mut wait = std::pin::pin!(rig.sup.wait_ready());
            assert!(wait.as_mut().poll(&mut cx).is_pending());
            assert_eq!(rig.sup.ready_waiter_count(), 1);
        }
        assert_eq!(
            rig.sup.ready_waiter_count(),
            0,
            "cancelled waits must leave nothing behind"
        );
    }

    #[test]
    fn a_repolled_wait_replaces_its_waker_and_the_latest_one_is_woken() {
        use std::future::Future;
        let rig = rig(true);
        let first = Arc::new(Flag(AtomicBool::new(false)));
        let latest = Arc::new(Flag(AtomicBool::new(false)));
        let first_waker = Waker::from(Arc::clone(&first));
        let latest_waker = Waker::from(Arc::clone(&latest));
        let mut wait = std::pin::pin!(rig.sup.wait_ready());
        assert!(wait
            .as_mut()
            .poll(&mut std::task::Context::from_waker(&first_waker))
            .is_pending());
        assert!(wait
            .as_mut()
            .poll(&mut std::task::Context::from_waker(&latest_waker))
            .is_pending());
        assert_eq!(rig.sup.ready_waiter_count(), 1, "a repoll must not append");

        rig.sup.start();
        assert!(eventually(|| latest.0.load(Ordering::SeqCst)));
        assert!(
            !first.0.load(Ordering::SeqCst),
            "the replaced waker must not be woken"
        );
        assert!(wait
            .as_mut()
            .poll(&mut std::task::Context::from_waker(&latest_waker))
            .is_ready());
        assert_eq!(rig.sup.ready_waiter_count(), 0);
    }

    #[test]
    fn suspend_drops_readiness_and_resume_restores_it() {
        use std::future::Future;
        let rig = rig(true);
        rig.sup.start();
        assert!(eventually(|| rig.sup.is_ready()));

        rig.sup.suspend().unwrap();
        assert!(!rig.sup.is_ready());
        let mut cx = std::task::Context::from_waker(Waker::noop());
        let mut ready = std::pin::pin!(rig.sup.wait_ready());
        assert!(ready.as_mut().poll(&mut cx).is_pending());

        rig.sup.resume().unwrap();
        assert!(eventually(|| rig.sup.is_ready()));
        assert!(ready.as_mut().poll(&mut cx).is_ready());
        assert_eq!(rig.sup.connect_count(), 2);
        assert!(rig.violations().is_empty());
    }

    #[test]
    fn suspend_stops_the_radio_before_returning_and_is_idempotent() {
        let r = rig(false);
        r.sup.start();
        settle();
        r.sup.events().send(LinkEvent::Connected);
        settle();
        r.sup.suspend().unwrap();
        assert!(!r.log.lock().unwrap().started);
        assert_eq!(r.sup.state(), LinkState::Suspended);
        r.sup.suspend().unwrap();
        assert_eq!(r.log.lock().unwrap().stops, 1);
        assert!(r.violations().is_empty());
    }

    /// Suspends a connected rig, resumes at once, and delivers the stop's
    /// events `delay` after the resume returned.
    fn stop_events_after_resume(delay: Duration) -> Rig {
        let r = rig(false);
        r.sup.start();
        settle();
        r.sup.events().send(LinkEvent::Connected);
        settle();
        r.sup.suspend().unwrap();
        let begun = Instant::now();
        r.sup.resume().unwrap();
        assert!(
            begun.elapsed() < Duration::from_millis(5),
            "resume must not wait"
        );
        thread::sleep(delay);
        r.sup.events().send(LinkEvent::Disconnected);
        r.sup.events().send(LinkEvent::Stopped);
        settle();
        r
    }

    #[test]
    fn a_stale_disconnect_after_a_quick_resume_starts_no_second_connect() {
        let r = stop_events_after_resume(Duration::from_millis(20));
        assert_eq!(r.connects(), 2);
        assert_eq!(r.sup.state(), LinkState::Connecting);
        assert!(r.violations().is_empty());
    }

    #[test]
    fn stop_events_arriving_long_after_resume_are_still_dropped() {
        // Fifty times the longest back-off in this rig, the worker's only timer:
        // only the `Stopped` event re-admits link events, never elapsed time.
        let r = stop_events_after_resume(Duration::from_millis(1_000));
        assert_eq!(r.connects(), 2);
        assert_eq!(r.sup.state(), LinkState::Connecting);
        r.sup.events().send(LinkEvent::Connected);
        settle();
        assert_eq!(r.sup.state(), LinkState::Connected);
        assert!(r.violations().is_empty());
    }

    #[test]
    fn duplicate_resume_makes_no_radio_call() {
        let r = rig(true);
        r.sup.start();
        settle();
        r.sup.resume().unwrap();
        r.sup.suspend().unwrap();
        r.sup.resume().unwrap();
        r.sup.resume().unwrap();
        settle();
        assert_eq!(r.connects(), 2);
        assert_eq!(r.sup.state(), LinkState::Connected);
        assert!(r.violations().is_empty());
    }

    #[test]
    fn a_failed_stop_is_retried_by_the_next_suspend() {
        let r = rig(false);
        r.sup.start();
        settle();
        r.log.lock().unwrap().fail_stop = true;
        assert!(r.sup.suspend().is_err());
        assert_eq!(r.sup.state(), LinkState::Suspended);
        assert!(
            !r.sup.is_suspended(),
            "a requested suspend whose stop failed has not taken effect"
        );
        r.log.lock().unwrap().fail_stop = false;
        r.sup.suspend().unwrap();
        assert_eq!(r.log.lock().unwrap().stops, 1);
        assert!(r.sup.is_suspended());
        r.sup.resume().unwrap();
        assert!(!r.sup.is_suspended());
    }

    #[test]
    fn rapid_suspend_resume_cycles_keep_one_connect_in_flight() {
        let r = rig(true);
        r.sup.start();
        settle();
        for _ in 0..30 {
            r.sup.suspend().unwrap();
            r.sup.resume().unwrap();
        }
        wait_for_state(&r.sup, LinkState::Connected);
        settle();
        assert_eq!(r.sup.state(), LinkState::Connected);
        assert!(r.violations().is_empty(), "{:?}", r.violations());
    }

    #[test]
    fn concurrent_callers_never_overlap_or_double_connect() {
        let r = rig(true);
        r.sup.start();
        settle();
        let sup = &r.sup;
        thread::scope(|s| {
            for t in 0..4 {
                s.spawn(move || {
                    for i in 0..25 {
                        if (i + t) % 2 == 0 {
                            sup.suspend().unwrap();
                        } else {
                            sup.resume().unwrap();
                        }
                    }
                });
            }
        });
        r.sup.resume().unwrap();
        wait_for_state(&r.sup, LinkState::Connected);
        r.sup.suspend().unwrap();
        assert!(!r.log.lock().unwrap().started);
        assert!(r.violations().is_empty(), "{:?}", r.violations());
    }

    #[test]
    fn a_failed_resume_schedules_its_retry_before_returning() {
        let r = rig(true);
        r.sup.start();
        wait_for_state(&r.sup, LinkState::Connected);
        r.sup.suspend().unwrap();
        r.log.lock().unwrap().fail_every_connect = 1;
        assert!(r.sup.resume().is_err());
        assert_eq!(r.sup.state(), LinkState::Backoff);
        r.log.lock().unwrap().fail_every_connect = 0;
        // A suspend/resume right behind the failure must not leave a stale retry behind.
        r.sup.suspend().unwrap();
        r.sup.resume().unwrap();
        wait_for_state(&r.sup, LinkState::Connected);
        settle();
        assert_eq!(r.connects(), 2);
        assert!(r.violations().is_empty(), "{:?}", r.violations());
    }

    #[test]
    fn concurrent_callers_with_failing_radio_calls_stay_safe() {
        let r = rig(true);
        {
            let mut log = r.log.lock().unwrap();
            log.fail_every_connect = 3;
            log.fail_every_stop = 4;
        }
        r.sup.start();
        settle();
        let sup = &r.sup;
        thread::scope(|s| {
            for t in 0..4 {
                s.spawn(move || {
                    for i in 0..40 {
                        let _ = if (i + t) % 2 == 0 {
                            sup.suspend()
                        } else {
                            sup.resume()
                        };
                        if i % 7 == t {
                            thread::sleep(Duration::from_millis(2));
                        }
                    }
                });
            }
        });
        {
            let mut log = r.log.lock().unwrap();
            log.fail_every_connect = 0;
            log.fail_every_stop = 0;
        }
        r.sup.suspend().unwrap();
        r.sup.resume().unwrap();
        wait_for_state(&r.sup, LinkState::Connected);
        r.sup.suspend().unwrap();
        assert!(!r.log.lock().unwrap().started);
        assert!(r.violations().is_empty(), "{:?}", r.violations());
    }
}
