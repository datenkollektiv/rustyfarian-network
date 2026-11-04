//! Sans-IO registry of the deadlines of waiting callers.
//!
//! The library has no timer (ADR 021), so a worker thread is the timer: a caller with a finite
//! deadline registers it, the worker never sleeps past [`DeadlineSet::earliest`], and when a
//! deadline passes ([`DeadlineSet::expire`]) it wakes the waiting callers, who then ask their
//! `TxCore` whether their frame never started or is on the air.
//!
//! Entries are removed by [`DeadlineId`], never by value, so two callers with the same deadline
//! cannot remove each other's entry.
//! [`DeadlineSet::register`] never allocates, so it may run under the worker's lock; when the
//! set is full the caller allocates a bigger [`DeadlineStorage`] outside the lock and hands it
//! to [`DeadlineSet::grow`].
//! Needs `alloc` (the `std` feature).

use alloc::vec::Vec;
use core::time::Duration;

/// Handle of one registered deadline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeadlineId(u64);

/// Spare room for a [`DeadlineSet`], allocated outside the lock.
#[derive(Debug)]
pub struct DeadlineStorage(Vec<(DeadlineId, Duration)>);

impl DeadlineStorage {
    /// Room for at least `capacity` deadlines.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self(Vec::with_capacity(capacity))
    }
}

/// [`DeadlineSet::register`] found no room.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DeadlineFull {
    /// Capacity of the [`DeadlineStorage`] to allocate and pass to [`DeadlineSet::grow`].
    pub wanted: usize,
}

/// The registered deadlines; see the [module documentation](self).
#[derive(Debug)]
pub struct DeadlineSet {
    entries: DeadlineStorage,
    next_id: u64,
}

impl DeadlineSet {
    /// An empty set with room for `capacity` deadlines before it needs to grow.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: DeadlineStorage::with_capacity(capacity),
            next_id: 0,
        }
    }

    /// Registers the absolute deadline `at`; never allocates.
    ///
    /// # Errors
    ///
    /// [`DeadlineFull`] when there is no room: allocate and [`grow`](Self::grow), then retry.
    pub fn register(&mut self, at: Duration) -> Result<DeadlineId, DeadlineFull> {
        if self.entries.0.len() == self.entries.0.capacity() {
            return Err(DeadlineFull {
                wanted: (self.entries.0.capacity() * 2).max(4),
            });
        }
        let id = DeadlineId(self.next_id);
        self.next_id += 1;
        self.entries.0.push((id, at));
        Ok(id)
    }

    /// Swaps in `bigger` when it has more room, moving the entries without allocating.
    ///
    /// Returns the storage that is no longer used (the old one, or `bigger` itself when it was
    /// not bigger); drop it after releasing the lock.
    pub fn grow(&mut self, mut bigger: DeadlineStorage) -> DeadlineStorage {
        if bigger.0.capacity() <= self.entries.0.capacity() {
            return bigger;
        }
        bigger.0.append(&mut self.entries.0);
        core::mem::replace(&mut self.entries, bigger)
    }

    /// Removes the deadline `id`; false when it was already removed or expired.
    pub fn remove(&mut self, id: DeadlineId) -> bool {
        match self.entries.0.iter().position(|(entry, _)| *entry == id) {
            Some(index) => {
                self.entries.0.swap_remove(index);
                true
            }
            None => false,
        }
    }

    /// The earliest registered deadline.
    #[must_use]
    pub fn earliest(&self) -> Option<Duration> {
        self.entries.0.iter().map(|(_, at)| *at).min()
    }

    /// Removes every deadline that has passed (`at <= now`); true if any was removed, which
    /// means the worker must wake the waiting callers.
    pub fn expire(&mut self, now: Duration) -> bool {
        let before = self.entries.0.len();
        self.entries.0.retain(|(_, at)| *at > now);
        self.entries.0.len() != before
    }

    /// Number of registered deadlines.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.0.len()
    }

    /// True when no deadline is registered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.0.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn removal_is_by_id_so_equal_deadlines_do_not_collide() {
        let mut set = DeadlineSet::with_capacity(4);
        let a = set.register(ms(100)).unwrap();
        let b = set.register(ms(100)).unwrap();
        assert_ne!(a, b);
        assert!(set.remove(a));
        assert_eq!(set.len(), 1);
        // `a` is gone: removing it again must not take `b`'s entry.
        assert!(!set.remove(a));
        assert_eq!(set.len(), 1);
        assert_eq!(set.earliest(), Some(ms(100)));
        assert!(set.remove(b));
        assert!(set.is_empty());
    }

    #[test]
    fn removing_the_second_of_two_equal_deadlines_keeps_the_first() {
        let mut set = DeadlineSet::with_capacity(4);
        let a = set.register(ms(100)).unwrap();
        let b = set.register(ms(100)).unwrap();
        assert!(set.remove(b));
        assert_eq!(set.len(), 1);
        assert!(set.remove(a));
        assert!(set.is_empty());
    }

    #[test]
    fn earliest_and_expire() {
        let mut set = DeadlineSet::with_capacity(4);
        assert_eq!(set.earliest(), None);
        let late = set.register(ms(300)).unwrap();
        set.register(ms(100)).unwrap();
        set.register(ms(200)).unwrap();
        assert_eq!(set.earliest(), Some(ms(100)));
        assert!(!set.expire(ms(99)));
        assert!(set.expire(ms(100)));
        assert_eq!(set.earliest(), Some(ms(200)));
        assert!(set.expire(ms(250)));
        assert!(!set.expire(ms(250)));
        assert!(set.remove(late));
        assert!(!set.expire(ms(1000)));
    }

    #[test]
    fn an_expired_entry_cannot_be_removed_again() {
        let mut set = DeadlineSet::with_capacity(2);
        let id = set.register(ms(10)).unwrap();
        assert!(set.expire(ms(10)));
        assert!(!set.remove(id));
    }

    #[test]
    fn a_full_set_asks_for_room_and_keeps_its_entries_when_grown() {
        let mut set = DeadlineSet::with_capacity(2);
        let a = set.register(ms(1)).unwrap();
        let b = set.register(ms(2)).unwrap();
        let wanted = match set.register(ms(3)) {
            Err(full) => full.wanted,
            Ok(_) => panic!("a full set must refuse"),
        };
        assert!(wanted > 2);
        let leftover = set.grow(DeadlineStorage::with_capacity(wanted));
        drop(leftover);
        let c = set.register(ms(3)).unwrap();
        assert_eq!(set.len(), 3);
        assert!(set.remove(a) && set.remove(b) && set.remove(c));
    }

    #[test]
    fn a_storage_that_is_not_bigger_is_handed_back() {
        let mut set = DeadlineSet::with_capacity(4);
        set.register(ms(1)).unwrap();
        let back = set.grow(DeadlineStorage::with_capacity(1));
        assert_eq!(back.0.capacity(), 1);
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn an_empty_zero_capacity_set_asks_for_room() {
        let mut set = DeadlineSet::with_capacity(0);
        assert_eq!(set.register(ms(1)), Err(DeadlineFull { wanted: 4 }));
    }
}
