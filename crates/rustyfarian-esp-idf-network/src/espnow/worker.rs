//! The `espnow-tx` worker: every ESP-NOW transmission of an [`EspIdfEspNow`](super::EspIdfEspNow)
//! runs on one thread that owns the send callback (ADR 020, ADR 022, tier-parity Step 4b).
//!
//! # Why a worker
//!
//! `esp_now_send` returns when the frame is queued; the MAC-layer result arrives later in a
//! global send callback that carries no sequence number.
//! The per-call callbacks this tier used before could read a late callback as the next frame's
//! ACK.
//! Now one permanent callback exists for the driver's lifetime, and at most one frame is ever
//! outstanding, so a callback belongs to the frame the worker handed out.
//! The queue, ids, deadlines, cancellation and the scan-versus-send exclusion live in the
//! host-tested [`TxCore`]; this module wraps it in a lock and runs it.
//!
//! # Threads, locks and the callback
//!
//! - `espnow-tx` (fixed stack, [`WORKER_STACK_SIZE`]) is the only thread that calls
//!   `esp_now_send`, changes the channel or TX power for a scan, and registers scan peers.
//! - Callers (async futures or blocking calls) never touch the radio.
//!   They submit under the lock, wait on the [`WaitList`] (blocking calls park their thread
//!   through a tiny `block_on`), and read their own slot's result.
//! - One `Mutex` ([`TxState`]) guards the [`TxCore`] (which also knows about shutdown), the
//!   [`InFlightMarker`] (frame in flight and the callback outcome), the [`DeadlineSet`] of the
//!   callers and the [`ScanGate`] (the scan job slot together with the scan exclusion).
//!   It is held for a few instructions and never across a radio call, a wait, or a waker call;
//!   wakers run after unlocking.
//!   It is never held together with the [`WaitList`] lock, so there is no lock order to violate.
//! - The send callback runs in the Wi-Fi task.
//!   It locks the mutex, lets the marker record the outcome for the frame in flight (matched by
//!   destination MAC) and notifies the `Condvar`; it does not log, wait, call the radio or wake
//!   wakers.
//!   A callback that arrives while no frame is in flight is ignored.
//! - Nothing that can free memory or build an error runs under the lock: scan jobs and results
//!   that the slot discards are handed back and dropped after unlocking, error values are mapped
//!   after unlocking, and a full [`DeadlineSet`] grows from storage allocated outside the lock.
//! - The worker sleeps on one `Condvar` for everything: a submission, the callback, a scan
//!   request or cancellation, a caller deadline, shutdown.
//!   While a frame is in flight it waits for the callback without a timeout (fail-closed,
//!   inventory row 21): only shutdown ends that wait.
//!   Caller deadlines still wake the worker, so a queued request expires on time.
//!
//! # Deadlines
//!
//! The library has no timer (ADR 021), so the worker is the timer: a caller with a finite
//! deadline registers it in [`TxState`], the worker never sleeps past the earliest one, wakes the
//! waiting callers at it, and each caller then asks [`TxCore::poll_at`] under the lock, which
//! decides from the deadline stored with the request whether its frame never started (expired)
//! or is on the air (waited out).
//! A deadline bounds waiting for a slot, queueing and the start of the transmission, so a timeout
//! of zero (or one shorter than the worker's reaction) fails with `Timeout` unless the worker
//! started the frame at once.
//!
//! # Blocking forms
//!
//! `EspNowDriver::send`, `send_and_wait` and `scan_for_peer_blocking` park the calling thread
//! until the worker has served them.
//! They must not run on a thread that also polls [`EspNowLink`](juggler::espnow::EspNowLink)
//! futures of the same driver: futures that were polled once and are then left unpolled on that
//! thread keep their queue slots ([`TX_SLOTS`]) reserved, and a blocking call on the same thread
//! can then wait for a slot that only that thread could free.
//! Plain `send` bounds this with [`SEND_QUEUE_DEADLINE`](super::SEND_QUEUE_DEADLINE) (it fails
//! with `Timeout`), while `send_and_wait` is bounded by its own timeout; a scan has no such bound.
//!
//! # Scan jobs
//!
//! A scan runs on the worker.
//! Admission is atomic: [`ScanGate::admit`] claims the radio ([`TxCore::begin_scan`]) and stores
//! the job in one call, so from then on application sends fail with `ScanInProgress` and queued
//! ones were completed as `Preempted`; a refused scan changes and preempts nothing.
//! The queue keeps one slot for the scan's probes, so the probe is never refused for lack of room.
//! The worker finishes the frame in flight, runs the [`ScanMachine`] with its probes as
//! [`SendKind::Probe`] frames through the same in-flight and callback path, restores channel,
//! TX power and peer registration, and only then does [`ScanGate::finish`] end the exclusion
//! while publishing the result.
//!
//! # Cancellation of a scan
//!
//! Dropping the async scan future (or its ticket) cancels the job in the [`ScanGate`] and wakes
//! the worker; it never touches the radio (`Drop` cannot await).
//! The worker feeds [`ScanInput::Abort`] at its next step, and the machine unwinds through its
//! own restore sequence, so the exclusion is held until the restore is done and the caller's
//! result is discarded.
//! The next step comes at one of these points:
//!
//! - During a probe: the frame on the air is waited out first (the channel must not change under
//!   a transmission; its callback typically arrives within milliseconds, but one that never
//!   arrives blocks the scan and every later transmission until shutdown, see "Threads, locks
//!   and the callback"), then the probe result is replaced by `Abort`.
//! - During a confirmation gap: the gap sleeps on the `Condvar`, so cancellation interrupts it
//!   at once and `Abort` follows.
//! - Inside the machine's teardown `Abort` stands for the step's expected input, so it is fed
//!   at every step and never skips a restore; a scan that had already found the peer keeps its
//!   new pin ([`ScanInput::or_abort`] keeps a `Failed` result).
//! - Before the worker picks the job up: the gate drops the job and ends the exclusion, and no
//!   radio action happens at all.
//! - After the scan finished: the unread result is discarded.
//!
//! Shutdown behaves like cancellation, and its wait for a callback that never fires ends.
//! Once shutdown is set the teardown skips the channel, TX-power and peer-table calls: a probe
//! may still be on the air, the driver is going away and ESP-NOW is deinitialised next.
//!
//! # Not covered by host tests
//!
//! This module needs ESP-IDF and cannot run on the host, so its `#[cfg(test)]` tests only
//! type-check (the workspace clippy compiles them) and cover error mapping.
//! The decisions live in `juggler` with host tests: [`TxCore`], [`ScanMachine`],
//! [`ScanGate`], [`InFlightMarker`] and [`DeadlineSet`].
//! The behaviour itself (threads, callback ordering, shutdown join, cancellation) is exercised by
//! the C3 smoke run only.
//!
//! # Fail-closed send callback
//!
//! `esp_now_send` accepts a frame and the MAC-layer result arrives later in the send callback.
//! The worker waits for that callback without a limit while the driver lives, because the
//! channel and the peer table must not change under a frame on the air and a late callback must
//! not be taken for the next frame's.
//! A callback that never fires therefore blocks that frame, every later transmission and every
//! scan until the driver is dropped (shutdown ends the wait); the docs describe the callback
//! timing as typical (a few milliseconds), never as guaranteed.

