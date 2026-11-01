//! [`WaitList`] and [`Wait`] — async waits on a condition that another thread
//! changes, for the std (ESP-IDF) tier.
//!
//! A [`Wait`] completes with the first `Some` its check returns. It registers
//! under its own id, replaces its waker when polled again by another task, and
//! removes its entry when dropped, so cancelled waits (timeouts, `select`) leave
//! nothing behind. The thread that changes the condition calls
//! [`WaitList::wake_all`].

use std::future::Future;
use std::pin::Pin;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll, Waker};
use std::vec::Vec;

/// Tasks waiting for a condition, by registration id.
#[derive(Debug, Default)]
pub struct WaitList {
    /// Next id, and the registered `(id, waker)` pairs.
    inner: Mutex<(u64, Vec<(u64, Waker)>)>,
}

impl WaitList {
    /// An empty list.
    pub const fn new() -> Self {
        Self {
            inner: Mutex::new((0, Vec::new())),
        }
    }

    fn lock(&self) -> MutexGuard<'_, (u64, Vec<(u64, Waker)>)> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn register(&self, id: &mut Option<u64>, waker: &Waker) {
        let mut inner = self.lock();
        let id = *id.get_or_insert_with(|| {
            let next = inner.0;
            inner.0 = inner.0.wrapping_add(1);
            next
        });
        match inner.1.iter_mut().find(|(waiter, _)| *waiter == id) {
            Some((_, registered)) => {
                if !registered.will_wake(waker) {
                    registered.clone_from(waker);
                }
            }
            // First poll, or `wake_all` removed this entry.
            None => inner.1.push((id, waker.clone())),
        }
    }

    fn unregister(&self, id: u64) {
        self.lock().1.retain(|(waiter, _)| *waiter != id);
    }

    /// Wakes every registered task and clears the list.
    pub fn wake_all(&self) {
        let woken = std::mem::take(&mut self.lock().1);
        for (_, waker) in woken {
            waker.wake();
        }
    }

    /// Registered tasks.
    pub fn len(&self) -> usize {
        self.lock().1.len()
    }

    /// `true` if no task is registered.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// A future that completes with the first `Some` from `check`.
    ///
    /// `check` runs on every poll, so it must be cheap and must not block.
    pub fn wait<T, F: FnMut() -> Option<T> + Unpin>(&self, check: F) -> Wait<'_, T, F> {
        Wait {
            list: self,
            check,
            id: None,
        }
    }
}

/// Future returned by [`WaitList::wait`].
#[derive(Debug)]
pub struct Wait<'a, T, F: FnMut() -> Option<T> + Unpin> {
    list: &'a WaitList,
    check: F,
    id: Option<u64>,
}

impl<T, F: FnMut() -> Option<T> + Unpin> Wait<'_, T, F> {
    fn finish(&mut self, value: T) -> Poll<T> {
        if let Some(id) = self.id.take() {
            self.list.unregister(id);
        }
        Poll::Ready(value)
    }
}

impl<T, F: FnMut() -> Option<T> + Unpin> Future for Wait<'_, T, F> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        let this = self.get_mut();
        if let Some(value) = (this.check)() {
            return this.finish(value);
        }
        // Register, then check again: a change in between still wakes us.
        this.list.register(&mut this.id, cx.waker());
        match (this.check)() {
            Some(value) => this.finish(value),
            None => Poll::Pending,
        }
    }
}

impl<T, F: FnMut() -> Option<T> + Unpin> Drop for Wait<'_, T, F> {
    fn drop(&mut self) {
        if let Some(id) = self.id {
            self.list.unregister(id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Arc;
    use std::task::Wake;

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

    fn poll_with<F: Future + Unpin>(future: &mut F, waker: &Waker) -> Poll<F::Output> {
        Pin::new(future).poll(&mut Context::from_waker(waker))
    }

    #[test]
    fn a_ready_condition_completes_without_registering() {
        let list = WaitList::new();
        let mut wait = list.wait(|| Some(7));
        assert_eq!(poll_with(&mut wait, Waker::noop()), Poll::Ready(7));
        assert!(list.is_empty());
    }

    #[test]
    fn wake_all_wakes_a_pending_wait_which_then_completes() {
        let list = WaitList::new();
        let flag = AtomicBool::new(false);
        let (counter, waker) = counting();
        let mut wait = list.wait(|| flag.load(Ordering::SeqCst).then_some(()));
        assert!(poll_with(&mut wait, &waker).is_pending());
        flag.store(true, Ordering::SeqCst);
        list.wake_all();
        assert_eq!(counter.0.load(Ordering::SeqCst), 1);
        assert!(poll_with(&mut wait, &waker).is_ready());
        assert!(list.is_empty());
    }

    #[test]
    fn a_dropped_wait_unregisters_itself() {
        let list = WaitList::new();
        for _ in 0..3 {
            let mut wait = list.wait(|| None::<()>);
            assert!(poll_with(&mut wait, Waker::noop()).is_pending());
            assert_eq!(list.len(), 1);
        }
        assert!(list.is_empty(), "cancelled waits must leave nothing behind");
    }

    #[test]
    fn a_repolled_wait_replaces_its_waker() {
        let list = WaitList::new();
        let (first, first_waker) = counting();
        let (latest, latest_waker) = counting();
        let mut wait = list.wait(|| None::<()>);
        assert!(poll_with(&mut wait, &first_waker).is_pending());
        assert!(poll_with(&mut wait, &latest_waker).is_pending());
        assert_eq!(list.len(), 1, "a repoll must not append");
        list.wake_all();
        assert_eq!(first.0.load(Ordering::SeqCst), 0);
        assert_eq!(latest.0.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_woken_wait_that_is_not_ready_registers_again() {
        let list = WaitList::new();
        let (counter, waker) = counting();
        let mut wait = list.wait(|| None::<()>);
        assert!(poll_with(&mut wait, &waker).is_pending());
        list.wake_all();
        assert!(list.is_empty());
        assert!(poll_with(&mut wait, &waker).is_pending());
        assert_eq!(list.len(), 1);
        list.wake_all();
        assert_eq!(counter.0.load(Ordering::SeqCst), 2);
    }
}
