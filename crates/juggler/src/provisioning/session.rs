//! The host-testable shared session state behind a provisioning portal.
//!
//! Moved out of `rustyfarian-esp-idf-network` so the commit/wait race fixed in
//! `docs/bugs/archive/002-provisioning-config-stack-clone-2026-09-27.md` and
//! its sibling atomic-commit fix can carry a real host-run regression test
//! (esp-idf-sys blocks host builds of that crate's own tests). Uses only
//! `std::sync::{Arc, Mutex, Condvar}` and `std::time::{Duration, Instant}`,
//! which are correct and available under ESP-IDF `std` too.

use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::provisioning::ProvisioningConfig;
use crate::provisioning::{
    resolve_wait, InvalidTransition, ProvisioningInput, ProvisioningState, WaitResolution,
};

/// Experimental: API may change before 1.0.
///
/// The three ways a provisioning session can terminate.
///
/// Returned by [`SessionState::wait_outcome`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionOutcome {
    /// A valid submission was committed.
    ///
    /// Carries no payload: a caller that only needs the outcome discards the
    /// config, so returning it here would clone the ~1.3 KB
    /// [`ProvisioningConfig`] onto the caller's stack for nothing (bug
    /// `docs/bugs/archive/002-provisioning-config-stack-clone-2026-09-27.md`). A caller
    /// that needs the config should use [`SessionState::wait_committed`]
    /// instead.
    Committed,
    /// The factory-reset button was pressed.
    FactoryResetRequested,
    /// The optional timeout elapsed with no terminal event.
    TimedOut,
}

/// Shared session state behind an `Arc<Mutex<…>>` plus a [`Condvar`].
struct StateInner {
    state: ProvisioningState,
    committed: Option<ProvisioningConfig>,
}

/// Experimental: API may change before 1.0.
///
/// Handle to the shared session state, cloned into every HTTP handler.
///
/// `std` `Mutex`/`Condvar` are available and correct under ESP-IDF `std`.
#[derive(Clone)]
pub struct SessionState {
    inner: Arc<(Mutex<StateInner>, Condvar)>,
    start: Instant,
}

impl Default for SessionState {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionState {
    /// Experimental: API may change before 1.0.
    ///
    /// Creates a new session state in [`ProvisioningState::AwaitingSubmission`].
    pub fn new() -> Self {
        Self {
            inner: Arc::new((
                Mutex::new(StateInner {
                    state: ProvisioningState::AwaitingSubmission,
                    committed: None,
                }),
                Condvar::new(),
            )),
            start: Instant::now(),
        }
    }

    /// Experimental: API may change before 1.0.
    ///
    /// The current provisioning state.
    pub fn current(&self) -> ProvisioningState {
        self.inner
            .0
            .lock()
            .map(|g| g.state)
            .unwrap_or(ProvisioningState::AwaitingSubmission)
    }