use std::ffi::CStr;
use std::future::Future;
use std::pin::pin;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::{JoinHandle, Thread};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use juggler::espnow::{
    Cancelled, Completion, DeadlineId, DeadlineSet, DeadlineStorage, GateRefusal, InFlightMarker,
    JobEpoch, MacAddress, Next, PeerConfig, Refusal, ScanAction, ScanConfig, ScanError, ScanGate,
    ScanInput, ScanMachine, ScanResult, SendKind, SendOutcome, TxCore, TxId, MAX_DATA_LEN,
};
use juggler::wait_list::WaitList;

use super::{EspIdfEspNowError, RadioMode};

/// Stack of the `espnow-tx` thread in bytes.
///
/// A starting value: the thread holds a frame copy (250 B), a scan machine, one probe call and
/// log formatting.
/// Measure the high-water mark on hardware (`espnow-tx` task) before shrinking it, and measure it
/// including a scan whose peer registration fails: that path builds an `anyhow` error and logs
/// the cause, and is the deepest on this thread.
pub(super) const WORKER_STACK_SIZE: usize = 6144;

/// Request slots of the send queue.
///
/// [`TxCore`] keeps one slot for the scan's probe, so application requests of concurrent callers
/// (and an abandoned frame still on the air, which is one of them) share the other
/// `TX_SLOTS - 1`; more callers wait for a slot until their deadline.
const TX_SLOTS: usize = 4;

/// Result of one transmission, as reported to the caller of the frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum TxOutcome {
    /// The peer acknowledged the frame at the MAC layer.
    Acked,
    /// The frame was transmitted but not acknowledged.
    NoAck,
    /// `esp_now_send` refused the frame with this `esp_err_t`; no callback follows.
    SendFailed(i32),
}

/// A scan request with an owned copy of its [`ScanConfig`], since the caller's config borrows.
struct ScanJob {
    mac: MacAddress,
    channels: Vec<u8>,
    probe_data: Vec<u8>,
    probe_timeout: Duration,
    burst_timeout: Duration,
    probe_confirmations: u8,
    confirmation_gap: Duration,
}

impl ScanJob {
    fn new(mac: MacAddress, config: &ScanConfig<'_>) -> Self {
        let ScanConfig {
            channels,
            probe_data,
            probe_timeout,
            burst_timeout,
            probe_confirmations,
            confirmation_gap,
        } = *config;
        Self {
            mac,
            channels: channels.to_vec(),
            probe_data: probe_data.to_vec(),
            probe_timeout,
            burst_timeout,
            probe_confirmations,
            confirmation_gap,
        }
    }

    /// The borrowed form the [`ScanMachine`] takes.
    fn config(&self) -> ScanConfig<'_> {
        ScanConfig {
            channels: &self.channels,
            probe_data: &self.probe_data,
            probe_timeout: self.probe_timeout,
            burst_timeout: self.burst_timeout,
            probe_confirmations: self.probe_confirmations,
            confirmation_gap: self.confirmation_gap,
        }
    }
}

/// A failed scan: the machine's verdict plus the driver error that caused it, if any.
pub(super) struct ScanFailure {
    kind: ScanError,
    cause: Option<anyhow::Error>,
}

impl ScanFailure {
    /// Reproduces the error shapes of the pre-state-machine implementation: a context message
    /// over the driver error for registration failures, the plain verdict otherwise.
    fn into_error(self) -> EspIdfEspNowError {
        match (self.kind, self.cause) {
            (ScanError::RegisterTempPeer, Some(cause)) => EspIdfEspNowError::Driver(
                cause.context("failed to register temporary peer for scanning"),
            ),
            (ScanError::RegisterFoundPeer { .. }, Some(cause)) => EspIdfEspNowError::Driver(
                cause.context("failed to re-register peer on discovered channel"),
            ),
            (ScanError::Aborted, Some(cause)) => EspIdfEspNowError::Driver(cause),
            (kind, _) => EspIdfEspNowError::Scan(kind),
        }
    }
}

