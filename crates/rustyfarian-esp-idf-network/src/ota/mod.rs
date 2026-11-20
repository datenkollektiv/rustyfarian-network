//! ESP-IDF OTA driver — streaming download + SHA-256 verify + partition swap.
//!
//! All public APIs are experimental; stabilisation is deferred to the `ota-library` feature.
//!
//! # Quick Start
//!
//! ```rust,no_run
//! use rustyfarian_esp_idf_network::ota::{OtaSession, OtaSessionConfig};
//!
//! let mut session = OtaSession::new(OtaSessionConfig { timeout_secs: 60 }).unwrap();
//! let expected_sha256 = [0u8; 32]; // replace with real digest
//! session.fetch_and_apply("http://192.168.1.1/firmware.bin", &expected_sha256).unwrap();
//! esp_idf_svc::hal::reset::restart();
//! ```
//!
//! # Firmware metadata (re-exported)
//!
//! The cooperative download deadline ([`Deadline`], [`ActivationPermit`]) and the read-error classifier ([`classify_read_error`]) are re-exported from `juggler::ota` as well.
//!
//! The persisted OTA records ([`OtaStore`], [`OtaKv`], [`reconcile_boot`], [`note_boot_slot`], and their outcome and error types) are re-exported from `juggler::ota` as well; [`EspNvsKv`], [`open_store`] and [`open_store_in`] are the NVS backend, [`read_hardware_facts`] and [`note_boot_slot_idf`] read the bootloader.
//!
//! The reason codes ([`FailReason`], [`RollbackReason`]) are re-exported from `juggler::ota` as well.
//! With the `ota-wire` feature, so is the MQTT OTA wire contract (`OtaCommand`, `Manifest`, `OtaStatus`, and their error types and size limits); with `ota-wire` and `mqtt`, `runtime` holds the MQTT OTA consumer runtime; with `ota-ddi`, [`ddi`] holds the DDI transport and runtime.
//!
//! [`ImageMetadata`], [`Version`], [`OtaState`], [`StreamingVerifier`], the
//! hex helpers ([`bytes_to_hex`], [`hex_to_bytes`]), and the update decision
//! policy ([`decide_update`], [`UpdateDecision`]), the offer decision with its
//! refused-version guard and admission gate ([`decide_offer`],
//! [`OfferDecision`], [`Admission`], [`BlockedBy`]), and boot
//! reconciliation ([`reconcile`], [`AttemptRecord`], [`BootFacts`],
//! [`ReconcileAction`], [`SlotId`], [`SlotState`]) are re-exported from
//! `juggler::ota` here — matching the `wifi`/`espnow` domains — so a
//! version-gated updater needs only this crate, with no separate `juggler`
//! dependency to parse a sidecar digest + version and decide whether to apply it:
//!
//! ```
//! use rustyfarian_esp_idf_network::ota::{decide_update, ImageMetadata, UpdateDecision, Version};
//!
//! let meta = ImageMetadata::parse(
//!     "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855",
//!     "1.4.0",
//! )
//! .unwrap();
//! assert_eq!(meta.version, Version::new(1, 4, 0));
//! assert_eq!(
//!     decide_update(Version::new(1, 3, 0), meta.version),
//!     UpdateDecision::Apply
//! );
//! ```

mod boot_facts;
#[cfg(feature = "ota-ddi")]
pub mod ddi;
mod downloader;
#[cfg(any(all(feature = "ota-wire", feature = "mqtt"), feature = "ota-ddi"))]
mod flags;
mod flasher;
#[cfg(any(all(feature = "ota-wire", feature = "mqtt"), feature = "ota-ddi"))]
mod health_driver;
#[cfg(any(all(feature = "ota-wire", feature = "mqtt"), feature = "ota-ddi"))]
mod records;
#[cfg(all(feature = "ota-wire", feature = "mqtt"))]
pub mod runtime;
#[cfg(any(all(feature = "ota-wire", feature = "mqtt"), feature = "ota-ddi"))]
mod stack;
mod store;

