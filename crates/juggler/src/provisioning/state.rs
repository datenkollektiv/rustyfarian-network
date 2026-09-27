//! The backend-neutral provisioning state machine.
//!
//! The ESP-IDF crate drives [`ProvisioningState`] from the HTTP handlers and
//! the NVS commit path; [`ProvisioningState::as_str`] feeds the `/status`
//! endpoint's `state` field. [`Committed`](ProvisioningState::Committed) is
//! terminal, like `OtaState::Booted`.

use core::fmt;

/// Experimental: API may change before 1.0.
///
/// The lifecycle states of a single provisioning session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisioningState {
    /// Portal is up and waiting for a form submission.
    AwaitingSubmission,
    /// A valid submission is being written to NVS.
    Persisting,
    /// Credentials are committed; the session is finished (terminal).
    Committed,
    /// A factory reset has been requested and is awaiting host action (terminal).
    FactoryResetPending,
}

/// Experimental: API may change before 1.0.
///
/// The inputs that drive [`ProvisioningState`] transitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisioningInput {
    /// A form submission passed validation.
    ValidSubmission,
    /// A form submission failed validation.
    InvalidSubmission,
    /// The NVS commit succeeded.
    PersistOk,
    /// The NVS commit failed.
    PersistFailed,
    /// The portal's factory-reset button was pressed.
    FactoryReset,
}

impl ProvisioningState {
    /// Experimental: API may change before 1.0.
    ///
    /// Applies `input`, returning the next state or an [`InvalidTransition`].
    ///
    /// The accepted transitions are: `AwaitingSubmission` + `ValidSubmission` →
    /// `Persisting`; `AwaitingSubmission` + `InvalidSubmission` →
    /// `AwaitingSubmission`; `AwaitingSubmission` + `FactoryReset` →
    /// `FactoryResetPending`; `Persisting` + `PersistOk` → `Committed`;
    /// `Persisting` + `PersistFailed` → `AwaitingSubmission`. Every other pair
    /// is an [`InvalidTransition`]; in particular both `Committed` and
    /// `FactoryResetPending` are terminal and accept no input.
    pub fn apply(self, input: ProvisioningInput) -> Result<ProvisioningState, InvalidTransition> {
        use ProvisioningInput::*;
        use ProvisioningState::*;
        match (self, input) {
            (AwaitingSubmission, ValidSubmission) => Ok(Persisting),
            (AwaitingSubmission, InvalidSubmission) => Ok(AwaitingSubmission),
            (AwaitingSubmission, FactoryReset) => Ok(FactoryResetPending),
            (Persisting, PersistOk) => Ok(Committed),
            (Persisting, PersistFailed) => Ok(AwaitingSubmission),
            (state, input) => Err(InvalidTransition { state, input }),
        }
    }

    /// Experimental: API may change before 1.0.
    ///
    /// The snake_case name used in the `/status` JSON `state` field.
    pub fn as_str(self) -> &'static str {
        match self {
            ProvisioningState::AwaitingSubmission => "awaiting_submission",
            ProvisioningState::Persisting => "persisting",
            ProvisioningState::Committed => "committed",
            ProvisioningState::FactoryResetPending => "factory_reset_pending",
        }
    }
}

/// Experimental: API may change before 1.0.
///
/// What a provisioning-session waiter should do, given the observable signals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitResolution {
    /// A config has been committed — return success.
    Committed,
    /// Factory-reset was requested (state reached `FactoryResetPending`) — return reset.
    FactoryReset,
    /// Neither yet — keep waiting.
    Pending,
}

/// Experimental: API may change before 1.0.
///
/// Decide a waiter's resolution from the provisioning state alone.
///
/// The state machine is the single source of truth for the waiter's signal —
/// NOT a separately tracked "config present" flag. This matters because the
/// committed [`ProvisioningConfig`](super::ProvisioningConfig) payload can be
/// moved out of shared state with `take()` (to avoid cloning the ~1.3 KB
/// struct onto a caller's stack — see `docs/bugs/archive/002-provisioning-config-stack-clone-2026-09-27.md`),
/// so a waiter observed after the payload is gone must still resolve
/// `Committed` from `ProvisioningState::Committed` itself.
/// `FactoryResetPending` resolves the waiter so an indefinite (no-timeout)
/// wait cannot hang after the portal's factory-reset button.
pub fn resolve_wait(state: ProvisioningState) -> WaitResolution {
    match state {
        ProvisioningState::Committed => WaitResolution::Committed,
        ProvisioningState::FactoryResetPending => WaitResolution::FactoryReset,
        _ => WaitResolution::Pending,
    }
}

/// Experimental: API may change before 1.0.
///
/// A rejected [`ProvisioningState::apply`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidTransition {
    /// The state the machine was in.
    pub state: ProvisioningState,
    /// The input that was not accepted from that state.
    pub input: ProvisioningInput,
}

