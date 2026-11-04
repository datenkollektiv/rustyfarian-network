//! Sans-IO slot for the single scan job of a worker-based driver.
//!
//! A driver that runs scans on a worker thread hands the scan from the caller to the worker
//! and the result back through one [`ScanJobSlot`] behind its lock.
//! The slot owns the rules that keep two scans from mixing up their jobs or results:
//!
//! - One job at a time: [`ScanJobSlot::admit`] refuses a new job until the slot is empty again,
//!   which includes a finished result nobody has taken yet.
//!   [`TxCore::end_scan`](super::TxCore::end_scan) alone cannot guarantee that, because the
//!   worker ends the scan exclusion when it publishes the result, before the caller reads it.
//! - Every job gets a [`JobEpoch`], and the caller-side operations ([`ScanJobSlot::take`],
//!   [`ScanJobSlot::cancel`]) act only on the job of that epoch, so a late caller of an old scan
//!   can neither take nor cancel a newer scan.
//! - Cancellation is a request: before pickup the job is dropped, while running the worker is
//!   asked to give up and its result is discarded, after it finished the result is discarded.
//!
//! [`ScanGate`] wraps the slot together with the [`TxCore`] scan exclusion, so that admitting,
//! cancelling and finishing a scan are each one call that keeps the two in step.
//!
//! The slot never drops a job or a result itself: every method that discards one hands it back,
//! so the caller can drop it after releasing its lock.

use core::task::Waker;

use super::tx::{ScanRefusal, TxCore};

/// Identifies one admitted job; a stale epoch matches nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobEpoch(u64);

/// What [`ScanJobSlot::cancel`] found.
#[derive(Debug, PartialEq, Eq)]
pub enum Cancelled<J, R> {
    /// The epoch is not the slot's job: nothing happened.
    Stale,
    /// The job had not been picked up and is dropped; the caller must end the scan exclusion
    /// itself, since no worker will run the job.
    Discarded(J),
    /// The worker is running the job and has been asked to give up; it ends the exclusion.
    Requested,
    /// The job had finished; its result is discarded.
    Result(R),
}

enum State<J, R> {
    Empty,
    Pending(J),
    Running { cancel: bool },
    Finished(R),
}

/// The scan job slot; see the [module documentation](self).
pub struct ScanJobSlot<J, R> {
    epoch: u64,
    state: State<J, R>,
}

impl<J, R> Default for ScanJobSlot<J, R> {
    fn default() -> Self {
        Self::new()
    }
}

impl<J, R> ScanJobSlot<J, R> {
    /// An empty slot.
    #[must_use]
    pub fn new() -> Self {
        Self {
            epoch: 0,
            state: State::Empty,
        }
    }

