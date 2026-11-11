//! Thread-based ESP-NOW transmit worker for std tiers (the ESP-IDF `EspIdfEspNow`).
//!
//! One `espnow-tx` thread serves every transmission, scan probe and scan of a driver, and owns
//! the radio through a narrow [`EspNowRadio`] port: one `esp_now_*` / `esp_wifi_*` call per
//! method, no decisions.
//! The shape mirrors [`LinkSupervisor`](crate::wifi::LinkSupervisor): a std-gated core and worker
//! here, the port and the callback registration in the tier, host tests against a mock port.
//! The tier keeps everything that is a fact of the chip or of ESP-IDF (the promiscuous bracket
//! of a send, the pthread naming, the callback running in the Wi-Fi task, error mapping); this
//! module keeps every decision (ADR 023 decisions 9 and 10).
//!
//! # Why a worker
//!
//! `esp_now_send` returns when the frame is queued; the MAC-layer result arrives later in a
//! global send callback that carries no sequence number.
//! One permanent callback exists for the driver's lifetime and at most one frame is ever
//! outstanding, so a callback belongs to the frame the worker handed out.
//! The queue, ids, deadlines, cancellation and the scan-versus-send exclusion live in the
//! sans-IO [`TxCore`]; this module wraps it in a lock and runs it.
//!
//! # Threads, locks and the callback
//!
//! - The worker thread is the only one that calls [`EspNowRadio`]: it sends, changes the channel
//!   or TX power for a scan, and registers scan peers.
//! - Callers (async futures, or blocking calls that park a thread on a `block_on`) never touch
//!   the radio.
//!   They submit under the lock, wait on the [`WaitList`], and read their own slot's result.
//! - One `Mutex` guards the [`TxCore`] (which also knows about shutdown), the
//!   [`InFlightMarker`] (frame in flight and the callback outcome), the [`DeadlineSet`] of the
//!   callers and the [`ScanGate`].
//!   It is held for a few instructions and never across a radio call, a wait, or a waker call;
//!   wakers run after unlocking.
//!   It is never held together with the [`WaitList`] lock, so there is no lock order to violate.
//! - The send callback is an event the tier feeds in through [`SendEvents::send_done`].
//!   It runs in the Wi-Fi task: it locks the mutex, lets the marker record the outcome for the
//!   frame in flight (matched by destination MAC) and notifies the `Condvar`; it does not log,
//!   wait, call the radio or wake wakers.
//!   A callback that arrives while no frame is in flight is ignored.
//! - The receive callback is an event too: the tier offers every frame to
//!   [`SendEvents::probe_reply`] first, the reply tap of a running scan.
//!   It keeps a `0xF4` [`ProbeReply`] from the scan's peer in the one-slot `ReplyMailbox`
//!   under the same mutex and notifies the `Condvar`; every other frame, and every reply while
//!   no probe to that peer is outstanding, is left to the tier's receive queue.
//!   The mailbox is armed when a probe is handed to the radio, not first at
//!   [`ScanAction::AwaitReply`]: the ACK callback and the reply both arrive from the Wi-Fi
//!   task, so a reply may beat the worker to the wait step.
//! - Nothing that can free memory or build an error runs under the lock: scan jobs and results
//!   the gate discards are handed back and dropped after unlocking, and a full [`DeadlineSet`]
//!   grows from storage allocated outside the lock.
//! - The worker sleeps on one `Condvar` for everything: a submission, the callback, a scan
//!   request or cancellation, a caller deadline, shutdown.
//!   While a frame is in flight it waits for the callback without a timeout (fail-closed,
//!   ADR 020): only shutdown ends that wait.
//!   Caller deadlines still wake the worker, so a queued request expires on time.
//!
//! # Deadlines
//!
//! The library has no timer (ADR 021), so the worker is the timer: a caller with a finite
//! deadline registers it, the worker never sleeps past the earliest one, wakes the waiting
//! callers at it, and each caller then asks [`TxCore::poll_at`] under the lock, which decides
//! from the deadline stored with the request whether its frame never started (expired) or is on
//! the air (waited out).
//! A deadline bounds waiting for a slot, queueing and the start of the transmission, so a timeout
//! of zero (or one shorter than the worker's reaction) fails with [`WorkerError::Timeout`]
//! unless the worker started the frame at once.
//!
//! # Scan jobs
//!
//! Admission is atomic: [`ScanGate::admit`] claims the radio ([`TxCore::begin_scan`]) and stores
//! the job in one call, so from then on application sends fail with
//! [`WorkerError::ScanInProgress`] and queued ones were completed as preempted; a refused scan
//! changes and preempts nothing.
//! The queue keeps one slot for the scan's probes.
//! The worker finishes the frame in flight, runs the [`ScanMachine`] with its probes as
//! [`SendKind::Probe`] frames through the same in-flight and callback path, restores channel,
//! TX power and peer registration, and only then does [`ScanGate::finish`] end the exclusion
//! while publishing the result.
//!
//! # Cancellation of a scan
//!
//! Dropping the scan future cancels the job in the [`ScanGate`] and wakes the worker; it never
//! touches the radio (`Drop` cannot await).
//! The worker feeds [`ScanInput::Abort`] at its next step, and the machine unwinds through its
//! own restore sequence, so the exclusion is held until the restore is done and the caller's
//! result is discarded.
//!
//! - During a probe: the frame on the air is waited out first (the channel must not change under
//!   a transmission), then the probe result is replaced by `Abort`.
//! - During a confirmation gap: the gap sleeps on the `Condvar`, so cancellation interrupts it.
//! - Inside the machine's teardown `Abort` stands for the step's expected input, so it never
//!   skips a restore; a scan that had already found the peer keeps its new pin
//!   ([`ScanInput::or_abort`] keeps a `Failed` result).
//! - Before the worker picks the job up: the gate drops the job and ends the exclusion, and no
//!   radio action happens at all.
//! - After the scan finished: the unread result is discarded.
//!
//! A scan started right after the cancel does not fail: [`ScanGate::admit`] answers
//! [`GateRefusal::Unwinding`] while the cancelled scan holds the slot, and
//! [`EspNowWorker::scan_for_peer`] waits on the [`WaitList`] until the worker has ended the cancelled
//! scan (probe waited out, restore done, [`ScanGate::finish`] or [`ScanGate::release`], which
//! wakes the list) and then admits again.
//!
//! Shutdown ends the wait for a callback that never fires, and the worker feeds
//! [`ScanInput::Shutdown`]: the machine finishes at once without a teardown, so no channel,
//! TX-power or peer call follows.
//!
//! # Fail-closed send callback
//!
//! The worker waits for a frame's callback without a limit while the driver lives, because the
//! channel and the peer table must not change under a frame on the air and a late callback must
//! not be taken for the next frame's.
//! A callback that never fires therefore blocks that frame, every later transmission and every
//! scan until shutdown.

use core::task::Poll;
use core::time::Duration;
use std::fmt::Debug;
use std::future::Future;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::task::Waker;
use std::thread;
use std::time::Instant;

use super::{
    Cancelled, Completed, Completion, DeadlineId, DeadlineSet, DeadlineStorage, GateRefusal,
    InFlightMarker, JobEpoch, MacAddress, Next, PeerConfig, ProbeReply, Refusal, ScanAction,
    ScanConfig, ScanError, ScanGate, ScanInput, ScanMachine, ScanResult, SendKind, SendOutcome,
    TxCore, TxId, WifiInterface, MAX_DATA_LEN,
};
use crate::wait_list::WaitList;

/// Request slots of the send queue.
///
/// [`TxCore`] keeps one slot for the scan's probe, so application requests of concurrent callers
/// (and an abandoned frame still on the air, which is one of them) share the other
/// `TX_SLOTS - 1`; more callers wait for a slot until their deadline.
pub const TX_SLOTS: usize = 4;

/// Initial capacity of the caller [`DeadlineSet`]; it grows outside the lock.
const DEADLINE_CAPACITY: usize = 8;

/// The radio calls a worker makes; each must return without blocking for long.
///
/// One driver call per method and no decisions: when to call, in which order and what a failure
/// means is the worker's business.
/// All methods run on the worker thread only.
pub trait EspNowRadio: Send + 'static {
    /// Driver error.
    type Error: Debug + Send + 'static;

    /// Hands one frame to the radio (`esp_now_send`); the result arrives later as
    /// [`SendEvents::send_done`].
    ///
    /// With `repin`, the tier first re-pins the radio to that channel in whatever way its chip
    /// needs (the ESP-IDF unassociated-station bracket, ADR 012) and must keep that bracket and
    /// the send consecutive.
    ///
    /// # Errors
    ///
    /// The driver refused the frame; no callback follows.
    fn send(&mut self, mac: &MacAddress, data: &[u8], repin: Option<u8>)
        -> Result<(), Self::Error>;

    /// Switches the radio to `channel` (`esp_wifi_set_channel`, no secondary channel).
    ///
    /// # Errors
    ///
    /// The driver refused the channel.
    fn set_channel(&mut self, channel: u8) -> Result<(), Self::Error>;

    /// Reads the maximum TX power in quarter-dBm (`esp_wifi_get_max_tx_power`).
    ///
    /// # Errors
    ///
    /// The driver could not report the level.
    fn tx_power(&mut self) -> Result<i8, Self::Error>;

    /// Sets the maximum TX power in quarter-dBm (`esp_wifi_set_max_tx_power`).
    ///
    /// # Errors
    ///
    /// The driver refused the level.
    fn set_tx_power(&mut self, quarter_dbm: i8) -> Result<(), Self::Error>;

    /// Registers an ESP-NOW peer (`esp_now_add_peer`).
    ///
    /// # Errors
    ///
    /// The driver refused the peer.
    fn add_peer(&mut self, peer: &PeerConfig) -> Result<(), Self::Error>;

    /// Removes an ESP-NOW peer (`esp_now_del_peer`).
    ///
    /// # Errors
    ///
    /// The driver refused the removal (for instance the peer is not registered).
    fn remove_peer(&mut self, mac: &MacAddress) -> Result<(), Self::Error>;
}

/// What a worker needs to know about the radio mode it serves; these are facts of the tier.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerConfig {
    /// Whether the driver owns the radio, which a channel scan needs (ADR 012).
    pub owns_radio: bool,
    /// Interface scan peers are registered on.
    pub interface: WifiInterface,
    /// Whether application frames carry the pinned channel as `repin` (unassociated station).
    pub repin: bool,
    /// The channel the owned radio starts on, if the driver set one.
    pub initial_pin: Option<u8>,
    /// TX power in quarter-dBm used for the scan burst.
    pub burst_tx_power: i8,
}

/// Result of one transmission, as reported to the caller of the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TxOutcome<E> {
    /// The peer acknowledged the frame at the MAC layer.
    Acked,
    /// The frame was transmitted but not acknowledged.
    NoAck,
    /// [`EspNowRadio::send`] refused the frame; no callback follows.
    SendFailed(E),
}

/// Why a request did not complete with a transmission.
#[derive(Debug)]
pub enum WorkerError<E> {
    /// The deadline passed before the transmission started (or before a queue slot freed up).
    Timeout,
    /// A scan runs or began: application sends are refused or were preempted.
    ScanInProgress,
    /// The worker is shut down.
    Shutdown,
    /// The request was lost (its result was already taken) or the worker was busy.
    Busy,
    /// The payload exceeds [`MAX_DATA_LEN`].
    PayloadTooLarge {
        /// Length of the refused payload.
        len: usize,
    },
    /// An acknowledged send to the broadcast address was requested.
    Broadcast,
    /// The driver does not own the radio, so it cannot scan.
    RadioNotOwned,
    /// The scan failed: the machine's verdict plus the driver error that caused it, if any.
    Scan {
        /// The machine's verdict.
        kind: ScanError,
        /// The port error behind a registration failure.
        cause: Option<E>,
    },
}

