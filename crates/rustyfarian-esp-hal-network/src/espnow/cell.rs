//! Interrupt-safe cells for the ESP-NOW handle (ADR 020).
//!
//! `esp-radio`'s `SendWaiter::wait` and `Drop` busy-spin on a flag that the send
//! callback sets from interrupt context.
//! A critical section (interrupts masked) held across a send therefore never returns.
//! Every cell here keeps the lock for a few instructions only: a value is *taken out*,
//! used with the lock released, and put back by a [`Lease`] guard (also on early return,
//! `?`, panic unwind and future cancellation).

use core::cell::{Cell, RefCell};
use core::future::poll_fn;
use core::ops::{Deref, DerefMut};
use core::task::{Poll, Waker};

use critical_section::Mutex;

/// A slot holding at most one `T`, lent out as a [`Lease`].
///
/// A second lease request while the value is out returns `None` instead of blocking or
/// spinning; the ESP-NOW handle reports that as `Busy`.
pub(crate) struct TakeCell<T> {
    slot: Mutex<RefCell<Option<T>>>,
}

impl<T> TakeCell<T> {
    /// Creates a cell holding `value`.
    pub(crate) const fn new(value: T) -> Self {
        Self {
            slot: Mutex::new(RefCell::new(Some(value))),
        }
    }

    /// Creates a cell that never holds a value.
    pub(crate) const fn empty() -> Self {
        Self {
            slot: Mutex::new(RefCell::new(None)),
        }
    }

    /// Takes the value out for the lifetime of the returned guard.
    ///
    /// Returns `None` when the cell is empty or the value is already leased.
    pub(crate) fn lease(&self) -> Option<Lease<'_, T>> {
        let value = critical_section::with(|cs| self.slot.borrow_ref_mut(cs).take())?;
        Some(Lease {
            cell: self,
            value: Some(value),
        })
    }

    /// Runs `f` on the stored value while the lock is held.
    ///
    /// `f` must be short and must not block, spin or wait for an interrupt.
    /// Returns `None` when the cell is empty or leased.
    pub(crate) fn with_locked<R>(&self, f: impl FnOnce(&T) -> R) -> Option<R> {
        critical_section::with(|cs| self.slot.borrow_ref(cs).as_ref().map(f))
    }

    fn put_back(&self, value: T) {
        // The value is dropped outside the lock if the slot is somehow occupied.
        let rejected = critical_section::with(|cs| {
            let mut slot = self.slot.borrow_ref_mut(cs);
            if slot.is_none() {
                *slot = Some(value);
                None
            } else {
                Some(value)
            }
        });
        drop(rejected);
    }
}

/// Exclusive access to a value taken out of a [`TakeCell`]; returns it on drop.
pub(crate) struct Lease<'a, T> {
    cell: &'a TakeCell<T>,
    /// `Some` from construction until `drop` takes it; nothing else clears it.
    value: Option<T>,
}

impl<T> Deref for Lease<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        match &self.value {
            Some(value) => value,
            None => unreachable!("a lease holds its value until it is dropped"),
        }
    }
}

impl<T> DerefMut for Lease<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        match &mut self.value {
            Some(value) => value,
            None => unreachable!("a lease holds its value until it is dropped"),
        }
    }
}

impl<T> Drop for Lease<'_, T> {
    fn drop(&mut self) {
        if let Some(value) = self.value.take() {
            self.cell.put_back(value);
        }
    }
}

/// A `Copy` value shared between tasks and interrupt context.
pub(crate) struct SharedCopy<T: Copy> {
    value: Mutex<Cell<T>>,
}

impl<T: Copy> SharedCopy<T> {
    pub(crate) const fn new(value: T) -> Self {
        Self {
            value: Mutex::new(Cell::new(value)),
        }
    }

    pub(crate) fn get(&self) -> T {
        critical_section::with(|cs| self.value.borrow(cs).get())
    }