impl fmt::Display for InvalidTransition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "invalid transition: {:?} cannot accept {:?}",
            self.state, self.input
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn awaiting_valid_submission_goes_to_persisting() {
        assert_eq!(
            ProvisioningState::AwaitingSubmission.apply(ProvisioningInput::ValidSubmission),
            Ok(ProvisioningState::Persisting)
        );
    }

    #[test]
    fn awaiting_invalid_submission_stays_awaiting() {
        assert_eq!(
            ProvisioningState::AwaitingSubmission.apply(ProvisioningInput::InvalidSubmission),
            Ok(ProvisioningState::AwaitingSubmission)
        );
    }

    #[test]
    fn awaiting_factory_reset_goes_to_pending() {
        assert_eq!(
            ProvisioningState::AwaitingSubmission.apply(ProvisioningInput::FactoryReset),
            Ok(ProvisioningState::FactoryResetPending)
        );
    }

    #[test]
    fn persisting_ok_goes_to_committed() {
        assert_eq!(
            ProvisioningState::Persisting.apply(ProvisioningInput::PersistOk),
            Ok(ProvisioningState::Committed)
        );
    }

    #[test]
    fn persisting_failed_returns_to_awaiting() {
        assert_eq!(
            ProvisioningState::Persisting.apply(ProvisioningInput::PersistFailed),
            Ok(ProvisioningState::AwaitingSubmission)
        );
    }

    #[test]
    fn every_invalid_transition_is_rejected() {
        use ProvisioningInput::*;
        use ProvisioningState::*;
        let all_states = [
            AwaitingSubmission,
            Persisting,
            Committed,
            FactoryResetPending,
        ];
        let all_inputs = [
            ValidSubmission,
            InvalidSubmission,
            PersistOk,
            PersistFailed,
            FactoryReset,
        ];
        let valid = [
            (AwaitingSubmission, ValidSubmission),
            (AwaitingSubmission, InvalidSubmission),
            (AwaitingSubmission, FactoryReset),
            (Persisting, PersistOk),
            (Persisting, PersistFailed),
        ];
        for state in all_states {
            for input in all_inputs {
                let result = state.apply(input);
                if valid.contains(&(state, input)) {
                    assert!(result.is_ok(), "{state:?} + {input:?} should be valid");
                } else {
                    assert_eq!(
                        result,
                        Err(InvalidTransition { state, input }),
                        "{state:?} + {input:?} should be invalid"
                    );
                }
            }
        }
    }

    #[test]
    fn committed_is_terminal() {
        use ProvisioningInput::*;
        for input in [
            ValidSubmission,
            InvalidSubmission,
            PersistOk,
            PersistFailed,
            FactoryReset,
        ] {
            assert!(ProvisioningState::Committed.apply(input).is_err());
        }
    }

    #[test]
    fn factory_reset_pending_is_terminal() {
        use ProvisioningInput::*;
        for input in [
            ValidSubmission,
            InvalidSubmission,
            PersistOk,
            PersistFailed,
            FactoryReset,
        ] {
            assert!(ProvisioningState::FactoryResetPending.apply(input).is_err());
        }
    }

    #[test]
    fn as_str_matches_status_schema() {
        assert_eq!(
            ProvisioningState::AwaitingSubmission.as_str(),
            "awaiting_submission"
        );
        assert_eq!(ProvisioningState::Persisting.as_str(), "persisting");
        assert_eq!(ProvisioningState::Committed.as_str(), "committed");
        assert_eq!(
            ProvisioningState::FactoryResetPending.as_str(),
            "factory_reset_pending"
        );
    }

    // --- resolve_wait tests ---

    #[test]
    fn resolve_wait_pending_when_awaiting_submission() {
        assert_eq!(
            resolve_wait(ProvisioningState::AwaitingSubmission),
            WaitResolution::Pending,
        );
    }

    #[test]
    fn resolve_wait_pending_when_persisting() {
        assert_eq!(
            resolve_wait(ProvisioningState::Persisting),
            WaitResolution::Pending,
        );
    }

    #[test]
    fn resolve_wait_factory_reset_when_factory_reset_pending() {
        // Regression: an indefinite wait must NOT hang when the portal's
        // factory-reset button is pressed.
        assert_eq!(
            resolve_wait(ProvisioningState::FactoryResetPending),
            WaitResolution::FactoryReset,
        );
    }

    #[test]
    fn resolve_wait_committed_state_resolves_committed_without_payload() {
        // Bug-002 regression guard: the signal is the state, not the payload.
        // `wait_committed` moves the config out of `guard.committed` with
        // `take()`, so a waiter that runs afterwards (or a second caller) must
        // still resolve `Committed` from `ProvisioningState::Committed` alone —
        // keying on `committed.is_some()` would wrongly report `Pending` once
        // the config has been taken.
        assert_eq!(
            resolve_wait(ProvisioningState::Committed),
            WaitResolution::Committed,
        );
    }

    #[test]
    fn invalid_transition_displays() {
        let t = InvalidTransition {
            state: ProvisioningState::Committed,
            input: ProvisioningInput::ValidSubmission,
        };
        let mut s = heapless::String::<64>::new();
        use core::fmt::Write;
        write!(s, "{t}").unwrap();
        assert!(s.as_str().contains("invalid transition"));
    }
}