/// A scan request with an owned copy of its [`ScanConfig`], since the caller's config borrows.
struct ScanJob {
    mac: MacAddress,
    channels: Vec<u8>,
    probe_timeout: Duration,
    burst_timeout: Duration,
    probe_confirmations: u8,
    confirmation_gap: Duration,
    reply_timeout: Duration,
}

impl ScanJob {
    fn new(mac: MacAddress, config: &ScanConfig<'_>) -> Self {
        // `..` skips the deprecated `probe_data`: the machine builds its own probe frames.
        let ScanConfig {
            channels,
            probe_timeout,
            burst_timeout,
            probe_confirmations,
            confirmation_gap,
            reply_timeout,
            ..
        } = *config;
        Self {
            mac,
            channels: channels.to_vec(),
            probe_timeout,
            burst_timeout,
            probe_confirmations,
            confirmation_gap,
            reply_timeout,
        }
    }

    /// The borrowed form the [`ScanMachine`] takes.
    fn config(&self) -> ScanConfig<'_> {
        ScanConfig {
            channels: &self.channels,
            probe_timeout: self.probe_timeout,
            burst_timeout: self.burst_timeout,
            probe_confirmations: self.probe_confirmations,
            confirmation_gap: self.confirmation_gap,
            reply_timeout: self.reply_timeout,
            ..ScanConfig::for_peer()
        }
    }
}

/// A failed scan: the machine's verdict plus the port error that caused it, if any.
struct ScanFailure<E> {
    kind: ScanError,
    cause: Option<E>,
}

impl<E> ScanFailure<E> {
    fn into_error(self) -> WorkerError<E> {
        WorkerError::Scan {
            kind: self.kind,
            cause: self.cause,
        }
    }
}

/// The one-slot mailbox of the reply tap: the probe reply the running scan waits for.
///
/// Armed by the worker from the moment a probe to `awaiting` is handed to the radio until the
/// scan moves past the reply wait, and read at [`ScanAction::AwaitReply`].
/// The receive callback fills it ([`SendEvents::probe_reply`]); a newer reply replaces an unread
/// one, since the machine matches the reply to the outstanding probe anyway.
#[derive(Default)]
struct ReplyMailbox {
    /// The peer whose `0xF4` replies the tap takes; `None` while no probe is outstanding.
    awaiting: Option<MacAddress>,
    /// The latest reply from `awaiting` since the slot was last read.
    reply: Option<ProbeReply>,
}

/// Everything behind the one lock.
struct TxState<E> {
    core: TxCore<TxOutcome<E>, TX_SLOTS>,
    /// The frame handed to the radio whose send callback has not arrived, and the verdict.
    inflight: InFlightMarker,
    /// Deadlines of callers that wait; the worker wakes them when one passes.
    deadlines: DeadlineSet,
    /// The single scan job: admission, hand-off, result, cancellation and the scan exclusion of
    /// `core`, kept in step by the gate.
    scan: ScanGate<ScanJob, Result<ScanResult, ScanFailure<E>>>,
    /// The probe reply the running scan waits for, fed by the receive callback.
    reply: ReplyMailbox,
}

/// One frame handed out by [`TxCore::next`], copied so the lock can be released to transmit.
struct Frame {
    id: TxId,
    mac: MacAddress,
    len: usize,
    data: [u8; MAX_DATA_LEN],
}

/// What the worker does after asking the queue.
// A short-lived stack value: boxing the frame would allocate once per transmission for nothing.
#[allow(clippy::large_enum_variant)]
enum Step {
    Send(Frame),
    /// Nothing to send; wake at this time (or on a wake-up) to expire queued requests.
    Wait(Option<Duration>),
}

/// State shared by the callers, the worker thread and the send callback.
struct Shared<E> {
    state: Mutex<TxState<E>>,
    /// Wakes the worker; nobody else waits on it.
    wake: Condvar,
    /// Wakes callers waiting for a slot, a result or a scan.
    waiters: WaitList,
    epoch: Instant,
    config: WorkerConfig,
    /// The channel the driver keeps pinned; [`u8::MAX`] means none.
    pinned: AtomicU8,
    /// The probe sequence number the last scan ended on; the next scan continues it, so a
    /// reply left over from the previous scan never matches a new probe.
    scan_seq: AtomicU8,
}

/// The error of a refused submission.
fn map_refusal<E>(refusal: Refusal, len: usize) -> WorkerError<E> {
    match refusal {
        Refusal::Expired => WorkerError::Timeout,
        // Not reached: a full queue is a wait (`try_submit` retries) until the deadline, after
        // which the core answers `Expired`.
        Refusal::Full => WorkerError::Busy,
        Refusal::Scanning => WorkerError::ScanInProgress,
        Refusal::TooLarge => WorkerError::PayloadTooLarge { len },
        Refusal::Broadcast => WorkerError::Broadcast,
        Refusal::Shutdown => WorkerError::Shutdown,
    }
}

/// The error of a request that did not complete with a transmission.
fn map_completion<E>(completion: Completion<TxOutcome<E>>) -> Result<TxOutcome<E>, WorkerError<E>> {
    match completion {
        Completion::Done(outcome) => Ok(outcome),
        Completion::Expired => Err(WorkerError::Timeout),
        Completion::Preempted => Err(WorkerError::ScanInProgress),
        Completion::Shutdown => Err(WorkerError::Shutdown),
        // The caller lost track of its own request: no result to report.
        Completion::Unknown => Err(WorkerError::Busy),
    }
}

fn map_gate_refusal<E>(refusal: GateRefusal) -> WorkerError<E> {
    match refusal {
        GateRefusal::Shutdown => WorkerError::Shutdown,
        // `Unwinding` is waited out by `admit_scan` and never reaches a caller; `Busy` is the
        // conservative answer if it ever does.
        GateRefusal::Busy | GateRefusal::Unwinding => WorkerError::Busy,
        GateRefusal::NotOwned => WorkerError::RadioNotOwned,
    }
}

/// Why a scan probe produced no MAC-layer result; only logged, the machine sees a local failure.
#[derive(Debug)]
enum ProbeFault<E> {
    /// The port refused the frame.
    Send(E),
    /// The probe never completed: refused, expired, or shutdown raced in.
    Worker(WorkerError<E>),
}

/// Removes a registered caller deadline when the call ends.
struct DeadlineEntry<'a, E> {
    shared: &'a Shared<E>,
    id: DeadlineId,
}

impl<E> Drop for DeadlineEntry<'_, E> {
    fn drop(&mut self) {
        self.shared.lock().deadlines.remove(self.id);
    }
}

/// Abandons a request when its caller is dropped before the result was taken.
struct PendingSend<'a, E> {
    shared: &'a Shared<E>,
    /// Cleared once the result has been consumed: a consumed id may be reused.
    id: Option<TxId>,
}

impl<E> Drop for PendingSend<'_, E> {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            self.shared.lock().core.abandon(id);
            self.shared.waiters.wake_all();
        }
    }
}

/// Proof of an admitted scan; dropping it before the result was taken requests cancellation.
struct ScanTicket<'a, E> {
    shared: &'a Shared<E>,
    epoch: JobEpoch,
    consumed: AtomicBool,
}

impl<E> ScanTicket<'_, E> {
    /// The scan's result once the worker has finished it (restore done, exclusion ended).
    ///
    /// Only the result of this ticket's own scan is taken.
    fn poll(&self) -> Option<Result<ScanResult, WorkerError<E>>> {
        let (taken, shutdown) = {
            let mut state = self.shared.lock();
            (state.scan.take(self.epoch), state.core.is_shut_down())
        };
        if let Some(result) = taken {
            self.consumed.store(true, Ordering::Relaxed);
            return Some(result.map_err(ScanFailure::into_error));
        }
        if shutdown {
            self.consumed.store(true, Ordering::Relaxed);
            return Some(Err(WorkerError::Shutdown));
        }
        None
    }
}

impl<E> Drop for ScanTicket<'_, E> {
    fn drop(&mut self) {
        if self.consumed.load(Ordering::Relaxed) {
            return;
        }
        let cancelled = {
            let mut guard = self.shared.lock();
            let state = &mut *guard;
            // A job no worker picked up is dropped and the gate ends the exclusion itself.
            state.scan.cancel(&mut state.core, self.epoch)
        };
        if matches!(cancelled, Cancelled::Requested) {
            self.shared.wake.notify_all();
        }
        // The discarded job or result is freed here, outside the lock.
        drop(cancelled);
    }
}

impl<E> Shared<E> {
    fn new(config: WorkerConfig) -> Self {
        Self {
            state: Mutex::new(TxState {
                core: TxCore::new(),
                inflight: InFlightMarker::new(),
                deadlines: DeadlineSet::with_capacity(DEADLINE_CAPACITY),
                scan: ScanGate::new(),
                reply: ReplyMailbox::default(),
            }),
            wake: Condvar::new(),
            waiters: WaitList::new(),
            epoch: Instant::now(),
            config,
            pinned: AtomicU8::new(config.initial_pin.unwrap_or(u8::MAX)),
            scan_seq: AtomicU8::new(0),
        }
    }

    fn lock(&self) -> MutexGuard<'_, TxState<E>> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Time on the [`TxCore`] clock.
    fn now(&self) -> Duration {
        self.epoch.elapsed()
    }

    fn pinned_channel(&self) -> Option<u8> {
        match self.pinned.load(Ordering::Relaxed) {
            u8::MAX => None,
            channel => Some(channel),
        }
    }

    fn on_send_done(&self, mac: &[u8], acked: bool) {
        let recorded = self.lock().inflight.on_callback(mac, acked);
        if recorded {
            self.wake.notify_all();
        }
    }

    /// The reply tap: keeps a `0xF4` reply from `mac` while a probe to that peer is
    /// outstanding, and reports whether the frame was taken.
    fn on_probe_reply(&self, mac: &[u8], data: &[u8]) -> bool {
        let Some(reply) = ProbeReply::parse(data) else {
            return false;
        };
        let taken = {
            let mut state = self.lock();
            match state.reply.awaiting {
                Some(awaiting) if awaiting[..] == *mac => {
                    state.reply.reply = Some(reply);
                    true
                }
                _ => false,
            }
        };
        if taken {
            self.wake.notify_all();
        }
        taken
    }

    /// Arms the reply tap for the probe to `mac` that is about to go out; a reply still in the
    /// slot answered an earlier probe and is dropped.
    fn arm_reply(&self, mac: MacAddress) {
        let mut state = self.lock();
        state.reply.awaiting = Some(mac);
        state.reply.reply = None;
    }

    /// Disarms the reply tap: replies reach the application again.
    fn disarm_reply(&self) {
        let mut state = self.lock();
        state.reply.awaiting = None;
        state.reply.reply = None;
    }

    /// Ends every request, tells the worker to stop and wakes every caller; idempotent.
    fn begin_shutdown(&self) {
        {
            self.lock().core.shutdown(&mut |_waker| {});
        }
        self.wake.notify_all();
        self.waiters.wake_all();
    }

    // -- caller side -------------------------------------------------------------------------

