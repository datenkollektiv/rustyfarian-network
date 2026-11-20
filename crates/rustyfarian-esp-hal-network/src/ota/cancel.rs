//! [`OtaCancel`] — the cancel switch of one cancellable download.

use core::cell::Cell;
use core::time::Duration;

use critical_section::Mutex;
use juggler::ota::download::{CancelGate, DownloadAction, DownloadInput, DownloadMachine};
use juggler::ota::{CancelRequest, OtaError};

/// Experimental: API may change before 1.0.
///
/// The cancel switch of one `EspHalOtaManager::fetch_and_apply_cancellable` call, shared with the task that cancels.
///
/// [`cancel`](Self::cancel) answers [`CancelRequest::Accepted`] until the download is handed the permit to activate the new slot, and [`CancelRequest::TooLate`] once that happened or the download ended.
/// An accepted cancel stops the download at its next step boundary (before the partition is prepared, after a read or a write, or right before the otadata activation write): the call returns `OtaError::Cancelled` and the boot slot is unchanged.
/// A socket read or flash operation in progress is not interrupted; the cancel takes effect when it returns.
/// One per download: a used switch answers `TooLate`.
/// It is `Sync` and needs no allocator, so share it as a `static` or by reference.
/// The ESP-IDF tier has the same type.
///
/// ```
/// use rustyfarian_esp_hal_network::ota::{CancelRequest, OtaCancel};
///
/// static CANCEL: OtaCancel = OtaCancel::new();
/// assert_eq!(CANCEL.cancel(), CancelRequest::Accepted);
/// ```
#[derive(Debug)]
pub struct OtaCancel(Mutex<Cell<CancelGate>>);

impl OtaCancel {
    /// Experimental: API may change before 1.0.
    ///
    /// A switch for a download that has not started.
    pub const fn new() -> Self {
        Self(Mutex::new(Cell::new(CancelGate::new())))
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Requests that the download stop; see the type documentation for the answer.
    pub fn cancel(&self) -> CancelRequest {
        self.with(CancelGate::request)
    }

    /// One machine step inside the critical section the requester takes, so a cancel cannot fall between the check and the permit.
    ///
    /// The step is pure and short; no I/O runs with interrupts masked.
    #[cfg_attr(
        not(all(
            feature = "embassy",
            any(
                feature = "esp32c3",
                feature = "esp32c6",
                feature = "esp32",
                feature = "esp32s3"
            )
        )),
        allow(dead_code, reason = "only the chip build's manager drives a download")
    )]
    pub(crate) fn poll(
        &self,
        machine: &mut DownloadMachine,
        input: DownloadInput,
        elapsed: Duration,
    ) -> Result<DownloadAction, OtaError> {
        self.with(|gate| gate.poll(machine, input, elapsed))
    }

    fn with<R>(&self, f: impl FnOnce(&mut CancelGate) -> R) -> R {
        critical_section::with(|cs| {
            let cell = self.0.borrow(cs);
            let mut gate = cell.get();
            let result = f(&mut gate);
            cell.set(gate);
            result
        })
    }
}

impl Default for OtaCancel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use juggler::ota::Deadline;

    #[test]
    fn a_cancel_before_the_download_ends_it_at_the_first_step() {
        let cancel = OtaCancel::new();
        assert_eq!(cancel.cancel(), CancelRequest::Accepted);
        let mut machine = DownloadMachine::new(Deadline::none(), Duration::from_secs(1), [0; 32]);
        let action = cancel.poll(&mut machine, DownloadInput::Begin, Duration::ZERO);
        assert!(matches!(
            action,
            Ok(DownloadAction::Done(Err(OtaError::Cancelled)))
        ));
        assert_eq!(cancel.cancel(), CancelRequest::TooLate);
    }
}