pub use boot_facts::{note_boot_slot_idf, read_hardware_facts, slot_evidence_line};
pub use store::{open_store, open_store_in, EspNvsKv};

// The health verdict types of the base `ota` feature, flat like the rest of the `juggler::ota` surface.
// The policy itself is `juggler::ota::health`, shared by the MQTT and the DDI runtime.
pub use juggler::ota::{DeadlineAction, HealthVerdict};

// The MQTT runtime's configuration types, flat like the rest of the `juggler::ota` surface.
// The state machines stay under `juggler::ota::runtime`; this crate's own `runtime` module is the glue.
#[cfg(feature = "ota-wire")]
pub use juggler::ota::{
    ConfigError, OtaSettings, OtaStacks, OtaTimings, MIN_STACK_BYTES, MIN_WORKER_STACK_BYTES,
};

// Re-export the full public surface of `juggler::ota` for domain parity with the
// `wifi`/`espnow` modules, so OTA consumers import metadata/version types from
// this crate rather than adding a redundant direct `juggler` dependency.
// `StreamingVerifier` is also used internally below (via this same import).
pub use juggler::ota::{
    bytes_to_hex, check_content_length, classify_read_error, decide_offer, decide_update,
    hex_to_bytes, note_boot_slot, read_with_busy_retry, reconcile, reconcile_boot,
    slot_for_partition, slot_from_label, slot_label, ActivationPermit, Admission, AttemptRecord,
    BlockedBy, BootDisposition, BootFacts, BootFault, BootOutcome, BootWarnings, BusyRetry,
    BusyTally, CancelRequest, CorruptRecord, Deadline, Delivery, FailReason, FailedDownload,
    HardwareFacts, HardwareReadError, ImageChip, ImageMetadata, KvError, KvStr, NoteBoot,
    NoteBootError, OfferDecision, OtaError, OtaKv, OtaState, OtaStore, PendingReport,
    PendingReportView, ReasonNote, ReconcileAction, RefusalNote, RepairedRecord, ReportRecord,
    RollbackNoted, RollbackReason, RollbackRequest, SettleOutcome, SlotId, SlotState, StoreError,
    StoredAttempt, StreamingVerifier, UpdateDecision, UpdateSlot, Version, FACTORY_SLOT,
    NVS_NAMESPACE,
};

// The MQTT OTA wire contract (ADR 018 amendment 2026-11-21): a DDI build leaves it out.
#[cfg(feature = "ota-wire")]
pub use juggler::ota::{
    CommandError, Manifest, ManifestError, OtaCommand, OtaStatus, StatusError, MAX_COMMAND_BYTES,
    MAX_MANIFEST_BYTES,
};

macro_rules! target_chip {
    ($($cfg:ident => $chip:literal),* $(,)?) => {
        $(
            /// Experimental: API may change before 1.0.
            ///
            /// The chip this firmware runs on, in the spelling a manifest's `target` uses (`"esp32"`, `"esp32c3"`, `"esp32c6"`, `"esp32s3"`, ...).
            ///
            /// Pass it to `Manifest::check_target`.
            /// Derived from the ESP-IDF target cfg (`esp_idf_idf_target_<chip>`), which `esp-idf-sys` propagates to this crate through `embuild::espidf::sysenv::output()` in `build.rs`, the same way the SoftAP cfg is.
            /// It is not defined when no known chip cfg is active, so a build for an unlisted chip fails with "cannot find `TARGET_CHIP`" instead of silently comparing manifests against a wrong chip name.
            ///
            /// ```ignore
            /// use rustyfarian_esp_idf_network::ota::{Manifest, TARGET_CHIP};
            ///
            /// let manifest = Manifest::parse(body)?;
            /// manifest.check_target(TARGET_CHIP)?;
            /// ```
            #[cfg($cfg)]
            pub const TARGET_CHIP: &str = $chip;
        )*
    };
}