    /// Registers a caller deadline so the worker wakes the waiting callers when it passes.
    ///
    /// A full set grows from storage allocated here, outside the lock, and the storage it no
    /// longer needs is freed outside the lock too.
    fn register_deadline(&self, at: Duration) -> DeadlineEntry<'_, E> {
        let mut spare: Option<DeadlineStorage> = None;
        loop {
            let mut state = self.lock();
            if let Some(bigger) = spare.take() {
                spare = Some(state.deadlines.grow(bigger));
            }
            match state.deadlines.register(at) {
                Ok(id) => {
                    drop(state);
                    drop(spare);
                    self.wake.notify_all();
                    return DeadlineEntry { shared: self, id };
                }
                Err(full) => {
                    drop(state);
                    drop(spare.take());
                    spare = Some(DeadlineStorage::with_capacity(full.wanted));
                }
            }
        }
    }

    /// One attempt to queue the frame.
    ///
    /// `None` means the queue is full and the deadline has not passed: wait and retry.
    /// Once it has passed the core answers [`Refusal::Expired`] instead.
    fn try_submit(
        &self,
        mac: &MacAddress,
        data: &[u8],
        deadline: Option<Duration>,
        kind: SendKind,
    ) -> Option<Result<TxId, WorkerError<E>>> {
        let now = self.now();
        let mut state = self.lock();
        match state.core.submit(now, *mac, data, deadline, kind) {
            Ok(id) => {
                drop(state);
                self.wake.notify_all();
                Some(Ok(id))
            }
            Err(Refusal::Full) => None,
            Err(refusal) => Some(Err(map_refusal(refusal, data.len()))),
        }
    }

    /// The caller's view of request `id`; `None` while it is pending.
    ///
    /// The core uses the deadline stored with the request: once it passed, a queued request
    /// expires and a frame on the air is waited out.
    fn poll_completion(&self, id: TxId) -> Option<Completion<TxOutcome<E>>> {
        let now = self.now();
        let mut state = self.lock();
        match state.core.poll_at(id, now, Waker::noop()) {
            Poll::Ready(completion) => Some(completion),
            Poll::Pending => None,
        }
    }

    async fn run_frame(
        &self,
        mac: &MacAddress,
        data: &[u8],
        timeout: Option<Duration>,
        kind: SendKind,
    ) -> Result<TxOutcome<E>, WorkerError<E>> {
        // `None` for `Duration::MAX` or a time too far ahead to represent: no bound.
        let deadline = timeout.and_then(|t| self.now().checked_add(t));
        let _deadline_entry = deadline.map(|at| self.register_deadline(at));
        let id = self
            .waiters
            .wait(|| self.try_submit(mac, data, deadline, kind))
            .await?;
        // No await sits between the queue accepting the request and this guard.
        let mut pending = PendingSend {
            shared: self,
            id: Some(id),
        };
        let completion = self.waiters.wait(|| self.poll_completion(id)).await;
        pending.id = None;
        // The slot is free again: callers waiting for one may proceed.
        self.waiters.wake_all();
        map_completion(completion)
    }

    /// One admission attempt: `None` while a cancelled scan still unwinds (wait and retry).
    ///
    /// `job` stays in place on `None`; it is consumed by an admission or by any other refusal.
    fn try_admit_scan(
        &self,
        job: &mut Option<ScanJob>,
    ) -> Option<Result<ScanTicket<'_, E>, WorkerError<E>>> {
        let mut preempted = false;
        let admitted = {
            let mut guard = self.lock();
            let state = &mut *guard;
            // One call: shutdown, a job or untaken result in the slot, then the radio claim,
            // with nothing changed (and nothing preempted) on a refusal.
            state.scan.admit(
                &mut state.core,
                self.config.owns_radio,
                job.take()?,
                &mut |_waker| preempted = true,
            )
        };
        let epoch = match admitted {
            Ok(epoch) => epoch,
            Err((GateRefusal::Unwinding, refused)) => {
                *job = Some(refused);
                return None;
            }
            Err((refusal, refused)) => {
                drop(refused);
                return Some(Err(map_gate_refusal(refusal)));
            }
        };
        self.wake.notify_all();
        if preempted {
            self.waiters.wake_all();
        }
        Some(Ok(ScanTicket {
            shared: self,
            epoch,
            consumed: AtomicBool::new(false),
        }))
    }

    /// Admits a scan; a scan cancelled just before still unwinds (its probe on the air, then
    /// the restore), so this waits on the wait list until the worker ends it
    /// ([`Shared::finish_scan_job`] wakes the list) and then admits again.
    /// Shutdown wakes the list too and the gate answers it as [`WorkerError::Shutdown`].
    /// The lock is never held across the await.
    async fn admit_scan(
        &self,
        mac: MacAddress,
        config: &ScanConfig<'_>,
    ) -> Result<ScanTicket<'_, E>, WorkerError<E>> {
        let mut job = Some(ScanJob::new(mac, config));
        self.waiters.wait(|| self.try_admit_scan(&mut job)).await
    }

    // -- worker side -------------------------------------------------------------------------

    /// Sleeps on the `Condvar` until a wake-up or `until`, never past a caller deadline.
    ///
    /// Callers whose deadline has passed are woken first; the guard is then re-taken, so the
    /// caller of `park` must re-check its conditions after every return.
    fn park<'a>(
        &'a self,
        mut state: MutexGuard<'a, TxState<E>>,
        until: Option<Duration>,
    ) -> MutexGuard<'a, TxState<E>> {
        let now = self.now();
        if state.deadlines.expire(now) {
            drop(state);
            self.waiters.wake_all();
            return self.lock();
        }
        let target = state.deadlines.earliest().into_iter().chain(until).min();
        match target {
            None => self
                .wake
                .wait(state)
                .unwrap_or_else(PoisonError::into_inner),
            Some(at) => {
                self.wake
                    .wait_timeout(state, at.saturating_sub(now))
                    .unwrap_or_else(PoisonError::into_inner)
                    .0
            }
        }
    }

    /// Asks the queue for the next frame and, for a frame, marks it as awaiting its callback in
    /// the same critical section.
    ///
    /// Also reports whether requests expired, so the caller wakes them after unlocking.
    fn next_frame(&self, state: &mut TxState<E>) -> (Step, bool) {
        let mut expired = false;
        let now = self.now();
        let step = match state.core.next(now, &mut |_waker| expired = true) {
            Next::Transmit { id, mac, data } => {
                let mut frame = Frame {
                    id,
                    mac,
                    len: data.len(),
                    data: [0; MAX_DATA_LEN],
                };
                frame.data[..data.len()].copy_from_slice(data);
                Step::Send(frame)
            }
            Next::WaitUntil(at) => Step::Wait(Some(at)),
            Next::Idle => Step::Wait(None),
        };
        if let Step::Send(frame) = &step {
            state.inflight.begin(frame.id, frame.mac);
        }
        (step, expired)
    }

    /// The channel to re-pin application frames to, if this mode needs it.
    fn repin_channel(&self) -> Option<u8> {
        if self.config.repin {
            self.pinned_channel()
        } else {
            None
        }
    }
}

impl<E: Debug> Shared<E> {
    /// Transmits `frame` and waits for its callback, then reports the result to its caller.
    ///
    /// `repin` is the channel to re-pin first (unassociated STA mode, application frames).
    /// Without a callback the wait ends only on shutdown (fail-closed).
    fn service<R: EspNowRadio<Error = E>>(&self, radio: &mut R, frame: &Frame, repin: Option<u8>) {
        let sent = radio.send(&frame.mac, &frame.data[..frame.len], repin);
        let mut state = self.lock();
        let result = match sent {
            Ok(()) => {
                // ADR 020 fail-closed exception: no time bound while the frame is on the air.
                loop {
                    if let Some(acked) = state.inflight.take_outcome() {
                        break Some(if acked {
                            TxOutcome::Acked
                        } else {
                            TxOutcome::NoAck
                        });
                    }
                    if state.core.is_shut_down() {
                        break None;
                    }
                    state = self.park(state, None);
                }
            }
            Err(error) => Some(TxOutcome::SendFailed(error)),
        };
        state.inflight.clear();
        // The caller is woken through the wait list, not through the slot's waker.
        let completed = result.map(|result| state.core.complete(frame.id, result));
        let shutdown = state.core.is_shut_down();
        drop(state);
        self.waiters.wake_all();
        match completed {
            // Only shutdown may end a request while its frame is on the air.
            Some(Completed::Unknown) => {
                debug_assert!(
                    shutdown,
                    "ESP-NOW frame {:?} completed while not in flight",
                    frame.id
                );
                if !shutdown {
                    log::warn!(
                        "ESP-NOW: send result for frame {:?} that is not in flight dropped",
                        frame.id
                    );
                }
            }
            Some(Completed::Discarded) => {
                log::debug!("ESP-NOW: result of an abandoned frame discarded");
            }
            Some(Completed::Delivered(_)) | None => {}
        }
    }

    /// The thread body: serve the queue and scan jobs until shutdown.
    fn run<R: EspNowRadio<Error = E>>(&self, radio: &mut R) {
        let mut state = self.lock();
        loop {
            if state.core.is_shut_down() {
                return;
            }
            let (step, expired) = self.next_frame(&mut state);
            match step {
                Step::Send(frame) => {
                    drop(state);
                    if expired {
                        self.waiters.wake_all();
                    }
                    self.service(radio, &frame, self.repin_channel());
                    state = self.lock();
                }
                Step::Wait(_) if expired => {
                    drop(state);
                    self.waiters.wake_all();
                    state = self.lock();
                }
                Step::Wait(until) => {
                    // Nothing is in flight here, so a pending scan may start.
                    if state.scan.may_start(&state.core) {
                        let Some((epoch, job)) = state.scan.pickup() else {
                            continue;
                        };
                        drop(state);
                        self.run_scan(radio, epoch, &job);
                        drop(job);
                        state = self.lock();
                    } else {
                        state = self.park(state, until);
                    }
                }
            }
        }
    }

    // -- scan --------------------------------------------------------------------------------

    /// True when the scan must be given up: its caller is gone or the driver is shutting down.
    fn abort_requested(&self, epoch: JobEpoch) -> bool {
        let state = self.lock();
        state.core.is_shut_down() || state.scan.cancel_requested(epoch)
    }

    /// The input to feed the scan machine in place of `input`: [`ScanInput::Shutdown`] once the
    /// driver shuts down, [`ScanInput::Abort`] (see [`ScanInput::or_abort`]) once the caller
    /// cancelled.
    fn scan_input(&self, epoch: JobEpoch, input: ScanInput) -> ScanInput {
        let state = self.lock();
        if state.core.is_shut_down() {
            ScanInput::Shutdown
        } else {
            input.or_abort(state.scan.cancel_requested(epoch))
        }
    }

    /// Sleeps until `until` on the scan's clock, ending early on cancellation or shutdown.
    fn sleep_until(&self, epoch: JobEpoch, until: Duration) {
        let mut state = self.lock();
        loop {
            if state.core.is_shut_down()
                || state.scan.cancel_requested(epoch)
                || self.now() >= until
            {
                return;
            }
            state = self.park(state, Some(until));
        }
    }

    /// Waits for a probe reply in the armed mailbox until `until` on the scan's clock.
    ///
    /// [`ScanInput::Reply`] for a reply, [`ScanInput::Completed`] once the deadline has passed;
    /// cancellation and shutdown end the wait early with `Completed` too (the caller substitutes
    /// `Abort` or `Shutdown` at the next step).
    /// A reply that arrived between the probe and this call is already in the slot.
    /// The deadline is checked before the slot, so a peer that streams replies the machine
    /// ignores cannot hold the scan in its wait past `until`.
    fn await_reply(&self, epoch: JobEpoch, until: Duration) -> ScanInput {
        let mut state = self.lock();
        loop {
            if state.core.is_shut_down()
                || state.scan.cancel_requested(epoch)
                || self.now() >= until
            {
                return ScanInput::Completed;
            }
            if let Some(reply) = state.reply.reply.take() {
                return ScanInput::Reply(reply);
            }
            state = self.park(state, Some(until));
        }
    }

