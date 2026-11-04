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
//! The slot never drops a job or a result itself: every method that discards one hands it back,
//! so the caller can drop it after releasing its lock.

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
}