    /// The one locked transition behind [`apply`](Self::apply),
    /// [`apply_and_notify`](Self::apply_and_notify), and
    /// [`commit`](Self::commit).
    ///
    /// On success, `on_accept` runs under the same lock before the new state
    /// is published, and waiters are woken if `notify` is set. A rejected
    /// transition is logged at `warn` and changes nothing. A poisoned mutex
    /// is reported as a rejection of `input` from the last observable state,
    /// so a caller never mistakes it for success.
    fn transition(
        &self,
        input: ProvisioningInput,
        notify: bool,
        on_accept: impl FnOnce(&mut StateInner),
    ) -> Result<ProvisioningState, InvalidTransition> {
        let mut guard = match self.inner.0.lock() {
            Ok(g) => g,
            Err(poisoned) => {
                let state = poisoned.into_inner().state;
                log::warn!("provisioning state machine: state mutex poisoned, rejecting {input:?}");
                return Err(InvalidTransition { state, input });
            }
        };
        match guard.state.apply(input) {
            Ok(next) => {
                on_accept(&mut guard);
                guard.state = next;
                if notify {
                    self.inner.1.notify_all();
                }
                Ok(next)
            }
            Err(t) => {
                log::warn!("provisioning state machine: {t}");
                Err(t)
            }
        }
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Drives the state machine by `input`, returning the new state.
    ///
    /// An invalid transition is logged at `warn`, changes nothing, and is
    /// returned as `Err` so a caller can refuse the request that caused it —
    /// the portal refuses a `/save` whose `ValidSubmission` is rejected
    /// because the session already reached a terminal state.
    pub fn apply(&self, input: ProvisioningInput) -> Result<ProvisioningState, InvalidTransition> {
        self.transition(input, false, |_| {})
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Applies `input` to the state machine AND notifies any condvar waiters.
    ///
    /// Use this instead of [`apply`](Self::apply) when the transition reaches a
    /// terminal state that a [`wait_outcome`](Self::wait_outcome) caller must
    /// observe — specifically the `FactoryReset → FactoryResetPending` path.
    /// Waiters are woken only when the transition is accepted; returns like
    /// [`apply`](Self::apply).
    pub fn apply_and_notify(
        &self,
        input: ProvisioningInput,
    ) -> Result<ProvisioningState, InvalidTransition> {
        self.transition(input, true, |_| {})
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Applies `ProvisioningInput::PersistOk`, stores the committed config, and
    /// wakes any waiter — all under one lock acquisition.
    ///
    /// Returns `Err` when the session is not `Persisting`; `config` is then
    /// dropped (and scrubbed) and the caller must not report success — see
    /// "Terminal states win" below.
    ///
    /// `wait_committed`/`wait_outcome` key on the state via [`resolve_wait`] —
    /// the state is the single source of truth for the waiter's signal, not
    /// the presence of `guard.committed`, because `wait_committed` moves the
    /// payload out with `take()` and a later observer must still see
    /// `Committed`. The state and the payload must therefore be published
    /// together: a separate `apply(PersistOk)` before storing the payload
    /// would let a waiter that wakes in between (timeout slice or spurious
    /// wakeup) observe `Committed` with no payload and return `None`.
    ///
    /// # Terminal states win
    ///
    /// Only `Persisting` accepts `PersistOk`. Once the session is `Committed`
    /// or `FactoryResetPending`, the first terminal event stands: `commit`
    /// returns `Err`, stores nothing, and drops (scrubbing) `config`. The
    /// caller must not report success in that case. Up to 0.5.0 a commit
    /// after a factory-reset request still stored the config and the waiter
    /// resolved `Committed`; that precedence is gone.
    pub fn commit(&self, config: ProvisioningConfig) -> Result<(), InvalidTransition> {
        self.transition(ProvisioningInput::PersistOk, true, |inner| {
            inner.committed = Some(config);
        })
        .map(|_| ())
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Seconds since the session started, for `/status` `uptime_s`.
    pub fn uptime_secs(&self) -> u64 {
        self.start.elapsed().as_secs()
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Blocks until the config is committed or the optional timeout elapses.
    ///
    /// A `Some(timeout)` is treated as a wall-clock deadline computed once at
    /// entry; spurious wakeups consume the elapsed slice instead of restarting
    /// the timer, so the total wait never exceeds the caller's requested
    /// duration.
    ///
    /// The config is moved out of shared state with `take()`, never cloned
    /// (bug `docs/bugs/archive/002-provisioning-config-stack-clone-2026-09-27.md`), so
    /// it can only be returned once: a second call after a successful return
    /// resolves `Committed` from the state (see [`resolve_wait`]) but finds
    /// `guard.committed` already empty and returns `None` immediately without
    /// blocking.
    ///
    /// **Single consumer.** The same holds for concurrent waiters: one commit
    /// wakes them all, the first takes the config, and every other one
    /// returns `None` — indistinguishable from a timeout by the return value
    /// alone, so this logs a `warn`. Have exactly one thread call this; any
    /// other thread that only needs to know the session finished should use
    /// [`wait_outcome`](Self::wait_outcome), and one that needs the config
    /// afterwards should read it back from the store it was persisted to.
    ///
    /// A factory-reset request does not resolve this wait (unlike
    /// [`wait_outcome`](Self::wait_outcome)) — it keeps blocking until a
    /// commit or the timeout, matching prior behaviour.
    pub fn wait_committed(&self, timeout: Option<Duration>) -> Option<ProvisioningConfig> {
        let (lock, cvar) = &*self.inner;
        let mut guard = match lock.lock() {
            Ok(g) => g,
            Err(e) => {
                log::warn!(
                    "provisioning wait_committed: state mutex poisoned, \
                     treating as timeout: {e}"
                );
                return None;
            }
        };
        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            if resolve_wait(guard.state) == WaitResolution::Committed {
                let config = guard.committed.take();
                if config.is_none() {
                    log::warn!(
                        "provisioning wait_committed: session committed but the config \
                         was already taken by an earlier or concurrent caller"
                    );
                }
                return config;
            }
            match deadline {
                None => match cvar.wait(guard) {
                    Ok(g) => guard = g,
                    Err(e) => {
                        log::warn!(
                            "provisioning wait_committed: condvar poisoned, \
                             treating as timeout: {e}"
                        );
                        return None;
                    }
                },
                Some(d) => {
                    let remaining = d.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return None;
                    }
                    match cvar.wait_timeout(guard, remaining) {
                        Ok((g, _)) => guard = g,
                        Err(e) => {
                            log::warn!(
                                "provisioning wait_committed: condvar poisoned, \
                                 treating as timeout: {e}"
                            );
                            return None;
                        }
                    }
                }
            }
        }
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Blocks until the session reaches a terminal state or the optional timeout
    /// elapses.
    ///
    /// Terminal states are:
    /// - A committed config → [`SessionOutcome::Committed`]
    /// - `FactoryResetPending` state → [`SessionOutcome::FactoryResetRequested`]
    /// - Timeout elapsed → [`SessionOutcome::TimedOut`]
    ///
    /// Unlike [`wait_committed`](Self::wait_committed), this method also wakes
    /// on the factory-reset path, provided the factory-reset handler calls
    /// [`apply_and_notify`](Self::apply_and_notify) rather than bare
    /// [`apply`](Self::apply).
    pub fn wait_outcome(&self, timeout: Option<Duration>) -> SessionOutcome {
        let (lock, cvar) = &*self.inner;
        let mut guard = match lock.lock() {
            Ok(g) => g,
            Err(e) => {
                log::warn!(
                    "provisioning wait_outcome: state mutex poisoned, \
                     treating as timeout: {e}"
                );
                return SessionOutcome::TimedOut;
            }
        };
        let deadline = timeout.map(|t| Instant::now() + t);
        loop {
            // Delegate the per-iteration terminal-state decision to the pure,
            // host-tested `resolve_wait` function so the "factory-reset unblocks
            // an indefinite wait" contract is locked by these unit tests.
            match resolve_wait(guard.state) {
                WaitResolution::Committed => return SessionOutcome::Committed,
                WaitResolution::FactoryReset => return SessionOutcome::FactoryResetRequested,
                WaitResolution::Pending => {}
            }
            match deadline {
                None => match cvar.wait(guard) {
                    Ok(g) => guard = g,
                    Err(e) => {
                        log::warn!(
                            "provisioning wait_outcome: condvar poisoned, \
                             treating as timeout: {e}"
                        );
                        return SessionOutcome::TimedOut;
                    }
                },
                Some(d) => {
                    let remaining = d.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return SessionOutcome::TimedOut;
                    }
                    match cvar.wait_timeout(guard, remaining) {
                        Ok((g, _)) => guard = g,
                        Err(e) => {
                            log::warn!(
                                "provisioning wait_outcome: condvar poisoned, \
                                 treating as timeout: {e}"
                            );
                            return SessionOutcome::TimedOut;
                        }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provisioning::{parse_form, SchemaProfile};
    use std::thread;

    use crate::test_support::test_psk;

    fn test_config() -> ProvisioningConfig {
        let psk = test_psk();
        let body = format!(
            "wifi_ssid=home&wifi_pass={psk}&dev_eui=0011223344556677\
             &join_eui=70B3D57ED005ABCD&app_key=00112233445566778899AABBCCDDEEFF\
             &ota_url=http://example.com/fw.bin&dev_name=hive"
        );
        parse_form(&body, SchemaProfile::LorawanFieldDevice).expect("valid fixture body")
    }

    /// Drives a fresh `SessionState` through `ValidSubmission` → `Persisting`
    /// so `commit` is a legal transition, matching the portal's own sequence
    /// (`ValidSubmission` applied before `commit`).
    fn persisting_session() -> SessionState {
        let session = SessionState::new();
        session
            .apply(ProvisioningInput::ValidSubmission)
            .expect("fresh session accepts ValidSubmission");
        assert_eq!(session.current(), ProvisioningState::Persisting);
        session
    }

    #[test]
    fn wait_committed_returns_config_after_commit() {
        let session = persisting_session();
        session
            .commit(test_config())
            .expect("commit from Persisting is accepted");

        let got = session
            .wait_committed(Some(Duration::ZERO))
            .expect("commit published the config before returning");
        assert_eq!(got.wifi_ssid(), "home");
    }

    #[test]
    fn wait_committed_second_call_returns_none_without_blocking() {
        let session = persisting_session();
        session
            .commit(test_config())
            .expect("commit from Persisting is accepted");
        assert!(session.wait_committed(Some(Duration::ZERO)).is_some());

        let start = Instant::now();
        let second = session.wait_committed(Some(Duration::from_secs(5)));
        assert!(second.is_none());
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "second wait_committed call must not block on the timeout"
        );
    }

    #[test]
    fn wait_outcome_returns_committed_after_payload_taken() {
        let session = persisting_session();
        session
            .commit(test_config())
            .expect("commit from Persisting is accepted");
        assert!(session.wait_committed(Some(Duration::ZERO)).is_some());

        assert_eq!(
            session.wait_outcome(Some(Duration::ZERO)),
            SessionOutcome::Committed
        );
    }

    #[test]
    fn wait_committed_blocks_until_woken_by_commit_from_another_thread() {
        let session = persisting_session();
        let waiter_session = session.clone();

        let handle = thread::spawn(move || waiter_session.wait_committed(None));

        // Give the waiter thread a moment to enter the blocking wait before
        // committing; not required for correctness (the condvar handles the
        // race either way) but keeps the test honest about exercising the wake
        // path rather than the immediate-return path.
        thread::sleep(Duration::from_millis(50));
        session
            .commit(test_config())
            .expect("commit from Persisting is accepted");

        let got = handle.join().expect("waiter thread panicked");
        assert_eq!(
            got.expect("commit must deliver the config").wifi_ssid(),
            "home"
        );
    }

    #[test]
    fn commit_from_non_persisting_state_stores_nothing() {
        let session = SessionState::new();
        assert_eq!(session.current(), ProvisioningState::AwaitingSubmission);

        assert!(session.commit(test_config()).is_err());

        assert_eq!(session.current(), ProvisioningState::AwaitingSubmission);
        assert!(session.wait_committed(Some(Duration::ZERO)).is_none());
    }

    #[test]
    fn factory_reset_resolves_wait_outcome_but_not_wait_committed() {
        let session = SessionState::new();
        session
            .apply_and_notify(ProvisioningInput::FactoryReset)
            .expect("fresh session accepts FactoryReset");
        assert_eq!(session.current(), ProvisioningState::FactoryResetPending);

        assert_eq!(
            session.wait_outcome(Some(Duration::from_millis(50))),
            SessionOutcome::FactoryResetRequested
        );

        let start = Instant::now();
        let got = session.wait_committed(Some(Duration::from_millis(50)));
        assert!(got.is_none());
        assert!(start.elapsed() >= Duration::from_millis(40));
    }

    #[test]
    fn wait_committed_times_out_with_no_commit() {
        let session = SessionState::new();
        let start = Instant::now();
        let got = session.wait_committed(Some(Duration::from_millis(50)));
        assert!(got.is_none());
        assert!(start.elapsed() >= Duration::from_millis(40));
    }

    /// Bug-002 follow-up: the first terminal event stands. A factory-reset
    /// request that lands while no submission is in flight makes any later
    /// submission invalid, so the portal refuses it instead of persisting and
    /// reporting success; the waiter resolves `FactoryResetRequested`. Up to
    /// 0.5.0 the later commit won instead.
    #[test]
    fn submission_after_factory_reset_is_rejected_and_reset_stands() {
        let session = SessionState::new();
        session
            .apply_and_notify(ProvisioningInput::FactoryReset)
            .expect("fresh session accepts FactoryReset");

        let rejected = session
            .apply(ProvisioningInput::ValidSubmission)
            .expect_err("a submission after a reset request must be refused");
        assert_eq!(rejected.state, ProvisioningState::FactoryResetPending);
        assert!(session.commit(test_config()).is_err());

        assert_eq!(session.current(), ProvisioningState::FactoryResetPending);
        assert_eq!(
            session.wait_outcome(Some(Duration::ZERO)),
            SessionOutcome::FactoryResetRequested
        );
    }

    /// A factory reset while a submission is being persisted is refused, so
    /// the in-flight commit completes and is the outcome.
    #[test]
    fn factory_reset_while_persisting_is_rejected_and_commit_stands() {
        let session = persisting_session();
        assert!(session
            .apply_and_notify(ProvisioningInput::FactoryReset)
            .is_err());

        session
            .commit(test_config())
            .expect("the in-flight commit is still accepted");
        assert_eq!(
            session.wait_outcome(Some(Duration::ZERO)),
            SessionOutcome::Committed
        );
    }

    /// A double submit after a commit is refused at `ValidSubmission`, before
    /// anything is persisted, and the committed config is untouched.
    #[test]
    fn submission_after_commit_is_rejected() {
        let session = persisting_session();
        session
            .commit(test_config())
            .expect("commit from Persisting is accepted");

        assert!(session.apply(ProvisioningInput::ValidSubmission).is_err());
        assert!(session
            .apply_and_notify(ProvisioningInput::FactoryReset)
            .is_err());
        assert_eq!(session.current(), ProvisioningState::Committed);
        assert!(session.wait_committed(Some(Duration::ZERO)).is_some());
    }

    /// `wait_committed` is single-consumer: of two concurrent waiters woken by
    /// one commit, exactly one receives the config.
    #[test]
    fn concurrent_wait_committed_delivers_config_to_exactly_one_waiter() {
        let session = persisting_session();
        let waiters: Vec<_> = (0..2)
            .map(|_| {
                let s = session.clone();
                thread::spawn(move || s.wait_committed(Some(Duration::from_secs(5))))
            })
            .collect();

        thread::sleep(Duration::from_millis(50));
        session
            .commit(test_config())
            .expect("commit from Persisting is accepted");

        let delivered = waiters
            .into_iter()
            .map(|h| h.join().expect("waiter thread panicked"))
            .filter(Option::is_some)
            .count();
        assert_eq!(delivered, 1);
    }
}