    /// Runs one scan job and ends the exclusion once everything is restored.
    fn run_scan<R: EspNowRadio<Error = E>>(&self, radio: &mut R, epoch: JobEpoch, job: &ScanJob) {
        if self.abort_requested(epoch) {
            // Cancelled or shut down before any radio action: nothing to restore.
            self.finish_scan_job(epoch, None);
            return;
        }
        let mut machine = ScanMachine::new(job.mac, job.config(), self.pinned_channel())
            .with_first_seq(self.scan_seq.load(Ordering::Relaxed));
        let result = self.drive_scan(radio, epoch, &mut machine);
        // The pin follows the machine whatever became of the caller: on a success the peer is
        // registered on the new channel, on any failure the machine restored the old one.
        if let Some(channel) = machine.pinned_channel() {
            self.pinned.store(channel, Ordering::Relaxed);
        }
        self.scan_seq.store(machine.seq(), Ordering::Relaxed);
        match &result {
            Ok(found) => log::info!("Peer {:02X?} found on channel {}", job.mac, found.channel),
            Err(failure) if matches!(failure.kind, ScanError::NotFound { .. }) => {
                log::warn!("Peer {:02X?} not found on any scanned channel", job.mac);
            }
            Err(_) => {}
        }
        self.finish_scan_job(epoch, Some(result));
    }

    /// Ends the exclusion and publishes the result, or discards it when the caller is gone.
    fn finish_scan_job(&self, epoch: JobEpoch, result: Option<Result<ScanResult, ScanFailure<E>>>) {
        let discarded = {
            let mut guard = self.lock();
            let state = &mut *guard;
            // The gate ends the exclusion in the call that publishes (or drops) the result.
            match result {
                Some(result) => state.scan.finish(&mut state.core, epoch, result),
                None => {
                    state.scan.release(&mut state.core, epoch);
                    None
                }
            }
        };
        self.waiters.wake_all();
        // A discarded result (cancelled caller) is freed outside the lock.
        drop(discarded);
    }

    /// Transmits one scan probe through the queue and reports how it went.
    ///
    /// `timeout` bounds the probe's queue wait only: a probe that has not started by then is
    /// never sent (a local failure for the machine).
    /// A probe that has started is waited out however long the callback takes (ADR 020
    /// fail-closed): the channel must not change under a frame on the air.
    fn transmit_probe<R: EspNowRadio<Error = E>>(
        &self,
        radio: &mut R,
        mac: &MacAddress,
        data: &[u8],
        timeout: Duration,
    ) -> Result<SendOutcome, ProbeFault<E>> {
        // `TxCore` keeps a slot for the probe, so a queue that application callers filled (and
        // whose preempted requests are not yet collected) cannot refuse it with `Full`.
        let now = self.now();
        let id = self
            .lock()
            .core
            .submit(now, *mac, data, now.checked_add(timeout), SendKind::Probe)
            .map_err(|refusal| ProbeFault::Worker(map_refusal(refusal, data.len())))?;
        // The queue holds nothing else (the worker drained it before the scan began, and
        // application sends are refused), so the first frame is ours; any other is serviced too.
        // No frame at all means the probe expired at pickup, or shutdown ended it.
        loop {
            let (step, expired) = {
                let mut state = self.lock();
                self.next_frame(&mut state)
            };
            if expired {
                self.waiters.wake_all();
            }
            let Step::Send(frame) = step else {
                break;
            };
            self.service(radio, &frame, None);
            if frame.id == id {
                break;
            }
        }
        let (completion, shutdown) = {
            let mut state = self.lock();
            let completion = match state.core.poll_result(id, Waker::noop()) {
                Poll::Ready(completion) => Some(completion),
                Poll::Pending => {
                    state.core.abandon(id);
                    None
                }
            };
            (completion, state.core.is_shut_down())
        };
        match completion.map(map_completion) {
            Some(Ok(TxOutcome::Acked)) => Ok(SendOutcome::Acked),
            Some(Ok(TxOutcome::NoAck)) => Ok(SendOutcome::NoAck),
            Some(Ok(TxOutcome::SendFailed(error))) => Err(ProbeFault::Send(error)),
            Some(Err(error)) => Err(ProbeFault::Worker(error)),
            None if shutdown => Err(ProbeFault::Worker(WorkerError::Shutdown)),
            None => Err(ProbeFault::Worker(WorkerError::Busy)),
        }
    }

    /// Drives a [`ScanMachine`] against the radio, on the worker thread.
    ///
    /// The machine owns the algorithm (ADR 009, ADR 012); this function only performs the
    /// requested I/O and reports the outcome.
    ///
    /// Note on radio side-effects: TX power is a global radio setting.
    /// While the burst is active, any concurrent Wi-Fi transmissions on this chip also go out at
    /// the boosted level.
    fn drive_scan<R: EspNowRadio<Error = E>>(
        &self,
        radio: &mut R,
        epoch: JobEpoch,
        machine: &mut ScanMachine<'_>,
    ) -> Result<ScanResult, ScanFailure<E>> {
        let mac = *machine.mac();
        let temp_peer = PeerConfig {
            mac,
            channel: 0,
            encrypt: false,
            interface: self.config.interface,
        };

        // `Some` once the level before the burst was read: `RestoreTxPower` puts it back.
        let mut saved_tx_power: Option<i8> = None;
        let mut last_register_error: Option<E> = None;
        // Whatever ends the scan, the reply tap must not outlive it.
        let _disarm = DisarmOnExit(self);
        // True while the reply tap is armed: from a probe's send until the scan moves past its
        // reply wait (see [`ReplyMailbox`]).
        let mut armed = false;

        let mut input = ScanInput::Start;
        loop {
            // On shutdown `Shutdown` ends the scan without a teardown; on cancellation `Abort`
            // replaces every result but a failure, and inside the machine's teardown it continues
            // the restore, so it may be fed at every step.
            if input != ScanInput::Start {
                input = self.scan_input(epoch, input);
            }
            let action = match machine.poll_bounded(self.now(), input) {
                Ok(action) => action,
                Err(kind) => return Err(ScanFailure { kind, cause: None }),
            };
            if armed
                && !matches!(
                    action,
                    ScanAction::SendProbe { .. } | ScanAction::AwaitReply { .. }
                )
            {
                self.disarm_reply();
                armed = false;
            }
            input = match action {
                ScanAction::RemovePeer => {
                    // Best effort: removes a stale registration, or the scan peer.
                    let _ = radio.remove_peer(&mac);
                    ScanInput::Completed
                }
                ScanAction::RegisterPeer { channel } => {
                    let peer = PeerConfig {
                        channel,
                        ..temp_peer
                    };
                    match radio.add_peer(&peer) {
                        Ok(()) => ScanInput::Completed,
                        Err(e) => {
                            last_register_error = Some(e);
                            ScanInput::Failed
                        }
                    }
                }
                ScanAction::RegisterFallbackPeer { channel } => {
                    let peer = PeerConfig {
                        channel,
                        ..temp_peer
                    };
                    match radio.add_peer(&peer) {
                        Ok(()) => log::debug!(
                            "Peer {:02X?} fallback-registered on last known channel {}",
                            mac,
                            channel
                        ),
                        Err(e) => log::warn!(
                            "Failed to fallback-register peer {:02X?} on channel {}: {:?} \
                             (next send will fail until the next successful scan)",
                            mac,
                            channel,
                            e
                        ),
                    }
                    ScanInput::Completed
                }
                ScanAction::BoostTxPower => {
                    // Auto-burst: boost TX power to maximum during channel scanning to maximise
                    // discovery range; the machine asks for the restore of the previous level.
                    // Without a readable level there is nothing to restore, so the power is
                    // left alone rather than boosted for good (as the esp-hal tier does).
                    match radio.tx_power() {
                        Ok(current) => {
                            saved_tx_power = Some(current);
                            if radio.set_tx_power(self.config.burst_tx_power).is_err() {
                                log::warn!(
                                    "Failed to boost TX power for scanning — \
                                     continuing at current level"
                                );
                            }
                        }
                        Err(e) => log::warn!(
                            "ESP-NOW scan: TX power unreadable ({:?}) — not boosting, \
                             since it could not be restored",
                            e
                        ),
                    }
                    ScanInput::Completed
                }
                ScanAction::RestoreChannel { channel } => {
                    // Fallback: restore the radio to the last pinned channel so the peer
                    // registration is consistent with the physical channel.
                    if let Err(e) = radio.set_channel(channel) {
                        log::warn!(
                            "Failed to restore radio to last known channel {}: {:?} \
                             (next send may target an inconsistent channel)",
                            channel,
                            e
                        );
                    }
                    ScanInput::Completed
                }
                ScanAction::SetChannel { channel } => {
                    log::debug!("Probing channel {} for peer {:02X?}", channel, mac);
                    // No frame is on the air: the scan started only after the frame in flight
                    // finished, and every probe is waited out.
                    match radio.set_channel(channel) {
                        Ok(()) => ScanInput::Completed,
                        Err(e) => {
                            machine
                                .log_fault(format_args!("failed to set channel {channel}: {e:?}"));
                            ScanInput::Failed
                        }
                    }
                }
                ScanAction::SendProbe {
                    channel,
                    confirmation,
                    timeout,
                } => {
                    // Armed before the probe leaves: a reply may arrive before the worker has
                    // been told about the ACK.
                    self.arm_reply(mac);
                    armed = true;
                    match self.transmit_probe(radio, &mac, &machine.probe_frame(channel), timeout) {
                        Ok(SendOutcome::Acked) => ScanInput::Sent(SendOutcome::Acked),
                        Ok(outcome) => {
                            if confirmation == 0 {
                                log::debug!("No ACK on channel {}", channel);
                            } else {
                                log::debug!(
                                    "Channel {} unconfirmed: probe {} not ACKed \
                                     (likely roaming peer)",
                                    channel,
                                    confirmation
                                );
                            }
                            ScanInput::Sent(outcome)
                        }
                        Err(e) => {
                            machine.log_fault(format_args!(
                                "probe on channel {channel} failed: {e:?}"
                            ));
                            ScanInput::Sent(SendOutcome::Local)
                        }
                    }
                }
                ScanAction::WaitUntil(deadline) => {
                    // Interruptible: cancellation or shutdown ends the gap at once.
                    self.sleep_until(epoch, deadline);
                    ScanInput::Completed
                }
                // Interruptible like the gap; a reply in the mailbox ends it at once and the
                // machine decides whether it answers the outstanding probe.
                ScanAction::AwaitReply { until } => self.await_reply(epoch, until),
                ScanAction::RestoreTxPower => {
                    match saved_tx_power.take() {
                        Some(previous) => {
                            if radio.set_tx_power(previous).is_err() {
                                log::warn!("Failed to restore TX power after scanning");
                            }
                            super::warn_on_restore_mismatch(Some(previous), radio.tx_power().ok());
                        }
                        None => log::debug!(
                            "ESP-NOW scan: TX power before the boost is unknown, not restoring"
                        ),
                    }
                    ScanInput::Completed
                }
                ScanAction::Done(result) => {
                    return result.map_err(|kind| match (kind, last_register_error.take()) {
                        (
                            ScanError::RegisterTempPeer | ScanError::RegisterFoundPeer { .. },
                            cause,
                        ) => ScanFailure { kind, cause },
                        _ => ScanFailure { kind, cause: None },
                    });
                }
                // ADR 023 decision 5: the loop returns at `Done`, so a no-op answer means the
                // machine was polled past its end, a driver bug.
                ScanAction::Ignored => {
                    log::error!("ESP-NOW scan: the machine ignored an input; giving up");
                    return Err(ScanFailure {
                        kind: ScanError::UnexpectedInput,
                        cause: None,
                    });
                }
            };
        }
    }
}