target_chip! {
    esp_idf_idf_target_esp32 => "esp32",
    esp_idf_idf_target_esp32s2 => "esp32s2",
    esp_idf_idf_target_esp32s3 => "esp32s3",
    esp_idf_idf_target_esp32c2 => "esp32c2",
    esp_idf_idf_target_esp32c3 => "esp32c3",
    esp_idf_idf_target_esp32c5 => "esp32c5",
    esp_idf_idf_target_esp32c6 => "esp32c6",
    esp_idf_idf_target_esp32h2 => "esp32h2",
    esp_idf_idf_target_esp32p4 => "esp32p4",
}

#[cfg(all(test, feature = "ota-wire"))]
mod target_chip_guard {
    use super::{Manifest, TARGET_CHIP};

    /// Fails to compile if no chip cfg reaches this crate (the const would be absent).
    #[test]
    fn target_chip_is_a_manifest_target_name() {
        assert!(TARGET_CHIP.starts_with("esp32"));
        let body = format!(
            r#"{{"version":"1.0.0","sha256":"{}","url":"http://h/fw.bin","target":"{TARGET_CHIP}"}}"#,
            "0".repeat(64)
        );
        let manifest = Manifest::parse(body.as_bytes()).expect("valid manifest");
        assert!(manifest.check_target(TARGET_CHIP).is_ok());
    }
}

use std::sync::Mutex;
use std::time::{Duration, Instant};

use downloader::{FirmwareDownloader, FirmwareResponse, DOWNLOAD_BUFFER_SIZE, HTTP_RX_BUFFER_SIZE};
use flasher::{FirmwareFlasher, OtaWriter};
use juggler::ota::download::{
    CancelGate, DownloadAction, DownloadInput, DownloadMachine, IMAGE_HEADER_LEN,
};
use juggler::ota::read_bounded;

pub use downloader::url_for_log;

use esp_idf_svc::ota::EspOta;

/// Experimental: API may change before 1.0.
///
/// Configuration for an [`OtaSession`].
#[derive(Debug, Clone, Copy)]
pub struct OtaSessionConfig {
    /// HTTP connection + read timeout in seconds.
    pub timeout_secs: u64,
}

/// Experimental: API may change before 1.0.
///
/// The cancel switch of one [`OtaSession::fetch_and_apply_cancellable`] call, shared with the thread that cancels.
///
/// [`cancel`](Self::cancel) answers [`CancelRequest::Accepted`] until the download is handed the permit to select the new boot slot, and [`CancelRequest::TooLate`] once that happened or the download ended.
/// An accepted cancel stops the download at its next step boundary (before the flash is prepared, after a read or a write, or right before the boot slot is selected): the flash session is aborted and the call returns [`OtaError::Cancelled`], with the boot slot unchanged.
/// A blocking read or flash operation in progress is not interrupted; the cancel takes effect when it returns.
/// One per download: a used switch answers `TooLate`.
/// It is `Sync`, so share it as a `static` or behind an `Arc`.
/// The esp-hal tier has the same type.
///
/// ```rust,no_run
/// # use std::sync::Arc;
/// # use rustyfarian_esp_idf_network::ota::{CancelRequest, OtaCancel, OtaSession, OtaSessionConfig};
/// let cancel = Arc::new(OtaCancel::new());
/// let remote = Arc::clone(&cancel);
/// std::thread::spawn(move || {
///     if remote.cancel() == CancelRequest::TooLate {
///         log::warn!("the new image is already being activated");
///     }
/// });
/// let mut session = OtaSession::new(OtaSessionConfig { timeout_secs: 30 }).unwrap();
/// let result = session.fetch_and_apply_cancellable("http://192.168.1.1/fw.bin", &[0u8; 32], &cancel);
/// ```
#[derive(Debug, Default)]
pub struct OtaCancel(Mutex<CancelGate>);

impl OtaCancel {
    /// Experimental: API may change before 1.0.
    ///
    /// A switch for a download that has not started.
    pub const fn new() -> Self {
        Self(Mutex::new(CancelGate::new()))
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Requests that the download stop; see the type documentation for the answer.
    pub fn cancel(&self) -> CancelRequest {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).request()
    }

