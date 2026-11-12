//! The action machine that sequences the portal's mutating requests
//! (ADR 023 rung 3).
//!
//! Both portals run `POST /save` and `POST /factory-reset` through
//! [`PortalMachine`]: the ESP-IDF tier from `httpd` handler threads behind a
//! `Mutex`, the bare-metal tier from an async task behind a critical-section
//! mutex. The tier feeds the request facts in as a [`PortalInput`] and performs
//! the returned [`PortalAction`]; it adds no branch of its own beyond the
//! mapping of each action to its response, store call, signal and event.
//!
//! # `/save`
//!
//! ```text
//! Save { nonce_ok: false, .. }        -> Forbidden       (403, no state change)
//! Save { nonce_ok, form_valid: false } -> RejectForm     (400 re-render, SubmissionRejected)
//! Save { nonce_ok, form_valid: true }  -> Persist        (store the record)
//! PersistDone { ok: true }             -> Committed      (publish, signal, Committed event, 200)
//! PersistDone { ok: false }            -> PersistFailed  (500 banner, the form stays open)
//! ```
//!
//! # `/factory-reset`
//!
//! ```text
//! FactoryReset { nonce_ok: false } -> Forbidden      (403, no state change)
//! FactoryReset { nonce_ok: true }  -> ResetAccepted  (signal, FactoryResetRequested event, 200)
//! ```
//!
//! # Effect order
//!
//! For [`PortalAction::Committed`] and [`PortalAction::ResetAccepted`] the
//! tier signals the waiter first and emits the event second, then responds:
//! a callback that inspects the session then sees the terminal state its
//! waiter was woken for. On `Committed` the payload is published together
//! with the signal: a waiter must never see `Committed` without it.
//!
//! # Out-of-phase input
//!
//! Every input the current state does not accept yields
//! [`PortalAction::Refused`] with the state unchanged (ADR 023 decision 5):
//! a second save after a commit, a reset while a save is persisting, a
//! persist result with no save in flight. The tier answers `409` and logs.
//! [`Committed`](ProvisioningState::Committed) and
//! [`FactoryResetPending`](ProvisioningState::FactoryResetPending) are
//! terminal: they absorb every input with `Refused` (or `Forbidden` for a bad
//! nonce, which never reaches the state).

use super::{InvalidTransition, ProvisioningInput, ProvisioningState};

/// Experimental: API may change before 1.0.
///
/// One mutating portal request, reduced to the facts the machine decides on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortalInput {
    /// A `POST /save` arrived.
    Save {
        /// The `_nonce` field matched the session nonce.
        nonce_ok: bool,
        /// The form parsed and validated (irrelevant when `nonce_ok` is false).
        form_valid: bool,
    },
    /// The store write the tier ran for [`PortalAction::Persist`] finished.
    PersistDone {
        /// The write succeeded.
        ok: bool,
    },
    /// A `POST /factory-reset` arrived.
    FactoryReset {
        /// The `_nonce` field matched the session nonce.
        nonce_ok: bool,
    },
}

impl PortalInput {
    /// Experimental: API may change before 1.0.
    ///
    /// The state-machine input this request applies, or `None` for a request
    /// with a bad nonce, which never reaches the state.
    pub fn transition_input(self) -> Option<ProvisioningInput> {
        match self {
            PortalInput::Save {
                nonce_ok: false, ..
            }
            | PortalInput::FactoryReset { nonce_ok: false } => None,
            PortalInput::Save {
                form_valid: true, ..
            } => Some(ProvisioningInput::ValidSubmission),
            PortalInput::Save {
                form_valid: false, ..
            } => Some(ProvisioningInput::InvalidSubmission),
            PortalInput::PersistDone { ok: true } => Some(ProvisioningInput::PersistOk),
            PortalInput::PersistDone { ok: false } => Some(ProvisioningInput::PersistFailed),
            PortalInput::FactoryReset { nonce_ok: true } => Some(ProvisioningInput::FactoryReset),
        }
    }

    /// Experimental: API may change before 1.0.
    ///
    /// The portal request that applies `input` with a valid nonce; the inverse
    /// of [`transition_input`](Self::transition_input).
    pub fn from_transition(input: ProvisioningInput) -> Self {
        match input {
            ProvisioningInput::ValidSubmission => PortalInput::Save {
                nonce_ok: true,
                form_valid: true,
            },
            ProvisioningInput::InvalidSubmission => PortalInput::Save {
                nonce_ok: true,
                form_valid: false,
            },
            ProvisioningInput::PersistOk => PortalInput::PersistDone { ok: true },
            ProvisioningInput::PersistFailed => PortalInput::PersistDone { ok: false },
            ProvisioningInput::FactoryReset => PortalInput::FactoryReset { nonce_ok: true },
        }
    }
}