/// Disarms the reply tap when a scan drive ends, on any path.
struct DisarmOnExit<'a, E>(&'a Shared<E>);

impl<E> Drop for DisarmOnExit<'_, E> {
    fn drop(&mut self) {
        self.0.disarm_reply();
    }
}

/// Stops the worker's callers when the thread ends, so a waiting caller fails with `Shutdown`
/// instead of hanging.
///
/// It runs on a normal return and on unwind.
struct ExitGuard<E>(Arc<Shared<E>>);

impl<E> Drop for ExitGuard<E> {
    fn drop(&mut self) {
        self.0.begin_shutdown();
    }
}

/// Cloneable handle for the tier's send and receive callbacks.
///
/// The handle owns a share of the worker's state, so a callback registered with it can never
/// outlive that state.
pub struct SendEvents<E>(Arc<Shared<E>>);

impl<E> Clone for SendEvents<E> {
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

impl<E> SendEvents<E> {
    /// Reports the MAC-layer result of the frame sent to `mac`.
    ///
    /// Runs in the Wi-Fi task: it only locks, stores and notifies.
    /// A result that matches no frame in flight is ignored.
    pub fn send_done(&self, mac: &[u8], acked: bool) {
        self.0.on_send_done(mac, acked);
    }

    /// The reply tap of a running scan: offers a received frame from `mac` to the scan.
    ///
    /// Returns `true` when the frame was taken: it is a `0xF4` [`ProbeReply`] and a scan probe
    /// to `mac` is outstanding (from its send until the scan moves past the reply wait), so the
    /// tier must not queue it for the application.
    /// Every other frame returns `false` untouched: a reply from another peer, a reply while no
    /// probe to `mac` is outstanding, or any payload that is not a reply.
    ///
    /// Runs in the Wi-Fi task, like [`send_done`](Self::send_done): it only parses, locks,
    /// stores and notifies.
    /// Call it before queueing the frame; the reply never reaches `receive`.
    pub fn probe_reply(&self, mac: &[u8], data: &[u8]) -> bool {
        self.0.on_probe_reply(mac, data)
    }
}

/// The ESP-NOW transmit worker: one thread plus thread-safe requests.
///
/// [`stop`](Self::stop) (also run on drop) ends every request and joins the thread, so no radio
/// call runs afterwards.
pub struct EspNowWorker<R: EspNowRadio> {
    shared: Arc<Shared<R::Error>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl<R: EspNowRadio> EspNowWorker<R> {
    /// Spawns the worker on `builder`, around `radio`.
    ///
    /// Feed the send callback through [`events`](Self::events).
    ///
    /// # Errors
    ///
    /// The thread could not be created.
    pub fn spawn(radio: R, config: WorkerConfig, builder: thread::Builder) -> io::Result<Self> {
        let shared = Arc::new(Shared::new(config));
        let worker = Arc::clone(&shared);
        let handle = builder.spawn(move || {
            let _exit = ExitGuard(Arc::clone(&worker));
            let mut radio = radio;
            worker.run(&mut radio);
        })?;
        Ok(Self {
            shared,
            handle: Some(handle),
        })
    }

    /// Handle for the tier's send callback.
    pub fn events(&self) -> SendEvents<R::Error> {
        SendEvents(Arc::clone(&self.shared))
    }

    /// The channel the driver keeps pinned, if any.
    pub fn pinned_channel(&self) -> Option<u8> {
        self.shared.pinned_channel()
    }

    /// Queues one frame for the worker and waits for its result.
    ///
    /// `timeout` bounds waiting for a slot, queueing and the start of the transmission; a frame
    /// that has started is waited out and its real result returned.
    /// Cancel-safe: dropping the future abandons the request, so a queued frame is never sent and
    /// a frame on the air has its result discarded.
    ///
    /// # Errors
    ///
    /// See [`WorkerError`].
    pub fn run_frame<'a>(
        &'a self,
        mac: &'a MacAddress,
        data: &'a [u8],
        timeout: Option<Duration>,
        kind: SendKind,
    ) -> impl Future<Output = Result<TxOutcome<R::Error>, WorkerError<R::Error>>> + 'a {
        self.shared.run_frame(mac, data, timeout, kind)
    }

    /// Admits a scan for `mac` and waits for the worker to finish it.
    ///
    /// Admission happens on the first poll, atomically with the scan exclusion.
    /// Dropping the future cancels the scan as described in the module documentation.
    ///
    /// # Errors
    ///
    /// [`WorkerError::Busy`] while another caller's scan runs or its result is untaken (a scan
    /// started right after a cancelled one waits for the cancelled scan's restore instead),
    /// [`WorkerError::RadioNotOwned`] for a caller-managed radio, [`WorkerError::Shutdown`], or
    /// [`WorkerError::Scan`].
    pub async fn scan_for_peer(
        &self,
        mac: &MacAddress,
        config: &ScanConfig<'_>,
    ) -> Result<ScanResult, WorkerError<R::Error>> {
        let ticket = self.shared.admit_scan(*mac, config).await?;
        self.shared.waiters.wait(|| ticket.poll()).await
    }

    /// Ends every request, tells the worker to stop and wakes every caller; idempotent, never
    /// blocks.
    ///
    /// A frame already on the air still finishes and its result is ignored; a running scan is
    /// aborted without a teardown.
    pub fn shutdown(&self) {
        self.shared.begin_shutdown();
    }

    /// [`shutdown`](Self::shutdown), then joins the worker thread; idempotent.
    ///
    /// The join does not depend on a send callback ever firing.
    pub fn stop(&mut self) {
        self.shared.begin_shutdown();
        if let Some(handle) = self.handle.take() {
            if handle.join().is_err() {
                log::error!("the espnow-tx thread panicked");
            }
        }
    }
}

impl<R: EspNowRadio> Drop for EspNowWorker<R> {
    fn drop(&mut self) {
        self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::espnow::command::{ProbeFrame, PROBE_FRAME_LEN};
    use core::pin::Pin;
    use core::task::Context;
    use std::task::Wake;

    const MAC: MacAddress = [2, 0, 0, 0, 0, 1];
    const OTHER: MacAddress = [2, 0, 0, 0, 0, 2];
    const THIRD: MacAddress = [2, 0, 0, 0, 0, 3];
    const PAYLOAD: [u8; 3] = [1, 2, 3];
    const WAIT: Duration = Duration::from_secs(5);
    const SETTLE: Duration = Duration::from_millis(120);

    const SOFTAP: WorkerConfig = WorkerConfig {
        owns_radio: true,
        interface: WifiInterface::Ap,
        repin: false,
        initial_pin: Some(1),
        burst_tx_power: 78,
    };

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    struct MockError(&'static str);

    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Call {
        Send {
            mac: MacAddress,
            len: usize,
            repin: Option<u8>,
        },
        SetChannel(u8),
        TxPower,
        SetTxPower(i8),
        AddPeer(u8),
        RemovePeer,
    }

    /// How the mock answers a send, as the Wi-Fi task would.
    #[derive(Debug, Clone, Copy)]
    enum Auto {
        /// The test delivers every callback itself.
        Off,
        /// Every frame is acknowledged at once.
        Ack,
        /// Frames are acknowledged only while the radio is on this channel.
        PeerOn(u8),
        /// Like `PeerOn(channel)`, and every acknowledged probe is answered at once with a
        /// reply naming `home`, as the echo app does (a peer whose radio sits on `home` and
        /// decodes the adjacent `channel` when the two differ).
        ReplyOn { channel: u8, home: u8 },
    }

    struct MockState {
        calls: Vec<Call>,
        channel: u8,
        tx_power: i8,
        readback_skew: i8,
        tx_power_unreadable: bool,
        fail_add_peer_on: Option<u8>,
        fail_send: bool,
        auto: Auto,
        events: Option<SendEvents<MockError>>,
        /// The payload of the latest send, so a test can answer a probe.
        last_payload: Vec<u8>,
    }

    struct MockShared {
        state: Mutex<MockState>,
        changed: Condvar,
    }

    impl MockShared {
        fn lock(&self) -> MutexGuard<'_, MockState> {
            self.state.lock().unwrap_or_else(PoisonError::into_inner)
        }

        fn record(&self, call: Call) {
            self.lock().calls.push(call);
            self.changed.notify_all();
        }
    }

    struct MockRadio(Arc<MockShared>);

    impl EspNowRadio for MockRadio {
        type Error = MockError;

        fn send(
            &mut self,
            mac: &MacAddress,
            data: &[u8],
            repin: Option<u8>,
        ) -> Result<(), MockError> {
            let (events, ack, home) = {
                let mut state = self.0.lock();
                state.calls.push(Call::Send {
                    mac: *mac,
                    len: data.len(),
                    repin,
                });
                self.0.changed.notify_all();
                if state.fail_send {
                    return Err(MockError("send"));
                }
                state.last_payload = data.to_vec();
                let (ack, home) = match state.auto {
                    Auto::Off => (None, None),
                    Auto::Ack => (Some(true), None),
                    Auto::PeerOn(channel) => (Some(state.channel == channel), None),
                    Auto::ReplyOn { channel, home } => {
                        let acked = state.channel == channel;
                        (Some(acked), acked.then_some(home))
                    }
                };
                (state.events.clone(), ack, home)
            };
            let Some(events) = events else {
                return Ok(());
            };
            if let Some(ack) = ack {
                events.send_done(mac, ack);
            }
            // The reply follows the ACK from the same (Wi-Fi) task, before the worker runs.
            if let Some(reply) = home
                .and_then(|home| ProbeFrame::parse(data).map(|p| ProbeReply::answering(&p, home)))
            {
                events.probe_reply(mac, &reply.encode());
            }
            Ok(())
        }

        fn set_channel(&mut self, channel: u8) -> Result<(), MockError> {
            self.0.lock().channel = channel;
            self.0.record(Call::SetChannel(channel));
            Ok(())
        }

        fn tx_power(&mut self) -> Result<i8, MockError> {
            let result = {
                let state = self.0.lock();
                if state.tx_power_unreadable {
                    Err(MockError("tx_power"))
                } else {
                    Ok(state.tx_power + state.readback_skew)
                }
            };
            self.0.record(Call::TxPower);
            result
        }

        fn set_tx_power(&mut self, quarter_dbm: i8) -> Result<(), MockError> {
            self.0.lock().tx_power = quarter_dbm;
            self.0.record(Call::SetTxPower(quarter_dbm));
            Ok(())
        }

        fn add_peer(&mut self, peer: &PeerConfig) -> Result<(), MockError> {
            let fail = self.0.lock().fail_add_peer_on == Some(peer.channel);
            self.0.record(Call::AddPeer(peer.channel));
            if fail {
                Err(MockError("add_peer"))
            } else {
                Ok(())
            }
        }

        fn remove_peer(&mut self, _mac: &MacAddress) -> Result<(), MockError> {
            self.0.record(Call::RemovePeer);
            Ok(())
        }
    }

    struct Flag {
        woken: Mutex<bool>,
        changed: Condvar,
    }

    impl Wake for Flag {
        fn wake(self: Arc<Self>) {
            *self.woken.lock().unwrap() = true;
            self.changed.notify_all();
        }
    }

    /// A future driven by the test: polled by hand, woken through a flag.
    struct Task<'a, T> {
        future: Pin<Box<dyn Future<Output = T> + 'a>>,
        flag: Arc<Flag>,
        waker: Waker,
    }

    impl<'a, T> Task<'a, T> {
        fn new(future: impl Future<Output = T> + 'a) -> Self {
            let flag = Arc::new(Flag {
                woken: Mutex::new(false),
                changed: Condvar::new(),
            });
            Self {
                future: Box::pin(future),
                waker: Waker::from(Arc::clone(&flag)),
                flag,
            }
        }

