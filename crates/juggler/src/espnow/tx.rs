//! Sans-IO ESP-NOW send queue shared by both tiers.
//!
//! [`TxCore`] is the pure core behind every async send and scan probe: it owns the request
//! slots, request ids, deadlines, cancellation and the scan-versus-send exclusion, and nothing
//! else.
//! It has no I/O, no clock, no lock and no allocation; the caller passes time as a
//! [`Duration`] since an arbitrary epoch and wraps the core in its own lock (a `Mutex` with a
//! `Condvar` on ESP-IDF, an embassy blocking mutex on esp-hal).
//!
//! # Roles
//!
//! - Callers: [`TxCore::submit`] a frame, then [`TxCore::poll_result`] until it is ready.
//! - One runner (a worker thread or a task): [`TxCore::next`] hands out the next frame, the
//!   runner transmits it and reports the result with [`TxCore::complete`].
//! - A scan owner brackets its work with [`TxCore::begin_scan`] and [`TxCore::end_scan`].
//! - The tier calls [`TxCore::shutdown`] on drop, so its runner can stop without a send callback.
//!
//! # Rules
//!
//! - FIFO: frames are transmitted in submission order, one in flight at a time.
//! - A deadline bounds queueing and the start of a transmission.
//!   A queued request whose deadline has passed is completed as [`Completion::Expired`] by
//!   [`TxCore::next`] and is never transmitted.
//!   A frame that has started is waited out and reports its real result, even past its deadline.
//! - [`TxCore::next`] checks the deadline and marks the frame in flight in one step, so a caller
//!   that reads [`TxCore::is_in_flight`] under the same lock sees either "started" (wait for the
//!   result) or "expired or still queued" (never transmitted); there is no window in between.
//! - Results are per slot and keyed by [`TxId`]: a caller never takes another request's result,
//!   and a result that arrives for an abandoned or unknown id is dropped.
//! - While a scan runs, [`SendKind::App`] submissions are refused at once with
//!   [`Refusal::Scanning`]; [`SendKind::Probe`] submissions (the scan's own frames) are allowed.
//!   Application requests queued before the scan began are completed as
//!   [`Completion::Preempted`] by [`TxCore::begin_scan`]; a frame in flight finishes first
//!   ([`TxCore::can_start_scan_io`]).
//!
//! # Cancellation and deadlines
//!
//! A caller that is dropped while waiting calls [`TxCore::abandon`]: a queued request is
//! removed and never transmitted, an in-flight one is marked so that its result is discarded
//! when [`TxCore::complete`] arrives (the slot stays reserved until then, so the id cannot be
//! reused and no other request can receive that result).
//! The started frame still runs to its end.
//!
//! When the caller's own deadline passes, it must not abandon blindly: call
//! [`TxCore::poll_expired`] under the lock instead.
//! It reports a finished request, expires a still-queued one (so it is never transmitted) and
//! returns `Pending` for a frame in flight, which the caller then keeps waiting for with
//! [`TxCore::poll_result`].
//! [`TxCore::is_in_flight`] answers the same question without consuming anything.
//!
//! # Wakers
//!
//! Each slot holds the waker registered by [`TxCore::poll_result`].
//! [`TxCore::complete`] returns it and [`TxCore::next`] passes the wakers of the requests it
//! expires to a caller-supplied sink; neither calls `wake`, so the tier wakes outside its lock
//! (the sink typically stores into a small fixed array that is drained after unlocking).
//!
//! # Slots
//!
//! A slot is reserved from [`TxCore::submit`] until its result is taken by `poll_result` or
//! `poll_expired`, or until `abandon`.
//! A caller that neither waits nor abandons keeps its slot, so it must always do one of them.

use core::task::{Poll, Waker};
use core::time::Duration;

use super::{MacAddress, BROADCAST_MAC, MAX_DATA_LEN};

/// Opaque handle of one submitted request.
///
/// Ids wrap around; [`TxCore::submit`] never hands out an id that a live slot still holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TxId(u32);

/// Who submits a frame: the application, or a peer scan's own probe.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendKind {
    /// An application frame; refused while a scan runs.
    App,
    /// A scan probe; allowed while a scan runs.
    Probe,
}

/// Why [`TxCore::submit`] refused a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Every slot is reserved.
    Full,
    /// A scan is running and the request is an application frame.
    Scanning,
    /// The payload exceeds [`MAX_DATA_LEN`].
    TooLarge,
    /// An application frame addressed to [`BROADCAST_MAC`]: `send_acked` promises a unicast MAC
    /// ACK, which broadcast never gives.
    /// Use `EspNowDriver::send` for broadcast.
    Broadcast,
    /// [`TxCore::shutdown`] was called.
    Shutdown,
}