    /// True when a new job may be admitted.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        matches!(self.state, State::Empty)
    }

    /// Stores `job` and returns its epoch, or gives `job` back when the slot is not empty
    /// (a job is pending or running, or a result has not been taken).
    ///
    /// # Errors
    ///
    /// The refused `job`.
    pub fn admit(&mut self, job: J) -> Result<JobEpoch, J> {
        if !self.is_empty() {
            return Err(job);
        }
        self.epoch += 1;
        self.state = State::Pending(job);
        Ok(JobEpoch(self.epoch))
    }

    /// True while the worker runs job `epoch`.
    #[must_use]
    pub fn is_running(&self, epoch: JobEpoch) -> bool {
        epoch.0 == self.epoch && matches!(self.state, State::Running { .. })
    }

    /// True while an admitted job waits for the worker.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        matches!(self.state, State::Pending(_))
    }

    /// The worker takes the pending job and starts running it.
    pub fn pickup(&mut self) -> Option<(JobEpoch, J)> {
        match core::mem::replace(&mut self.state, State::Running { cancel: false }) {
            State::Pending(job) => Some((JobEpoch(self.epoch), job)),
            other => {
                self.state = other;
                None
            }
        }
    }

    /// True when the running job `epoch` was cancelled by its caller.
    #[must_use]
    pub fn cancel_requested(&self, epoch: JobEpoch) -> bool {
        epoch.0 == self.epoch && matches!(self.state, State::Running { cancel: true })
    }

    /// The worker publishes the result of the running job `epoch`.
    ///
    /// Returns the result back when it is to be discarded: its caller cancelled, or `epoch` is
    /// not the running job.
    pub fn finish(&mut self, epoch: JobEpoch, result: R) -> Option<R> {
        match self.state {
            State::Running { cancel } if epoch.0 == self.epoch => {
                if cancel {
                    self.state = State::Empty;
                    Some(result)
                } else {
                    self.state = State::Finished(result);
                    None
                }
            }
            _ => Some(result),
        }
    }

    /// The worker ends the running job `epoch` without a result (nothing to report).
    pub fn release(&mut self, epoch: JobEpoch) {
        if epoch.0 == self.epoch && matches!(self.state, State::Running { .. }) {
            self.state = State::Empty;
        }
    }

    /// The caller takes the finished result of job `epoch`; any other epoch gets `None`.
    pub fn take(&mut self, epoch: JobEpoch) -> Option<R> {
        if epoch.0 != self.epoch {
            return None;
        }
        match core::mem::replace(&mut self.state, State::Empty) {
            State::Finished(result) => Some(result),
            other => {
                self.state = other;
                None
            }
        }
    }

    /// The caller gives up on job `epoch`; see [`Cancelled`].
    pub fn cancel(&mut self, epoch: JobEpoch) -> Cancelled<J, R> {
        if epoch.0 != self.epoch {
            return Cancelled::Stale;
        }
        match core::mem::replace(&mut self.state, State::Empty) {
            State::Pending(job) => Cancelled::Discarded(job),
            State::Running { .. } => {
                self.state = State::Running { cancel: true };
                Cancelled::Requested
            }
            State::Finished(result) => Cancelled::Result(result),
            State::Empty => Cancelled::Stale,
        }
    }
}

/// Why [`ScanGate::admit`] refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateRefusal {
    /// The [`TxCore`] was shut down.
    Shutdown,
    /// A job is pending or running, or the result of the previous one has not been taken, or the
    /// [`TxCore`] already holds a scan.
    Busy,
    /// The driver does not own the radio.
    NotOwned,
}

/// The scan job slot together with the scan exclusion of a [`TxCore`].
///
/// The exclusion and the job must change together or a scan can leak the gate (every later
/// application send refused) or lose its job.
/// Each call here keeps them in step, so a worker-thread driver cannot get the order wrong.
/// The gate does not own the [`TxCore`]: every call takes it by reference, under the same lock
/// as the gate.
///
/// - [`admit`](Self::admit): reserves the radio and stores the job in one step, or refuses and
///   changes nothing (queued application requests are not preempted by a refused scan).
/// - [`may_start`](Self::may_start): the worker's pickup test, a pending job and no frame on
///   the air.
/// - [`cancel`](Self::cancel): before pickup it drops the job and ends the exclusion itself,
///   since no worker will run the job; while running it only asks the worker to give up.
/// - [`finish`](Self::finish) and [`release`](Self::release): the worker ends the exclusion in
///   the same call that publishes the result, so a result is never visible while the exclusion
///   still holds, and the slot stays occupied until the caller takes the result.
///
/// The exclusion is ended only for the job that holds it: a stale epoch changes nothing.
pub struct ScanGate<J, R> {
    slot: ScanJobSlot<J, R>,
}

impl<J, R> Default for ScanGate<J, R> {
    fn default() -> Self {
        Self::new()
    }
}

impl<J, R> ScanGate<J, R> {
    /// An empty gate.
    #[must_use]
    pub fn new() -> Self {
        Self {
            slot: ScanJobSlot::new(),
        }
    }

