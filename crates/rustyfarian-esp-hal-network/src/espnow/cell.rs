//! Interrupt-safe cells for the ESP-NOW handle (ADR 020).
//!
//! `esp-radio`'s `SendWaiter::wait` and `Drop` busy-spin on a flag that the send
//! callback sets from interrupt context.
//! A critical section (interrupts masked) held across a send therefore never returns.
//! Every cell here keeps the lock for a few instructions only: a value is *taken out*,
//! used with the lock released, and put back by a [`Lease`] guard (also on early return,
//! `?`, panic unwind and future cancellation).

use core::cell::{Cell, RefCell};
use core::ops::{Deref, DerefMut};

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

    /// Replaces the value with `f(old)` and returns the new value.
    pub(crate) fn update(&self, f: impl FnOnce(T) -> T) -> T {
        critical_section::with(|cs| {
            let cell = self.value.borrow(cs);
            let new = f(cell.get());
            cell.set(new);
            new
        })
    }
}

/// Flag that admits one live holder at a time; released when the guard is dropped.
///
/// Used for the process-wide ESP-NOW instance (`esp-radio` panics on a second `EspNow`
/// instance; this guard turns that into an error) and for serializing scans on one handle.
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

#[cfg(test)]
mod tests {
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
    fn shared_copy_update_returns_new_value() {
        let c = SharedCopy::new(3u32);
        assert_eq!(c.update(|v| v + 1), 4);
        assert_eq!(c.get(), 4);
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
}