/// Why [`TxCore::begin_scan`] refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanRefusal {
    /// Another scan is running.
    Busy,
    /// The driver does not own the radio.
    NotOwned,
}

/// Final state of a request, as seen by its caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completion<R> {
    /// The runner transmitted the frame and reported this result.
    Done(R),
    /// The deadline passed before the transmission started; nothing was sent.
    ///
    /// Also reported for an id that is no longer live (already consumed or abandoned), never
    /// another request's result.
    Expired,
    /// An application request was still queued when a scan began; nothing was sent.
    /// Tiers classify it as a local failure.
    Preempted,
    /// [`TxCore::shutdown`] ended the request, in flight or not; its late result is ignored.
    Shutdown,
}

/// What [`TxCore::abandon`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Abandon {
    /// The request was queued, finished or unknown: its slot is free, nothing will be sent.
    Dropped,
    /// The frame is on the air; its result will be discarded when it arrives.
    InFlight,
}

/// What the runner does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next<'a> {
    /// Transmit this frame, then report the result with [`TxCore::complete`].
    /// The frame is already marked in flight.
    Transmit {
        /// Id to pass to [`TxCore::complete`].
        id: TxId,
        /// Destination.
        mac: MacAddress,
        /// Payload, at most [`MAX_DATA_LEN`] bytes.
        data: &'a [u8],
    },
    /// A frame is in flight and queued requests carry deadlines: call [`TxCore::next`] again at
    /// this time (or earlier, on a new submission) to expire them.
    WaitUntil(Duration),
    /// Nothing to do until a submission or a completion.
    Idle,
}

/// True once `now` has reached `deadline`; a request without a deadline never expires.
fn expired(deadline: Option<Duration>, now: Duration) -> bool {
    deadline.is_some_and(|at| now >= at)
}

/// Stores `waker` unless the slot already holds one that wakes the same task.
fn register(slot: &mut Option<Waker>, waker: &Waker) {
    match slot {
        Some(existing) if existing.will_wake(waker) => {}
        _ => *slot = Some(waker.clone()),
    }
}

enum State<R> {
    Free,
    Queued,
    InFlight,
    /// In flight, but its caller is gone: the result is discarded.
    AbandonedInFlight,
    Done(R),
    Expired,
    Preempted,
    Shutdown,
}

struct Slot<R> {
    state: State<R>,
    id: u32,
    seq: u64,
    kind: SendKind,
    mac: MacAddress,
    len: usize,
    data: [u8; MAX_DATA_LEN],
    deadline: Option<Duration>,
    waker: Option<Waker>,
}

impl<R> Slot<R> {
    fn free() -> Self {
        Self {
            state: State::Free,
            id: 0,
            seq: 0,
            kind: SendKind::App,
            mac: [0; 6],
            len: 0,
            data: [0; MAX_DATA_LEN],
            deadline: None,
            waker: None,
        }
    }

    fn release(&mut self) {
        self.state = State::Free;
        self.waker = None;
    }
}

/// Send queue with `N` slots; see the [module documentation](self).
pub struct TxCore<R, const N: usize> {
    slots: [Slot<R>; N],
    next_id: u32,
    next_seq: u64,
    scanning: bool,
    shut_down: bool,
}

impl<R, const N: usize> Default for TxCore<R, N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<R, const N: usize> TxCore<R, N> {
    /// An empty queue, no scan running.
    #[must_use]
    pub fn new() -> Self {
        Self {
            slots: core::array::from_fn(|_| Slot::free()),
            next_id: 0,
            next_seq: 0,
            scanning: false,
            shut_down: false,
        }
    }

    /// Starts the id counter at `id`, to exercise wrap-around.
    #[cfg(test)]
    fn with_next_id(mut self, id: u32) -> Self {
        self.next_id = id;
        self
    }

    // -- caller side -------------------------------------------------------------------------

    /// Queues a frame.
    ///
    /// `deadline` is an absolute time on the caller's clock; `None` waits for ever.
    /// Checked in this order: shutdown, payload size, broadcast, scan gate, free slot.
    /// Broadcast is refused for [`SendKind::App`] only: `send_acked` promises a unicast MAC ACK,
    /// so use `EspNowDriver::send` for broadcast.
    ///
    /// # Errors
    ///
    /// See [`Refusal`].
    pub fn submit(
        &mut self,
        now: Duration,
        mac: MacAddress,
        data: &[u8],
        deadline: Option<Duration>,
        kind: SendKind,
    ) -> Result<TxId, Refusal> {
        let _ = now;
        if self.shut_down {
            return Err(Refusal::Shutdown);
        }
        if data.len() > MAX_DATA_LEN {
            return Err(Refusal::TooLarge);
        }
        if kind == SendKind::App && mac == BROADCAST_MAC {
            return Err(Refusal::Broadcast);
        }
        if self.scanning && kind == SendKind::App {
            return Err(Refusal::Scanning);
        }
        let index = self
            .slots
            .iter()
            .position(|slot| matches!(slot.state, State::Free))
            .ok_or(Refusal::Full)?;
        // Terminates: fewer than `N` slots are live, and `N` is far below `u32::MAX`.
        let id = loop {
            let candidate = self.next_id;
            self.next_id = self.next_id.wrapping_add(1);
            if !self.is_live(candidate) {
                break candidate;
            }
        };
        let seq = self.next_seq;
        self.next_seq += 1;
        let slot = &mut self.slots[index];
        slot.state = State::Queued;
        slot.id = id;
        slot.seq = seq;
        slot.kind = kind;
        slot.mac = mac;
        slot.len = data.len();
        slot.data[..data.len()].copy_from_slice(data);
        slot.deadline = deadline;
        slot.waker = None;
        Ok(TxId(id))
    }