/// Initial capacity of the caller [`DeadlineSet`]; it grows outside the lock.
const DEADLINE_CAPACITY: usize = 8;

/// Everything behind the one lock.
struct TxState {
    core: TxCore<TxOutcome, TX_SLOTS>,
    /// The frame handed to the radio whose send callback has not arrived, and the verdict.
    inflight: InFlightMarker,
    /// Deadlines of callers that wait; the worker wakes them when one passes.
    deadlines: DeadlineSet,
    /// The single scan job: admission, hand-off, result, cancellation and the scan exclusion of
    /// `core`, kept in step by the gate.
    scan: ScanGate<ScanJob, Result<ScanResult, ScanFailure>>,
}

/// One frame handed out by [`TxCore::next`], copied so the lock can be released to transmit.
struct Frame {
    id: TxId,
    mac: MacAddress,
    len: usize,
    data: [u8; MAX_DATA_LEN],
}

/// State shared by the driver, the `espnow-tx` thread and the send callback.
pub(super) struct TxShared {
    state: Mutex<TxState>,
    /// Wakes the worker; nobody else waits on it.
    wake: Condvar,
    /// Wakes callers waiting for a slot, a result or a scan.
    waiters: WaitList,
    epoch: Instant,
    mode: RadioMode,
    /// The channel the driver keeps pinned; [`u8::MAX`] means none.
    pinned: AtomicU8,
}

// ---------------------------------------------------------------------------------------------
// Pure decisions
// ---------------------------------------------------------------------------------------------

/// The error of a request that did not complete with a transmission.
fn map_completion(completion: Completion<TxOutcome>) -> Result<TxOutcome, EspIdfEspNowError> {
    match completion {
        Completion::Done(outcome) => Ok(outcome),
        Completion::Expired => Err(EspIdfEspNowError::Timeout),
        Completion::Preempted => Err(EspIdfEspNowError::ScanInProgress),
        Completion::Shutdown => Err(EspIdfEspNowError::Shutdown),
    }
}

/// The error of a refused submission; `Full` is only reported once the caller's deadline passed.
fn map_refusal(refusal: Refusal, len: usize) -> EspIdfEspNowError {
    match refusal {
        Refusal::Full => EspIdfEspNowError::Timeout,
        Refusal::Scanning => EspIdfEspNowError::ScanInProgress,
        Refusal::TooLarge => EspIdfEspNowError::PayloadTooLarge { len },
        Refusal::Broadcast => EspIdfEspNowError::Broadcast,
        Refusal::Shutdown => EspIdfEspNowError::Shutdown,
    }
}

/// The error of a probe that never completed: shutdown raced in, or the queue held no frame.
fn probe_lost(shutdown: bool) -> EspIdfEspNowError {
    if shutdown {
        EspIdfEspNowError::Shutdown
    } else {
        EspIdfEspNowError::Busy
    }
}

fn map_gate_refusal(refusal: GateRefusal) -> EspIdfEspNowError {
    match refusal {
        GateRefusal::Shutdown => EspIdfEspNowError::Shutdown,
        GateRefusal::Busy => EspIdfEspNowError::Busy,
        GateRefusal::NotOwned => EspIdfEspNowError::RadioNotOwned,
    }
}

/// The driver error for an `esp_now_send` that refused the frame.
fn send_failure(code: i32) -> EspIdfEspNowError {
    EspIdfEspNowError::Driver(match esp_idf_svc::sys::EspError::from(code) {
        Some(error) => anyhow::Error::new(error).context("failed to send ESP-NOW frame"),
        None => anyhow::anyhow!("esp_now_send returned {code}"),
    })
}

/// `Ok` only for a MAC-layer ACK ([`EspIdfEspNow::send_acked`](super::EspIdfEspNow::send_acked)).
pub(super) fn require_ack(outcome: TxOutcome) -> Result<(), EspIdfEspNowError> {
    match outcome {
        TxOutcome::Acked => Ok(()),
        TxOutcome::NoAck => Err(EspIdfEspNowError::NoAck),
        TxOutcome::SendFailed(code) => Err(send_failure(code)),
    }
}

/// `Ok` once the frame left the radio, acknowledged or not (plain `EspNowDriver::send`).
pub(super) fn ignore_ack(outcome: TxOutcome) -> Result<(), EspIdfEspNowError> {
    match outcome {
        TxOutcome::Acked | TxOutcome::NoAck => Ok(()),
        TxOutcome::SendFailed(code) => Err(send_failure(code)),
    }
}

// ---------------------------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------------------------

/// Removes a registered caller deadline when the call ends.
struct DeadlineEntry<'a> {
    shared: &'a TxShared,
    id: DeadlineId,
}

impl Drop for DeadlineEntry<'_> {
    fn drop(&mut self) {
        self.shared.lock().deadlines.remove(self.id);
    }
}

/// Abandons a request when its caller is dropped before the result was taken.
struct PendingSend<'a> {
    shared: &'a TxShared,
    /// Cleared once the result has been consumed: a consumed id may be reused.
    id: Option<TxId>,
}

impl Drop for PendingSend<'_> {
    fn drop(&mut self) {
        if let Some(id) = self.id.take() {
            self.shared.lock().core.abandon(id);
            self.shared.waiters.wake_all();
        }
    }
}

/// Proof of an admitted scan; dropping it before the result was taken requests cancellation.
pub(super) struct ScanTicket<'a> {
    shared: &'a TxShared,
    epoch: JobEpoch,
    consumed: AtomicBool,
}