    /// Whether a cancel was accepted and the download has not yet ended on it: polled between two attempts of a retried download.
    #[cfg(feature = "ota-ddi")]
    pub(crate) fn is_requested(&self) -> bool {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_requested()
    }

    /// A switch for a download that is tried again after a failed attempt: it stays open across attempts (`CancelGate::for_retries`), until [`close`](Self::close).
    #[cfg(feature = "ota-ddi")]
    pub(crate) const fn for_retries() -> Self {
        Self(Mutex::new(CancelGate::for_retries()))
    }

    /// Closes the switch for good, so a later cancel answers `TooLate`, and returns whether a cancel had been accepted before, in one step under the lock.
    #[cfg(feature = "ota-ddi")]
    pub(crate) fn close(&self) -> bool {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).close()
    }

    /// One machine step under the lock the requester takes, so a cancel cannot fall between the check and the permit.
    fn poll(
        &self,
        machine: &mut DownloadMachine,
        input: DownloadInput,
        elapsed: Duration,
    ) -> Result<DownloadAction, OtaError> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .poll(machine, input, elapsed)
    }
}

/// Experimental: API may change before 1.0.
///
/// Single-use OTA session that streams firmware from a plain HTTP server,
/// verifies the SHA-256 digest, and writes to the inactive OTA partition.
///
/// # Example
///
/// ```rust,no_run
/// # use rustyfarian_esp_idf_network::ota::{OtaSession, OtaSessionConfig};
/// let mut session = OtaSession::new(OtaSessionConfig { timeout_secs: 60 }).unwrap();
/// ```
#[derive(Debug)]
pub struct OtaSession {
    config: OtaSessionConfig,
    deadline: Deadline,
    expected_chip: Option<ImageChip>,
}