    /// True when a slot, in any non-free state, holds `id`.
    fn is_live(&self, id: u32) -> bool {
        self.slots
            .iter()
            .any(|slot| !matches!(slot.state, State::Free) && slot.id == id)
    }

    /// The slot a caller may still wait on: live and not abandoned.
    fn caller_slot(&mut self, id: TxId) -> Option<&mut Slot<R>> {
        self.slots.iter_mut().find(|slot| {
            slot.id == id.0 && !matches!(slot.state, State::Free | State::AbandonedInFlight)
        })
    }

    /// Polls the result of request `id` and registers `waker` for it.
    ///
    /// `Ready` consumes the result and frees the slot.
    pub fn poll_result(&mut self, id: TxId, waker: &Waker) -> Poll<Completion<R>> {
        let Some(slot) = self.caller_slot(id) else {
            return Poll::Ready(Completion::Expired);
        };
        match core::mem::replace(&mut slot.state, State::Free) {
            State::Done(result) => {
                slot.release();
                Poll::Ready(Completion::Done(result))
            }
            State::Expired => {
                slot.release();
                Poll::Ready(Completion::Expired)
            }
            State::Preempted => {
                slot.release();
                Poll::Ready(Completion::Preempted)
            }
            State::Shutdown => {
                slot.release();
                Poll::Ready(Completion::Shutdown)
            }
            pending @ (State::Queued | State::InFlight) => {
                slot.state = pending;
                register(&mut slot.waker, waker);
                Poll::Pending
            }
            State::Free | State::AbandonedInFlight => Poll::Ready(Completion::Expired),
        }
    }

    /// Like [`poll_result`](Self::poll_result), for a caller whose own deadline passed.
    ///
    /// A finished request is reported; a queued one is removed and reported
    /// [`Completion::Expired`] (it will never be transmitted); a frame in flight is `Pending`
    /// (the waker is registered) and must be waited out.
    pub fn poll_expired(&mut self, id: TxId, waker: &Waker) -> Poll<Completion<R>> {
        if let Some(slot) = self.caller_slot(id) {
            if matches!(slot.state, State::Queued) {
                slot.release();
                return Poll::Ready(Completion::Expired);
            }
        }
        self.poll_result(id, waker)
    }

    /// True while the frame of request `id` has been handed to the runner and its result has
    /// not arrived.
    #[must_use]
    pub fn is_in_flight(&self, id: TxId) -> bool {
        self.slots
            .iter()
            .any(|slot| slot.id == id.0 && matches!(slot.state, State::InFlight))
    }

    /// Gives up on request `id` (the caller is gone); see [`Abandon`].
    pub fn abandon(&mut self, id: TxId) -> Abandon {
        let Some(slot) = self.caller_slot(id) else {
            return Abandon::Dropped;
        };
        if matches!(slot.state, State::InFlight) {
            slot.state = State::AbandonedInFlight;
            slot.waker = None;
            Abandon::InFlight
        } else {
            slot.release();
            Abandon::Dropped
        }
    }

    // -- runner side -------------------------------------------------------------------------