        /// Polls once, without waiting: the request is submitted on the first poll.
        fn poll_once(&mut self) -> Option<T> {
            *self.flag.woken.lock().unwrap() = false;
            match self
                .future
                .as_mut()
                .poll(&mut Context::from_waker(&self.waker))
            {
                Poll::Ready(value) => Some(value),
                Poll::Pending => None,
            }
        }

        /// Polls whenever woken until the future completes or `timeout` passes.
        fn wait(&mut self, timeout: Duration) -> Option<T> {
            let end = Instant::now() + timeout;
            loop {
                if let Some(value) = self.poll_once() {
                    return Some(value);
                }
                let mut woken = self.flag.woken.lock().unwrap();
                while !*woken {
                    let left = end.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        return None;
                    }
                    woken = self.flag.changed.wait_timeout(woken, left).unwrap().0;
                }
            }
        }

        fn result(&mut self) -> T {
            self.wait(WAIT).expect("the request did not complete")
        }

        fn stays_pending(&mut self) -> bool {
            self.wait(SETTLE).is_none()
        }
    }

    type FrameResult = Result<TxOutcome<MockError>, WorkerError<MockError>>;

    struct Fixture {
        worker: EspNowWorker<MockRadio>,
        mock: Arc<MockShared>,
    }

    impl Fixture {
        fn new(config: WorkerConfig) -> Self {
            let mock = Arc::new(MockShared {
                state: Mutex::new(MockState {
                    calls: Vec::new(),
                    channel: 1,
                    tx_power: 52,
                    readback_skew: 0,
                    tx_power_unreadable: false,
                    fail_add_peer_on: None,
                    fail_send: false,
                    auto: Auto::Off,
                    events: None,
                    last_payload: Vec::new(),
                }),
                changed: Condvar::new(),
            });
            let worker = EspNowWorker::spawn(
                MockRadio(Arc::clone(&mock)),
                config,
                thread::Builder::new().name("test-espnow-tx".into()),
            )
            .unwrap();
            mock.lock().events = Some(worker.events());
            Self { worker, mock }
        }

        fn edit(&self, change: impl FnOnce(&mut MockState)) {
            change(&mut self.mock.lock());
        }

        fn calls(&self) -> Vec<Call> {
            self.mock.lock().calls.clone()
        }

        fn sends(&self) -> usize {
            self.calls()
                .iter()
                .filter(|call| matches!(call, Call::Send { .. }))
                .count()
        }

        fn wait_for(&self, what: &str, predicate: impl Fn(&[Call]) -> bool) {
            let end = Instant::now() + WAIT;
            let mut state = self.mock.lock();
            while !predicate(&state.calls) {
                let left = end.saturating_duration_since(Instant::now());
                assert!(!left.is_zero(), "timed out waiting for {what}");
                state = self
                    .mock
                    .changed
                    .wait_timeout(state, left)
                    .unwrap_or_else(PoisonError::into_inner)
                    .0;
            }
        }

        fn wait_for_sends(&self, count: usize) {
            self.wait_for("sends", |calls| {
                calls
                    .iter()
                    .filter(|call| matches!(call, Call::Send { .. }))
                    .count()
                    >= count
            });
        }

        /// The Wi-Fi task reporting the result of the frame sent to `mac`.
        fn callback(&self, mac: &MacAddress, acked: bool) {
            self.worker.events().send_done(mac, acked);
        }

        fn frame(
            &self,
            mac: &'static MacAddress,
            timeout: Option<Duration>,
        ) -> Task<'_, FrameResult> {
            Task::new(self.worker.run_frame(mac, &PAYLOAD, timeout, SendKind::App))
        }

        fn scan<'a>(
            &'a self,
            config: &'a ScanConfig<'a>,
        ) -> Task<'a, Result<ScanResult, WorkerError<MockError>>> {
            Task::new(self.worker.scan_for_peer(&MAC, config))
        }

        /// Sends one frame to `mac` until the scan exclusion has ended, and returns its result.
        fn frame_after_scan(&self, mac: &'static MacAddress) -> FrameResult {
            let end = Instant::now() + WAIT;
            loop {
                let result = self.frame(mac, Some(WAIT)).result();
                if !matches!(result, Err(WorkerError::ScanInProgress)) || Instant::now() > end {
                    return result;
                }
                thread::sleep(Duration::from_millis(2));
            }
        }
    }

    fn scan_config(channels: &'static [u8]) -> ScanConfig<'static> {
        ScanConfig::for_peer()
            .with_channels(channels)
            .with_probe_confirmations(0)
    }

    /// A probe send: the three-byte `0xF3` frame the machine builds.
    fn probe(repin: Option<u8>) -> Call {
        Call::Send {
            mac: MAC,
            len: PROBE_FRAME_LEN,
            repin,
        }
    }

    fn is_scan_io(call: &Call) -> bool {
        !matches!(call, Call::Send { .. })
    }

    // -- sends and late callbacks ---------------------------------------------------------------

    #[test]
    fn an_acked_frame_completes_with_its_own_callback() {
        let fixture = Fixture::new(SOFTAP);
        let mut frame = fixture.frame(&MAC, Some(WAIT));
        assert!(frame.stays_pending());
        fixture.wait_for_sends(1);
        assert!(
            frame.stays_pending(),
            "no callback yet: the frame is on the air"
        );
        fixture.callback(&MAC, true);
        assert!(matches!(frame.result(), Ok(TxOutcome::Acked)));
        assert_eq!(
            fixture.calls(),
            [Call::Send {
                mac: MAC,
                len: PAYLOAD.len(),
                repin: None
            }]
        );
    }

    #[test]
    fn an_unacked_frame_is_no_ack_and_a_refused_frame_a_port_error() {
        let fixture = Fixture::new(SOFTAP);
        let mut frame = fixture.frame(&MAC, Some(WAIT));
        frame.poll_once();
        fixture.wait_for_sends(1);
        fixture.callback(&MAC, false);
        assert!(matches!(frame.result(), Ok(TxOutcome::NoAck)));

        fixture.edit(|state| state.fail_send = true);
        let refused = fixture.frame(&MAC, Some(WAIT)).result();
        assert!(matches!(
            refused,
            Ok(TxOutcome::SendFailed(MockError("send")))
        ));

        fixture.edit(|state| {
            state.fail_send = false;
            state.auto = Auto::Ack;
        });
        assert!(matches!(
            fixture.frame(&MAC, Some(WAIT)).result(),
            Ok(TxOutcome::Acked)
        ));
    }

    #[test]
    fn a_callback_without_a_frame_in_flight_is_ignored() {
        let fixture = Fixture::new(SOFTAP);
        fixture.callback(&MAC, true);
        let mut frame = fixture.frame(&MAC, Some(WAIT));
        frame.poll_once();
        fixture.wait_for_sends(1);
        assert!(
            frame.stays_pending(),
            "the stray callback must not ack the frame"
        );
        fixture.callback(&OTHER, true);
        assert!(
            frame.stays_pending(),
            "a callback for another peer is not this frame's"
        );
        fixture.callback(&MAC, true);
        assert!(matches!(frame.result(), Ok(TxOutcome::Acked)));
        fixture.callback(&MAC, true);

        let mut next = fixture.frame(&MAC, Some(WAIT));
        next.poll_once();
        fixture.wait_for_sends(2);
        assert!(
            next.stays_pending(),
            "a callback after completion is not the next frame's"
        );
        fixture.callback(&MAC, false);
        assert!(matches!(next.result(), Ok(TxOutcome::NoAck)));
    }

    #[test]
    fn a_callback_after_the_caller_abandoned_the_frame_is_discarded() {
        let fixture = Fixture::new(SOFTAP);
        let mut first = fixture.frame(&MAC, None);
        first.poll_once();
        fixture.wait_for_sends(1);
        drop(first);
        let mut second = fixture.frame(&OTHER, None);
        second.poll_once();
        assert!(second.stays_pending());
        assert_eq!(
            fixture.sends(),
            1,
            "an abandoned frame on the air still holds the radio"
        );
        fixture.callback(&MAC, false);
        fixture.wait_for_sends(2);
        assert!(
            second.stays_pending(),
            "the abandoned frame's callback is not the second's"
        );
        fixture.callback(&OTHER, true);
        assert!(matches!(second.result(), Ok(TxOutcome::Acked)));
    }

    #[test]
    fn a_queued_request_expires_on_time_and_the_late_callback_is_not_its() {
        let fixture = Fixture::new(SOFTAP);
        let mut first = fixture.frame(&MAC, None);
        first.poll_once();
        fixture.wait_for_sends(1);

        let started = Instant::now();
        let mut queued = fixture.frame(&OTHER, Some(Duration::from_millis(50)));
        assert!(matches!(queued.result(), Err(WorkerError::Timeout)));
        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(fixture.sends(), 1, "the expired frame was never sent");

        fixture.callback(&MAC, true);
        assert!(matches!(first.result(), Ok(TxOutcome::Acked)));
        thread::sleep(SETTLE);
        assert_eq!(
            fixture.sends(),
            1,
            "the expired frame is still not sent afterwards"
        );

        fixture.edit(|state| state.auto = Auto::Ack);
        assert!(matches!(
            fixture.frame(&THIRD, Some(WAIT)).result(),
            Ok(TxOutcome::Acked)
        ));
    }

    #[test]
    fn a_frame_on_the_air_is_waited_out_past_its_deadline() {
        let fixture = Fixture::new(SOFTAP);
        let mut frame = fixture.frame(&MAC, Some(Duration::from_millis(30)));
        frame.poll_once();
        fixture.wait_for_sends(1);
        assert!(
            frame.stays_pending(),
            "the deadline bounds the start of the frame, not its callback"
        );
        fixture.callback(&MAC, true);
        assert!(matches!(frame.result(), Ok(TxOutcome::Acked)));
    }

    #[test]
    fn a_zero_timeout_expires_unless_the_frame_starts_at_once() {
        let fixture = Fixture::new(SOFTAP);
        let mut busy = fixture.frame(&MAC, None);
        busy.poll_once();
        fixture.wait_for_sends(1);
        let mut zero = fixture.frame(&OTHER, Some(Duration::ZERO));
        assert!(matches!(zero.result(), Err(WorkerError::Timeout)));
        assert_eq!(fixture.sends(), 1);
        fixture.callback(&MAC, true);
        assert!(matches!(busy.result(), Ok(TxOutcome::Acked)));

        fixture.edit(|state| state.auto = Auto::Ack);
        let idle = fixture.frame(&THIRD, Some(Duration::ZERO)).result();
        match idle {
            Ok(TxOutcome::Acked) => assert_eq!(fixture.sends(), 2),
            Err(WorkerError::Timeout) => assert_eq!(fixture.sends(), 1),
            other => panic!("unexpected result {other:?}"),
        }
    }

    #[test]
    fn a_full_queue_makes_callers_wait_for_a_slot_until_their_deadline() {
        static MACS: [MacAddress; 5] = [
            [2, 0, 0, 0, 0, 10],
            [2, 0, 0, 0, 0, 11],
            [2, 0, 0, 0, 0, 12],
            [2, 0, 0, 0, 0, 13],
            [2, 0, 0, 0, 0, 14],
        ];
        let fixture = Fixture::new(SOFTAP);
        let mut first = fixture.frame(&MACS[0], None);
        first.poll_once();
        fixture.wait_for_sends(1);
        let mut second = fixture.frame(&MACS[1], None);
        let mut third = fixture.frame(&MACS[2], None);
        second.poll_once();
        third.poll_once();

        let mut refused = fixture.frame(&MACS[3], Some(Duration::from_millis(60)));
        assert!(
            matches!(refused.result(), Err(WorkerError::Timeout)),
            "TX_SLOTS - 1 application requests fill the queue"
        );

        let mut patient = fixture.frame(&MACS[4], None);
        assert!(patient.stays_pending());
        let mut results = Vec::new();
        for (index, expected) in [0, 1, 2, 4].into_iter().enumerate() {
            patient.poll_once();
            fixture.wait_for_sends(index + 1);
            let sent = fixture.calls();
            let Call::Send { mac, .. } = sent.last().unwrap() else {
                panic!("the last call is not a send");
            };
            assert_eq!(*mac, MACS[expected]);
            fixture.callback(mac, true);
            results.push(match index {
                0 => first.result(),
                1 => second.result(),
                2 => third.result(),
                _ => patient.result(),
            });
        }
        assert!(results
            .iter()
            .all(|result| matches!(result, Ok(TxOutcome::Acked))));
    }

    #[test]
    fn refusals_are_reported_before_anything_is_queued() {
        let fixture = Fixture::new(SOFTAP);
        let too_large = [0u8; MAX_DATA_LEN + 1];
        let mut oversize = Task::new(fixture.worker.run_frame(
            &MAC,
            &too_large,
            None,
            SendKind::App,
        ));
        assert!(matches!(
            oversize.result(),
            Err(WorkerError::PayloadTooLarge { len }) if len == MAX_DATA_LEN + 1
        ));
        let mut broadcast = Task::new(fixture.worker.run_frame(
            &super::super::BROADCAST_MAC,
            &PAYLOAD,
            None,
            SendKind::App,
        ));
        assert!(matches!(broadcast.result(), Err(WorkerError::Broadcast)));
        assert_eq!(fixture.sends(), 0);
    }

    #[test]
    fn application_frames_repin_the_pinned_channel_and_probes_do_not() {
        let fixture = Fixture::new(WorkerConfig {
            repin: true,
            initial_pin: Some(7),
            ..SOFTAP
        });
        fixture.edit(|state| state.auto = Auto::PeerOn(1));
        assert!(matches!(
            fixture.frame(&MAC, Some(WAIT)).result(),
            Ok(TxOutcome::Acked)
        ));
        let config = scan_config(&[1]);
        assert!(fixture.scan(&config).result().is_ok());
        assert_eq!(fixture.worker.pinned_channel(), Some(1));
        assert!(matches!(
            fixture.frame(&MAC, Some(WAIT)).result(),
            Ok(TxOutcome::Acked)
        ));
        let repins: Vec<_> = fixture
            .calls()
            .into_iter()
            .filter_map(|call| match call {
                Call::Send { len, repin, .. } => Some((len, repin)),
                _ => None,
            })
            .collect();
        // Without a reply tap the ACK nominates channel 1 and the candidate probe settles it.
        assert_eq!(
            repins,
            [
                (PAYLOAD.len(), Some(7)),
                (super::super::command::PROBE_FRAME_LEN, None),
                (super::super::command::PROBE_FRAME_LEN, None),
                (PAYLOAD.len(), Some(1))
            ]
        );
    }

    // -- scans ----------------------------------------------------------------------------------

    #[test]
    fn a_found_peer_is_pinned_and_everything_is_restored() {
        let fixture = Fixture::new(SOFTAP);
        fixture.edit(|state| state.auto = Auto::PeerOn(6));
        let config = scan_config(&[1, 6, 11]);
        let result = fixture.scan(&config).result();
        assert_eq!(result.unwrap().channel, 6);
        assert_eq!(fixture.worker.pinned_channel(), Some(6));
        assert_eq!(
            fixture.calls(),
            [
                Call::RemovePeer,
                Call::AddPeer(0),
                Call::TxPower,
                Call::SetTxPower(78),
                Call::SetChannel(1),
                probe(None),
                Call::SetChannel(6),
                probe(None),
                // Without a reply tap the ACK on 6 is a candidate: the sweep finishes first.
                Call::SetChannel(11),
                probe(None),
                Call::SetChannel(6),
                probe(None),
                Call::SetTxPower(52),
                Call::TxPower,
                Call::RemovePeer,
                Call::AddPeer(6),
            ]
        );
    }

    // Hand mutation check 2026-11-11 (task 3 of the probe-reply feature): with the MAC guard
    // of `Shared::on_probe_reply` dropped, `the_reply_tap_takes_only_replies_to_the_
    // outstanding_probe_of_the_scanned_peer` failed (30/1); with the tap armed only at
    // `AwaitReply` instead of at `SendProbe`, that test and
    // `a_probe_reply_ends_the_scan_on_the_peers_home_channel` failed (29/2), because the
    // reply that follows the ACK from the same task was handed to the application.
    #[test]
    fn a_probe_reply_ends_the_scan_on_the_peers_home_channel() {
        // Bug 018: the peer sits on 11, decodes and ACKs the probe sent on 10/6; its reply
        // names 11, so the scan ends there without probing on, and without the candidate return.
        let fixture = Fixture::new(SOFTAP);
        fixture.edit(|state| {
            state.auto = Auto::ReplyOn {
                channel: 6,
                home: 11,
            }
        });
        let config = scan_config(&[1, 6, 11]);
        let result = fixture.scan(&config).result();
        assert_eq!(result.unwrap().channel, 11);
        assert_eq!(fixture.worker.pinned_channel(), Some(11));
        assert_eq!(
            fixture.calls(),
            [
                Call::RemovePeer,
                Call::AddPeer(0),
                Call::TxPower,
                Call::SetTxPower(78),
                Call::SetChannel(1),
                probe(None),
                Call::SetChannel(6),
                probe(None),
                // The reply named 11: the radio moves there and the teardown follows at once.
                Call::SetChannel(11),
                Call::SetTxPower(52),
                Call::TxPower,
                Call::RemovePeer,
                Call::AddPeer(11),
            ]
        );
        // The scan is over: a late reply is the application's.
        let late = ProbeReply {
            seq: 9,
            tx_channel: 6,
            home_channel: 11,
        };
        assert!(!fixture.worker.events().probe_reply(&MAC, &late.encode()));
    }

    #[test]
    fn the_reply_tap_takes_only_replies_to_the_outstanding_probe_of_the_scanned_peer() {
        let fixture = Fixture::new(SOFTAP);
        let events = fixture.worker.events();
        let config = scan_config(&[6]).with_reply_timeout(Duration::from_secs(30));
        let idle = ProbeReply {
            seq: 0,
            tx_channel: 6,
            home_channel: 6,
        };
        assert!(
            !events.probe_reply(&MAC, &idle.encode()),
            "no scan runs: the reply is the application's"
        );

        let mut scan = fixture.scan(&config);
        scan.poll_once();
        fixture.wait_for_sends(1);
        // The probe is on the air and its ACK not yet reported: the tap is already armed.
        let probe = ProbeFrame::parse(&fixture.mock.lock().last_payload).unwrap();
        let reply = ProbeReply::answering(&probe, 6).encode();
        assert!(
            !events.probe_reply(&OTHER, &reply),
            "a reply from another peer is not taken"
        );
        assert!(
            !events.probe_reply(&MAC, &probe.encode()),
            "a probe (0xF3) from the peer is not a reply"
        );
        assert!(
            !events.probe_reply(&MAC, b"hello"),
            "an application frame from the peer is not taken"
        );
        let stale = ProbeReply {
            seq: probe.seq.wrapping_add(1),
            ..ProbeReply::answering(&probe, 6)
        };
        assert!(
            events.probe_reply(&MAC, &stale.encode()),
            "a reply from the peer is taken while its probe is outstanding"
        );
        fixture.callback(&MAC, true);
        assert!(
            scan.stays_pending(),
            "the stale reply answers another probe: the wait goes on"
        );
        assert!(
            events.probe_reply(&MAC, &reply),
            "the tap stays armed through an ignored reply"
        );
        assert_eq!(scan.result().unwrap().channel, 6);
        assert_eq!(fixture.sends(), 1, "a reply needs no confirmation probe");
        assert!(
            !events.probe_reply(&MAC, &reply),
            "after the scan the tap is disarmed"
        );
    }

    #[test]
    fn a_reply_timeout_completes_the_wait_and_the_ack_only_peer_is_found_after_the_sweep() {
        let fixture = Fixture::new(SOFTAP);
        fixture.edit(|state| state.auto = Auto::PeerOn(6));
        let reply_timeout = Duration::from_millis(200);
        let config = scan_config(&[6, 11]).with_reply_timeout(reply_timeout);
        let started = Instant::now();
        let result = fixture.scan(&config).result();
        let elapsed = started.elapsed();
        assert_eq!(result.unwrap().channel, 6);
        assert!(
            elapsed >= 2 * reply_timeout,
            "each of the two ACKed probes (sweep and candidate) waits out the reply timeout \
             before `Completed` is fed: {elapsed:?}"
        );
        assert_eq!(
            fixture.calls()[4..10],
            [
                Call::SetChannel(6),
                probe(None),
                // Timed out without a reply: 6 is the candidate and the sweep goes on.
                Call::SetChannel(11),
                probe(None),
                Call::SetChannel(6),
                probe(None),
            ]
        );
        assert_eq!(fixture.sends(), 3);
    }

    #[test]
    fn a_missing_peer_restores_the_old_channel_and_keeps_the_pin() {
        let fixture = Fixture::new(SOFTAP);
        fixture.edit(|state| state.auto = Auto::PeerOn(99));
        let config = scan_config(&[3, 4]);
        let error = fixture.scan(&config).result().unwrap_err();
        assert!(matches!(
            error,
            WorkerError::Scan {
                kind: ScanError::NotFound { probed: 2, .. },
                cause: None
            }
        ));
        assert_eq!(fixture.worker.pinned_channel(), Some(1));
        let calls = fixture.calls();
        assert_eq!(
            calls[calls.len() - 5..],
            [
                Call::SetTxPower(52),
                Call::TxPower,
                Call::RemovePeer,
                Call::SetChannel(1),
                Call::AddPeer(1),
            ]
        );
    }

    #[test]
    fn a_failed_rescan_registration_restores_the_old_pin_and_reports_the_cause() {
        let fixture = Fixture::new(SOFTAP);
        fixture.edit(|state| {
            state.auto = Auto::PeerOn(6);
            state.fail_add_peer_on = Some(6);
        });
        let config = scan_config(&[6]);
        let error = fixture.scan(&config).result().unwrap_err();
        assert!(matches!(
            error,
            WorkerError::Scan {
                kind: ScanError::RegisterFoundPeer { channel: 6 },
                cause: Some(MockError("add_peer"))
            }
        ));
        assert_eq!(fixture.worker.pinned_channel(), Some(1));
        let calls = fixture.calls();
        assert_eq!(
            calls[calls.len() - 3..],
            [Call::AddPeer(6), Call::SetChannel(1), Call::AddPeer(1)]
        );
    }

    #[test]
    fn a_temporary_peer_that_cannot_be_registered_probes_nothing() {
        let fixture = Fixture::new(SOFTAP);
        fixture.edit(|state| state.fail_add_peer_on = Some(0));
        let config = scan_config(&[6]);
        let error = fixture.scan(&config).result().unwrap_err();
        assert!(matches!(
            error,
            WorkerError::Scan {
                kind: ScanError::RegisterTempPeer,
                cause: Some(MockError("add_peer"))
            }
        ));
        assert_eq!(fixture.sends(), 0);
        assert!(!fixture.calls().contains(&Call::SetTxPower(78)));
    }

    #[test]
    fn a_tx_power_read_back_mismatch_does_not_change_the_restore() {
        let fixture = Fixture::new(SOFTAP);
        fixture.edit(|state| {
            state.auto = Auto::PeerOn(6);
            state.readback_skew = 4;
        });
        let config = scan_config(&[6]);
        assert_eq!(fixture.scan(&config).result().unwrap().channel, 6);
        let restores = fixture
            .calls()
            .into_iter()
            .filter(|call| *call == Call::SetTxPower(56))
            .count();
        assert_eq!(
            restores, 1,
            "the level read before the boost is written back once and only read back"
        );
    }

    #[test]
    fn an_unknown_tx_power_is_not_boosted_or_restored() {
        let fixture = Fixture::new(SOFTAP);
        fixture.edit(|state| {
            state.auto = Auto::PeerOn(6);
            state.tx_power_unreadable = true;
        });
        let config = scan_config(&[6]);
        assert_eq!(fixture.scan(&config).result().unwrap().channel, 6);
        let writes: Vec<_> = fixture
            .calls()
            .into_iter()
            .filter(|call| matches!(call, Call::SetTxPower(_)))
            .collect();
        assert!(
            writes.is_empty(),
            "no level to restore, so the power is not touched: {writes:?}"
        );
    }

    #[test]
    fn a_scan_preempts_queued_requests_and_waits_for_the_frame_in_flight() {
        let fixture = Fixture::new(SOFTAP);
        let mut flying = fixture.frame(&MAC, None);
        flying.poll_once();
        fixture.wait_for_sends(1);
        let mut queued = fixture.frame(&OTHER, None);
        queued.poll_once();

        let config = scan_config(&[6]);
        let mut scan = fixture.scan(&config);
        scan.poll_once();
        assert!(matches!(queued.result(), Err(WorkerError::ScanInProgress)));
        assert!(matches!(
            fixture.frame(&THIRD, Some(WAIT)).result(),
            Err(WorkerError::ScanInProgress)
        ));
        assert!(
            fixture.calls().iter().all(|call| !is_scan_io(call)),
            "the scan does not touch the radio while a frame is on the air"
        );

        fixture.edit(|state| state.auto = Auto::PeerOn(6));
        fixture.callback(&MAC, true);
        assert!(matches!(flying.result(), Ok(TxOutcome::Acked)));
        assert_eq!(scan.result().unwrap().channel, 6);
    }

    #[test]
    fn a_second_scan_is_busy_and_a_caller_managed_radio_cannot_scan() {
        let fixture = Fixture::new(SOFTAP);
        let config = scan_config(&[6]);
        fixture.edit(|state| state.auto = Auto::PeerOn(6));
        let mut running = fixture.scan(&config);
        running.poll_once();
        assert!(
            matches!(fixture.scan(&config).result(), Err(WorkerError::Busy)),
            "a running scan or its unread result holds the slot"
        );
        assert!(running.result().is_ok());

        let managed = Fixture::new(WorkerConfig {
            owns_radio: false,
            ..SOFTAP
        });
        assert!(matches!(
            managed.scan(&config).result(),
            Err(WorkerError::RadioNotOwned)
        ));
        assert!(managed.calls().is_empty());
    }

    // -- cancellation ---------------------------------------------------------------------------

    #[test]
    fn cancelling_during_a_probe_waits_it_out_and_then_restores_without_probing_on() {
        let fixture = Fixture::new(SOFTAP);
        let config = scan_config(&[1, 6]);
        let mut scan = fixture.scan(&config);
        scan.poll_once();
        fixture.wait_for_sends(1);
        drop(scan);
        thread::sleep(SETTLE);
        assert!(
            !fixture.calls().contains(&Call::SetTxPower(52)),
            "the channel and the power must not change under a probe on the air"
        );

        fixture.callback(&MAC, false);
        fixture.wait_for("the fallback registration", |calls| {
            calls.last() == Some(&Call::AddPeer(1))
        });
        let calls = fixture.calls();
        assert!(
            !calls.contains(&Call::SetChannel(6)),
            "no probe follows the cancel"
        );
        assert_eq!(fixture.sends(), 1);
        assert_eq!(
            calls[calls.len() - 5..],
            [
                Call::SetTxPower(52),
                Call::TxPower,
                Call::RemovePeer,
                Call::SetChannel(1),
                Call::AddPeer(1),
            ]
        );
        assert_eq!(fixture.worker.pinned_channel(), Some(1));

        fixture.edit(|state| state.auto = Auto::Ack);
        assert!(matches!(
            fixture.frame_after_scan(&OTHER),
            Ok(TxOutcome::Acked)
        ));
    }

    #[test]
    fn a_scan_started_right_after_a_cancel_waits_for_the_restore_and_finds_the_peer() {
        let fixture = Fixture::new(SOFTAP);
        let config = scan_config(&[6]);
        let mut first = fixture.scan(&config);
        first.poll_once();
        fixture.wait_for_sends(1);
        // The cancelled scan's probe is still on the air: its restore cannot have run.
        drop(first);
        let mut second = fixture.scan(&config);
        assert!(
            second.stays_pending(),
            "the second scan waits for the unwinding one instead of failing with Busy"
        );
        assert_eq!(fixture.sends(), 1, "no probe of the second scan yet");

        fixture.edit(|state| state.auto = Auto::PeerOn(6));
        fixture.callback(&MAC, false);
        let found = second
            .result()
            .expect("the second scan is admitted after the restore");
        assert_eq!(found.channel, 6);
        assert_eq!(fixture.worker.pinned_channel(), Some(6));
    }

    #[test]
    fn shutdown_ends_the_wait_for_an_unwinding_scan() {
        let fixture = Fixture::new(SOFTAP);
        let config = scan_config(&[6]);
        let mut first = fixture.scan(&config);
        first.poll_once();
        fixture.wait_for_sends(1);
        drop(first);
        let mut second = fixture.scan(&config);
        assert!(second.stays_pending());
        fixture.worker.shutdown();
        assert!(matches!(second.result(), Err(WorkerError::Shutdown)));
    }

    #[test]
    fn a_late_probe_callback_after_the_cancel_does_not_ack_the_next_frame() {
        let fixture = Fixture::new(SOFTAP);
        let config = scan_config(&[1]);
        let mut scan = fixture.scan(&config);
        scan.poll_once();
        fixture.wait_for_sends(1);
        drop(scan);
        fixture.callback(&MAC, true);
        fixture.wait_for("the fallback registration", |calls| {
            calls.last() == Some(&Call::AddPeer(1))
        });
        fixture.callback(&MAC, true);

        let end = Instant::now() + WAIT;
        let mut frame = loop {
            let mut frame = fixture.frame(&OTHER, Some(WAIT));
            match frame.wait(Duration::from_millis(30)) {
                Some(Err(WorkerError::ScanInProgress)) if Instant::now() < end => {}
                Some(other) => panic!("unexpected result {other:?}"),
                None => break frame,
            }
        };
        fixture.wait_for_sends(2);
        assert!(
            frame.stays_pending(),
            "the late callback is not this frame's"
        );
        fixture.callback(&OTHER, true);
        assert!(matches!(frame.result(), Ok(TxOutcome::Acked)));
    }

    #[test]
    fn cancelling_during_a_confirmation_gap_ends_the_gap_at_once() {
        let fixture = Fixture::new(SOFTAP);
        fixture.edit(|state| state.auto = Auto::PeerOn(1));
        let config = scan_config(&[1])
            .with_probe_confirmations(1)
            .with_confirmation_gap(Duration::from_secs(30));
        let mut scan = fixture.scan(&config);
        scan.poll_once();
        // The sweep probe and, after its reply wait, the candidate probe; the gap follows.
        fixture.wait_for_sends(2);
        thread::sleep(SETTLE);
        let started = Instant::now();
        drop(scan);
        assert!(matches!(
            fixture.frame_after_scan(&OTHER),
            Ok(TxOutcome::Acked)
        ));
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_eq!(
            fixture.sends(),
            3,
            "the confirmation probe never went out, only the frame"
        );
        assert!(fixture.calls().contains(&Call::SetTxPower(52)));
    }

    #[test]
    fn cancelling_before_the_worker_picks_the_job_up_touches_no_radio() {
        let fixture = Fixture::new(SOFTAP);
        let mut flying = fixture.frame(&MAC, None);
        flying.poll_once();
        fixture.wait_for_sends(1);

        let config = scan_config(&[6]);
        let mut scan = fixture.scan(&config);
        scan.poll_once();
        drop(scan);
        fixture.edit(|state| state.auto = Auto::Ack);
        fixture.callback(&MAC, true);
        assert!(matches!(flying.result(), Ok(TxOutcome::Acked)));
        assert!(matches!(
            fixture.frame(&OTHER, Some(WAIT)).result(),
            Ok(TxOutcome::Acked)
        ));
        assert!(fixture.calls().iter().all(|call| !is_scan_io(call)));
    }

    #[test]
    fn an_unread_result_blocks_a_new_scan_until_the_old_ticket_is_dropped() {
        let fixture = Fixture::new(SOFTAP);
        fixture.edit(|state| state.auto = Auto::PeerOn(6));
        let config = scan_config(&[6]);
        let mut first = fixture.scan(&config);
        first.poll_once();
        fixture.wait_for("the first scan", |calls| {
            calls.last() == Some(&Call::AddPeer(6))
        });
        assert!(matches!(
            fixture.scan(&config).result(),
            Err(WorkerError::Busy)
        ));
        drop(first);
        assert_eq!(fixture.scan(&config).result().unwrap().channel, 6);
    }

    // -- shutdown -------------------------------------------------------------------------------

    #[test]
    fn shutdown_ends_queued_and_in_flight_requests_and_refuses_new_ones() {
        let mut fixture = Fixture::new(SOFTAP);
        let mut flying = fixture.frame(&MAC, None);
        flying.poll_once();
        fixture.wait_for_sends(1);
        let mut queued = fixture.frame(&OTHER, None);
        queued.poll_once();

        fixture.worker.shutdown();
        assert!(matches!(flying.result(), Err(WorkerError::Shutdown)));
        assert!(matches!(queued.result(), Err(WorkerError::Shutdown)));
        assert!(matches!(
            fixture.frame(&THIRD, Some(WAIT)).result(),
            Err(WorkerError::Shutdown)
        ));
        let config = scan_config(&[6]);
        assert!(matches!(
            fixture.scan(&config).result(),
            Err(WorkerError::Shutdown)
        ));

        drop((flying, queued));
        let started = Instant::now();
        fixture.worker.stop();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the join does not wait for the missing callback"
        );
        fixture.callback(&MAC, true);
        assert_eq!(fixture.sends(), 1, "the queued frame was never sent");
        fixture.worker.stop();
    }

    #[test]
    fn shutdown_during_a_probe_aborts_the_scan_without_a_teardown() {
        let mut fixture = Fixture::new(SOFTAP);
        let config = scan_config(&[1, 6]);
        let mut scan = fixture.scan(&config);
        scan.poll_once();
        fixture.wait_for_sends(1);
        let before = fixture.calls();

        fixture.worker.shutdown();
        assert!(matches!(
            scan.result(),
            Err(WorkerError::Shutdown
                | WorkerError::Scan {
                    kind: ScanError::Aborted,
                    ..
                })
        ));
        drop(scan);
        fixture.worker.stop();
        assert_eq!(
            fixture.calls(),
            before,
            "no channel, power or peer call follows a shutdown"
        );
    }

    #[test]
    fn dropping_the_worker_joins_it_with_a_frame_still_on_the_air() {
        let fixture = Fixture::new(SOFTAP);
        let mut flying = fixture.frame(&MAC, None);
        flying.poll_once();
        fixture.wait_for_sends(1);
        drop(flying);
        let Fixture { worker, mock } = fixture;
        drop(worker);
        assert_eq!(mock.lock().calls.len(), 1);
    }
}
