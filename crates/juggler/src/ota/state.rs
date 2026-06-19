//! Backend-neutral OTA partition-swap state machine.

/// Experimental: API may change before 1.0.
///
/// The ordered stages of an OTA firmware update.
///
/// Transitions always move forward via [`next_state`](OtaState::next_state).
/// `Booted` is the terminal state — `next_state` returns `None` there.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum OtaState {
    /// No update in progress.
    Idle,
    /// Firmware image is being downloaded from the update server.
    Downloading,
    /// Downloaded image is being verified (SHA-256 check).
    Verifying,
    /// Verified image is being written to the inactive flash slot.
    Writing,
    /// Flash write complete; waiting for the next reboot to activate the new slot.
    SwapPending,
    /// New firmware has booted successfully.
    Booted,
}

impl OtaState {
    /// Experimental: API may change before 1.0.
    ///
    /// Return the next state in the linear OTA progression, or `None` if
    /// already in the terminal `Booted` state.
    pub fn next_state(&self) -> Option<OtaState> {
        match self {
            OtaState::Idle => Some(OtaState::Downloading),
            OtaState::Downloading => Some(OtaState::Verifying),
            OtaState::Verifying => Some(OtaState::Writing),
            OtaState::Writing => Some(OtaState::SwapPending),
            OtaState::SwapPending => Some(OtaState::Booted),
            OtaState::Booted => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_transitions_to_downloading() {
        assert_eq!(OtaState::Idle.next_state(), Some(OtaState::Downloading));
    }

    #[test]
    fn downloading_transitions_to_verifying() {
        assert_eq!(
            OtaState::Downloading.next_state(),
            Some(OtaState::Verifying)
        );
    }

    #[test]
    fn verifying_transitions_to_writing() {
        assert_eq!(OtaState::Verifying.next_state(), Some(OtaState::Writing));
    }

    #[test]
    fn writing_transitions_to_swap_pending() {
        assert_eq!(OtaState::Writing.next_state(), Some(OtaState::SwapPending));
    }

    #[test]
    fn swap_pending_transitions_to_booted() {
        assert_eq!(OtaState::SwapPending.next_state(), Some(OtaState::Booted));
    }

    #[test]
    fn booted_returns_none() {
        assert_eq!(OtaState::Booted.next_state(), None);
    }

    #[test]
    fn full_chain_covers_all_states() {
        let mut state = OtaState::Idle;
        let expected = [
            OtaState::Downloading,
            OtaState::Verifying,
            OtaState::Writing,
            OtaState::SwapPending,
            OtaState::Booted,
        ];
        for next in expected {
            state = state.next_state().unwrap();
            assert_eq!(state, next);
        }
        assert!(state.next_state().is_none());
    }
}