/// Experimental: API may change before 1.0.
///
/// What the tier does next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortalAction {
    /// The nonce did not match: respond `403`. The state is unchanged.
    Forbidden,
    /// The form failed validation: emit `SubmissionRejected` and re-render the
    /// form with the field errors as `400`. The session stays open.
    RejectForm,
    /// The submission is valid and the state moved to
    /// [`Persisting`](ProvisioningState::Persisting): emit
    /// `SubmissionAccepted` where the tier's event set has it, store the
    /// record, then feed [`PortalInput::PersistDone`].
    Persist,
    /// The record is stored and the session is committed: publish the
    /// payload and signal the waiter, emit the `Committed` event, respond
    /// `200` with the committed page.
    Committed,
    /// The store write failed and the session is open again: respond `500`
    /// with the form and a banner.
    PersistFailed,
    /// The factory reset is accepted: signal the waiter, emit
    /// `FactoryResetRequested`, respond `200`.
    ResetAccepted,
    /// The input is out of phase for the current state, which is unchanged:
    /// respond `409` and log.
    Refused(InvalidTransition),
}

/// Experimental: API may change before 1.0.
///
/// The portal's request sequencer; a `Copy` wrapper around
/// [`ProvisioningState`] that starts in
/// [`AwaitingSubmission`](ProvisioningState::AwaitingSubmission).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortalMachine {
    state: ProvisioningState,
}

impl Default for PortalMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl PortalMachine {
    /// Experimental: API may change before 1.0.
    ///
    /// A machine awaiting the first submission.
    pub const fn new() -> Self {
        Self {
            state: ProvisioningState::AwaitingSubmission,
        }
    }

