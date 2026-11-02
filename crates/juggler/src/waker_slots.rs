//! [`WakerSlots`] — a fixed set of wakers that refuses instead of mass-waking when full.
//!
//! A full registry that wakes everyone to make room (embassy-sync's
//! `MultiWakerRegistration`) turns more waiters than slots into a wake-up storm:
//! every woken task registers again and wakes the others. `WakerSlots` reports
//! "full" instead, and the caller falls back to a timer for that waiter.
//! The slot of a waiter that was dropped stays taken until the next
//! [`wake_all`](WakerSlots::wake_all); the set never grows.
//!
//! [`poll_ready`] is the shared poll protocol the tiers use around it: register
//! under the slot lock, release the lock, check the condition, then classify.

use core::task::Waker;

/// Up to `N` registered wakers, each woken once by [`wake_all`](Self::wake_all).
#[derive(Debug)]
pub struct WakerSlots<const N: usize> {
    slots: [Option<Waker>; N],
}

impl<const N: usize> WakerSlots<N> {
    /// An empty set.
    pub const fn new() -> Self {
        Self {
            slots: [const { None }; N],
        }
    }

    /// Registers `waker`; `false` if every slot holds another task's waker.
    ///
    /// A task that is already registered keeps its slot.
    pub fn register(&mut self, waker: &Waker) -> bool {
        if self
            .slots
            .iter()
            .any(|slot| slot.as_ref().is_some_and(|w| w.will_wake(waker)))
        {
            return true;
        }
        match self.slots.iter_mut().find(|slot| slot.is_none()) {
            Some(slot) => {
                *slot = Some(waker.clone());
                true
            }
            None => false,
        }
    }

    /// Wakes every registered waker and frees all slots.
    pub fn wake_all(&mut self) {
        for slot in &mut self.slots {
            if let Some(waker) = slot.take() {
                waker.wake();
            }
        }
    }

    /// Number of taken slots.
    pub fn len(&self) -> usize {
        self.slots.iter().filter(|slot| slot.is_some()).count()
    }

    /// `true` if no slot is taken.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl<const N: usize> Default for WakerSlots<N> {
    fn default() -> Self {
        Self::new()
    }
}

/// What a readiness poll should do, from [`poll_ready`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadyPoll {
    /// The condition held; nothing was registered.
    Ready,
    /// The waker is registered; wait for [`WakerSlots::wake_all`].
    Waiting,
    /// Every slot was taken, so no waker is registered; re-check on a timer.
    Full,
}

/// Decides one readiness poll from the registration and condition results.
///
/// Call in this order: register the waker under the slot lock, release the lock,
/// check the condition, then pass both results here. Registering first means a
/// condition change between the two still wakes the waiter; checking first would
/// leave it parked until the next change.
///
/// Takes the two results rather than a closure because the condition check must
/// not run while the slot lock is held: the task that publishes the change may
/// need that same lock, so the check belongs outside it.
pub fn poll_ready(waker_registered: bool, condition_met: bool) -> ReadyPoll {
    if condition_met {
        ReadyPoll::Ready
    } else if waker_registered {
        ReadyPoll::Waiting
    } else {
        ReadyPoll::Full
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::sync::Arc;
    use alloc::task::Wake;
    use core::sync::atomic::{AtomicU32, Ordering};

    struct Counter(AtomicU32);

    impl Wake for Counter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn counting() -> (Arc<Counter>, Waker) {
        let counter = Arc::new(Counter(AtomicU32::new(0)));
        (Arc::clone(&counter), Waker::from(counter))
    }

    #[test]
    fn poll_ready_covers_every_registration_and_condition_pair() {
        assert_eq!(poll_ready(true, true), ReadyPoll::Ready);
        assert_eq!(poll_ready(false, true), ReadyPoll::Ready);
        assert_eq!(poll_ready(true, false), ReadyPoll::Waiting);
        assert_eq!(poll_ready(false, false), ReadyPoll::Full);
    }

    #[test]
    fn a_full_set_reports_full_and_accepts_after_wake_all() {
        let mut slots = WakerSlots::<2>::new();
        let (a, wa) = counting();
        let (b, wb) = counting();
        let (_, wc) = counting();
        slots.register(&wa);
        slots.register(&wb);
        let registered = slots.register(&wc);
        assert!(!registered, "the third task gets no slot");
        assert_eq!(poll_ready(registered, false), ReadyPoll::Full);
        slots.wake_all();
        assert_eq!(a.0.load(Ordering::SeqCst), 1, "the registered tasks wake");
        assert_eq!(b.0.load(Ordering::SeqCst), 1);
        assert!(slots.register(&wc), "a freed slot is offered again");
    }

    #[test]
    fn a_full_set_refuses_without_waking_anyone() {
        let mut slots = WakerSlots::<2>::new();
        let (a, wa) = counting();
        let (b, wb) = counting();
        let (c, wc) = counting();
        assert!(slots.register(&wa));
        assert!(slots.register(&wb));
        assert!(!slots.register(&wc), "a third task gets no slot");
        assert_eq!(a.0.load(Ordering::SeqCst), 0, "no mass wake on overflow");
        assert_eq!(b.0.load(Ordering::SeqCst), 0);
        assert_eq!(c.0.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn the_same_task_keeps_one_slot() {
        let mut slots = WakerSlots::<2>::new();
        let (_, wa) = counting();
        assert!(slots.register(&wa));
        assert!(slots.register(&wa.clone()));
        assert_eq!(slots.len(), 1);
    }

    #[test]
    fn wake_all_wakes_each_once_and_frees_the_slots() {
        let mut slots = WakerSlots::<3>::new();
        let (a, wa) = counting();
        let (b, wb) = counting();
        slots.register(&wa);
        slots.register(&wb);
        slots.wake_all();
        assert_eq!(a.0.load(Ordering::SeqCst), 1);
        assert_eq!(b.0.load(Ordering::SeqCst), 1);
        assert!(slots.is_empty());
        slots.wake_all();
        assert_eq!(
            a.0.load(Ordering::SeqCst),
            1,
            "a freed slot is not woken again"
        );
    }
}