    /// Admits `job`: claims the radio on `core` and stores the job, atomically.
    ///
    /// Checked in this order: shutdown, a job or an untaken result in the slot, then
    /// [`TxCore::begin_scan`] (not owned, already scanning).
    /// Nothing is changed on a refusal, in particular no queued application request is preempted.
    /// On success those requests were completed as `Preempted` and their wakers went to `wake`.
    ///
    /// # Errors
    ///
    /// The refusal and the unconsumed `job`.
    pub fn admit<T, const N: usize>(
        &mut self,
        core: &mut TxCore<T, N>,
        owns_radio: bool,
        job: J,
        wake: &mut impl FnMut(Waker),
    ) -> Result<JobEpoch, (GateRefusal, J)> {
        if core.is_shut_down() {
            return Err((GateRefusal::Shutdown, job));
        }
        if !self.slot.is_empty() {
            return Err((GateRefusal::Busy, job));
        }
        if let Err(refusal) = core.begin_scan(owns_radio, wake) {
            let refusal = match refusal {
                ScanRefusal::Busy => GateRefusal::Busy,
                ScanRefusal::NotOwned => GateRefusal::NotOwned,
            };
            return Err((refusal, job));
        }
        match self.slot.admit(job) {
            Ok(epoch) => Ok(epoch),
            Err(job) => {
                // Unreachable while the slot was empty above; kept so the exclusion can never
                // leak if the slot's rule changes.
                core.end_scan();
                Err((GateRefusal::Busy, job))
            }
        }
    }

    /// True when the worker may pick the job up: one is pending and no frame is on the air.
    #[must_use]
    pub fn may_start<T, const N: usize>(&self, core: &TxCore<T, N>) -> bool {
        self.slot.has_pending() && core.can_start_scan_io()
    }

    /// The worker takes the pending job; see [`ScanJobSlot::pickup`].
    pub fn pickup(&mut self) -> Option<(JobEpoch, J)> {
        self.slot.pickup()
    }

    /// True when the running job `epoch` was cancelled by its caller.
    #[must_use]
    pub fn cancel_requested(&self, epoch: JobEpoch) -> bool {
        self.slot.cancel_requested(epoch)
    }

    /// The worker publishes the result of the running job `epoch` and ends the exclusion.
    ///
    /// Returns the result back when it is to be discarded (its caller cancelled, or `epoch` is
    /// not the running job); drop it after releasing the lock.
    pub fn finish<T, const N: usize>(
        &mut self,
        core: &mut TxCore<T, N>,
        epoch: JobEpoch,
        result: R,
    ) -> Option<R> {
        if self.slot.is_running(epoch) {
            core.end_scan();
        }
        self.slot.finish(epoch, result)
    }

    /// The worker ends the running job `epoch` without a result and ends the exclusion.
    pub fn release<T, const N: usize>(&mut self, core: &mut TxCore<T, N>, epoch: JobEpoch) {
        if self.slot.is_running(epoch) {
            core.end_scan();
        }
        self.slot.release(epoch);
    }

    /// The caller takes the finished result of job `epoch`; see [`ScanJobSlot::take`].
    pub fn take(&mut self, epoch: JobEpoch) -> Option<R> {
        self.slot.take(epoch)
    }

    /// The caller gives up on job `epoch`; see [`Cancelled`].
    ///
    /// A job not yet picked up is dropped and the exclusion ends here (the returned
    /// [`Cancelled::Discarded`] is how the caller knows); a running job is told to give up and
    /// its worker ends the exclusion.
    pub fn cancel<T, const N: usize>(
        &mut self,
        core: &mut TxCore<T, N>,
        epoch: JobEpoch,
    ) -> Cancelled<J, R> {
        let cancelled = self.slot.cancel(epoch);
        if matches!(cancelled, Cancelled::Discarded(_)) {
            core.end_scan();
        }
        cancelled
    }

    /// True when a new job could be admitted as far as the slot is concerned.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slot.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Slot = ScanJobSlot<&'static str, &'static str>;

    fn running(slot: &mut Slot, job: &'static str) -> JobEpoch {
        let epoch = slot.admit(job).unwrap();
        assert_eq!(slot.pickup(), Some((epoch, job)));
        epoch
    }

