//! [`MockLink`] — a [`NetworkLink`] test double for firmware host tests.

use core::cell::Cell;
use core::task::Poll;

use super::{LinkState, NetworkLink};

/// Error returned by [`MockLink`] when a failure was requested.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MockLinkError;

/// A [`NetworkLink`] whose state the test drives by hand.
///
/// `wait_connected` completes as soon as the state is `Connected` and otherwise
/// wakes itself on every poll, so a test drives it with a polling `block_on`.
#[derive(Debug)]
pub struct MockLink {
    state: Cell<LinkState>,
    suspended: Cell<bool>,
    connects: Cell<u32>,
    /// When `true`, `suspend` and `resume` fail with [`MockLinkError`].
    pub fail_calls: Cell<bool>,
}

impl MockLink {
    /// A link that has not started yet ([`LinkState::Idle`]).
    pub const fn new() -> Self {
        Self {
            state: Cell::new(LinkState::Idle),
            suspended: Cell::new(false),
            connects: Cell::new(0),
            fail_calls: Cell::new(false),
        }
    }

    /// Simulates a completed association.
    pub fn associate(&self) {
        self.state.set(LinkState::Connected);
        self.connects.set(self.connects.get() + 1);
    }

    /// Simulates a lost link that the machine retries after a back-off.
    pub fn lose_link(&self) {
        self.state.set(LinkState::Backoff);
    }
}

impl Default for MockLink {
    fn default() -> Self {
        Self::new()
    }
}

impl NetworkLink for MockLink {
    type Error = MockLinkError;

    fn state(&self) -> LinkState {
        self.state.get()
    }

    fn is_suspended(&self) -> bool {
        self.suspended.get()
    }

    fn connect_count(&self) -> u32 {
        self.connects.get()
    }

    fn suspend(&self) -> Result<(), Self::Error> {
        if self.fail_calls.get() {
            return Err(MockLinkError);
        }
        self.state.set(LinkState::Suspended);
        self.suspended.set(true);
        Ok(())
    }

    fn resume(&self) -> Result<(), Self::Error> {
        if self.fail_calls.get() {
            return Err(MockLinkError);
        }
        self.suspended.set(false);
        self.state.set(LinkState::Connecting);
        Ok(())
    }

    async fn wait_connected(&self) {
        core::future::poll_fn(|cx| {
            if self.state.get() == LinkState::Connected {
                Poll::Ready(())
            } else {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        })
        .await
    }
}
