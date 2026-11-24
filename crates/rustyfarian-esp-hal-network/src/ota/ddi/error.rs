//! Why the esp-hal DDI runtime could not be built or started.

use core::fmt;

use juggler::ota::ddi::{DdiSettingsError, DdiTransportError};
use juggler::ota::{OtaError, StoreError};

/// Experimental: API may change before 1.0.
///
/// Why the DDI runtime could not be built (`DdiRuntime::new`) or started (`DdiRuntime::start`).
///
/// The ESP-IDF tier has the first three variants; this tier adds [`Store`](Self::Store) and [`Flash`](Self::Flash) for what its `start` reports as an `anyhow` error there (ADR 022: only the error type differs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DdiRuntimeError {
    /// The settings were refused.
    Settings(DdiSettingsError),
    /// The transport configuration was refused.
    Transport(DdiTransportError),
    /// `TARGET_CHIP` is not a profile chip name; cannot happen for a known chip.
    InvalidChip,
    /// The record store was lent out during the start (a nested call; cannot happen in a correct firmware).
    Store(StoreError),
    /// No `SharedFlash` exists, or the OTA update partition cannot be found.
    Flash(OtaError),
}

impl fmt::Display for DdiRuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DdiRuntimeError::Settings(e) => write!(f, "invalid DDI settings: {e}"),
            DdiRuntimeError::Transport(e) => write!(f, "invalid DDI transport: {e}"),
            DdiRuntimeError::InvalidChip => f.write_str("the target chip is not a profile chip"),
            DdiRuntimeError::Store(e) => write!(f, "the OTA record store is unavailable: {e}"),
            DdiRuntimeError::Flash(e) => write!(f, "no OTA update partition: {}", e.code()),
        }
    }
}

impl core::error::Error for DdiRuntimeError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        match self {
            DdiRuntimeError::Settings(e) => Some(e),
            DdiRuntimeError::Transport(e) => Some(e),
            DdiRuntimeError::Store(e) => Some(e),
            DdiRuntimeError::InvalidChip | DdiRuntimeError::Flash(_) => None,
        }
    }
}

impl From<DdiSettingsError> for DdiRuntimeError {
    fn from(e: DdiSettingsError) -> Self {
        DdiRuntimeError::Settings(e)
    }
}

impl From<DdiTransportError> for DdiRuntimeError {
    fn from(e: DdiTransportError) -> Self {
        DdiRuntimeError::Transport(e)
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::ToString;

    use super::*;
    use juggler::ota::KvError;

    #[test]
    fn every_variant_displays_and_the_wrapped_ones_have_a_source() {
        use core::error::Error;
        let store = DdiRuntimeError::Store(StoreError::Kv(KvError::new(KvError::CODE_FLASH)));
        assert!(store.source().is_some());
        assert!(store.to_string().starts_with("the OTA record store"));
        let transport = DdiRuntimeError::from(DdiTransportError::TlsUnavailable);
        assert!(transport.source().is_some());
        let flash = DdiRuntimeError::Flash(OtaError::PartitionNotFound);
        assert!(flash.source().is_none());
        assert!(flash.to_string().starts_with("no OTA update partition"));
        assert!(DdiRuntimeError::InvalidChip.source().is_none());
    }
}