    #[test]
    fn a_job_goes_through_admit_pickup_finish_take() {
        let mut slot = Slot::new();
        let epoch = slot.admit("a").unwrap();
        assert!(slot.has_pending());
        assert_eq!(slot.pickup(), Some((epoch, "a")));
        assert!(!slot.has_pending());
        assert_eq!(slot.take(epoch), None);
        assert_eq!(slot.finish(epoch, "ra"), None);
        assert_eq!(slot.take(epoch), Some("ra"));
        assert!(slot.is_empty());
        assert_eq!(slot.take(epoch), None);
    }

    #[test]
    fn a_second_job_is_refused_until_the_result_is_taken() {
        let mut slot = Slot::new();
        let a = slot.admit("a").unwrap();
        assert_eq!(slot.admit("b"), Err("b"));
        slot.pickup();
        assert_eq!(slot.admit("b"), Err("b"));
        assert_eq!(slot.finish(a, "ra"), None);
        // The exclusion of the tier's `TxCore` is over here, the result is not taken yet.
        assert_eq!(slot.admit("b"), Err("b"));
        assert_eq!(slot.take(a), Some("ra"));
        assert!(slot.admit("b").is_ok());
    }

    /// The review's interleaving (C1): A finishes unread, B must not overwrite A's result, and
    /// A's ticket must never get B's result.
    #[test]
    fn a_stale_ticket_never_gets_or_cancels_a_newer_scan() {
        let mut slot = Slot::new();
        let a = running(&mut slot, "a");
        assert_eq!(slot.finish(a, "ra"), None);
        assert_eq!(slot.admit("b"), Err("b"));
        assert_eq!(slot.take(a), Some("ra"));

        let b = slot.admit("b").unwrap();
        assert_ne!(a, b);
        slot.pickup();
        assert_eq!(slot.finish(b, "rb"), None);
        // A's late ticket sees nothing and cancels nothing.
        assert_eq!(slot.take(a), None);
        assert_eq!(slot.cancel(a), Cancelled::Stale);
        assert_eq!(slot.take(b), Some("rb"));
    }

    #[test]
    fn a_stale_cancel_does_not_stop_a_running_newer_scan() {
        let mut slot = Slot::new();
        let a = running(&mut slot, "a");
        assert_eq!(slot.finish(a, "ra"), None);
        assert_eq!(slot.take(a), Some("ra"));
        let b = running(&mut slot, "b");
        assert_eq!(slot.cancel(a), Cancelled::Stale);
        assert!(!slot.cancel_requested(b));
        assert_eq!(slot.finish(b, "rb"), None);
        assert_eq!(slot.take(b), Some("rb"));
    }

    #[test]
    fn cancel_before_pickup_discards_the_job() {
        let mut slot = Slot::new();
        let a = slot.admit("a").unwrap();
        assert_eq!(slot.cancel(a), Cancelled::Discarded("a"));
        assert!(slot.is_empty());
        assert_eq!(slot.pickup(), None);
        assert_eq!(slot.cancel(a), Cancelled::Stale);
    }

    #[test]
    fn cancel_while_running_asks_the_worker_and_discards_the_result() {
        let mut slot = Slot::new();
        let a = running(&mut slot, "a");
        assert!(!slot.cancel_requested(a));
        assert_eq!(slot.cancel(a), Cancelled::Requested);
        assert!(slot.cancel_requested(a));
        assert_eq!(slot.admit("b"), Err("b"));
        assert_eq!(slot.finish(a, "ra"), Some("ra"));
        assert!(slot.is_empty());
    }

    #[test]
    fn cancel_after_finish_discards_the_result() {
        let mut slot = Slot::new();
        let a = running(&mut slot, "a");
        assert_eq!(slot.finish(a, "ra"), None);
        assert_eq!(slot.cancel(a), Cancelled::Result("ra"));
        assert!(slot.is_empty());
    }