impl OtaSession {
    /// Experimental: API may change before 1.0.
    ///
    /// Create a new OTA session.
    pub fn new(config: OtaSessionConfig) -> Result<Self, OtaError> {
        Ok(Self {
            config,
            deadline: Deadline::none(),
            expected_chip: None,
        })
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Limit the whole [`fetch_and_apply`](Self::fetch_and_apply) call to `total`.
    ///
    /// The deadline is **cooperative**: the clock starts on entry to
    /// `fetch_and_apply` and is checked at operation boundaries, namely after
    /// connect and headers, after partition preparation (the `begin()` erase),
    /// after every chunk read and every flash write, after SHA-256
    /// verification, and immediately before the call that selects the new boot
    /// slot.
    /// A failed check aborts the flash session and returns
    /// [`OtaError::DownloadTimeout`].
    ///
    /// Blocking operations cannot be interrupted; expiry is detected when they
    /// return.
    /// Finalization is the exception: once the permit is granted and
    /// finalization starts, its result is returned even if it finishes after
    /// the deadline.
    /// Each network wait is clipped to `min(per-read timeout
    /// (`OtaSessionConfig::timeout_secs`), remaining deadline)`, as on the
    /// esp-hal tier: the connect budget sets the connection's timeout when it
    /// is created and the read budget is applied before every read, in whole
    /// milliseconds rounded up (`Deadline::budget_millis`).
    /// Each read request is capped to the HTTP client's receive buffer and,
    /// once less than one per-read timeout is left, split by
    /// `juggler::ota::read_bounded` into a one-byte read with the whole budget
    /// and a follow-up of the rest with a 1 ms wait, sized to fit the time
    /// left; an idle follow-up returns the bytes read so far.
    /// A read is not one transport wait: `esp_http_client_read` waits again
    /// whenever a read returns fewer bytes than requested, so while more than
    /// one per-read timeout is left, a server that trickles bytes can keep one
    /// call going for up to one wait per requested byte (at most the receive
    /// buffer, 1024 waits of the per-read timeout).
    /// The worst-case overrun is therefore the DNS resolution inside connect
    /// (not covered by that timeout), that trickling read, or one flash
    /// operation (erase, write, finalization), whichever is longer.
    ///
    /// The deadline is expired when `elapsed >= total`.
    /// `Duration::ZERO` therefore fails with `DownloadTimeout` before any
    /// network or flash access.
    /// Not calling this method means no total limit; apart from the read-error
    /// mapping below, behaviour is unchanged from v0.5.0.
    ///
    /// An operation that fails reports its own error (`ChecksumMismatch`,
    /// `FlashWriteFailed`, `InsufficientSpace`, ...); `DownloadTimeout` from the
    /// deadline is reported only when the operation at that boundary succeeded
    /// but the deadline has elapsed.
    /// A single read that times out on its own (`ESP_ERR_HTTP_EAGAIN`) is
    /// `DownloadTimeout` regardless of the deadline; any other read error is
    /// `ServerUnreachable`.
    ///
    /// Invariant: a timeout never selects the new boot slot.
    /// Finalization requires an [`ActivationPermit`] that only a successful
    /// pre-activation deadline check can produce; if that check fails the flash
    /// session is aborted and the boot slot is unchanged.
    ///
    /// ```rust,no_run
    /// # use std::time::Duration;
    /// # use rustyfarian_esp_idf_network::ota::{OtaSession, OtaSessionConfig};
    /// let mut session = OtaSession::new(OtaSessionConfig { timeout_secs: 30 })
    ///     .unwrap()
    ///     .with_deadline(Duration::from_secs(300));
    /// ```
    #[must_use]
    pub fn with_deadline(mut self, total: Duration) -> Self {
        self.deadline = Deadline::after(total);
        self
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Require the image header to name `chip` before any flash is touched.
    ///
    /// The first 16 body bytes are read before the inactive partition is erased and checked by `juggler::ota::download::check_image_header`: a different chip id fails with [`OtaError::TargetMismatch`], a missing `0xE9` magic byte or an image shorter than the header with `DownloadFailed { status: 0 }`, both without erasing anything.
    /// On a match those bytes are written first, so the image and its SHA-256 are unchanged.
    /// `esp_ota_end` still validates the whole image.
    /// Not calling this method means no header check.
    /// The esp-hal tier has the same method on `EspHalOtaManager`.
    ///
    /// ```rust,no_run
    /// # use rustyfarian_esp_idf_network::ota::{ImageChip, OtaSession, OtaSessionConfig, TARGET_CHIP};
    /// let chip = ImageChip::from_target(TARGET_CHIP).expect("a chip the header check knows");
    /// let mut session = OtaSession::new(OtaSessionConfig { timeout_secs: 30 })
    ///     .unwrap()
    ///     .with_expected_chip(chip);
    /// ```
    #[must_use]
    pub fn with_expected_chip(mut self, chip: ImageChip) -> Self {
        self.expected_chip = Some(chip);
        self
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Fetch firmware from `url`, verify its SHA-256 against `expected_sha256`,
    /// write it to the inactive OTA slot, and set that slot as the boot partition.
    ///
    /// The server is contacted before any flash is touched: a connection or
    /// HTTP-status failure, a response without a `Content-Length` or with a
    /// `Transfer-Encoding` (`DownloadFailed { status: 0 }`), and an image larger
    /// than the inactive partition ([`OtaError::InsufficientSpace`]) all fail
    /// without erasing anything; the rule is [`check_content_length`], shared
    /// with the esp-hal tier.
    /// A body that ends before `Content-Length` bytes arrive fails with
    /// `DownloadFailed { status: 0 }`, as on the esp-hal tier.
    /// The order of the steps is `juggler::ota::download::DownloadMachine`,
    /// which the esp-hal tier runs as well.
    ///
    /// The operation is streaming: the full image is never held in RAM.
    /// Once the headers pass, only the sectors the image needs are erased and
    /// bytes are written to the inactive partition as they arrive — flash
    /// **is** modified during the download, even on the failure path.
    /// The connection stays open, unread, during that erase; a server whose
    /// send timeout is shorter than the erase drops it, which fails safely as
    /// `ServerUnreachable` with the boot slot unchanged.
    /// On verification failure (or any download / flash error), the OTA write
    /// session is aborted and the **boot slot is left unchanged**, so the
    /// device continues to boot the running image; the inactive partition is
    /// left in an undefined state and will be overwritten on the next OTA
    /// attempt. Power loss between mid-download and slot activation likewise
    /// leaves the boot slot unchanged.
    ///
    /// With [`with_deadline`](Self::with_deadline) set, the whole call is bounded
    /// by that total (see its documentation for the checkpoints and overrun
    /// limits).
    ///
    /// Only plain `http://` URLs are supported (see ADR 011).
    /// To cancel the download from another thread, use
    /// [`fetch_and_apply_cancellable`](Self::fetch_and_apply_cancellable).
    pub fn fetch_and_apply(
        &mut self,
        url: &str,
        expected_sha256: &[u8; 32],
    ) -> Result<(), OtaError> {
        self.fetch_and_apply_cancellable(url, expected_sha256, &OtaCancel::new())
    }

    /// Experimental: API may change before 1.0.
    ///
    /// [`fetch_and_apply`](Self::fetch_and_apply) that another thread can stop through `cancel`.
    ///
    /// Returns [`OtaError::Cancelled`] exactly when a [`OtaCancel::cancel`] was accepted; the boot slot is then unchanged.
    pub fn fetch_and_apply_cancellable(
        &mut self,
        url: &str,
        expected_sha256: &[u8; 32],
        cancel: &OtaCancel,
    ) -> Result<(), OtaError> {
        self.fetch_and_apply_with(expected_sha256, None, cancel, |timeout| {
            FirmwareDownloader::new(url).with_timeout(timeout).connect()
        })
    }

    /// The download loop behind every public fetch: `open` connects with the given timeout and returns the `200` response.
    ///
    /// With `expected_size`, the declared `Content-Length` must equal it (`DownloadMachine::with_expected_size`).
    /// Crate-private so that no public method gains a header parameter (ADR 027); the DDI transport passes an `open` that sends its `Authorization` header.
    pub(crate) fn fetch_and_apply_with<F>(
        &mut self,
        expected_sha256: &[u8; 32],
        expected_size: Option<u64>,
        cancel: &OtaCancel,
        mut open: F,
    ) -> Result<(), OtaError>
    where
        F: FnMut(Duration) -> Result<FirmwareResponse, OtaError>,
    {
        let start = Instant::now();
        let per_op = Duration::from_secs(self.config.timeout_secs);
        let mut machine = DownloadMachine::new(self.deadline, per_op, *expected_sha256);
        if let Some(size) = expected_size {
            machine = machine.with_expected_size(size);
        }
        if let Some(chip) = self.expected_chip {
            machine = machine.with_expected_chip(chip);
        }

        // Until `Prepare`, nothing is open that needs an abort.
        let mut connected: Option<(FirmwareResponse, FirmwareFlasher)> = None;
        let mut header = [0u8; IMAGE_HEADER_LEN];
        let mut input = DownloadInput::Begin;
        let (len, mut response, mut flasher) = loop {
            let action = cancel
                .poll(&mut machine, input, start.elapsed())
                .inspect_err(|e| log::error!("OTA download stopped: {}", e))?;
            input = match action {
                // The connection's timeout is `min(per-read timeout, remaining deadline)`.
                DownloadAction::Connect { budget } => {
                    match open(wait_for(budget)).and_then(size_slot) {
                        Ok((response, flasher, capacity)) => {
                            let headers = DownloadInput::Headers {
                                content_length: response.content_length(),
                                chunked: response.chunked(),
                                capacity,
                            };
                            connected = Some((response, flasher));
                            headers
                        }
                        Err(e) => DownloadInput::ConnectFailed(e),
                    }
                }
                // The image header, read before the slot is erased; the machine keeps it.
                DownloadAction::ReadHeader { max, budget } => match connected.as_mut() {
                    Some((response, _)) => {
                        let began = Instant::now();
                        match read_bounded(
                            &mut header,
                            max,
                            HTTP_RX_BUFFER_SIZE,
                            budget,
                            per_op,
                            || began.elapsed(),
                            |buf, wait| response.read(buf, wait_for(wait)),
                        ) {
                            Ok(n) => DownloadInput::header_read(&header[..n]),
                            Err(e) => DownloadInput::ReadFailed(e),
                        }
                    }
                    None => return Err(unexpected(&action)),
                },
                DownloadAction::Prepare { len } => match connected.take() {
                    Some((response, flasher)) => break (len, response, flasher),
                    None => return Err(unexpected(&action)),
                },
                DownloadAction::Done(result) => return result,
                other => return Err(unexpected(&other)),
            };
        };

        log::info!("Firmware size: {} bytes", len);
        let mut ota_writer = match flasher.begin(len as usize) {
            Ok(writer) => writer,
            Err(e) => {
                return match cancel.poll(
                    &mut machine,
                    DownloadInput::PrepareFailed(e),
                    start.elapsed(),
                ) {
                    Ok(DownloadAction::Done(result)) => result,
                    Ok(other) => Err(unexpected(&other)),
                    Err(e) => Err(e),
                }
            }
        };

        let mut verifier = StreamingVerifier::new();
        let mut buffer = [0u8; DOWNLOAD_BUFFER_SIZE];
        let mut input = DownloadInput::Prepared;
        loop {
            let action = match cancel.poll(&mut machine, input, start.elapsed()) {
                Ok(action) => action,
                Err(e) => {
                    log::error!(
                        "OTA download stopped: {} — aborting, boot slot unchanged",
                        e
                    );
                    return Err(abort_with(ota_writer, e));
                }
            };
            input = match action {
                DownloadAction::Read { max, budget } => {
                    let began = Instant::now();
                    match read_bounded(
                        &mut buffer,
                        max,
                        HTTP_RX_BUFFER_SIZE,
                        budget,
                        per_op,
                        || began.elapsed(),
                        |buf, wait| response.read(buf, wait_for(wait)),
                    ) {
                        Ok(n) => DownloadInput::Read(n),
                        Err(e) => DownloadInput::ReadFailed(e),
                    }
                }
                DownloadAction::WriteHeader(bytes) => {
                    verifier.update(&bytes);
                    match ota_writer.write_chunk(&bytes) {
                        Ok(()) => DownloadInput::Written,
                        Err(e) => DownloadInput::WriteFailed(e),
                    }
                }
                DownloadAction::Write { n } => {
                    verifier.update(&buffer[..n]);
                    match ota_writer.write_chunk(&buffer[..n]) {
                        Ok(()) => DownloadInput::Written,
                        Err(e) => DownloadInput::WriteFailed(e),
                    }
                }
                DownloadAction::Finalize => {
                    log::info!("Download complete, verifying {} bytes", len);
                    DownloadInput::Digest(
                        core::mem::replace(&mut verifier, StreamingVerifier::new()).finalize(),
                    )
                }
                DownloadAction::Abort(e) => {
                    log::error!("OTA download failed: {} — aborting, boot slot unchanged", e);
                    let _ = ota_writer.abort();
                    return finish(cancel, &mut machine, DownloadInput::Aborted, start);
                }
                DownloadAction::Activate(permit) => {
                    log::info!("SHA-256 verified — completing OTA partition swap");
                    let result = ota_writer.complete(permit);
                    return finish(
                        cancel,
                        &mut machine,
                        DownloadInput::Activated(result),
                        start,
                    );
                }
                other => return Err(abort_with(ota_writer, unexpected(&other))),
            };
        }
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Mark the running slot valid, cancelling the bootloader's automatic rollback.
    ///
    /// Call this after the new firmware has passed its health check.
    pub fn mark_valid(&mut self) -> Result<(), OtaError> {
        let mut ota = EspOta::new().map_err(|e| {
            log::error!("Failed to acquire EspOta handle for mark_valid: {:?}", e);
            OtaError::FlashWriteFailed
        })?;

        ota.mark_running_slot_valid().map_err(|e| {
            log::error!("mark_running_slot_valid failed: {:?}", e);
            OtaError::FlashWriteFailed
        })
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Whether the previous update was rolled back: ESP-IDF knows a last invalid
    /// partition (`esp_ota_get_last_invalid_partition`, an `Aborted` or `Invalid` otadata entry).
    ///
    /// A firmware that fetches a fixed target version calls this before fetching, so a rolled-back
    /// image is not downloaded again on every boot.
    /// The esp-hal tier has the same method on `EspHalOtaManager`.
    pub fn previous_update_rolled_back(&mut self) -> Result<bool, OtaError> {
        let ota = EspOta::new().map_err(|e| {
            log::error!(
                "Failed to acquire EspOta handle for previous_update_rolled_back: {:?}",
                e
            );
            OtaError::FlashWriteFailed
        })?;
        ota.get_last_invalid_slot()
            .map(|slot| slot.is_some())
            .map_err(|e| {
                log::error!("get_last_invalid_slot failed: {:?}", e);
                OtaError::FlashWriteFailed
            })
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Revert to the previous OTA slot and reboot.
    ///
    /// This function does not return on success — the device reboots into the
    /// previous firmware.
    /// Returns `Err` only if rollback is not possible (e.g. no valid previous slot).
    pub fn rollback(&mut self) -> Result<(), OtaError> {
        let mut ota = EspOta::new().map_err(|e| {
            log::error!("Failed to acquire EspOta handle for rollback: {:?}", e);
            OtaError::FlashWriteFailed
        })?;

        // `mark_running_slot_invalid_and_reboot` never returns on success (device reboots).
        // It only returns on failure — surface as `FlashWriteFailed` so callers
        // can distinguish "no rollback target" (would surface as
        // `PartitionNotFound` from `EspOta::new` above) from "rollback path
        // failed for some other reason".
        let err = ota.mark_running_slot_invalid_and_reboot();
        log::error!("Rollback failed (no valid previous slot?): {:?}", err);
        Err(OtaError::FlashWriteFailed)
    }
}

/// Abort the flash session and hand back the error that ended it.
///
/// The abort result is deliberately ignored: the original error is the one the
/// caller needs, and `OtaWriter::abort` already logs its own failure.
fn abort_with(writer: OtaWriter<'_>, err: OtaError) -> OtaError {
    let _ = writer.abort();
    err
}

/// Size the inactive partition for a response whose headers have been read.
///
/// The partition is looked up only after the server answered, so nothing on
/// flash is touched for an unreachable server.
fn size_slot(
    response: FirmwareResponse,
) -> Result<(FirmwareResponse, FirmwareFlasher, u64), OtaError> {
    let flasher = FirmwareFlasher::new()?;
    let capacity = flasher.update_partition_capacity()? as u64;
    Ok((response, flasher, capacity))
}

/// The HTTP client wait for a download budget (whole milliseconds, at least 1).
fn wait_for(budget: Duration) -> Duration {
    Duration::from_millis(u64::from(Deadline::budget_millis(budget)))
}

/// Feed the last outcome and return the machine's final result.
fn finish(
    cancel: &OtaCancel,
    machine: &mut DownloadMachine,
    input: DownloadInput,
    start: Instant,
) -> Result<(), OtaError> {
    match cancel.poll(machine, input, start.elapsed())? {
        DownloadAction::Done(result) => result,
        other => Err(unexpected(&other)),
    }
}

/// An action the loop's current phase cannot perform: an internal error.
fn unexpected(action: &DownloadAction) -> OtaError {
    log::error!("OTA download: unexpected step {:?}", action);
    OtaError::DownloadFailed { status: 0 }
}