    /// Experimental: API may change before 1.0.
    ///
    /// The current provisioning state (for `/status` and the session handle).
    pub fn state(&self) -> ProvisioningState {
        self.state
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Applies one request and returns the action the tier performs.
    pub fn poll(&mut self, input: PortalInput) -> PortalAction {
        let Some(transition) = input.transition_input() else {
            return PortalAction::Forbidden;
        };
        match self.state.apply(transition) {
            Ok(next) => {
                self.state = next;
                match transition {
                    ProvisioningInput::ValidSubmission => PortalAction::Persist,
                    ProvisioningInput::InvalidSubmission => PortalAction::RejectForm,
                    ProvisioningInput::PersistOk => PortalAction::Committed,
                    ProvisioningInput::PersistFailed => PortalAction::PersistFailed,
                    ProvisioningInput::FactoryReset => PortalAction::ResetAccepted,
                }
            }
            Err(t) => PortalAction::Refused(t),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ProvisioningState::*;

    const SAVE_OK: PortalInput = PortalInput::Save {
        nonce_ok: true,
        form_valid: true,
    };
    const SAVE_INVALID: PortalInput = PortalInput::Save {
        nonce_ok: true,
        form_valid: false,
    };
    const SAVE_BAD_NONCE: PortalInput = PortalInput::Save {
        nonce_ok: false,
        form_valid: true,
    };
    const PERSIST_OK: PortalInput = PortalInput::PersistDone { ok: true };
    const PERSIST_FAIL: PortalInput = PortalInput::PersistDone { ok: false };
    const RESET: PortalInput = PortalInput::FactoryReset { nonce_ok: true };
    const RESET_BAD_NONCE: PortalInput = PortalInput::FactoryReset { nonce_ok: false };

    const ALL_INPUTS: [PortalInput; 7] = [
        SAVE_OK,
        SAVE_INVALID,
        SAVE_BAD_NONCE,
        PERSIST_OK,
        PERSIST_FAIL,
        RESET,
        RESET_BAD_NONCE,
    ];

    fn in_state(state: ProvisioningState) -> PortalMachine {
        PortalMachine { state }
    }

    #[test]
    fn save_valid_persist_ok_commits() {
        let mut m = PortalMachine::new();
        assert_eq!(m.poll(SAVE_OK), PortalAction::Persist);
        assert_eq!(m.state(), Persisting);
        assert_eq!(m.poll(PERSIST_OK), PortalAction::Committed);
        assert_eq!(m.state(), Committed);
    }

    #[test]
    fn save_valid_persist_fail_then_retry_commits() {
        let mut m = PortalMachine::new();
        assert_eq!(m.poll(SAVE_OK), PortalAction::Persist);
        assert_eq!(m.poll(PERSIST_FAIL), PortalAction::PersistFailed);
        assert_eq!(m.state(), AwaitingSubmission);
        assert_eq!(m.poll(SAVE_OK), PortalAction::Persist);
        assert_eq!(m.poll(PERSIST_OK), PortalAction::Committed);
        assert_eq!(m.state(), Committed);
    }

    #[test]
    fn save_invalid_rejects_and_stays_open() {
        let mut m = PortalMachine::new();
        assert_eq!(m.poll(SAVE_INVALID), PortalAction::RejectForm);
        assert_eq!(m.state(), AwaitingSubmission);
        assert_eq!(m.poll(SAVE_OK), PortalAction::Persist);
    }

    #[test]
    fn bad_nonce_forbidden_without_state_change() {
        for state in [
            AwaitingSubmission,
            Persisting,
            Committed,
            FactoryResetPending,
        ] {
            for input in [SAVE_BAD_NONCE, RESET_BAD_NONCE] {
                let mut m = in_state(state);
                assert_eq!(m.poll(input), PortalAction::Forbidden);
                assert_eq!(m.state(), state);
            }
        }
    }

    #[test]
    fn second_save_after_commit_refused() {
        let mut m = in_state(Committed);
        assert_eq!(
            m.poll(SAVE_OK),
            PortalAction::Refused(InvalidTransition {
                state: Committed,
                input: ProvisioningInput::ValidSubmission,
            })
        );
        assert_eq!(m.state(), Committed);
    }

    #[test]
    fn factory_reset_after_commit_refused() {
        let mut m = in_state(Committed);
        assert!(matches!(m.poll(RESET), PortalAction::Refused(_)));
        assert_eq!(m.state(), Committed);
    }

    #[test]
    fn factory_reset_from_open_accepted() {
        let mut m = PortalMachine::new();
        assert_eq!(m.poll(RESET), PortalAction::ResetAccepted);
        assert_eq!(m.state(), FactoryResetPending);
    }

    #[test]
    fn persist_done_without_save_refused() {
        for input in [PERSIST_OK, PERSIST_FAIL] {
            let mut m = PortalMachine::new();
            assert!(matches!(m.poll(input), PortalAction::Refused(_)));
            assert_eq!(m.state(), AwaitingSubmission);
        }
    }

    /// Every (state, input) pair the transition table does not accept is
    /// refused with the state unchanged; the accepted pairs map to their one
    /// action.
    #[test]
    fn every_out_of_phase_input_is_refused_and_state_unchanged() {
        let table: [(ProvisioningState, PortalInput, PortalAction); 5] = [
            (AwaitingSubmission, SAVE_OK, PortalAction::Persist),
            (AwaitingSubmission, SAVE_INVALID, PortalAction::RejectForm),
            (AwaitingSubmission, RESET, PortalAction::ResetAccepted),
            (Persisting, PERSIST_OK, PortalAction::Committed),
            (Persisting, PERSIST_FAIL, PortalAction::PersistFailed),
        ];
        for state in [
            AwaitingSubmission,
            Persisting,
            Committed,
            FactoryResetPending,
        ] {
            for input in ALL_INPUTS {
                let mut m = in_state(state);
                let action = m.poll(input);
                if let Some(&(_, _, expected)) =
                    table.iter().find(|(s, i, _)| *s == state && *i == input)
                {
                    assert_eq!(action, expected, "{state:?} + {input:?}");
                } else if input.transition_input().is_none() {
                    assert_eq!(action, PortalAction::Forbidden, "{state:?} + {input:?}");
                    assert_eq!(m.state(), state);
                } else {
                    assert_eq!(
                        action,
                        PortalAction::Refused(InvalidTransition {
                            state,
                            input: input.transition_input().unwrap(),
                        }),
                        "{state:?} + {input:?}"
                    );
                    assert_eq!(m.state(), state, "{state:?} + {input:?}");
                }
            }
        }
    }

    #[test]
    fn terminal_states_absorb_every_input_with_refused() {
        for state in [Committed, FactoryResetPending] {
            for input in ALL_INPUTS {
                let mut m = in_state(state);
                let action = m.poll(input);
                assert!(
                    matches!(action, PortalAction::Refused(_) | PortalAction::Forbidden),
                    "{state:?} + {input:?} gave {action:?}"
                );
                assert_eq!(m.state(), state);
            }
        }
    }

    #[test]
    fn transition_input_round_trips() {
        for input in [
            ProvisioningInput::ValidSubmission,
            ProvisioningInput::InvalidSubmission,
            ProvisioningInput::PersistOk,
            ProvisioningInput::PersistFailed,
            ProvisioningInput::FactoryReset,
        ] {
            assert_eq!(
                PortalInput::from_transition(input).transition_input(),
                Some(input)
            );
        }
    }
}