    #[test]
    fn release_ends_a_running_job_without_a_result() {
        let mut slot = Slot::new();
        let a = running(&mut slot, "a");
        slot.release(a);
        assert!(slot.is_empty());
        let b = running(&mut slot, "b");
        slot.release(a);
        assert!(!slot.is_empty());
        assert_eq!(slot.finish(b, "rb"), None);
    }

    #[test]
    fn finish_with_a_wrong_epoch_is_discarded() {
        let mut slot = Slot::new();
        let a = running(&mut slot, "a");
        assert_eq!(slot.finish(a, "ra"), None);
        assert_eq!(slot.take(a), Some("ra"));
        let b = running(&mut slot, "b");
        assert_eq!(slot.finish(a, "late"), Some("late"));
        assert_eq!(slot.finish(b, "rb"), None);
    }

    mod gate {
        use super::super::*;
        use crate::espnow::{Completion, Next, SendKind};
        use core::task::{Poll, Waker};
        use core::time::Duration;

        type Core = TxCore<u8, 4>;
        type Gate = ScanGate<&'static str, &'static str>;

        const MAC: [u8; 6] = [1, 2, 3, 4, 5, 6];

        fn queue_app(core: &mut Core) -> crate::espnow::TxId {
            core.submit(Duration::ZERO, MAC, b"x", None, SendKind::App)
                .unwrap()
        }

        fn admit(gate: &mut Gate, core: &mut Core, job: &'static str) -> JobEpoch {
            gate.admit(core, true, job, &mut |_| {}).unwrap()
        }

        #[test]
        fn admission_claims_the_radio_and_stores_the_job() {
            let mut gate = Gate::new();
            let mut core = Core::new();
            let epoch = admit(&mut gate, &mut core, "a");
            assert!(core.is_scanning());
            assert!(gate.may_start(&core));
            assert_eq!(gate.pickup(), Some((epoch, "a")));
            assert_eq!(
                core.submit(Duration::ZERO, MAC, b"x", None, SendKind::App),
                Err(crate::espnow::Refusal::Scanning)
            );
        }

        #[test]
        fn admission_preempts_queued_requests_and_hands_out_their_wakers() {
            let mut gate = Gate::new();
            let mut core = Core::new();
            let id = queue_app(&mut core);
            assert_eq!(core.poll_result(id, Waker::noop()), Poll::Pending);
            let mut woken = 0;
            gate.admit(&mut core, true, "a", &mut |_| woken += 1)
                .unwrap();
            assert_eq!(woken, 1);
            assert_eq!(
                core.poll_result(id, Waker::noop()),
                Poll::Ready(Completion::Preempted)
            );
        }

        #[test]
        fn a_refused_admission_preempts_nothing_and_leaves_the_gate_open() {
            for case in 0..4 {
                let mut gate = Gate::new();
                let mut core = Core::new();
                let mut owns = true;
                let mut expected = GateRefusal::Busy;
                match case {
                    // The previous scan's result has not been taken: the exclusion is long gone.
                    0 => {
                        let epoch = admit(&mut gate, &mut core, "old");
                        gate.pickup();
                        assert_eq!(gate.finish(&mut core, epoch, "r"), None);
                    }
                    // A job is pending.
                    1 => {
                        admit(&mut gate, &mut core, "old");
                    }
                    2 => {
                        owns = false;
                        expected = GateRefusal::NotOwned;
                    }
                    _ => {
                        core.shutdown(&mut |_| {});
                        expected = GateRefusal::Shutdown;
                    }
                }
                let held = core.is_scanning();
                let id = core.submit(Duration::ZERO, MAC, b"x", None, SendKind::App);
                // Shutdown refuses submissions as well; the queued request exists in the others.
                let queued = id.ok().filter(|_| !held);
                let mut woken = 0;
                let refused = gate.admit(&mut core, owns, "new", &mut |_| woken += 1);
                assert_eq!(refused, Err((expected, "new")), "case {case}");
                assert_eq!(woken, 0, "case {case}");
                assert_eq!(
                    core.is_scanning(),
                    held,
                    "case {case}: gate state unchanged"
                );
                if let Some(id) = queued {
                    assert_eq!(
                        core.poll_result(id, Waker::noop()),
                        Poll::Pending,
                        "case {case}: the queued request was not preempted"
                    );
                }
            }
        }