    /// Expires queued requests whose deadline has passed (`now >= deadline`), handing their
    /// wakers to `wake`, then returns the next frame to transmit.
    ///
    /// While a frame is in flight no other is handed out: the answer is
    /// [`Next::WaitUntil`] with the earliest queued deadline, or [`Next::Idle`].
    pub fn next(&mut self, now: Duration, wake: &mut impl FnMut(Waker)) -> Next<'_> {
        for slot in &mut self.slots {
            if matches!(slot.state, State::Queued) && expired(slot.deadline, now) {
                slot.state = State::Expired;
                if let Some(waker) = slot.waker.take() {
                    wake(waker);
                }
            }
        }
        let in_flight = self
            .slots
            .iter()
            .any(|slot| matches!(slot.state, State::InFlight | State::AbandonedInFlight));
        if in_flight {
            let earliest = self
                .slots
                .iter()
                .filter(|slot| matches!(slot.state, State::Queued))
                .filter_map(|slot| slot.deadline)
                .min();
            return earliest.map_or(Next::Idle, Next::WaitUntil);
        }
        let oldest = self
            .slots
            .iter_mut()
            .filter(|slot| matches!(slot.state, State::Queued))
            .min_by_key(|slot| slot.seq);
        match oldest {
            None => Next::Idle,
            Some(slot) => {
                slot.state = State::InFlight;
                Next::Transmit {
                    id: TxId(slot.id),
                    mac: slot.mac,
                    data: &slot.data[..slot.len],
                }
            }
        }
    }

    /// Reports the result of the frame handed out as `id`; returns the caller's waker, which
    /// the tier wakes outside its lock.
    ///
    /// A result for an id that is not in flight (stale, unknown) is dropped and returns `None`,
    /// as does the result of an abandoned request.
    pub fn complete(&mut self, id: TxId, result: R) -> Option<Waker> {
        let slot = self.slots.iter_mut().find(|slot| {
            slot.id == id.0 && matches!(slot.state, State::InFlight | State::AbandonedInFlight)
        })?;
        if matches!(slot.state, State::AbandonedInFlight) {
            slot.release();
            return None;
        }
        slot.state = State::Done(result);
        slot.waker.take()
    }

    // -- scan gate ---------------------------------------------------------------------------

    /// Claims the scan gate and the radio in one step.
    ///
    /// From now on [`SendKind::App`] submissions are refused with [`Refusal::Scanning`].
    /// Application requests still queued are completed at once as [`Completion::Preempted`],
    /// their wakers handed to `wake`.
    /// A frame already in flight is not touched: the scan must wait for
    /// [`can_start_scan_io`](Self::can_start_scan_io) before its first radio action (channel,
    /// TX power), and may submit [`SendKind::Probe`] frames meanwhile.
    /// The exclusion lasts until the tier calls [`end_scan`](Self::end_scan), which it does only
    /// after the scan's restore (channel, TX power, pin) is done.
    ///
    /// # Errors
    ///
    /// [`ScanRefusal::NotOwned`] when `owns_radio` is false (checked first),
    /// [`ScanRefusal::Busy`] when a scan already runs.
    pub fn begin_scan(
        &mut self,
        owns_radio: bool,
        wake: &mut impl FnMut(Waker),
    ) -> Result<(), ScanRefusal> {
        if !owns_radio {
            return Err(ScanRefusal::NotOwned);
        }
        if self.scanning {
            return Err(ScanRefusal::Busy);
        }
        self.scanning = true;
        for slot in &mut self.slots {
            if matches!(slot.state, State::Queued) && slot.kind == SendKind::App {
                slot.state = State::Preempted;
                if let Some(waker) = slot.waker.take() {
                    wake(waker);
                }
            }
        }
        Ok(())
    }

    /// True when no frame is on the air, so a scan may touch the radio (channel, TX power).
    #[must_use]
    pub fn can_start_scan_io(&self) -> bool {
        !self
            .slots
            .iter()
            .any(|slot| matches!(slot.state, State::InFlight | State::AbandonedInFlight))
    }

    /// Ends every request and refuses further submissions with [`Refusal::Shutdown`].
    ///
    /// Queued and in-flight requests complete as [`Completion::Shutdown`] and their wakers go to
    /// `wake`; the result of the in-flight frame is ignored if it still arrives.
    /// Finished results stay collectable.
    /// This lets a worker that waits for the send callback stop and join on drop even if the
    /// callback never fires; while the driver lives, a started frame is waited out for ever
    /// (fail-closed, ADR 020).
    pub fn shutdown(&mut self, wake: &mut impl FnMut(Waker)) {
        self.shut_down = true;
        for slot in &mut self.slots {
            match slot.state {
                State::Queued | State::InFlight => {
                    slot.state = State::Shutdown;
                    if let Some(waker) = slot.waker.take() {
                        wake(waker);
                    }
                }
                State::AbandonedInFlight => slot.release(),
                State::Free
                | State::Done(_)
                | State::Expired
                | State::Preempted
                | State::Shutdown => {}
            }
        }
    }

    /// Releases the scan gate (idempotent); call it only after the scan has restored the
    /// channel, TX power and pin.
    pub fn end_scan(&mut self) {
        self.scanning = false;
    }

    /// True while a scan holds the gate.
    #[must_use]
    pub fn is_scanning(&self) -> bool {
        self.scanning
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::task::Wake;
    use std::vec::Vec;

    const MAC: MacAddress = [1, 2, 3, 4, 5, 6];

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    struct Counter(AtomicUsize);

    impl Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn counting_waker() -> (Waker, Arc<Counter>) {
        let counter = Arc::new(Counter(AtomicUsize::new(0)));
        (Waker::from(counter.clone()), counter)
    }

    fn wakes(counter: &Counter) -> usize {
        counter.0.load(Ordering::SeqCst)
    }

    type Core = TxCore<u8, 3>;

    fn app(core: &mut Core, now: u64, data: &[u8], deadline: Option<u64>) -> TxId {
        core.submit(ms(now), MAC, data, deadline.map(ms), SendKind::App)
            .unwrap()
    }

    /// Runs `next`, collecting the wakers of expired requests.
    fn step(core: &mut Core, now: u64) -> (Option<(TxId, Vec<u8>)>, Vec<Waker>) {
        let mut woken = Vec::new();
        let out = match core.next(ms(now), &mut |w| woken.push(w)) {
            Next::Transmit { id, data, .. } => Some((id, data.to_vec())),
            Next::WaitUntil(_) | Next::Idle => None,
        };
        (out, woken)
    }

    #[test]
    fn frames_go_out_in_submission_order_one_at_a_time() {
        let mut core = Core::new();
        let a = app(&mut core, 0, b"a", None);
        let b = app(&mut core, 0, b"b", None);
        let c = app(&mut core, 0, b"c", None);

        let (first, _) = step(&mut core, 1);
        assert_eq!(first, Some((a, b"a".to_vec())));
        assert!(core.is_in_flight(a));
        // One in flight: nothing else is handed out until a completes.
        assert_eq!(step(&mut core, 2).0, None);
        assert!(!core.is_in_flight(b));

        core.complete(a, 1);
        assert_eq!(step(&mut core, 3).0, Some((b, b"b".to_vec())));
        core.complete(b, 2);
        assert_eq!(step(&mut core, 4).0, Some((c, b"c".to_vec())));
        core.complete(c, 3);
        assert_eq!(step(&mut core, 5).0, None);
    }

    #[test]
    fn transmit_carries_mac_and_payload() {
        let mut core = Core::new();
        let id = app(&mut core, 0, &[9, 8, 7], None);
        match core.next(ms(0), &mut |_| {}) {
            Next::Transmit { id: got, mac, data } => {
                assert_eq!((got, mac, data), (id, MAC, &[9u8, 8, 7][..]));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn a_result_reaches_its_own_caller_and_wakes_it() {
        let mut core = Core::new();
        let (wa, ca) = counting_waker();
        let (wb, cb) = counting_waker();
        let a = app(&mut core, 0, b"a", None);
        let b = app(&mut core, 0, b"b", None);
        assert_eq!(core.poll_result(a, &wa), Poll::Pending);
        assert_eq!(core.poll_result(b, &wb), Poll::Pending);

        step(&mut core, 0);
        let woken = core.complete(a, 11).expect("a's waker");
        woken.wake();
        assert_eq!((wakes(&ca), wakes(&cb)), (1, 0));
        // b's poll must not see a's result.
        assert_eq!(core.poll_result(b, &wb), Poll::Pending);
        assert_eq!(core.poll_result(a, &wa), Poll::Ready(Completion::Done(11)));

        step(&mut core, 0);
        core.complete(b, 22).expect("b's waker").wake();
        assert_eq!(wakes(&cb), 1);
        assert_eq!(core.poll_result(b, &wb), Poll::Ready(Completion::Done(22)));
    }

    #[test]
    fn an_oversize_payload_is_refused() {
        let mut core = Core::new();
        let big = [0u8; MAX_DATA_LEN + 1];
        assert_eq!(
            core.submit(ms(0), MAC, &big, None, SendKind::App),
            Err(Refusal::TooLarge)
        );
        let max = [0u8; MAX_DATA_LEN];
        assert!(core.submit(ms(0), MAC, &max, None, SendKind::App).is_ok());
    }

    #[test]
    fn a_full_queue_refuses_then_expiry_frees_room() {
        let mut core = Core::new();
        let (w, c) = counting_waker();
        let mut ids = Vec::new();
        for _ in 0..3 {
            ids.push(app(&mut core, 0, b"x", Some(100)));
        }
        assert_eq!(
            core.submit(ms(0), MAC, b"x", None, SendKind::App),
            Err(Refusal::Full)
        );
        let _ = core.poll_result(ids[1], &w);
        // Hand out ids[0] so the rest wait; then let their deadline pass.
        let (first, _) = step(&mut core, 10);
        assert_eq!(first.map(|f| f.0), Some(ids[0]));
        let (none, woken) = step(&mut core, 100);
        assert_eq!(none, None);
        assert_eq!(woken.len(), 1, "only ids[1] had registered a waker");
        for w in woken {
            w.wake();
        }
        assert_eq!(wakes(&c), 1);
        assert_eq!(
            core.poll_result(ids[1], &w),
            Poll::Ready(Completion::Expired)
        );
        assert_eq!(
            core.poll_result(ids[2], &w),
            Poll::Ready(Completion::Expired)
        );
        // The slots of the expired requests are free again.
        assert!(core.submit(ms(100), MAC, b"x", None, SendKind::App).is_ok());
    }

    #[test]
    fn a_request_that_expires_before_start_is_never_transmitted() {
        let mut core = Core::new();
        let a = app(&mut core, 0, b"a", Some(50));
        let b = app(&mut core, 0, b"b", None);
        let (out, _) = step(&mut core, 50);
        assert_eq!(out, Some((b, b"b".to_vec())), "a is skipped, b goes out");
        assert!(!core.is_in_flight(a));
        let (w, _) = counting_waker();
        assert_eq!(core.poll_result(a, &w), Poll::Ready(Completion::Expired));
    }

    #[test]
    fn a_started_frame_is_waited_out_past_its_deadline() {
        let mut core = Core::new();
        let (w, _) = counting_waker();
        let a = app(&mut core, 0, b"a", Some(50));
        assert_eq!(step(&mut core, 49).0.map(|f| f.0), Some(a));
        // The deadline passes while the frame is on the air.
        assert_eq!(step(&mut core, 60).0, None);
        assert!(core.is_in_flight(a));
        assert_eq!(core.poll_expired(a, &w), Poll::Pending);
        core.complete(a, 5);
        assert_eq!(core.poll_result(a, &w), Poll::Ready(Completion::Done(5)));
    }

    #[test]
    fn poll_expired_expires_a_queued_request_and_reports_finished_ones() {
        let mut core = Core::new();
        let (w, _) = counting_waker();
        let a = app(&mut core, 0, b"a", None);
        let b = app(&mut core, 0, b"b", None);
        step(&mut core, 0);
        // b is queued behind a: the caller's deadline removes it, and it is never sent.
        assert_eq!(core.poll_expired(b, &w), Poll::Ready(Completion::Expired));
        core.complete(a, 7);
        assert_eq!(core.poll_expired(a, &w), Poll::Ready(Completion::Done(7)));
        assert_eq!(step(&mut core, 1).0, None);
    }

    #[test]
    fn next_reports_the_earliest_queued_deadline_while_a_frame_is_in_flight() {
        let mut core = Core::new();
        app(&mut core, 0, b"a", None);
        app(&mut core, 0, b"b", Some(300));
        app(&mut core, 0, b"c", Some(200));
        step(&mut core, 0);
        assert_eq!(core.next(ms(10), &mut |_| {}), Next::WaitUntil(ms(200)),);
    }

    #[test]
    fn abandoning_a_queued_request_means_it_is_never_transmitted() {
        let mut core = Core::new();
        let a = app(&mut core, 0, b"a", None);
        let b = app(&mut core, 0, b"b", None);
        assert_eq!(core.abandon(a), Abandon::Dropped);
        assert_eq!(step(&mut core, 0).0, Some((b, b"b".to_vec())));
    }

    #[test]
    fn an_abandoned_in_flight_result_never_reaches_a_later_request() {
        let mut core = Core::new();
        let (w, _) = counting_waker();
        let a = app(&mut core, 0, b"a", None);
        step(&mut core, 0);
        assert_eq!(core.abandon(a), Abandon::InFlight);
        // The slot stays reserved: a fresh request gets another slot and another id.
        let b = app(&mut core, 0, b"b", None);
        assert_ne!(a, b);
        // The abandoned frame is still on the air, so b waits.
        assert_eq!(step(&mut core, 1).0, None);
        assert!(core.complete(a, 1).is_none(), "discarded, no waker");
        assert_eq!(
            core.poll_result(b, &w),
            Poll::Pending,
            "a's result is not b's"
        );
        assert_eq!(step(&mut core, 2).0, Some((b, b"b".to_vec())));
        core.complete(b, 2);
        assert_eq!(core.poll_result(b, &w), Poll::Ready(Completion::Done(2)));
        // Polling the abandoned id never yields anything but Expired.
        assert_eq!(core.poll_result(a, &w), Poll::Ready(Completion::Expired));
    }

    #[test]
    fn a_stale_completion_does_not_complete_the_frame_now_in_flight() {
        let mut core = Core::new();
        let (w, _) = counting_waker();
        let a = app(&mut core, 0, b"a", None);
        step(&mut core, 0);
        core.abandon(a);
        core.complete(a, 1);
        let b = app(&mut core, 0, b"b", None);
        assert_eq!(step(&mut core, 1).0.map(|f| f.0), Some(b));
        // A late duplicate for a must not complete b.
        assert!(core.complete(a, 99).is_none());
        assert!(core.is_in_flight(b));
        assert_eq!(core.poll_result(b, &w), Poll::Pending);
        // Nor does a completion for a frame that was never handed out.
        let c = app(&mut core, 0, b"c", None);
        assert!(core.complete(c, 98).is_none());
        assert_eq!(core.poll_result(c, &w), Poll::Pending);
    }

    #[test]
    fn a_consumed_id_never_returns_another_requests_result() {
        let mut core = Core::new();
        let (w, _) = counting_waker();
        let a = app(&mut core, 0, b"a", None);
        step(&mut core, 0);
        core.complete(a, 1);
        assert_eq!(core.poll_result(a, &w), Poll::Ready(Completion::Done(1)));
        let b = app(&mut core, 0, b"b", None);
        step(&mut core, 0);
        core.complete(b, 2);
        assert_eq!(core.poll_result(a, &w), Poll::Ready(Completion::Expired));
        assert_eq!(core.poll_result(b, &w), Poll::Ready(Completion::Done(2)));
    }

    #[test]
    fn the_scan_gate_refuses_app_frames_and_allows_probes() {
        let mut core = Core::new();
        core.begin_scan(true, &mut |_| {}).unwrap();
        assert!(core.is_scanning());
        assert_eq!(
            core.submit(ms(0), MAC, b"x", None, SendKind::App),
            Err(Refusal::Scanning)
        );
        assert!(core.submit(ms(0), MAC, b"p", None, SendKind::Probe).is_ok());
        core.end_scan();
        assert!(!core.is_scanning());
        assert!(core.submit(ms(0), MAC, b"x", None, SendKind::App).is_ok());
    }

    #[test]
    fn a_second_scan_is_busy_and_an_unowned_radio_is_refused() {
        let mut core = Core::new();
        assert_eq!(
            core.begin_scan(false, &mut |_| {}),
            Err(ScanRefusal::NotOwned)
        );
        assert!(!core.is_scanning(), "a refused scan holds nothing");
        core.begin_scan(true, &mut |_| {}).unwrap();
        assert_eq!(core.begin_scan(true, &mut |_| {}), Err(ScanRefusal::Busy));
        assert_eq!(
            core.begin_scan(false, &mut |_| {}),
            Err(ScanRefusal::NotOwned)
        );
        core.end_scan();
        assert!(core.begin_scan(true, &mut |_| {}).is_ok());
    }

    #[test]
    fn ids_wrap_around_without_colliding_with_live_slots() {
        let (w, _) = counting_waker();
        let mut core = Core::new().with_next_id(u32::MAX);
        let x = app(&mut core, 0, b"x", None); // MAX
        let y = app(&mut core, 0, b"y", None); // 0
        core.abandon(x);
        let z = app(&mut core, 0, b"z", None); // 1
        let t = app(&mut core, 0, b"t", None); // 2, never y's id 0 or z's id 1
        let live = [y, z, t];
        for (i, p) in live.iter().enumerate() {
            for q in &live[i + 1..] {
                assert_ne!(p, q);
            }
        }
        // Each id still reaches only its own request.
        assert_eq!(step(&mut core, 0).0.map(|f| f.0), Some(y));
        core.complete(y, 4);
        assert_eq!(core.poll_result(y, &w), Poll::Ready(Completion::Done(4)));
        assert_eq!(core.poll_result(z, &w), Poll::Pending);
    }

    #[test]
    fn the_counter_skips_an_id_a_live_slot_still_holds() {
        let mut core = Core::new().with_next_id(5);
        let a = app(&mut core, 0, b"a", None); // id 5
                                               // Force the counter back onto the live id, as a full wrap would.
        core.next_id = 5;
        let b = app(&mut core, 0, b"b", None);
        assert_ne!(a, b);
    }

    #[test]
    fn expiry_hands_back_the_waker_of_an_expired_request() {
        let mut core = Core::new();
        let (w, c) = counting_waker();
        app(&mut core, 0, b"a", None);
        let b = app(&mut core, 0, b"b", Some(10));
        step(&mut core, 0);
        let _ = core.poll_result(b, &w);
        let (_, woken) = step(&mut core, 10);
        assert_eq!(woken.len(), 1);
        woken.into_iter().for_each(Waker::wake);
        assert_eq!(wakes(&c), 1);
    }

    #[test]
    fn begin_scan_preempts_queued_app_requests_and_hands_back_their_wakers() {
        let mut core = Core::new();
        let (w, c) = counting_waker();
        let a = app(&mut core, 0, b"a", None);
        let b = app(&mut core, 0, b"b", None);
        let probe = core
            .submit(ms(0), MAC, b"p", None, SendKind::Probe)
            .unwrap();
        step(&mut core, 0); // a is in flight
        let _ = core.poll_result(b, &w);
        let mut woken = Vec::new();
        core.begin_scan(true, &mut |w| woken.push(w)).unwrap();
        assert_eq!(woken.len(), 1);
        woken.into_iter().for_each(Waker::wake);
        assert_eq!(wakes(&c), 1);
        assert_eq!(core.poll_result(b, &w), Poll::Ready(Completion::Preempted));
        // The in-flight app frame is untouched, the queued probe survives.
        assert!(core.is_in_flight(a));
        assert_eq!(core.poll_result(a, &w), Poll::Pending);
        core.complete(a, 1);
        assert_eq!(core.poll_result(a, &w), Poll::Ready(Completion::Done(1)));
        assert_eq!(step(&mut core, 1).0, Some((probe, b"p".to_vec())));
    }

    #[test]
    fn a_refused_scan_preempts_nothing() {
        let mut core = Core::new();
        let a = app(&mut core, 0, b"a", None);
        assert_eq!(
            core.begin_scan(false, &mut |_| {}),
            Err(ScanRefusal::NotOwned)
        );
        core.begin_scan(true, &mut |_| {}).unwrap();
        let (w, _) = counting_waker();
        assert_eq!(core.poll_result(a, &w), Poll::Ready(Completion::Preempted));
        core.end_scan();
        let b = app(&mut core, 0, b"b", None);
        core.begin_scan(true, &mut |_| {}).unwrap();
        assert_eq!(core.begin_scan(true, &mut |_| {}), Err(ScanRefusal::Busy));
        assert_eq!(core.poll_result(b, &w), Poll::Ready(Completion::Preempted));
    }

    #[test]
    fn scan_io_waits_for_the_frame_in_flight() {
        let mut core = Core::new();
        assert!(core.can_start_scan_io());
        let a = app(&mut core, 0, b"a", None);
        step(&mut core, 0);
        core.begin_scan(true, &mut |_| {}).unwrap();
        assert!(!core.can_start_scan_io(), "the channel must not change yet");
        core.complete(a, 1);
        assert!(core.can_start_scan_io());
        // An abandoned frame still on the air blocks scan I/O too.
        core.end_scan();
        let b = app(&mut core, 0, b"b", None);
        step(&mut core, 1);
        core.abandon(b);
        assert!(!core.can_start_scan_io());
        core.complete(b, 2);
        assert!(core.can_start_scan_io());
    }

    #[test]
    fn app_broadcast_is_refused_but_a_probe_may_broadcast() {
        let mut core = Core::new();
        assert_eq!(
            core.submit(ms(0), BROADCAST_MAC, b"x", None, SendKind::App),
            Err(Refusal::Broadcast)
        );
        assert!(core
            .submit(ms(0), BROADCAST_MAC, b"x", None, SendKind::Probe)
            .is_ok());
        assert!(core.submit(ms(0), MAC, b"x", None, SendKind::App).is_ok());
    }

    #[test]
    fn shutdown_ends_queued_and_in_flight_requests_and_refuses_new_ones() {
        let mut core = Core::new();
        let (w, c) = counting_waker();
        let (w2, c2) = counting_waker();
        let a = app(&mut core, 0, b"a", None);
        let b = app(&mut core, 0, b"b", None);
        step(&mut core, 0);
        let _ = core.poll_result(a, &w);
        let _ = core.poll_result(b, &w2);
        let mut woken = Vec::new();
        core.shutdown(&mut |w| woken.push(w));
        assert_eq!(woken.len(), 2);
        woken.into_iter().for_each(Waker::wake);
        assert_eq!((wakes(&c), wakes(&c2)), (1, 1));
        assert!(!core.is_in_flight(a));
        assert!(core.can_start_scan_io());
        // The send callback firing later is ignored.
        assert!(core.complete(a, 9).is_none());
        assert_eq!(core.poll_result(a, &w), Poll::Ready(Completion::Shutdown));
        assert_eq!(core.poll_result(b, &w), Poll::Ready(Completion::Shutdown));
        assert_eq!(
            core.submit(ms(0), MAC, b"x", None, SendKind::App),
            Err(Refusal::Shutdown)
        );
        assert_eq!(
            core.submit(ms(0), MAC, b"x", None, SendKind::Probe),
            Err(Refusal::Shutdown)
        );
        assert_eq!(step(&mut core, 1).0, None);
    }

    #[test]
    fn shutdown_keeps_finished_results_and_frees_abandoned_slots() {
        let mut core = Core::new();
        let (w, _) = counting_waker();
        let a = app(&mut core, 0, b"a", None);
        step(&mut core, 0);
        core.complete(a, 3);
        let b = app(&mut core, 0, b"b", None);
        step(&mut core, 0);
        core.abandon(b);
        core.shutdown(&mut |_| {});
        assert_eq!(core.poll_result(a, &w), Poll::Ready(Completion::Done(3)));
        assert!(core.can_start_scan_io());
    }
}