impl ScanTicket<'_> {
    /// The scan's result once the worker has finished it (restore done, exclusion ended).
    ///
    /// Only the result of this ticket's own scan is taken; the error is built after unlocking.
    fn poll(&self) -> Option<Result<ScanResult, EspIdfEspNowError>> {
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
            return Some(Err(EspIdfEspNowError::Shutdown));
        }
        None
    }
}

impl Drop for ScanTicket<'_> {
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

/// Wakes a parked thread; the waker of [`block_on`].
struct Unparker(Thread);

impl Wake for Unparker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// Runs `future` to completion on the calling thread, parking between polls.
pub(super) fn block_on<F: Future>(future: F) -> F::Output {
    let waker = Waker::from(Arc::new(Unparker(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let mut future = pin!(future);
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut cx) {
            return value;
        }
        std::thread::park();
    }
}

impl TxShared {
    /// `initial_pin` is the channel the owned radio starts on, if the driver set one.
    pub(super) fn new(mode: RadioMode, initial_pin: Option<u8>) -> Self {
        Self {
            state: Mutex::new(TxState {
                core: TxCore::new(),
                inflight: InFlightMarker::new(),
                deadlines: DeadlineSet::with_capacity(DEADLINE_CAPACITY),
                scan: ScanGate::new(),
            }),
            wake: Condvar::new(),
            waiters: WaitList::new(),
            epoch: Instant::now(),
            mode,
            pinned: AtomicU8::new(initial_pin.unwrap_or(u8::MAX)),
        }
    }

    fn lock(&self) -> MutexGuard<'_, TxState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Time on the [`TxCore`] clock.
    fn now(&self) -> Duration {
        self.epoch.elapsed()
    }

    /// The channel the driver keeps pinned, if any.
    pub(super) fn pinned_channel(&self) -> Option<u8> {
        match self.pinned.load(Ordering::Relaxed) {
            u8::MAX => None,
            channel => Some(channel),
        }
    }

    /// The send callback: records the verdict for the frame in flight.
    ///
    /// Runs in the Wi-Fi task, so it only locks, stores and notifies (see the module docs).
    pub(super) fn on_send_done(&self, mac: &[u8], acked: bool) {
        let recorded = self.lock().inflight.on_callback(mac, acked);
        if recorded {
            self.wake.notify_all();
        }
    }

    /// Ends every request, tells the worker to stop and wakes every caller; idempotent.
    pub(super) fn begin_shutdown(&self) {
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
    fn register_deadline(&self, at: Duration) -> DeadlineEntry<'_> {
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
    fn try_submit(
        &self,
        mac: &MacAddress,
        data: &[u8],
        deadline: Option<Duration>,
        kind: SendKind,
    ) -> Option<Result<TxId, EspIdfEspNowError>> {
        let now = self.now();
        let mut state = self.lock();
        match state.core.submit(now, *mac, data, deadline, kind) {
            Ok(id) => {
                drop(state);
                self.wake.notify_all();
                Some(Ok(id))
            }
            Err(Refusal::Full) if deadline.is_none_or(|at| now < at) => None,
            Err(refusal) => Some(Err(map_refusal(refusal, data.len()))),
        }
    }

    /// The caller's view of request `id`; `None` while it is pending.
    ///
    /// The core uses the deadline stored with the request: once it passed, a queued request
    /// expires and a frame on the air is waited out.
    fn poll_completion(&self, id: TxId) -> Option<Completion<TxOutcome>> {
        let now = self.now();
        let mut state = self.lock();
        match state.core.poll_at(id, now, Waker::noop()) {
            Poll::Ready(completion) => Some(completion),
            Poll::Pending => None,
        }
    }

    /// Queues one frame for the worker and waits for its result.
    ///
    /// `timeout` bounds waiting for a slot, queueing and the start of the transmission; a frame
    /// that has started is waited out and its real result returned.
    /// Cancel-safe: dropping the future abandons the request, so a queued frame is never sent and
    /// a frame on the air has its result discarded.
    pub(super) async fn run_frame(
        &self,
        mac: &MacAddress,
        data: &[u8],
        timeout: Option<Duration>,
        kind: SendKind,
    ) -> Result<TxOutcome, EspIdfEspNowError> {
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

    /// Admits a scan: the exclusion starts here, atomically with the job slot.
    ///
    /// # Errors
    ///
    /// `Busy` while a scan runs, `RadioNotOwned` for a caller-managed radio, `Shutdown`.
    pub(super) fn admit_scan(
        &self,
        mac: MacAddress,
        config: &ScanConfig<'_>,
    ) -> Result<ScanTicket<'_>, EspIdfEspNowError> {
        let job = ScanJob::new(mac, config);
        let mut preempted = false;
        let admitted = {
            let mut guard = self.lock();
            let state = &mut *guard;
            // One call: shutdown, a job or untaken result in the slot, then the radio claim,
            // with nothing changed (and nothing preempted) on a refusal.
            state.scan.admit(
                &mut state.core,
                self.mode.owns_radio(),
                job,
                &mut |_waker| preempted = true,
            )
        };
        let epoch = match admitted {
            Ok(epoch) => epoch,
            Err((refusal, job)) => {
                drop(job);
                return Err(map_gate_refusal(refusal));
            }
        };
        self.wake.notify_all();
        if preempted {
            self.waiters.wake_all();
        }
        Ok(ScanTicket {
            shared: self,
            epoch,
            consumed: AtomicBool::new(false),
        })
    }

    /// Waits for the scan of `ticket` to finish.
    pub(super) async fn finish_scan(
        &self,
        ticket: ScanTicket<'_>,
    ) -> Result<ScanResult, EspIdfEspNowError> {
        self.waiters.wait(|| ticket.poll()).await
    }

    // -- worker side -------------------------------------------------------------------------

    /// Sleeps on the `Condvar` until a wake-up or `until`, never past a caller deadline.
    ///
    /// Callers whose deadline has passed are woken first; the guard is then re-taken, so the
    /// caller of `park` must re-check its conditions after every return.
    fn park<'a>(
        &'a self,
        mut state: MutexGuard<'a, TxState>,
        until: Option<Duration>,
    ) -> MutexGuard<'a, TxState> {
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
    fn next_frame(&self, state: &mut TxState) -> (Step, bool) {
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

    /// Transmits `frame` and waits for its callback, then reports the result to its caller.
    ///
    /// `repin` is the channel to re-pin first (unassociated STA mode, application frames).
    /// Without a callback the wait ends only on shutdown (fail-closed).
    fn service(&self, frame: &Frame, repin: Option<u8>) {
        let code = send_frame(&frame.mac, &frame.data[..frame.len], repin);
        let mut state = self.lock();
        let result = if code == esp_idf_svc::sys::ESP_OK {
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
        } else {
            Some(TxOutcome::SendFailed(code))
        };
        state.inflight.clear();
        if let Some(result) = result {
            // The caller is woken through the wait list, not through the slot's waker.
            let _ = state.core.complete(frame.id, result);
        }
        drop(state);
        self.waiters.wake_all();
    }

    /// The channel to re-pin application frames to, if this mode needs it.
    fn repin_channel(&self) -> Option<u8> {
        match self.mode {
            RadioMode::OwnedStaPromisc => self.pinned_channel(),
            RadioMode::CallerManagedSta | RadioMode::OwnedSoftAp => None,
        }
    }

    /// The thread body: serve the queue and scan jobs until shutdown.
    fn run(&self) {
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
                    self.service(&frame, self.repin_channel());
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
                        self.run_scan(epoch, &job);
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

    /// False once shutdown is set: the teardown then skips the channel and TX-power calls, as a
    /// probe may still be on the air and the driver is going away.
    fn radio_io_allowed(&self) -> bool {
        !self.lock().core.is_shut_down()
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

    /// Runs one scan job and ends the exclusion once everything is restored.
    fn run_scan(&self, epoch: JobEpoch, job: &ScanJob) {
        if self.abort_requested(epoch) {
            // Cancelled or shut down before any radio action: nothing to restore.
            self.finish_scan_job(epoch, None);
            return;
        }
        let mut machine = ScanMachine::new(job.mac, job.config(), self.pinned_channel());
        let result = self.drive_scan(epoch, &mut machine);
        // The pin follows the machine whatever became of the caller: on a success the peer is
        // registered on the new channel, on any failure the machine restored the old one.
        if let Some(channel) = machine.pinned_channel() {
            self.pinned.store(channel, Ordering::Relaxed);
        }
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
    fn finish_scan_job(&self, epoch: JobEpoch, result: Option<Result<ScanResult, ScanFailure>>) {
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
    /// The probe is waited out however long the callback takes (fail-closed): the channel must
    /// not change under a frame on the air.
    /// The machine's probe timeout therefore bounds nothing on this tier.
    fn transmit_probe(
        &self,
        mac: &MacAddress,
        data: &[u8],
    ) -> Result<SendOutcome, EspIdfEspNowError> {
        // `TxCore` keeps a slot for the probe, so a queue that application callers filled (and
        // whose preempted requests are not yet collected) cannot refuse it with `Full`.
        let id = self
            .lock()
            .core
            .submit(self.now(), *mac, data, None, SendKind::Probe)
            .map_err(|refusal| map_refusal(refusal, data.len()))?;
        // The queue holds nothing else (the worker drained it before the scan began, and
        // application sends are refused), so the first frame is ours; any other is serviced too.
        loop {
            let (step, expired) = {
                let mut state = self.lock();
                self.next_frame(&mut state)
            };
            if expired {
                self.waiters.wake_all();
            }
            let Step::Send(frame) = step else {
                let shutdown = {
                    let mut state = self.lock();
                    state.core.abandon(id);
                    state.core.is_shut_down()
                };
                return Err(probe_lost(shutdown));
            };
            self.service(&frame, None);
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
            Some(Ok(TxOutcome::SendFailed(code))) => Err(send_failure(code)),
            Some(Err(error)) => Err(error),
            None => Err(probe_lost(shutdown)),
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
    /// On dual-radio (Wi-Fi + ESP-NOW on the same chip) deployments, schedule scans during quiet
    /// periods if predictable per-frame power matters.
    fn drive_scan(
        &self,
        epoch: JobEpoch,
        machine: &mut ScanMachine<'_>,
    ) -> Result<ScanResult, ScanFailure> {
        let mac = *machine.mac();
        let probe_data = machine.probe_data();
        let temp_peer = PeerConfig {
            mac,
            channel: 0,
            encrypt: false,
            interface: self.mode.wifi_interface(),
        };

        let mut saved_tx_power: i8 = 0;
        let mut have_saved_power = false;
        let mut power_boosted = false;
        let mut last_register_error: Option<anyhow::Error> = None;

        let mut input = ScanInput::Start;
        loop {
            // On cancellation or shutdown `Abort` replaces every result but a failure; inside
            // the machine's teardown it continues the restore, so it may be fed at every step.
            if input != ScanInput::Start {
                input = input.or_abort(self.abort_requested(epoch));
            }
            let action = machine.poll(self.now(), input);
            input = match action {
                // Shutdown: the driver is going away and ESP-NOW is deinitialised next, so the
                // teardown leaves channel, TX power and peer registration as they are.
                ScanAction::RemovePeer
                | ScanAction::RegisterPeer { .. }
                | ScanAction::RegisterFallbackPeer { .. }
                    if !self.radio_io_allowed() =>
                {
                    ScanInput::Completed
                }
                ScanAction::RemovePeer => {
                    // Best effort: removes a stale registration, or the scan peer.
                    let _ = super::peer_del(&mac);
                    ScanInput::Completed
                }
                ScanAction::RegisterPeer { channel } => {
                    let peer = PeerConfig {
                        channel,
                        ..temp_peer
                    };
                    match super::peer_add(&peer) {
                        Ok(()) => ScanInput::Completed,
                        Err(e) => {
                            last_register_error = Some(e.into_anyhow());
                            ScanInput::Failed
                        }
                    }
                }
                ScanAction::RegisterFallbackPeer { channel } => {
                    let peer = PeerConfig {
                        channel,
                        ..temp_peer
                    };
                    match super::peer_add(&peer) {
                        Ok(()) => log::debug!(
                            "Peer {:02X?} fallback-registered on last known channel {}",
                            mac,
                            channel
                        ),
                        Err(e) => log::warn!(
                            "Failed to fallback-register peer {:02X?} on channel {}: {:#} \
                             (next send will fail until the next successful scan)",
                            mac,
                            channel,
                            e
                        ),
                    }
                    ScanInput::Completed
                }
                ScanAction::BoostTxPower if !self.radio_io_allowed() => ScanInput::Completed,
                ScanAction::BoostTxPower => {
                    // Auto-burst: boost TX power to maximum during channel scanning to maximise
                    // discovery range, then restore the previous level.
                    // SAFETY: esp_wifi_get_max_tx_power is an FFI call into the ESP-IDF Wi-Fi
                    // subsystem, which is initialised by init_with_radio().  It writes the
                    // current TX power to the &mut i8 we pass in.
                    have_saved_power =
                        unsafe { esp_idf_svc::sys::esp_wifi_get_max_tx_power(&mut saved_tx_power) }
                            == esp_idf_svc::sys::ESP_OK;

                    let burst_power = juggler::wifi::TxPowerLevel::Max.to_quarter_dbm();
                    // SAFETY: esp_wifi_set_max_tx_power is an FFI call into the ESP-IDF Wi-Fi
                    // subsystem, which is initialised by init_with_radio().  burst_power comes
                    // from TxPowerLevel::Max which is within the valid [8, 84] quarter-dBm
                    // range required by the API.
                    if unsafe { esp_idf_svc::sys::esp_wifi_set_max_tx_power(burst_power) }
                        == esp_idf_svc::sys::ESP_OK
                    {
                        power_boosted = true;
                    } else {
                        log::warn!(
                            "Failed to boost TX power for scanning — continuing at current level"
                        );
                    }
                    ScanInput::Completed
                }
                ScanAction::RestoreChannel { .. } | ScanAction::SetChannel { .. }
                    if !self.radio_io_allowed() =>
                {
                    // Shutdown: a probe may still be on the air, and the driver is going away.
                    ScanInput::Completed
                }
                ScanAction::RestoreChannel { channel } => {
                    // Fallback: restore the radio to the last pinned channel so the peer
                    // registration is consistent with the physical channel.
                    // SAFETY: scans are admitted only on a driver-owned radio, so the radio is
                    // owned and running. channel is a valid 2.4 GHz channel written by a
                    // previous successful scan.
                    let set_ch_ret = unsafe {
                        esp_idf_svc::sys::esp_wifi_set_channel(
                            channel,
                            esp_idf_svc::sys::wifi_second_chan_t_WIFI_SECOND_CHAN_NONE,
                        )
                    };
                    if set_ch_ret != esp_idf_svc::sys::ESP_OK {
                        log::warn!(
                            "Failed to restore radio to last known channel {}: {:#x} \
                             (next send may target an inconsistent channel)",
                            channel,
                            set_ch_ret
                        );
                    }
                    ScanInput::Completed
                }
                ScanAction::SetChannel { channel } => {
                    log::debug!("Probing channel {} for peer {:02X?}", channel, mac);
                    // SAFETY: esp_wifi_set_channel is an FFI call into the ESP-IDF Wi-Fi
                    // subsystem, which is initialised by init_with_radio().  In SoftAP mode
                    // the channel change is a CSA with no connected stations, so it takes
                    // effect immediately.  No frame is on the air: the scan started only after
                    // the frame in flight finished, and every probe is waited out.
                    let ret = unsafe {
                        esp_idf_svc::sys::esp_wifi_set_channel(
                            channel,
                            esp_idf_svc::sys::wifi_second_chan_t_WIFI_SECOND_CHAN_NONE,
                        )
                    };
                    if ret == esp_idf_svc::sys::ESP_OK {
                        ScanInput::Completed
                    } else {
                        machine.log_fault(format_args!(
                            "failed to set channel {channel}: error code {ret}"
                        ));
                        ScanInput::Failed
                    }
                }
                ScanAction::SendProbe {
                    channel,
                    confirmation,
                    timeout: _,
                } => match self.transmit_probe(&mac, probe_data) {
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
                        machine.log_fault(format_args!("probe on channel {channel} failed: {e}"));
                        ScanInput::Sent(SendOutcome::Local)
                    }
                },
                ScanAction::WaitUntil(deadline) => {
                    // Interruptible: cancellation or shutdown ends the gap at once.
                    self.sleep_until(epoch, deadline);
                    ScanInput::Completed
                }
                ScanAction::RestoreTxPower if !self.radio_io_allowed() => ScanInput::Completed,
                ScanAction::RestoreTxPower => {
                    // Restore TX power after scanning regardless of outcome.
                    // SAFETY: esp_wifi_set_max_tx_power is an FFI call into the ESP-IDF Wi-Fi
                    // subsystem, which is initialised by init_with_radio().  saved_tx_power was
                    // previously read from esp_wifi_get_max_tx_power (and only used here when
                    // have_saved_power is true), so the value is in the range the API produced.
                    if have_saved_power
                        && unsafe { esp_idf_svc::sys::esp_wifi_set_max_tx_power(saved_tx_power) }
                            != esp_idf_svc::sys::ESP_OK
                    {
                        log::warn!("Failed to restore TX power after scanning");
                    }
                    power_boosted = false;
                    ScanInput::Completed
                }
                ScanAction::Done(result) => {
                    if power_boosted && have_saved_power && self.radio_io_allowed() {
                        // SAFETY: as in `RestoreTxPower`; covers a machine that finished
                        // without reaching it.
                        let _ =
                            unsafe { esp_idf_svc::sys::esp_wifi_set_max_tx_power(saved_tx_power) };
                    }
                    return result.map_err(|kind| match (kind, last_register_error.take()) {
                        (
                            ScanError::RegisterTempPeer | ScanError::RegisterFoundPeer { .. },
                            cause,
                        ) => ScanFailure { kind, cause },
                        _ => ScanFailure { kind, cause: None },
                    });
                }
            };
        }
    }
}

/// What the worker does after asking the queue.
// A short-lived stack value: boxing the frame would allocate once per transmission for nothing.
#[allow(clippy::large_enum_variant)]
enum Step {
    Send(Frame),
    /// Nothing to send; wake at this time (or on a wake-up) to expire queued requests.
    Wait(Option<Duration>),
}

/// Hands one frame to the radio and returns the `esp_err_t`.
///
/// With `repin`, the radio is first re-pinned to that channel inside a promiscuous bracket
/// (unassociated STA mode, ADR 012).
///
/// `set_promiscuous(false)` and `esp_now_send` must be consecutive instructions to minimise the
/// scheduler window in which the Wi-Fi background scan task (priority 23) can preempt a
/// lower-priority thread and drift the channel.
/// Even with the tightest possible bracket the race is not fully eliminated: see ADR 012 for the
/// ~0-20 % `ESP_ERR_ESPNOW_CHAN` rate this path exhibits in production.
fn send_frame(mac: &MacAddress, data: &[u8], repin: Option<u8>) -> i32 {
    let Some(channel) = repin else {
        // SAFETY: plain FFI call; `mac` points to 6 bytes and `data` to `data.len()` bytes, both
        // valid for the duration of the call (ESP-NOW copies the payload before returning).
        return unsafe { esp_idf_svc::sys::esp_now_send(mac.as_ptr(), data.as_ptr(), data.len()) };
    };
    // SAFETY: repin is only set for RadioMode::OwnedStaPromisc, so the radio is owned and
    // running. `channel` was written by a successful scan, so it is a valid 2.4 GHz channel.
    // `mac` and `data` outlive the call.
    //
    // The tight bracket captures every intermediate `esp_err_t` so that a channel-race or
    // country-code failure surfaces as a specific log line rather than an opaque "peer did not
    // ACK" downstream.
    let (promisc_on_ret, set_ch_ret, promisc_off_ret, send_ret) = unsafe {
        let on = esp_idf_svc::sys::esp_wifi_set_promiscuous(true);
        let set = esp_idf_svc::sys::esp_wifi_set_channel(
            channel,
            esp_idf_svc::sys::wifi_second_chan_t_WIFI_SECOND_CHAN_NONE,
        );
        let off = esp_idf_svc::sys::esp_wifi_set_promiscuous(false);
        let send = esp_idf_svc::sys::esp_now_send(mac.as_ptr(), data.as_ptr(), data.len());
        (on, set, off, send)
    };

    if promisc_on_ret != esp_idf_svc::sys::ESP_OK {
        log::warn!(
            "esp_wifi_set_promiscuous(true) failed: {:#x} \
             (channel re-pin skipped, send proceeded on previous channel)",
            promisc_on_ret
        );
    }
    if set_ch_ret != esp_idf_svc::sys::ESP_OK {
        log::warn!(
            "esp_wifi_set_channel({}) failed: {:#x} \
             (frame sent on whatever channel the radio was on)",
            channel,
            set_ch_ret
        );
    }
    if promisc_off_ret != esp_idf_svc::sys::ESP_OK {
        log::warn!(
            "esp_wifi_set_promiscuous(false) failed: {:#x} \
             (radio left in promiscuous mode)",
            promisc_off_ret
        );
    }
    send_ret
}

/// Stops the worker's callers when the thread ends, so a waiting caller fails with `Shutdown`
/// instead of hanging.
///
/// It runs on a normal return and on unwind; the workspace builds with `panic = "abort"` on
/// ESP-IDF, where a panic aborts the chip instead, so the unwind case matters for host or unwind
/// builds only.
struct ExitGuard(Arc<TxShared>);

impl Drop for ExitGuard {
    fn drop(&mut self) {
        self.0.begin_shutdown();
    }
}

/// Labels the pthreads spawned from the calling thread while it lives, because
/// `std::thread::Builder::name` does not reach FreeRTOS on ESP-IDF (`docs/project-lore.md`,
/// "ESP-IDF FreeRTOS Tasks & Threads").
///
/// Rust passes its own stack size to `pthread_create`, which overrides the configured one, so
/// this only sets the FreeRTOS task name; it must stay at most 15 characters.
/// Dropping the guard restores the pthread configuration the caller had, exactly; a caller that
/// had none gets a default configuration back (ESP-IDF cannot clear one), which creates threads
/// like no configuration does.
struct ThreadNaming {
    /// The caller's configuration, `None` when it had none.
    saved: Option<esp_idf_svc::sys::esp_pthread_cfg_t>,
    /// False when the naming could not be set up: nothing to restore.
    active: bool,
}

impl ThreadNaming {
    fn set(name: &'static CStr) -> Self {
        use esp_idf_svc::sys::{
            esp_pthread_cfg_t, esp_pthread_get_cfg, esp_pthread_get_default_config,
            esp_pthread_set_cfg, ESP_ERR_NOT_FOUND, ESP_OK,
        };
        // SAFETY: plain FFI calls with pointers to live locals; `name` is a 'static C string, as
        // ESP-IDF keeps the pointer.
        unsafe {
            let mut current = esp_pthread_get_default_config();
            let got = esp_pthread_get_cfg(&mut current);
            let saved = match got {
                ESP_OK => Some(current),
                ESP_ERR_NOT_FOUND => None,
                code => {
                    log::warn!(
                        "esp_pthread_get_cfg failed: {code:#x}; the espnow-tx thread keeps its \
                         default FreeRTOS name"
                    );
                    return Self {
                        saved: None,
                        active: false,
                    };
                }
            };
            let mut cfg: esp_pthread_cfg_t = saved.unwrap_or(current);
            cfg.thread_name = name.as_ptr();
            cfg.inherit_cfg = false;
            let set = esp_pthread_set_cfg(&cfg);
            if set != ESP_OK {
                log::warn!(
                    "esp_pthread_set_cfg failed: {set:#x}; the espnow-tx thread keeps its \
                     default FreeRTOS name"
                );
            }
            Self {
                saved,
                active: set == ESP_OK,
            }
        }
    }
}

impl Drop for ThreadNaming {
    fn drop(&mut self) {
        use esp_idf_svc::sys::{esp_pthread_get_default_config, esp_pthread_set_cfg, ESP_OK};
        if !self.active {
            return;
        }
        // SAFETY: plain FFI calls with pointers to live locals; the restored configuration is
        // the caller's own, whose `thread_name` pointer it set itself.
        let restored = unsafe {
            match &self.saved {
                Some(cfg) => esp_pthread_set_cfg(cfg),
                None => esp_pthread_set_cfg(&esp_pthread_get_default_config()),
            }
        };
        if restored != ESP_OK {
            log::warn!("failed to restore the caller's pthread configuration: {restored:#x}");
        }
    }
}

/// Spawns the `espnow-tx` thread on `shared`.
///
/// # Errors
///
/// Fails if the thread cannot be created (out of memory).
pub(super) fn spawn(shared: Arc<TxShared>) -> anyhow::Result<JoinHandle<()>> {
    let spawned = {
        let _naming = ThreadNaming::set(c"espnow-tx");
        std::thread::Builder::new()
            .name("espnow-tx".into())
            .stack_size(WORKER_STACK_SIZE)
            .spawn(move || {
                let _exit = ExitGuard(Arc::clone(&shared));
                shared.run();
            })
    };
    spawned.context("failed to spawn the espnow-tx thread")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completions_map_to_the_contract_errors() {
        assert!(matches!(
            map_completion(Completion::Done(TxOutcome::Acked)),
            Ok(TxOutcome::Acked)
        ));
        assert!(matches!(
            map_completion(Completion::Expired),
            Err(EspIdfEspNowError::Timeout)
        ));
        assert!(matches!(
            map_completion(Completion::Preempted),
            Err(EspIdfEspNowError::ScanInProgress)
        ));
        assert!(matches!(
            map_completion(Completion::Shutdown),
            Err(EspIdfEspNowError::Shutdown)
        ));
    }

    #[test]
    fn refusals_map_to_the_contract_errors() {
        assert!(matches!(
            map_refusal(Refusal::Full, 3),
            EspIdfEspNowError::Timeout
        ));
        assert!(matches!(
            map_refusal(Refusal::Scanning, 3),
            EspIdfEspNowError::ScanInProgress
        ));
        assert!(matches!(
            map_refusal(Refusal::TooLarge, 300),
            EspIdfEspNowError::PayloadTooLarge { len: 300 }
        ));
        assert!(matches!(
            map_refusal(Refusal::Broadcast, 3),
            EspIdfEspNowError::Broadcast
        ));
        assert!(matches!(
            map_refusal(Refusal::Shutdown, 3),
            EspIdfEspNowError::Shutdown
        ));
        assert!(matches!(
            map_gate_refusal(GateRefusal::Shutdown),
            EspIdfEspNowError::Shutdown
        ));
        assert!(matches!(
            map_gate_refusal(GateRefusal::Busy),
            EspIdfEspNowError::Busy
        ));
        assert!(matches!(
            map_gate_refusal(GateRefusal::NotOwned),
            EspIdfEspNowError::RadioNotOwned
        ));
    }

    #[test]
    fn only_an_unacked_transmission_is_no_ack() {
        assert!(require_ack(TxOutcome::Acked).is_ok());
        assert!(matches!(
            require_ack(TxOutcome::NoAck),
            Err(EspIdfEspNowError::NoAck)
        ));
        assert!(ignore_ack(TxOutcome::NoAck).is_ok());
    }

    #[test]
    fn a_lost_probe_is_shutdown_when_shutdown_raced_in() {
        assert!(matches!(probe_lost(true), EspIdfEspNowError::Shutdown));
        assert!(matches!(probe_lost(false), EspIdfEspNowError::Busy));
    }
}