        #[test]
        fn cancelling_before_pickup_drops_the_job_and_ends_the_exclusion() {
            let mut gate = Gate::new();
            let mut core = Core::new();
            let epoch = admit(&mut gate, &mut core, "a");
            assert_eq!(gate.cancel(&mut core, epoch), Cancelled::Discarded("a"));
            assert!(!core.is_scanning());
            assert!(gate.is_empty());
            assert!(!gate.may_start(&core));
        }

        #[test]
        fn cancelling_a_running_job_keeps_the_exclusion_until_the_worker_finishes() {
            let mut gate = Gate::new();
            let mut core = Core::new();
            let epoch = admit(&mut gate, &mut core, "a");
            gate.pickup();
            assert_eq!(gate.cancel(&mut core, epoch), Cancelled::Requested);
            assert!(core.is_scanning(), "the worker has not restored yet");
            assert!(gate.cancel_requested(epoch));
            assert_eq!(gate.finish(&mut core, epoch, "r"), Some("r"));
            assert!(!core.is_scanning());
            assert!(gate.is_empty());
        }

        #[test]
        fn finish_ends_the_exclusion_before_the_result_is_taken() {
            let mut gate = Gate::new();
            let mut core = Core::new();
            let epoch = admit(&mut gate, &mut core, "a");
            gate.pickup();
            assert_eq!(gate.finish(&mut core, epoch, "r"), None);
            assert!(!core.is_scanning(), "exclusion ended with the publication");
            // The slot keeps the result, so no second scan overwrites it.
            assert_eq!(
                gate.admit(&mut core, true, "b", &mut |_| {}),
                Err((GateRefusal::Busy, "b"))
            );
            assert!(!core.is_scanning());
            assert_eq!(gate.take(epoch), Some("r"));
            assert!(gate.admit(&mut core, true, "b", &mut |_| {}).is_ok());
        }

        #[test]
        fn releasing_a_running_job_ends_the_exclusion() {
            let mut gate = Gate::new();
            let mut core = Core::new();
            let epoch = admit(&mut gate, &mut core, "a");
            gate.pickup();
            gate.release(&mut core, epoch);
            assert!(!core.is_scanning());
            assert!(gate.is_empty());
        }

        #[test]
        fn a_stale_epoch_cannot_end_the_exclusion_of_a_newer_scan() {
            let mut gate = Gate::new();
            let mut core = Core::new();
            let old = admit(&mut gate, &mut core, "a");
            gate.pickup();
            gate.finish(&mut core, old, "r");
            gate.take(old);
            let new = admit(&mut gate, &mut core, "b");
            gate.pickup();
            assert_eq!(gate.finish(&mut core, old, "late"), Some("late"));
            gate.release(&mut core, old);
            assert_eq!(gate.cancel(&mut core, old), Cancelled::Stale);
            assert!(core.is_scanning(), "the newer scan still holds the radio");
            assert!(!gate.cancel_requested(new));
        }

        #[test]
        fn the_worker_may_not_start_while_a_frame_is_on_the_air() {
            let mut gate = Gate::new();
            let mut core = Core::new();
            let id = queue_app(&mut core);
            // The frame is handed out before the scan is admitted.
            assert!(matches!(
                core.next(Duration::ZERO, &mut |_| {}),
                Next::Transmit { .. }
            ));
            admit(&mut gate, &mut core, "a");
            assert!(!gate.may_start(&core), "the frame is still on the air");
            core.complete(id, 1);
            assert!(gate.may_start(&core));
            gate.pickup();
            assert!(!gate.may_start(&core), "nothing is pending any more");
        }
    }
}
