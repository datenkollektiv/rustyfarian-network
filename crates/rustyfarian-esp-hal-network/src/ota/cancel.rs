//! [`OtaCancel`] — the cancel switch of one cancellable download.

use core::cell::Cell;
use core::time::Duration;

use critical_section::Mutex;
use juggler::ota::download::{CancelGate, DownloadAction, DownloadInput, DownloadMachine};
use juggler::ota::{CancelRequest, OtaError};

/// Experimental: API may change before 1.0.
///
/// The cancel switch of one download, shared with the task that cancels.
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

    /// Whether a cancel was accepted and the download has not yet ended on it: polled between two attempts of a retried download.
    #[cfg(any(feature = "ota-ddi", test))]
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "the DDI runtime's download loop polls it")
    )]
    pub(crate) fn is_requested(&self) -> bool {
        self.with(|gate| gate.is_requested())
    }

    /// A switch for a download that is tried again after a failed attempt: it stays open across attempts (`CancelGate::for_retries`), until [`close`](Self::close).
    #[cfg(any(feature = "ota-ddi", test))]
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "the DDI runtime's download loop creates it")
    )]
    pub(crate) const fn for_retries() -> Self {
        Self(Mutex::new(Cell::new(CancelGate::for_retries())))
    }

    /// Re-arms the switch for the next download of a runtime that keeps one switch for all its downloads: the state of [`for_retries`](Self::for_retries) again.
    ///
    /// Only between two downloads: a cancel of the earlier one, accepted or not, is forgotten.
    #[cfg(any(feature = "ota-ddi", test))]
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "the DDI runtime re-arms it per download")
    )]
    pub(crate) fn rearm_for_retries(&self) {
        self.with(|gate| *gate = CancelGate::for_retries());
    }

    /// Closes the switch for good, so a later cancel answers `TooLate`, and returns whether a cancel had been accepted before, in one step inside the critical section.
    #[cfg(any(feature = "ota-ddi", test))]
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "the DDI runtime's download loop closes it")
    )]
    pub(crate) fn close(&self) -> bool {
        self.with(CancelGate::close)
    }

    /// One machine step inside the critical section the requester takes, so a cancel cannot fall between the check and the permit.
    ///
    /// The step is pure and short; no I/O runs with interrupts masked.
    #[cfg_attr(
        not(all(
            feature = "embassy",
            feature = "ota-ddi",
            any(
                feature = "esp32c3",
                feature = "esp32c6",
                feature = "esp32",
                feature = "esp32s3"
            )
        )),
        allow(
            dead_code,
            reason = "only the chip build's manager drives a download, for the DDI client"
        )
    )]
    pub(crate) fn poll(
        &self,
        machine: &mut DownloadMachine,
        input: DownloadInput,
        elapsed: Duration,
    ) -> Result<DownloadAction, OtaError> {
        self.with(|gate| gate.poll(machine, input, elapsed))
    }

    /// Whether the download has to stop between two steps of the slot preparation (the sector-wise erase): an accepted cancel or an expired deadline.
    ///
    /// `Err` is the end of the download, with the error a cancel or deadline during the body gives (`Cancelled`, `DownloadTimeout`); see `DownloadMachine::check_preparing`.
    #[cfg_attr(
        not(all(
            feature = "embassy",
            feature = "ota-ddi",
            any(
                feature = "esp32c3",
                feature = "esp32c6",
                feature = "esp32",
                feature = "esp32s3"
            )
        )),
        allow(
            dead_code,
            reason = "only the chip build's manager erases sector by sector"
        )
    )]
    pub(crate) fn check_preparing(
        &self,
        machine: &mut DownloadMachine,
        elapsed: Duration,
    ) -> Result<(), OtaError> {
        self.with(|gate| gate.check_preparing(machine, elapsed))
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

    #[test]
    fn a_retried_download_stays_cancellable_until_closed() {
        let cancel = OtaCancel::for_retries();
        let mut machine = DownloadMachine::new(Deadline::none(), Duration::from_secs(1), [0; 32]);
        assert!(cancel
            .poll(&mut machine, DownloadInput::Begin, Duration::ZERO)
            .is_ok());
        let failed = cancel.poll(
            &mut machine,
            DownloadInput::ConnectFailed(OtaError::ServerUnreachable),
            Duration::ZERO,
        );
        assert!(matches!(
            failed,
            Ok(DownloadAction::Done(Err(OtaError::ServerUnreachable)))
        ));
        assert!(!cancel.is_requested());
        assert_eq!(cancel.cancel(), CancelRequest::Accepted);
        assert!(cancel.is_requested());
        assert!(cancel.close());
        assert_eq!(cancel.cancel(), CancelRequest::TooLate);
    }

    #[test]
    fn a_closed_switch_is_cancellable_again_after_a_rearm() {
        let cancel = OtaCancel::for_retries();
        assert_eq!(cancel.cancel(), CancelRequest::Accepted);
        assert!(cancel.close());
        assert_eq!(cancel.cancel(), CancelRequest::TooLate);
        cancel.rearm_for_retries();
        assert!(!cancel.is_requested());
        assert_eq!(cancel.cancel(), CancelRequest::Accepted);
        assert!(cancel.is_requested());
    }

    #[test]
    fn closing_an_untouched_retry_switch_reports_no_cancel() {
        let cancel = OtaCancel::for_retries();
        assert!(!cancel.close());
        assert_eq!(cancel.cancel(), CancelRequest::TooLate);
    }

    #[test]
    fn a_cancel_between_erase_steps_ends_with_the_cancel_error() {
        let cancel = OtaCancel::new();
        let mut machine = DownloadMachine::new(Deadline::none(), Duration::from_secs(1), [0; 32]);
        cancel
            .poll(&mut machine, DownloadInput::Begin, Duration::ZERO)
            .unwrap();
        let headers = DownloadInput::Headers {
            content_length: Some(10),
            chunked: false,
            capacity: 4096,
        };
        assert!(matches!(
            cancel.poll(&mut machine, headers, Duration::ZERO),
            Ok(DownloadAction::Prepare { len: 10 })
        ));
        assert_eq!(cancel.check_preparing(&mut machine, Duration::ZERO), Ok(()));
        assert_eq!(cancel.cancel(), CancelRequest::Accepted);
        assert_eq!(
            cancel.check_preparing(&mut machine, Duration::ZERO),
            Err(OtaError::Cancelled)
        );
        assert_eq!(cancel.cancel(), CancelRequest::TooLate);
    }

    #[test]
    fn the_deadline_between_erase_steps_ends_with_the_timeout_error() {
        let cancel = OtaCancel::new();
        let deadline = Deadline::after(Duration::from_secs(5));
        let mut machine = DownloadMachine::new(deadline, Duration::from_secs(1), [0; 32]);
        cancel
            .poll(&mut machine, DownloadInput::Begin, Duration::ZERO)
            .unwrap();
        let headers = DownloadInput::Headers {
            content_length: Some(10),
            chunked: false,
            capacity: 4096,
        };
        cancel.poll(&mut machine, headers, Duration::ZERO).unwrap();
        assert_eq!(
            cancel.check_preparing(&mut machine, Duration::from_secs(4)),
            Ok(())
        );
        assert_eq!(
            cancel.check_preparing(&mut machine, Duration::from_secs(5)),
            Err(OtaError::DownloadTimeout)
        );
    }
}