    pub(crate) fn set(&self, value: T) {
        critical_section::with(|cs| self.value.borrow(cs).set(value));
    }
}

/// Flag that admits one live holder at a time; released when the guard is dropped.
///
/// Used for the process-wide ESP-NOW instance (`esp-radio` panics on a second `EspNow`
/// instance; this guard turns that into an error) and for the single sender runner of a handle.
/// Cores without atomic compare-and-swap (ESP32-C3) are covered because the flag lives
/// behind a critical section.
/// A rejected claim never touches the flag, so it cannot release the live holder.
pub(crate) struct Singleton<'a> {
    flag: &'a SharedCopy<bool>,
}

impl<'a> Singleton<'a> {
    /// Claims `flag`; `None` when it is already claimed.
    pub(crate) fn acquire(flag: &'a SharedCopy<bool>) -> Option<Self> {
        let was_taken = critical_section::with(|cs| {
            let cell = flag.value.borrow(cs);
            cell.replace(true)
        });
        if was_taken {
            None
        } else {
            Some(Self { flag })
        }
    }
}

impl Drop for Singleton<'_> {
    fn drop(&mut self) {
        self.flag.set(false);
    }
}

/// Wakers collected while a lock is held and woken after it is released.
///
/// A fixed array, no allocation.
/// When it is full the extra waker is woken at once, under the lock: still correct (waking is
/// interrupt-safe and only queues the task), just not deferred.
/// Wakes whatever is left when dropped, so no waiter is lost on an early return.
pub(crate) struct Wakeups<const N: usize> {
    wakers: [Option<Waker>; N],
}

impl<const N: usize> Wakeups<N> {
    pub(crate) const fn new() -> Self {
        Self {
            wakers: [const { None }; N],
        }
    }

    pub(crate) fn push(&mut self, waker: Waker) {
        match self.wakers.iter_mut().find(|slot| slot.is_none()) {
            Some(slot) => *slot = Some(waker),
            None => waker.wake(),
        }
    }

    /// Wakes every collected waker; call it with no lock held.
    pub(crate) fn wake_all(&mut self) {
        for slot in &mut self.wakers {
            if let Some(waker) = slot.take() {
                waker.wake();
            }
        }
    }
}

impl<const N: usize> Drop for Wakeups<N> {
    fn drop(&mut self) {
        self.wake_all();
    }
}

/// Up to `N` registered wakers that move into a [`Wakeups`] to be woken after unlocking.
pub(crate) struct WaiterList<const N: usize> {
    slots: [Option<Waker>; N],
}

impl<const N: usize> WaiterList<N> {
    pub(crate) const fn new() -> Self {
        Self {
            slots: [const { None }; N],
        }
    }

    /// Registers `waker`; `false` when every slot holds another task's waker.
    pub(crate) fn register(&mut self, waker: &Waker) -> bool {
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

    /// Moves every registered waker into `out`.
    pub(crate) fn drain_into<const M: usize>(&mut self, out: &mut Wakeups<M>) {
        for slot in &mut self.slots {
            if let Some(waker) = slot.take() {
                out.push(waker);
            }
        }
    }
}

struct GateState<const N: usize> {
    held: bool,
    waiters: WaiterList<N>,
}

/// Async mutual exclusion for at most one holder, with up to `N` parked waiters.
///
/// `embassy_sync::mutex::Mutex` keeps a single waker, so two waiting tasks wake each other in
/// a loop; this gate parks `N` of them and wakes them all on release.
/// A waiter beyond `N` re-polls itself (correct, but it spins until the gate frees).
pub(crate) struct Gate<const N: usize> {
    state: Mutex<RefCell<GateState<N>>>,
}

impl<const N: usize> Gate<N> {
    pub(crate) const fn new() -> Self {
        Self {
            state: Mutex::new(RefCell::new(GateState {
                held: false,
                waiters: WaiterList::new(),
            })),
        }
    }

    /// Waits for the gate; the guard releases it when dropped (also on cancellation).
    pub(crate) async fn acquire(&self) -> GateGuard<'_, N> {
        poll_fn(|cx| {
            let acquired = critical_section::with(|cs| {
                let mut state = self.state.borrow_ref_mut(cs);
                if state.held {
                    if !state.waiters.register(cx.waker()) {
                        cx.waker().wake_by_ref();
                    }
                    false
                } else {
                    state.held = true;
                    true
                }
            });
            if acquired {
                Poll::Ready(GateGuard { gate: self })
            } else {
                Poll::Pending
            }
        })
        .await
    }
}

/// Holds a [`Gate`]; releases it and wakes the waiters on drop.
pub(crate) struct GateGuard<'a, const N: usize> {
    gate: &'a Gate<N>,
}

impl<const N: usize> Drop for GateGuard<'_, N> {
    fn drop(&mut self) {
        let mut wakes = Wakeups::<N>::new();
        critical_section::with(|cs| {
            let mut state = self.gate.state.borrow_ref_mut(cs);
            state.held = false;
            state.waiters.drain_into(&mut wakes);
        });
        wakes.wake_all();
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;

    #[test]
    fn lease_returns_value_on_drop() {
        let cell = TakeCell::new(7u32);
        {
            let mut lease = cell.lease().expect("first lease");
            *lease += 1;
        }
        let lease = cell.lease().expect("second lease");
        assert_eq!(*lease, 8);
    }

    #[test]
    fn second_lease_while_out_is_refused() {
        let cell = TakeCell::new(1u8);
        let first = cell.lease().expect("first lease");
        assert!(cell.lease().is_none());
        drop(first);
        assert!(cell.lease().is_some());
    }

    #[test]
    fn empty_cell_never_leases() {
        let cell: TakeCell<u8> = TakeCell::empty();
        assert!(cell.lease().is_none());
    }

    #[test]
    fn with_locked_sees_value_only_while_in_cell() {
        let cell = TakeCell::new(5u8);
        assert_eq!(cell.with_locked(|v| *v), Some(5));
        let lease = cell.lease().expect("lease");
        assert_eq!(cell.with_locked(|v| *v), None);
        drop(lease);
        assert_eq!(cell.with_locked(|v| *v), Some(5));
    }

    #[test]
    fn lease_is_returned_on_early_exit() {
        fn work(cell: &TakeCell<u8>) -> Result<(), ()> {
            let _lease = cell.lease().ok_or(())?;
            Err(())
        }
        let cell = TakeCell::new(0u8);
        assert!(work(&cell).is_err());
        assert!(cell.lease().is_some());
    }

    #[test]
    fn shared_copy_keeps_the_last_value_set() {
        let c = SharedCopy::new(3u32);
        assert_eq!(c.get(), 3);
        c.set(9);
        assert_eq!(c.get(), 9);
    }

    #[test]
    fn singleton_admits_one_holder_and_releases_on_drop() {
        static FLAG: SharedCopy<bool> = SharedCopy::new(false);
        let first = Singleton::acquire(&FLAG).expect("first holder");
        assert!(Singleton::acquire(&FLAG).is_none());
        drop(first);
        assert!(Singleton::acquire(&FLAG).is_some());
    }

    #[test]
    fn rejected_claims_do_not_release_the_live_holder() {
        let flag = SharedCopy::new(false);
        let holder = Singleton::acquire(&flag).expect("holder");
        assert!(Singleton::acquire(&flag).is_none());
        assert!(Singleton::acquire(&flag).is_none());
        assert!(flag.get(), "flag still claimed after rejected attempts");
        drop(holder);
        assert!(!flag.get());
        assert!(Singleton::acquire(&flag).is_some());
    }

    struct Counter(core::sync::atomic::AtomicUsize);

    impl std::task::Wake for Counter {
        fn wake(self: std::sync::Arc<Self>) {
            self.0.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
        }
    }

    fn counting_waker() -> (std::sync::Arc<Counter>, Waker) {
        let counter = std::sync::Arc::new(Counter(core::sync::atomic::AtomicUsize::new(0)));
        (counter.clone(), Waker::from(counter))
    }

    fn woken(counter: &Counter) -> usize {
        counter.0.load(core::sync::atomic::Ordering::Relaxed)
    }

    #[test]
    fn wakeups_defer_until_woken_and_wake_each_once() {
        let (counter, waker) = counting_waker();
        let mut wakes = Wakeups::<2>::new();
        wakes.push(waker.clone());
        wakes.push(waker);
        assert_eq!(woken(&counter), 0);
        wakes.wake_all();
        assert_eq!(woken(&counter), 2);
        wakes.wake_all();
        assert_eq!(woken(&counter), 2);
    }

    #[test]
    fn wakeups_wake_an_overflowing_waker_at_once_and_the_rest_on_drop() {
        let (counter, waker) = counting_waker();
        {
            let mut wakes = Wakeups::<1>::new();
            wakes.push(waker.clone());
            assert_eq!(woken(&counter), 0);
            wakes.push(waker);
            assert_eq!(woken(&counter), 1, "overflow is woken immediately");
        }
        assert_eq!(woken(&counter), 2, "drop wakes the collected waker");
    }

    #[test]
    fn waiter_list_registers_once_per_task_and_refuses_when_full() {
        let (_a, wake_a) = counting_waker();
        let (_b, wake_b) = counting_waker();
        let mut list = WaiterList::<1>::new();
        assert!(list.register(&wake_a));
        assert!(list.register(&wake_a), "same task keeps its slot");
        assert!(!list.register(&wake_b), "second task finds the list full");
    }

    #[test]
    fn waiter_list_drain_moves_wakers_and_frees_slots() {
        let (counter, waker) = counting_waker();
        let mut list = WaiterList::<2>::new();
        assert!(list.register(&waker));
        let mut wakes = Wakeups::<2>::new();
        list.drain_into(&mut wakes);
        assert_eq!(woken(&counter), 0);
        wakes.wake_all();
        assert_eq!(woken(&counter), 1);
        let (_b, wake_b) = counting_waker();
        assert!(list.register(&wake_b));
        let mut again = Wakeups::<2>::new();
        list.drain_into(&mut again);
    }

    fn poll_once<F: core::future::Future>(
        fut: core::pin::Pin<&mut F>,
        waker: &Waker,
    ) -> Poll<F::Output> {
        fut.poll(&mut core::task::Context::from_waker(waker))
    }

    #[test]
    fn gate_admits_one_holder_and_wakes_a_waiter_on_release() {
        let gate = Gate::<2>::new();
        let (_h, hold_waker) = counting_waker();
        let mut first = core::pin::pin!(gate.acquire());
        let Poll::Ready(guard) = poll_once(first.as_mut(), &hold_waker) else {
            panic!("free gate is acquired at once");
        };
        let (counter, waker) = counting_waker();
        let mut second = core::pin::pin!(gate.acquire());
        assert!(poll_once(second.as_mut(), &waker).is_pending());
        assert_eq!(woken(&counter), 0);
        drop(guard);
        assert_eq!(woken(&counter), 1, "release wakes the parked waiter");
        assert!(poll_once(second.as_mut(), &waker).is_ready());
    }

    #[test]
    fn gate_is_released_when_a_waiter_is_cancelled_after_acquiring() {
        let gate = Gate::<1>::new();
        let (_c, waker) = counting_waker();
        {
            let mut fut = core::pin::pin!(gate.acquire());
            let Poll::Ready(_guard) = poll_once(fut.as_mut(), &waker) else {
                panic!("free gate is acquired at once");
            };
        }
        let mut again = core::pin::pin!(gate.acquire());
        assert!(poll_once(again.as_mut(), &waker).is_ready());
    }
}
