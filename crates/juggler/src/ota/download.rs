//! The firmware download order as an action machine (ADR 023 rung 3).
//!
//! Both tiers run `fetch_and_apply` through [`DownloadMachine`]: blocking threads on ESP-IDF, async on esp-hal, with transport and flash I/O outside `embedded-hal`.
//! The machine owns the order of the steps (deadline, connect and headers, the Content-Length admission, the image-header chip check when one is expected, preparing the slot, the read and write stream with a deadline check after each, the SHA-256 compare, the final deadline and the activation permit); the tier loop only performs the returned [`DownloadAction`] and feeds the outcome back as a [`DownloadInput`].
//! A cancel enters through [`DownloadMachine::cancel`], from another thread or task through a [`CancelGate`].
//!
//! Pure and `no_std`: time enters as the elapsed time since the call started, and the machine never reads a clock.
//!
//! ```
//! use core::time::Duration;
//! use juggler::ota::download::{DownloadAction, DownloadInput, DownloadMachine};
//! use juggler::ota::Deadline;
//!
//! let mut m = DownloadMachine::new(Deadline::none(), Duration::from_secs(30), [0; 32]);
//! assert!(matches!(m.poll(DownloadInput::Begin, Duration::ZERO), DownloadAction::Connect { .. }));
//! let headers = DownloadInput::Headers { content_length: None, chunked: false, capacity: 4096 };
//! assert!(matches!(m.poll(headers, Duration::ZERO), DownloadAction::Done(Err(_))));
//! ```

use core::time::Duration;

use super::deadline::{ActivationPermit, Deadline};
use super::error::OtaError;

/// Experimental: API may change before 1.0.
///
/// The Content-Length admission rule both tiers apply before any flash is touched.
///
/// - A missing `Content-Length`, or any `Transfer-Encoding` (`chunked`), is a protocol-shape rejection: `DownloadFailed { status: 0 }`.
/// - A zero length is rejected the same way: there is no image, and `esp_ota_begin` would read a size of `0` as "erase the whole partition".
/// - A length above `capacity` (the inactive OTA partition), or above `u32::MAX`, is `InsufficientSpace`.
///
/// Returns the image length to stream.
///
/// ```
/// use juggler::ota::{check_content_length, OtaError};
///
/// assert_eq!(check_content_length(Some(1024), false, 4096), Ok(1024));
/// assert_eq!(check_content_length(None, false, 4096), Err(OtaError::DownloadFailed { status: 0 }));
/// assert_eq!(check_content_length(Some(4097), false, 4096), Err(OtaError::InsufficientSpace));
/// ```
pub fn check_content_length(
    content_length: Option<u64>,
    chunked: bool,
    capacity: u64,
) -> Result<u32, OtaError> {
    const SHAPE_REJECTED: OtaError = OtaError::DownloadFailed { status: 0 };
    if chunked {
        return Err(SHAPE_REJECTED);
    }
    match content_length {
        None | Some(0) => Err(SHAPE_REJECTED),
        Some(len) if len > capacity => Err(OtaError::InsufficientSpace),
        Some(len) => u32::try_from(len).map_err(|_| OtaError::InsufficientSpace),
    }
}

/// Experimental: API may change before 1.0.
///
/// The bytes of an ESP application image header the early chip check reads before any flash is touched.
///
/// The header is `esp_image_header_t` (ESP-IDF `components/bootloader_support/include/esp_app_format.h`): the magic byte at offset 0 and the `chip_id` at offset 12 are inside the first 16 bytes.
pub const IMAGE_HEADER_LEN: usize = 16;

/// The first byte of every ESP application image (`ESP_IMAGE_HEADER_MAGIC`).
const IMAGE_MAGIC: u8 = 0xE9;

/// Offset of the little-endian `u16` `chip_id` in `esp_image_header_t`: after the 8-byte common header, `wp_pin` and the three `spi_pin_drv` bytes.
const CHIP_ID_OFFSET: usize = 12;

/// Experimental: API may change before 1.0.
///
/// A chip as an ESP application image header names it (`esp_chip_id_t` in ESP-IDF v6.1 `esp_app_format.h`).
///
/// ```
/// use juggler::ota::download::ImageChip;
///
/// assert_eq!(ImageChip::from_target("esp32c3"), Some(ImageChip::Esp32C3));
/// assert_eq!(ImageChip::Esp32C3.id(), 5);
/// assert_eq!(ImageChip::from_target("ESP32C3"), None);
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageChip {
    /// ESP32, chip id 0.
    Esp32,
    /// ESP32-S2, chip id 2.
    Esp32S2,
    /// ESP32-C3, chip id 5.
    Esp32C3,
    /// ESP32-S3, chip id 9.
    Esp32S3,
    /// ESP32-C2, chip id 12.
    Esp32C2,
    /// ESP32-C6, chip id 13.
    Esp32C6,
    /// ESP32-H2, chip id 16.
    Esp32H2,
    /// ESP32-P4, chip id 18.
    Esp32P4,
    /// ESP32-C5, chip id 23.
    Esp32C5,
}

impl ImageChip {
    const ALL: [ImageChip; 9] = [
        ImageChip::Esp32,
        ImageChip::Esp32S2,
        ImageChip::Esp32C3,
        ImageChip::Esp32S3,
        ImageChip::Esp32C2,
        ImageChip::Esp32C6,
        ImageChip::Esp32H2,
        ImageChip::Esp32P4,
        ImageChip::Esp32C5,
    ];

    /// The `chip_id` an image built for this chip carries in its header.
    pub const fn id(self) -> u16 {
        match self {
            ImageChip::Esp32 => 0x0000,
            ImageChip::Esp32S2 => 0x0002,
            ImageChip::Esp32C3 => 0x0005,
            ImageChip::Esp32S3 => 0x0009,
            ImageChip::Esp32C2 => 0x000C,
            ImageChip::Esp32C6 => 0x000D,
            ImageChip::Esp32H2 => 0x0010,
            ImageChip::Esp32P4 => 0x0012,
            ImageChip::Esp32C5 => 0x0017,
        }
    }

    /// The ESP-IDF target name (`"esp32"`, `"esp32c3"`, ...), the spelling of the IDF tier's `TARGET_CHIP`.
    pub const fn target(self) -> &'static str {
        match self {
            ImageChip::Esp32 => "esp32",
            ImageChip::Esp32S2 => "esp32s2",
            ImageChip::Esp32C3 => "esp32c3",
            ImageChip::Esp32S3 => "esp32s3",
            ImageChip::Esp32C2 => "esp32c2",
            ImageChip::Esp32C6 => "esp32c6",
            ImageChip::Esp32H2 => "esp32h2",
            ImageChip::Esp32P4 => "esp32p4",
            ImageChip::Esp32C5 => "esp32c5",
        }
    }

    /// The chip for an ESP-IDF target name by exact match, `None` for any other text.
    pub fn from_target(name: &str) -> Option<ImageChip> {
        Self::ALL.into_iter().find(|chip| chip.target() == name)
    }
}

/// Experimental: API may change before 1.0.
///
/// Checks the first [`IMAGE_HEADER_LEN`] bytes of an image against the chip it must run on.
///
/// A first byte other than `0xE9` is not an ESP image: `DownloadFailed { status: 0 }`, the protocol-shape rejection.
/// A `chip_id` other than `expected.id()`, an unknown id included, is [`OtaError::TargetMismatch`].
///
/// ```
/// use juggler::ota::download::{check_image_header, ImageChip};
/// use juggler::ota::OtaError;
///
/// let mut header = [0u8; 16];
/// header[0] = 0xE9;
/// header[12] = 5;
/// assert_eq!(check_image_header(&header, ImageChip::Esp32C3), Ok(()));
/// assert_eq!(check_image_header(&header, ImageChip::Esp32S3), Err(OtaError::TargetMismatch));
/// ```
pub fn check_image_header(
    header: &[u8; IMAGE_HEADER_LEN],
    expected: ImageChip,
) -> Result<(), OtaError> {
    if header[0] != IMAGE_MAGIC {
        return Err(OtaError::DownloadFailed { status: 0 });
    }
    let chip_id = u16::from_le_bytes([header[CHIP_ID_OFFSET], header[CHIP_ID_OFFSET + 1]]);
    if chip_id == expected.id() {
        Ok(())
    } else {
        Err(OtaError::TargetMismatch)
    }
}

/// Experimental: API may change before 1.0.
///
/// The outcome of the previous [`DownloadAction`], fed to [`DownloadMachine::poll`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DownloadInput {
    /// The first call.
    Begin,
    /// [`DownloadAction::Connect`] succeeded: the `200` response headers and the inactive partition's size.
    Headers {
        /// The declared `Content-Length`, `None` when absent.
        content_length: Option<u64>,
        /// The response carries a `Transfer-Encoding` header.
        chunked: bool,
        /// Size in bytes of the partition the image would be written to.
        capacity: u64,
    },
    /// [`DownloadAction::Connect`] failed: the connection, the response, or the partition lookup that sizes the image.
    ConnectFailed(OtaError),
    /// [`DownloadAction::Prepare`] succeeded: the write session is open.
    Prepared,
    /// [`DownloadAction::Prepare`] failed; no write session exists.
    PrepareFailed(OtaError),
    /// [`DownloadAction::ReadHeader`] returned `n` bytes, the first of them in `bytes`; `0` is the end of the stream.
    ///
    /// Build it with [`DownloadInput::header_read`].
    HeaderRead {
        /// The bytes read, from the start; only the first `n` (at most [`IMAGE_HEADER_LEN`]) are meaningful.
        bytes: [u8; IMAGE_HEADER_LEN],
        /// How many bytes the read returned.
        n: usize,
    },
    /// [`DownloadAction::Read`] returned `n` bytes; `0` is the end of the stream.
    Read(usize),
    /// [`DownloadAction::ReadHeader`] or [`DownloadAction::Read`] failed.
    ReadFailed(OtaError),
    /// [`DownloadAction::WriteHeader`] or [`DownloadAction::Write`] succeeded.
    Written,
    /// [`DownloadAction::WriteHeader`] or [`DownloadAction::Write`] failed, or the tail flush of [`DownloadAction::Finalize`] did.
    WriteFailed(OtaError),
    /// [`DownloadAction::Finalize`] computed this SHA-256 of the streamed bytes.
    Digest([u8; 32]),
    /// [`DownloadAction::Abort`] was performed.
    Aborted,
    /// [`DownloadAction::Activate`] returned.
    Activated(Result<(), OtaError>),
}

impl DownloadInput {
    /// The answer to [`DownloadAction::ReadHeader`] for the bytes `read` returned.
    ///
    /// A read longer than [`IMAGE_HEADER_LEN`] keeps its length, so the machine refuses it as a read past the requested bytes.
    pub fn header_read(read: &[u8]) -> Self {
        let mut bytes = [0; IMAGE_HEADER_LEN];
        let kept = read.len().min(IMAGE_HEADER_LEN);
        bytes[..kept].copy_from_slice(&read[..kept]);
        DownloadInput::HeaderRead {
            bytes,
            n: read.len(),
        }
    }
}

/// Experimental: API may change before 1.0.
///
/// What the tier loop must do next.
#[derive(Debug)]
pub enum DownloadAction {
    /// Connect, send the GET and read the response headers within `budget`, look up the inactive partition's size, and answer with [`DownloadInput::Headers`] or [`DownloadInput::ConnectFailed`].
    Connect {
        /// `min(per-operation timeout, remaining deadline)`.
        budget: Duration,
    },
    /// Read at most `max` bytes of the image header within `budget`, before the slot is prepared, and answer with [`DownloadInput::HeaderRead`] or [`DownloadInput::ReadFailed`].
    ///
    /// Only a machine with an expected chip asks for it.
    ReadHeader {
        /// Header bytes still missing; never read past them.
        max: u32,
        /// `min(per-operation timeout, remaining deadline)`.
        budget: Duration,
    },
    /// Open the write session for an image of exactly `len` bytes (erasing what it needs) and answer with [`DownloadInput::Prepared`] or [`DownloadInput::PrepareFailed`].
    Prepare {
        /// The admitted image length.
        len: u32,
    },
    /// Feed these header bytes, the first bytes of the image, to the SHA-256 and write them to the start of the slot, then answer with [`DownloadInput::Written`] or [`DownloadInput::WriteFailed`].
    ///
    /// Comes right after [`DownloadInput::Prepared`] when the header was read before the slot was prepared.
    WriteHeader([u8; IMAGE_HEADER_LEN]),
    /// Read at most `max` body bytes within `budget` and answer with [`DownloadInput::Read`] or [`DownloadInput::ReadFailed`].
    Read {
        /// Bytes still expected; never read past them.
        max: u32,
        /// `min(per-operation timeout, remaining deadline)`.
        budget: Duration,
    },
    /// Feed the `n` bytes just read to the SHA-256 and write them to the slot, then answer with [`DownloadInput::Written`] or [`DownloadInput::WriteFailed`].
    Write {
        /// Bytes of the last read.
        n: usize,
    },
    /// Flush any buffered tail to the slot, compute the SHA-256 and answer with [`DownloadInput::Digest`] (or [`DownloadInput::WriteFailed`] when the flush failed).
    Finalize,
    /// Abort the write session (the boot slot stays unchanged), then answer with [`DownloadInput::Aborted`].
    Abort(OtaError),
    /// Select the new boot slot with this permit and answer with [`DownloadInput::Activated`].
    Activate(ActivationPermit),
    /// The download is over; return this result.
    Done(Result<(), OtaError>),
    /// The input did not answer the pending action; nothing changed (ADR 023 decision 5).
    ///
    /// A tier loop never produces this on its own, so it treats it as an internal error.
    Ignored,
}

/// Experimental: API may change before 1.0.
///
/// Whether a cancel request can still take effect, the answer of [`DownloadMachine::cancel`] and [`CancelGate::request`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelRequest {
    /// The download stops at its next step boundary and ends with [`OtaError::Cancelled`]; the boot slot stays unchanged.
    Accepted,
    /// The download is activating the new slot or is already over; the request has no effect.
    TooLate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Idle,
    Connecting,
    /// Reading the image header; the chip it must name.
    ReadingHeader(ImageChip),
    Preparing,
    Reading,
    Writing,
    Finalizing,
    Aborting(OtaError),
    Activating,
    Done,
}

/// Experimental: API may change before 1.0.
///
/// The firmware download order (see the [module docs](self)).
///
/// The rules:
/// - the deadline is checked on entry, after connect, after preparation, after every read and every write, and by the activation permit;
/// - nothing is prepared before the headers pass [`check_content_length`];
/// - with an [expected chip](Self::with_expected_chip), nothing is prepared before the first [`IMAGE_HEADER_LEN`] bytes pass [`check_image_header`]; those bytes are written first after preparation ([`DownloadAction::WriteHeader`]), so the slot and the SHA-256 see the image unchanged;
/// - a stream that ends before, or runs past, the admitted length is `DownloadFailed { status: 0 }`;
/// - every error after a successful preparation yields [`DownloadAction::Abort`] first, then [`DownloadAction::Done`] with that error;
/// - an operation's own error wins over an expired deadline at the same boundary;
/// - an accepted [`cancel`](Self::cancel) is honoured by the next answer to the pending action: before preparation (the header reads included) it ends the download, after it the write session is aborted first; it wins over any operation error and the deadline, so the result is [`OtaError::Cancelled`] exactly when the cancel was accepted;
/// - an input that does not answer the pending action yields [`DownloadAction::Ignored`] and changes nothing.
#[derive(Debug, Clone, Copy)]
pub struct DownloadMachine {
    deadline: Deadline,
    per_op: Duration,
    expected: [u8; 32],
    size: Option<u64>,
    chip: Option<ImageChip>,
    header: [u8; IMAGE_HEADER_LEN],
    phase: Phase,
    len: u32,
    received: u32,
    steps: usize,
    cancelled: bool,
}

impl DownloadMachine {
    /// A machine for one download bounded by `deadline`, with `per_op` as the wait of one network operation, expecting the SHA-256 `expected`.
    pub const fn new(deadline: Deadline, per_op: Duration, expected: [u8; 32]) -> Self {
        Self {
            deadline,
            per_op,
            expected,
            size: None,
            chip: None,
            header: [0; IMAGE_HEADER_LEN],
            phase: Phase::Idle,
            len: 0,
            received: 0,
            steps: 0,
            cancelled: false,
        }
    }

    /// Also requires the declared `Content-Length` to equal `size`, as a DDI artifact's `size` does.
    ///
    /// A different length ends the download before any flash is touched with [`OtaError::SizeMismatch`]; the length checks of [`check_content_length`] come first.
    /// The machine already requires the bytes received to equal the `Content-Length`, so they equal `size` too.
    #[must_use]
    pub const fn with_expected_size(mut self, size: u64) -> Self {
        self.size = Some(size);
        self
    }

    /// Also requires the image header to name `chip`, checked before any flash is touched.
    ///
    /// After the headers are admitted the machine asks for the first [`IMAGE_HEADER_LEN`] body bytes ([`DownloadAction::ReadHeader`]) and checks them with [`check_image_header`]: a wrong chip ends the download with [`OtaError::TargetMismatch`], a missing magic byte or an image shorter than the header with `DownloadFailed { status: 0 }`, both without a [`DownloadAction::Prepare`].
    /// On a match the slot is prepared and the header bytes are written first ([`DownloadAction::WriteHeader`]).
    /// `esp_ota_end` and the bootloader still validate the whole image.
    #[must_use]
    pub const fn with_expected_chip(mut self, chip: ImageChip) -> Self {
        self.chip = Some(chip);
        self
    }

    /// Requests that the download stop, as an operator cancel does.
    ///
    /// Until the machine has returned [`DownloadAction::Activate`] or [`DownloadAction::Done`] the request is [`CancelRequest::Accepted`]: the next answer to the pending action (before preparation, after a read or a write, or right before the activation permit) yields [`DownloadAction::Abort`] or [`DownloadAction::Done`] with [`OtaError::Cancelled`].
    /// After that it is [`CancelRequest::TooLate`] and changes nothing.
    /// A second accepted request changes nothing either.
    pub fn cancel(&mut self) -> CancelRequest {
        match self.phase {
            Phase::Activating | Phase::Done => CancelRequest::TooLate,
            _ => {
                self.cancelled = true;
                CancelRequest::Accepted
            }
        }
    }

    /// The most [`poll`](Self::poll) calls a download of `len` bytes can take when every successful read returns at least `min_read` bytes: `2 * ceil(len / min_read) + 8`.
    ///
    /// Transports may return short reads, so the tiers pass `min_read = 1`.
    /// The header reads of an [expected chip](Self::with_expected_chip) replace the first body reads, so the bound holds for them too.
    pub const fn max_steps(len: u32, min_read: usize) -> usize {
        let min_read = if min_read == 0 { 1 } else { min_read };
        2 * (len as usize).div_ceil(min_read) + 8
    }

    /// [`poll`](Self::poll) under the step bound of ADR 023 decision 6.
    ///
    /// The bound is [`max_steps`](Self::max_steps) of the admitted length (zero before admission) with `min_read = 1`.
    ///
    /// # Errors
    ///
    /// `DownloadTimeout` once the bound is exceeded, `DownloadFailed { status: 0 }` for [`DownloadAction::Ignored`].
    /// After a successful [`DownloadAction::Prepare`] the tier must abort its write session before returning either.
    pub fn poll_bounded(
        &mut self,
        input: DownloadInput,
        elapsed: Duration,
    ) -> Result<DownloadAction, OtaError> {
        self.steps += 1;
        if self.steps > Self::max_steps(self.len, 1) {
            return Err(OtaError::DownloadTimeout);
        }
        match self.poll(input, elapsed) {
            DownloadAction::Ignored => Err(OtaError::DownloadFailed { status: 0 }),
            action => Ok(action),
        }
    }

    /// Advances the download; `elapsed` is the time since the download started.
    pub fn poll(&mut self, input: DownloadInput, elapsed: Duration) -> DownloadAction {
        use DownloadAction as A;
        use DownloadInput as I;
        if self.cancelled {
            if let Some(action) = self.honour_cancel(input) {
                return action;
            }
        }
        match (self.phase, input) {
            (Phase::Idle, I::Begin) => match self.deadline.check(elapsed) {
                Err(e) => self.done(Err(e)),
                Ok(()) => {
                    self.phase = Phase::Connecting;
                    A::Connect {
                        budget: self.budget(elapsed),
                    }
                }
            },
            (
                Phase::Connecting,
                I::Headers {
                    content_length,
                    chunked,
                    capacity,
                },
            ) => {
                let admitted = self
                    .deadline
                    .check(elapsed)
                    .and_then(|()| check_content_length(content_length, chunked, capacity))
                    .and_then(|len| match self.size {
                        Some(size) if size != u64::from(len) => Err(OtaError::SizeMismatch),
                        _ => Ok(len),
                    })
                    .and_then(|len| {
                        if self.chip.is_some() && (len as usize) < IMAGE_HEADER_LEN {
                            Err(OtaError::DownloadFailed { status: 0 })
                        } else {
                            Ok(len)
                        }
                    });
                match admitted {
                    Err(e) => self.done(Err(e)),
                    Ok(len) => {
                        self.len = len;
                        if let Some(chip) = self.chip {
                            self.read_header(chip, elapsed)
                        } else {
                            self.phase = Phase::Preparing;
                            A::Prepare { len }
                        }
                    }
                }
            }
            (Phase::ReadingHeader(chip), I::HeaderRead { bytes, n }) => {
                let have = self.received as usize;
                if n == 0 || n > IMAGE_HEADER_LEN - have {
                    return self.done(Err(OtaError::DownloadFailed { status: 0 }));
                }
                self.header[have..have + n].copy_from_slice(&bytes[..n]);
                self.received += n as u32;
                if have + n < IMAGE_HEADER_LEN {
                    return match self.deadline.check(elapsed) {
                        Err(e) => self.done(Err(e)),
                        Ok(()) => self.read_header(chip, elapsed),
                    };
                }
                match check_image_header(&self.header, chip)
                    .and_then(|()| self.deadline.check(elapsed))
                {
                    Err(e) => self.done(Err(e)),
                    Ok(()) => {
                        self.phase = Phase::Preparing;
                        A::Prepare { len: self.len }
                    }
                }
            }
            (Phase::Connecting, I::ConnectFailed(e))
            | (Phase::ReadingHeader(_), I::ReadFailed(e))
            | (Phase::Preparing, I::PrepareFailed(e)) => self.done(Err(e)),
            (Phase::Preparing, I::Prepared) => match self.deadline.check(elapsed) {
                Err(e) => self.abort(e),
                Ok(()) if self.chip.is_some() => {
                    self.phase = Phase::Writing;
                    A::WriteHeader(self.header)
                }
                Ok(()) => self.read(elapsed),
            },
            (Phase::Reading, I::Read(n)) => {
                let remaining = (self.len - self.received) as usize;
                if n == 0 || n > remaining {
                    return self.abort(OtaError::DownloadFailed { status: 0 });
                }
                self.received += n as u32;
                match self.deadline.check(elapsed) {
                    Err(e) => self.abort(e),
                    Ok(()) => {
                        self.phase = Phase::Writing;
                        A::Write { n }
                    }
                }
            }
            (Phase::Reading, I::ReadFailed(e))
            | (Phase::Writing | Phase::Finalizing, I::WriteFailed(e)) => self.abort(e),
            (Phase::Writing, I::Written) => match self.deadline.check(elapsed) {
                Err(e) => self.abort(e),
                Ok(()) if self.received == self.len => {
                    self.phase = Phase::Finalizing;
                    A::Finalize
                }
                Ok(()) => self.read(elapsed),
            },
            (Phase::Finalizing, I::Digest(digest)) => {
                if digest != self.expected {
                    return self.abort(OtaError::ChecksumMismatch);
                }
                match self.deadline.permit_activation(elapsed) {
                    Err(e) => self.abort(e),
                    Ok(permit) => {
                        self.phase = Phase::Activating;
                        A::Activate(permit)
                    }
                }
            }
            (Phase::Aborting(e), I::Aborted) => self.done(Err(e)),
            (Phase::Activating, I::Activated(result)) => self.done(result),
            _ => A::Ignored,
        }
    }

    /// The step an accepted cancel takes when `input` answers the pending action, `None` when it does not.
    fn honour_cancel(&mut self, input: DownloadInput) -> Option<DownloadAction> {
        use DownloadInput as I;
        const CANCELLED: OtaError = OtaError::Cancelled;
        match (self.phase, input) {
            (Phase::Idle, I::Begin)
            | (Phase::Connecting, I::Headers { .. } | I::ConnectFailed(_))
            | (Phase::ReadingHeader(_), I::HeaderRead { .. } | I::ReadFailed(_))
            | (Phase::Preparing, I::PrepareFailed(_))
            | (Phase::Aborting(_), I::Aborted) => Some(self.done(Err(CANCELLED))),
            (Phase::Preparing, I::Prepared)
            | (Phase::Reading, I::Read(_) | I::ReadFailed(_))
            | (Phase::Writing, I::Written | I::WriteFailed(_))
            | (Phase::Finalizing, I::Digest(_) | I::WriteFailed(_)) => Some(self.abort(CANCELLED)),
            _ => None,
        }
    }

    fn budget(&self, elapsed: Duration) -> Duration {
        self.deadline.clip(self.per_op, elapsed)
    }

    fn read_header(&mut self, chip: ImageChip, elapsed: Duration) -> DownloadAction {
        self.phase = Phase::ReadingHeader(chip);
        DownloadAction::ReadHeader {
            max: IMAGE_HEADER_LEN as u32 - self.received,
            budget: self.budget(elapsed),
        }
    }

    fn read(&mut self, elapsed: Duration) -> DownloadAction {
        self.phase = Phase::Reading;
        DownloadAction::Read {
            max: self.len - self.received,
            budget: self.budget(elapsed),
        }
    }

    fn abort(&mut self, e: OtaError) -> DownloadAction {
        self.phase = Phase::Aborting(e);
        DownloadAction::Abort(e)
    }

    fn done(&mut self, result: Result<(), OtaError>) -> DownloadAction {
        self.phase = Phase::Done;
        DownloadAction::Done(result)
    }
}

/// Experimental: API may change before 1.0.
///
/// What a cancel requester and a download loop on another thread or task share: one per download.
///
/// The tier keeps it behind a lock, routes every [`DownloadMachine`] step through [`poll`](Self::poll) and every cancel through [`request`](Self::request), both under that lock.
/// A request then either reaches the machine before it hands out the activation permit (and is [`CancelRequest::Accepted`]) or comes after it (and is [`CancelRequest::TooLate`]); no request is accepted and then ignored.
/// Pure and `no_std`: the lock is the tier's (a `Mutex` on ESP-IDF, a critical section on esp-hal).
///
/// ```
/// use core::time::Duration;
/// use juggler::ota::download::{CancelGate, CancelRequest, DownloadAction, DownloadInput, DownloadMachine};
/// use juggler::ota::{Deadline, OtaError};
///
/// let mut gate = CancelGate::new();
/// let mut m = DownloadMachine::new(Deadline::none(), Duration::from_secs(30), [0; 32]);
/// assert_eq!(gate.request(), CancelRequest::Accepted);
/// let action = gate.poll(&mut m, DownloadInput::Begin, Duration::ZERO);
/// assert!(matches!(action, Ok(DownloadAction::Done(Err(OtaError::Cancelled)))));
/// assert_eq!(gate.request(), CancelRequest::TooLate);
/// ```
#[derive(Debug, Clone, Copy, Default)]
pub struct CancelGate {
    requested: bool,
    closed: bool,
    retrying: bool,
}

impl CancelGate {
    /// A gate with no request, for a download that has not started.
    pub const fn new() -> Self {
        Self {
            requested: false,
            closed: false,
            retrying: false,
        }
    }

    /// A gate for a download that is tried again after a failed attempt: one gate spans every attempt.
    ///
    /// A step that ends an attempt with an error ([`DownloadAction::Done`] with `Err`, or a loop error) leaves the gate open, so a cancel that arrives between a failed attempt and the decision to retry stays accepted.
    /// Only the activation permit and a successful end close it; the caller closes it for good with [`close`](Self::close) when it gives up.
    ///
    /// ```
    /// use core::time::Duration;
    /// use juggler::ota::download::{CancelGate, CancelRequest, DownloadAction, DownloadInput, DownloadMachine};
    /// use juggler::ota::{Deadline, OtaError};
    ///
    /// let mut gate = CancelGate::for_retries();
    /// let mut m = DownloadMachine::new(Deadline::none(), Duration::from_secs(30), [0; 32]);
    /// gate.poll(&mut m, DownloadInput::Begin, Duration::ZERO).unwrap();
    /// let failed = gate.poll(&mut m, DownloadInput::ConnectFailed(OtaError::ServerUnreachable), Duration::ZERO);
    /// assert!(matches!(failed, Ok(DownloadAction::Done(Err(OtaError::ServerUnreachable)))));
    /// assert_eq!(gate.request(), CancelRequest::Accepted);
    /// assert!(gate.close());
    /// assert_eq!(gate.request(), CancelRequest::TooLate);
    /// ```
    pub const fn for_retries() -> Self {
        Self {
            requested: false,
            closed: false,
            retrying: true,
        }
    }

    /// Whether a cancel was accepted: a download that waits between two attempts polls this to end at once.
    pub const fn is_requested(&self) -> bool {
        self.requested
    }

    /// Closes the gate for good, so every later request is too late, and returns whether a cancel had been accepted before.
    ///
    /// The caller gives up on the download with `Cancelled` when this returns `true`; the answer and the close are one step under the caller's lock.
    pub fn close(&mut self) -> bool {
        self.closed = true;
        self.requested
    }

    /// A cancel request: [`CancelRequest::Accepted`] until the download has been handed the activation permit or has ended, [`CancelRequest::TooLate`] after.
    ///
    /// A request before the download starts is accepted, and the download then ends at its first step.
    pub fn request(&mut self) -> CancelRequest {
        if self.closed {
            CancelRequest::TooLate
        } else {
            self.requested = true;
            CancelRequest::Accepted
        }
    }

    /// One [`DownloadMachine::poll_bounded`] step that first hands a requested cancel to the machine.
    ///
    /// Closes the gate when the step returns [`DownloadAction::Activate`], [`DownloadAction::Done`] or an error, after which every request is too late; a gate made by [`for_retries`](Self::for_retries) stays open after a failed attempt.
    ///
    /// # Errors
    ///
    /// The errors of [`DownloadMachine::poll_bounded`].
    pub fn poll(
        &mut self,
        machine: &mut DownloadMachine,
        input: DownloadInput,
        elapsed: Duration,
    ) -> Result<DownloadAction, OtaError> {
        if self.requested {
            let _ = machine.cancel();
        }
        let action = machine.poll_bounded(input, elapsed);
        let ends_for_good = match action {
            Ok(DownloadAction::Activate(_) | DownloadAction::Done(Ok(()))) => true,
            Ok(DownloadAction::Done(Err(_))) | Err(_) => !self.retrying,
            _ => false,
        };
        if ends_for_good {
            self.closed = true;
        }
        action
    }
}

/// Experimental: API may change before 1.0.
///
/// The size of one read request, capped to what the transport and the caller's buffer hold.
///
/// This does NOT make a read one transport wait.
/// ESP-IDF's `esp_http_client_read` loops: while the request is not filled and the body has
/// data left, it calls `esp_transport_read` again with the full client timeout, and TCP `recv`
/// returns whatever has arrived, so a read that returns fewer bytes than asked for simply
/// waits again.
/// A request of `n` bytes can therefore take up to `n` transport waits (a server that sends one
/// byte shortly before each timeout), and the cap `min(max, buffer, transport_rx)` only limits
/// that worst case to `transport_rx` waits.
/// Use [`bounded_read_request`] to keep a call inside the time left of a deadline.
/// Never returns 0 for a positive `max`, `buffer` and `transport_rx`.
///
/// ```
/// use juggler::ota::capped_read_len;
///
/// assert_eq!(capped_read_len(1_000_000, 4096, 1024), 1024);
/// assert_eq!(capped_read_len(100, 4096, 1024), 100);
/// ```
pub const fn capped_read_len(max: u32, buffer: usize, transport_rx: usize) -> usize {
    let mut len = max as usize;
    if buffer < len {
        len = buffer;
    }
    if transport_rx < len {
        len = transport_rx;
    }
    len
}

/// Experimental: API may change before 1.0.
///
/// The wait of the follow-up read inside one [`read_bounded`] call, once the deadline is near.
///
/// The follow-up only collects bytes that have already arrived behind the first one, so it must
/// not wait for new data: a short wait lets a fast peer fill the buffer while an idle one returns
/// at once.
/// `esp_http_client_read` itself returns the bytes read so far when a later wait elapses
/// (`if (ridx) return ridx;`) and `-ESP_ERR_HTTP_EAGAIN` only when none arrived (ESP-IDF v5.3.3
/// `components/esp_http_client/esp_http_client.c`, `esp_http_client_read`, the
/// `ERR_TCP_TRANSPORT_CONNECTION_TIMEOUT` branch), so the wait costs at most this much per
/// transport read.
/// 1 ms is the finest timeout the transport takes (`esp_http_client_set_timeout_ms`), so the
/// follow-up never waits longer than needed, and a follow-up of a whole 1024-byte chunk fits
/// once a little over one second of the budget is left.
pub const FOLLOW_UP_WAIT: Duration = Duration::from_millis(1);

/// Experimental: API may change before 1.0.
///
/// One read request: how many bytes to ask for and how long the transport waits may last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadRequest {
    /// The most bytes one read action returns.
    pub len: usize,
    /// The wait of the first transport read; the tier rounds it to the transport's granularity.
    pub wait: Duration,
    /// `Some` when the deadline is near: the first read asks for ONE byte and the rest of `len`
    /// is a follow-up read with this wait (see [`read_bounded`]); `None` is a single read of `len`.
    pub follow_up_wait: Option<Duration>,
}

/// Experimental: API may change before 1.0.
///
/// Sizes one blocking read so that, once the deadline is closer than one per-read timeout, the
/// whole call fits the time left.
///
/// `budget` is the [`DownloadAction::Read`] budget, `min(per_op, remaining)`, and `per_op` the
/// configured per-read timeout.
/// While `budget >= per_op` the request is [`capped_read_len`] with a wait of `budget`: a slow
/// server may stall for a full `per_op` before the read fails, so the waits are not shortened.
/// That leaves a RESIDUAL OVERRUN: a call that makes one wait per partial segment (up to
/// `len` waits against a server that trickles bytes) can run past the deadline.
/// Once `budget < per_op` the deadline is near: the first read is ONE byte with a wait of the
/// whole `budget` (one transport wait, and an idle gap shorter than the budget cannot fail the
/// read early, because `esp_http_client_read` returns `-ESP_ERR_HTTP_EAGAIN` when a wait ends
/// with no bytes and the machine treats a failed read as final), and the other `len - 1` bytes
/// are a follow-up with [`FOLLOW_UP_WAIT`], so a fast peer still delivers chunk-sized reads.
///
/// ```
/// use core::time::Duration;
/// use juggler::ota::{bounded_read_request, FOLLOW_UP_WAIT};
///
/// let per_op = Duration::from_secs(10);
/// let near = bounded_read_request(1_000_000, 4096, 1024, Duration::from_secs(2), per_op);
/// assert_eq!(near.len, 1024);
/// assert_eq!(near.wait, Duration::from_secs(2));
/// assert_eq!(near.follow_up_wait, Some(FOLLOW_UP_WAIT));
/// ```
pub fn bounded_read_request(
    max: u32,
    buffer: usize,
    transport_rx: usize,
    budget: Duration,
    per_op: Duration,
) -> ReadRequest {
    let len = capped_read_len(max, buffer, transport_rx);
    let near = budget < per_op && len > 1;
    ReadRequest {
        len,
        wait: budget,
        follow_up_wait: near.then_some(FOLLOW_UP_WAIT),
    }
}

/// Experimental: API may change before 1.0.
///
/// How one follow-up read ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FollowUp {
    /// `n > 0` more bytes arrived.
    Bytes(usize),
    /// Nothing more is available right now: keep the bytes read so far.
    ///
    /// This is a timeout (`-ESP_ERR_HTTP_EAGAIN`, no bytes yet), which is not a failure of a
    /// follow-up, and a zero read (end of stream, which the next read action reports itself).
    Idle,
    /// A real read failure: it ends the download.
    Failed(OtaError),
}

/// Experimental: API may change before 1.0.
///
/// Classifies the result of a follow-up read (see [`FollowUp`]).
///
/// ```
/// use juggler::ota::{classify_follow_up, FollowUp, OtaError};
///
/// assert_eq!(classify_follow_up(Ok(7)), FollowUp::Bytes(7));
/// assert_eq!(classify_follow_up(Err(OtaError::DownloadTimeout)), FollowUp::Idle);
/// assert_eq!(
///     classify_follow_up(Err(OtaError::ServerUnreachable)),
///     FollowUp::Failed(OtaError::ServerUnreachable)
/// );
/// ```
pub fn classify_follow_up(read: Result<usize, OtaError>) -> FollowUp {
    match read {
        Ok(0) | Err(OtaError::DownloadTimeout) => FollowUp::Idle,
        Ok(n) => FollowUp::Bytes(n),
        Err(e) => FollowUp::Failed(e),
    }
}

/// Experimental: API may change before 1.0.
///
/// Performs one [`DownloadAction::Read`] against `read`, a blocking transport read of `buf`
/// with a wait, and returns the bytes now at the start of `buf`.
///
/// `elapsed` is the time since this call began.
/// The first read follows [`bounded_read_request`].
/// Near the deadline a first read of one byte is followed by a read of the rest with
/// [`FOLLOW_UP_WAIT`], sized so that even a peer delivering a byte per wait ends inside what is
/// left of `budget` (`esp_http_client_read` makes at most one more wait than it returns bytes);
/// an idle follow-up returns what was read ([`FollowUp::Idle`]) instead of an error, so the
/// flash write stays chunk-sized.
///
/// ```
/// use core::time::Duration;
/// use juggler::ota::read_bounded;
///
/// let mut buf = [0u8; 8];
/// let n = read_bounded(&mut buf, 8, 8, Duration::from_secs(1), Duration::from_secs(10),
///     || Duration::ZERO, |b, _| { b.fill(1); Ok(b.len()) }).unwrap();
/// assert_eq!(n, 8);
/// ```
///
/// # Errors
///
/// The error of the first read, or of a follow-up that is not [`FollowUp::Idle`].
pub fn read_bounded<R, C>(
    buf: &mut [u8],
    max: u32,
    transport_rx: usize,
    budget: Duration,
    per_op: Duration,
    mut elapsed: C,
    mut read: R,
) -> Result<usize, OtaError>
where
    R: FnMut(&mut [u8], Duration) -> Result<usize, OtaError>,
    C: FnMut() -> Duration,
{
    let request = bounded_read_request(max, buf.len(), transport_rx, budget, per_op);
    let Some(follow_up_wait) = request.follow_up_wait else {
        return read(&mut buf[..request.len], request.wait);
    };
    let first = read(&mut buf[..1], request.wait)?;
    if first == 0 {
        return Ok(0);
    }
    let remaining = budget.saturating_sub(elapsed());
    let wait = follow_up_wait.min(remaining);
    let waits = if wait.is_zero() {
        0
    } else {
        usize::try_from(remaining.as_nanos() / wait.as_nanos()).unwrap_or(usize::MAX)
    };
    let more = request
        .len
        .saturating_sub(first)
        .min(waits.saturating_sub(1));
    if more == 0 {
        return Ok(first);
    }
    match classify_follow_up(read(&mut buf[first..first + more], wait)) {
        FollowUp::Bytes(n) => Ok(first + n),
        FollowUp::Idle => Ok(first),
        FollowUp::Failed(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn one_read_never_asks_for_more_than_one_transport_buffer() {
        use super::capped_read_len;
        assert_eq!(capped_read_len(u32::MAX, 4096, 1024), 1024);
        assert_eq!(capped_read_len(4096, 4096, 512), 512);
        assert_eq!(capped_read_len(700, 4096, 1024), 700);
        assert_eq!(capped_read_len(10, 4, 1024), 4);
        assert_eq!(capped_read_len(0, 4096, 1024), 0);
    }

    use core::cell::Cell;

    /// A model of `esp_http_client_read` over a peer with `avail` bytes ready and `trickle`
    /// more arriving one per millisecond; time passes only inside the model.
    struct Peer {
        now: Cell<Duration>,
        avail: Cell<usize>,
        trickle: bool,
        fail_after_first: bool,
        calls: Cell<usize>,
        waits: Cell<Duration>,
    }

    impl Peer {
        fn new(avail: usize, trickle: bool) -> Self {
            Peer {
                now: Cell::new(Duration::ZERO),
                avail: Cell::new(avail),
                trickle,
                fail_after_first: false,
                calls: Cell::new(0),
                waits: Cell::new(Duration::ZERO),
            }
        }

        fn read(&self, buf: &mut [u8], wait: Duration) -> Result<usize, OtaError> {
            self.calls.set(self.calls.get() + 1);
            if self.fail_after_first && self.calls.get() > 1 {
                return Err(OtaError::ServerUnreachable);
            }
            let mut got = 0;
            while got < buf.len() {
                if self.avail.get() > 0 {
                    self.avail.set(self.avail.get() - 1);
                    got += 1;
                } else if self.trickle && wait >= Duration::from_millis(1) {
                    self.now.set(self.now.get() + Duration::from_millis(1));
                    got += 1;
                } else {
                    self.now.set(self.now.get() + wait);
                    self.waits.set(self.waits.get() + wait);
                    return if got > 0 {
                        Ok(got)
                    } else {
                        Err(OtaError::DownloadTimeout)
                    };
                }
            }
            Ok(got)
        }

        fn run(&self, budget: Duration) -> Result<usize, OtaError> {
            let mut buf = [0u8; 4096];
            read_bounded(
                &mut buf,
                u32::MAX,
                1024,
                budget,
                Duration::from_secs(10),
                || self.now.get(),
                |b, w| self.read(b, w),
            )
        }
    }

    #[test]
    fn a_fast_peer_near_the_deadline_still_fills_a_whole_chunk() {
        let peer = Peer::new(4096, false);
        assert_eq!(peer.run(Duration::from_secs(2)), Ok(1024));
        assert_eq!(peer.calls.get(), 2);
    }

    #[test]
    fn an_idle_follow_up_returns_the_bytes_read_so_far_not_an_error() {
        let peer = Peer::new(10, false);
        assert_eq!(peer.run(Duration::from_secs(2)), Ok(10));
        assert!(peer.now.get() <= Duration::from_secs(2));
    }

    #[test]
    fn a_silent_peer_fails_after_exactly_the_budget() {
        let peer = Peer::new(0, false);
        let budget = Duration::from_millis(1_500);
        assert_eq!(peer.run(budget), Err(OtaError::DownloadTimeout));
        assert_eq!(peer.now.get(), budget);
    }

    #[test]
    fn a_trickling_peer_cannot_push_one_call_past_the_budget() {
        for budget_ms in [3u64, 4, 5, 20, 50, 400, 2_000] {
            let peer = Peer::new(0, true);
            let budget = Duration::from_millis(budget_ms);
            assert!(peer.run(budget).is_ok(), "budget {budget_ms}");
            assert!(peer.now.get() <= budget, "budget {budget_ms}");
        }
    }

    #[test]
    fn a_read_failure_after_the_first_byte_is_not_hidden() {
        let mut peer = Peer::new(100, false);
        peer.fail_after_first = true;
        assert_eq!(
            peer.run(Duration::from_secs(2)),
            Err(OtaError::ServerUnreachable)
        );
    }

    #[test]
    fn a_full_budget_is_one_plain_read() {
        let peer = Peer::new(4096, false);
        assert_eq!(peer.run(Duration::from_secs(10)), Ok(1024));
        assert_eq!(peer.calls.get(), 1);
    }

    #[test]
    fn the_follow_up_classification() {
        assert_eq!(classify_follow_up(Ok(3)), FollowUp::Bytes(3));
        assert_eq!(classify_follow_up(Ok(0)), FollowUp::Idle);
        assert_eq!(
            classify_follow_up(Err(OtaError::DownloadTimeout)),
            FollowUp::Idle
        );
        assert_eq!(
            classify_follow_up(Err(OtaError::ServerUnreachable)),
            FollowUp::Failed(OtaError::ServerUnreachable)
        );
    }

    #[test]
    fn a_read_near_the_deadline_is_one_byte_then_a_short_follow_up() {
        let per_op = Duration::from_secs(10);
        let budget = Duration::from_millis(9_999);
        assert_eq!(
            bounded_read_request(u32::MAX, 4096, 1024, budget, per_op),
            ReadRequest {
                len: 1024,
                wait: budget,
                follow_up_wait: Some(FOLLOW_UP_WAIT)
            }
        );
        let one = bounded_read_request(1, 4096, 1024, budget, per_op);
        assert_eq!((one.len, one.follow_up_wait), (1, None));
    }

    #[test]
    fn a_read_with_a_full_per_op_budget_keeps_the_whole_wait() {
        let per_op = Duration::from_secs(10);
        assert_eq!(
            bounded_read_request(u32::MAX, 4096, 1024, per_op, per_op),
            ReadRequest {
                len: 1024,
                wait: per_op,
                follow_up_wait: None
            }
        );
        assert_eq!(bounded_read_request(10, 4096, 1024, per_op, per_op).len, 10);
    }

    use super::*;
    use DownloadInput as I;

    const SHA: [u8; 32] = [7; 32];
    const SHAPE: OtaError = OtaError::DownloadFailed { status: 0 };

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// A comparable label for an action (`ActivationPermit` is deliberately not comparable).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum K {
        Connect,
        ReadHeader(u32),
        Prepare(u32),
        WriteHeader,
        Read(u32),
        Write(usize),
        Finalize,
        Abort(OtaError),
        Activate,
        Done(Result<(), OtaError>),
        Ignored,
    }

    fn kind(a: &DownloadAction) -> K {
        match a {
            DownloadAction::Connect { .. } => K::Connect,
            DownloadAction::ReadHeader { max, .. } => K::ReadHeader(*max),
            DownloadAction::Prepare { len } => K::Prepare(*len),
            DownloadAction::WriteHeader(_) => K::WriteHeader,
            DownloadAction::Read { max, .. } => K::Read(*max),
            DownloadAction::Write { n } => K::Write(*n),
            DownloadAction::Finalize => K::Finalize,
            DownloadAction::Abort(e) => K::Abort(*e),
            DownloadAction::Activate(_) => K::Activate,
            DownloadAction::Done(r) => K::Done(*r),
            DownloadAction::Ignored => K::Ignored,
        }
    }

    fn machine(deadline: Deadline) -> DownloadMachine {
        DownloadMachine::new(deadline, ms(1_000), SHA)
    }

    fn headers(len: u64) -> DownloadInput {
        I::Headers {
            content_length: Some(len),
            chunked: false,
            capacity: 4096,
        }
    }

    /// A machine that has opened the write session for `len` bytes and asked for the first read.
    fn prepared(deadline: Deadline, len: u64) -> DownloadMachine {
        let mut m = machine(deadline);
        assert_eq!(kind(&m.poll(I::Begin, ms(0))), K::Connect);
        assert_eq!(kind(&m.poll(headers(len), ms(0))), K::Prepare(len as u32));
        assert_eq!(kind(&m.poll(I::Prepared, ms(0))), K::Read(len as u32));
        m
    }

    #[test]
    fn expected_size_must_equal_the_content_length() {
        let sized = |size| machine(Deadline::none()).with_expected_size(size);
        let mut m = sized(1024);
        assert_eq!(kind(&m.poll(I::Begin, ms(0))), K::Connect);
        assert_eq!(kind(&m.poll(headers(1024), ms(0))), K::Prepare(1024));

        for declared in [1023, 1025] {
            let mut m = sized(1024);
            m.poll(I::Begin, ms(0));
            assert_eq!(
                kind(&m.poll(headers(declared), ms(0))),
                K::Done(Err(OtaError::SizeMismatch))
            );
        }

        // A chunked response is refused by the length checks, as without an expected size.
        let mut m = sized(1024);
        m.poll(I::Begin, ms(0));
        let chunked = I::Headers {
            content_length: None,
            chunked: true,
            capacity: 4096,
        };
        assert_eq!(kind(&m.poll(chunked, ms(0))), K::Done(Err(SHAPE)));

        // A size above `u32::MAX` never equals an admitted length.
        let mut m = sized(u64::from(u32::MAX) + 1);
        m.poll(I::Begin, ms(0));
        assert_eq!(
            kind(&m.poll(headers(1024), ms(0))),
            K::Done(Err(OtaError::SizeMismatch))
        );

        // The partition check comes first, so an oversized image is still `InsufficientSpace`.
        let mut m = sized(8192);
        m.poll(I::Begin, ms(0));
        assert_eq!(
            kind(&m.poll(headers(8192), ms(0))),
            K::Done(Err(OtaError::InsufficientSpace))
        );
    }

    #[test]
    fn content_length_cases() {
        assert_eq!(check_content_length(None, false, 4096), Err(SHAPE));
        assert_eq!(check_content_length(Some(1024), true, 4096), Err(SHAPE));
        assert_eq!(check_content_length(None, true, 4096), Err(SHAPE));
        assert_eq!(check_content_length(Some(0), false, 4096), Err(SHAPE));
        assert_eq!(check_content_length(Some(4096), false, 4096), Ok(4096));
        assert_eq!(
            check_content_length(Some(4097), false, 4096),
            Err(OtaError::InsufficientSpace)
        );
        assert_eq!(check_content_length(Some(1024), false, 4096), Ok(1024));
        assert_eq!(
            check_content_length(Some(u64::MAX), false, u64::MAX),
            Err(OtaError::InsufficientSpace)
        );
    }

    #[test]
    fn happy_path_order() {
        let mut m = prepared(Deadline::none(), 10);
        let mut seen = alloc::vec![];
        let mut input = I::Read(6);
        loop {
            let a = m.poll(input, ms(1));
            seen.push(kind(&a));
            input = match a {
                DownloadAction::Write { .. } => I::Written,
                DownloadAction::Read { max, .. } => I::Read(max as usize),
                DownloadAction::Finalize => I::Digest(SHA),
                DownloadAction::Activate(_) => I::Activated(Ok(())),
                DownloadAction::Done(_) => break,
                other => panic!("{other:?}"),
            };
        }
        assert_eq!(
            seen,
            [
                K::Write(6),
                K::Read(4),
                K::Write(4),
                K::Finalize,
                K::Activate,
                K::Done(Ok(()))
            ]
        );
    }

    #[test]
    fn no_prepare_when_headers_fail() {
        let cases = [
            (None, false, 4096, SHAPE),
            (Some(100), true, 4096, SHAPE),
            (Some(0), false, 4096, SHAPE),
            (Some(4097), false, 4096, OtaError::InsufficientSpace),
        ];
        for (content_length, chunked, capacity, err) in cases {
            let mut m = machine(Deadline::none());
            m.poll(I::Begin, ms(0));
            let a = m.poll(
                I::Headers {
                    content_length,
                    chunked,
                    capacity,
                },
                ms(0),
            );
            assert_eq!(kind(&a), K::Done(Err(err)), "{content_length:?} {chunked}");
        }
        let mut m = machine(Deadline::none());
        m.poll(I::Begin, ms(0));
        let a = m.poll(I::ConnectFailed(OtaError::ServerUnreachable), ms(0));
        assert_eq!(kind(&a), K::Done(Err(OtaError::ServerUnreachable)));
    }

    #[test]
    fn expired_deadline_stops_before_any_access() {
        let mut m = machine(Deadline::after(Duration::ZERO));
        let a = m.poll(I::Begin, ms(0));
        assert_eq!(kind(&a), K::Done(Err(OtaError::DownloadTimeout)));

        let mut m = machine(Deadline::after(ms(10)));
        m.poll(I::Begin, ms(0));
        let a = m.poll(headers(100), ms(10));
        assert_eq!(kind(&a), K::Done(Err(OtaError::DownloadTimeout)));
    }

    #[test]
    fn a_failed_prepare_has_nothing_to_abort() {
        let mut m = machine(Deadline::none());
        m.poll(I::Begin, ms(0));
        m.poll(headers(100), ms(0));
        let a = m.poll(I::PrepareFailed(OtaError::FlashWriteFailed), ms(0));
        assert_eq!(kind(&a), K::Done(Err(OtaError::FlashWriteFailed)));
    }

    #[test]
    fn abort_on_every_error_after_prepare() {
        let digest_mismatch: &[I] = &[I::Read(10), I::Written, I::Digest([0; 32])];
        let read_failed: &[I] = &[I::ReadFailed(OtaError::ServerUnreachable)];
        let write_failed: &[I] = &[I::Read(4), I::WriteFailed(OtaError::FlashWriteFailed)];
        let tail_failed: &[I] = &[
            I::Read(10),
            I::Written,
            I::WriteFailed(OtaError::FlashWriteFailed),
        ];
        let short_body: &[I] = &[I::Read(4), I::Written, I::Read(0)];
        let long_body: &[I] = &[I::Read(11)];
        let cases: [(&[I], OtaError); 6] = [
            (read_failed, OtaError::ServerUnreachable),
            (write_failed, OtaError::FlashWriteFailed),
            (tail_failed, OtaError::FlashWriteFailed),
            (digest_mismatch, OtaError::ChecksumMismatch),
            (short_body, SHAPE),
            (long_body, SHAPE),
        ];
        for (script, err) in cases {
            let mut m = prepared(Deadline::none(), 10);
            let mut last = K::Ignored;
            for input in script {
                last = kind(&m.poll(*input, ms(1)));
            }
            assert_eq!(last, K::Abort(err), "{script:?}");
            assert_eq!(kind(&m.poll(I::Aborted, ms(1))), K::Done(Err(err)));
        }
        let mut m = prepared(Deadline::after(ms(5)), 10);
        assert_eq!(
            kind(&m.poll(I::Prepared, ms(1))),
            K::Ignored,
            "already prepared"
        );
        let mut m = machine(Deadline::after(ms(5)));
        m.poll(I::Begin, ms(0));
        m.poll(headers(10), ms(0));
        assert_eq!(
            kind(&m.poll(I::Prepared, ms(5))),
            K::Abort(OtaError::DownloadTimeout),
            "expired during the erase"
        );
    }

    #[test]
    fn no_activate_after_deadline() {
        let mut m = prepared(Deadline::after(ms(50)), 10);
        m.poll(I::Read(10), ms(10));
        assert_eq!(kind(&m.poll(I::Written, ms(20))), K::Finalize);
        let a = m.poll(I::Digest(SHA), ms(50));
        assert_eq!(kind(&a), K::Abort(OtaError::DownloadTimeout));
        assert_eq!(
            kind(&m.poll(I::Aborted, ms(51))),
            K::Done(Err(OtaError::DownloadTimeout))
        );
    }

    #[test]
    fn deadline_checked_after_each_read_and_write() {
        let mut m = prepared(Deadline::after(ms(50)), 10);
        assert_eq!(
            kind(&m.poll(I::Read(4), ms(50))),
            K::Abort(OtaError::DownloadTimeout)
        );

        let mut m = prepared(Deadline::after(ms(50)), 10);
        assert_eq!(kind(&m.poll(I::Read(4), ms(49))), K::Write(4));
        assert_eq!(
            kind(&m.poll(I::Written, ms(50))),
            K::Abort(OtaError::DownloadTimeout)
        );

        let mut m = prepared(Deadline::after(ms(50)), 10);
        assert_eq!(kind(&m.poll(I::Read(10), ms(10))), K::Write(10));
        assert_eq!(
            kind(&m.poll(I::Written, ms(50))),
            K::Abort(OtaError::DownloadTimeout),
            "also after the last write"
        );
    }

    #[test]
    fn an_operation_error_wins_over_an_expired_deadline() {
        let mut m = prepared(Deadline::after(ms(50)), 10);
        let a = m.poll(I::ReadFailed(OtaError::ServerUnreachable), ms(500));
        assert_eq!(kind(&a), K::Abort(OtaError::ServerUnreachable));
        let mut m = prepared(Deadline::after(ms(50)), 10);
        m.poll(I::Read(10), ms(10));
        m.poll(I::Written, ms(10));
        let a = m.poll(I::Digest([0; 32]), ms(500));
        assert_eq!(kind(&a), K::Abort(OtaError::ChecksumMismatch));
    }

    #[test]
    fn the_budget_is_clipped_by_the_deadline() {
        let mut m = machine(Deadline::after(ms(1_500)));
        match m.poll(I::Begin, ms(800)) {
            DownloadAction::Connect { budget } => assert_eq!(budget, ms(700)),
            other => panic!("{other:?}"),
        }
        m.poll(headers(10), ms(900));
        match m.poll(I::Prepared, ms(1_000)) {
            DownloadAction::Read { max, budget } => assert_eq!((max, budget), (10, ms(500))),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn out_of_phase_input_is_ignored_and_state_unchanged() {
        let all = [
            I::Begin,
            headers(10),
            I::ConnectFailed(OtaError::ServerUnreachable),
            I::Prepared,
            I::PrepareFailed(OtaError::FlashWriteFailed),
            I::Read(1),
            I::ReadFailed(OtaError::ServerUnreachable),
            I::Written,
            I::WriteFailed(OtaError::FlashWriteFailed),
            I::Digest(SHA),
            I::Aborted,
            I::Activated(Ok(())),
        ];
        let mut m = prepared(Deadline::none(), 10);
        for input in [
            I::Begin,
            headers(10),
            I::Prepared,
            I::Written,
            I::Digest(SHA),
            I::Aborted,
            I::Activated(Ok(())),
        ] {
            assert_eq!(kind(&m.poll(input, ms(1))), K::Ignored, "{input:?}");
        }
        assert_eq!(
            kind(&m.poll(I::Read(10), ms(1))),
            K::Write(10),
            "the next correct input still advances"
        );

        let mut done = machine(Deadline::none());
        done.poll(I::Begin, ms(0));
        done.poll(I::ConnectFailed(OtaError::ServerUnreachable), ms(0));
        for input in all {
            assert_eq!(kind(&done.poll(input, ms(1))), K::Ignored, "{input:?}");
        }
    }

    #[test]
    fn poll_bounded_turns_ignored_into_an_error() {
        let mut m = machine(Deadline::none());
        assert_eq!(m.poll_bounded(I::Written, ms(0)).unwrap_err(), SHAPE);
        assert!(m.poll_bounded(I::Begin, ms(0)).is_ok());
    }

    #[test]
    fn poll_bounded_stops_a_loop_that_never_ends() {
        let mut m = machine(Deadline::none());
        let mut result = Ok(());
        for _ in 0..100 {
            if let Err(e) = m.poll_bounded(I::Begin, ms(0)) {
                result = Err(e);
                if e == OtaError::DownloadTimeout {
                    break;
                }
            }
        }
        assert_eq!(result, Err(OtaError::DownloadTimeout));
    }

    #[test]
    fn max_steps_formula() {
        assert_eq!(DownloadMachine::max_steps(0, 1), 8);
        assert_eq!(DownloadMachine::max_steps(10, 4), 2 * 3 + 8);
        assert_eq!(DownloadMachine::max_steps(10, 0), 2 * 10 + 8);
    }

    /// One failure to inject into a scripted tier.
    #[derive(Debug, Clone, Copy)]
    enum Fault {
        None,
        Connect,
        Prepare,
        Read(usize),
        Write(usize),
        ShortBody(usize),
        Digest,
        Tail,
        Activate,
        Deadline(u64),
    }

    /// Runs a simulated tier through the bounded loop, with the early chip check when `chip` is set, and returns (result, steps, aborted).
    fn run(
        len: u32,
        chunk: usize,
        fault: Fault,
        chip: bool,
    ) -> (Result<(), OtaError>, usize, bool) {
        let deadline = match fault {
            Fault::Deadline(at) => Deadline::after(ms(at)),
            _ => Deadline::none(),
        };
        let mut m = machine(deadline);
        if chip {
            m = m.with_expected_chip(ImageChip::Esp32C3);
        }
        let header = header_for(ImageChip::Esp32C3);
        let mut header_at = 0;
        let mut input = I::Begin;
        let mut steps = 0;
        let mut reads = 0;
        let mut writes = 0;
        let mut prepared = false;
        let mut aborted = false;
        let mut t = 0;
        loop {
            t += 1;
            steps += 1;
            let action = match m.poll_bounded(input, ms(t)) {
                Ok(action) => action,
                Err(e) => panic!("bounded loop failed with {e:?} after {steps} steps"),
            };
            input = match action {
                DownloadAction::Connect { .. } => match fault {
                    Fault::Connect => I::ConnectFailed(OtaError::ServerUnreachable),
                    _ => I::Headers {
                        content_length: Some(u64::from(len)),
                        chunked: false,
                        capacity: u64::from(len),
                    },
                },
                DownloadAction::Prepare { .. } => match fault {
                    Fault::Prepare => I::PrepareFailed(OtaError::FlashWriteFailed),
                    _ => {
                        prepared = true;
                        I::Prepared
                    }
                },
                DownloadAction::ReadHeader { max, .. } => {
                    reads += 1;
                    match fault {
                        Fault::Read(at) if reads == at => {
                            I::ReadFailed(OtaError::ServerUnreachable)
                        }
                        Fault::ShortBody(at) if reads == at => I::header_read(&[]),
                        _ => {
                            let n = (max as usize).min(chunk);
                            header_at += n;
                            I::header_read(&header[header_at - n..header_at])
                        }
                    }
                }
                DownloadAction::Read { max, .. } => {
                    reads += 1;
                    match fault {
                        Fault::Read(at) if reads == at => {
                            I::ReadFailed(OtaError::ServerUnreachable)
                        }
                        Fault::ShortBody(at) if reads == at => I::Read(0),
                        _ => I::Read((max as usize).min(chunk)),
                    }
                }
                DownloadAction::Write { .. } | DownloadAction::WriteHeader(_) => {
                    writes += 1;
                    match fault {
                        Fault::Write(at) if writes == at => {
                            I::WriteFailed(OtaError::FlashWriteFailed)
                        }
                        _ => I::Written,
                    }
                }
                DownloadAction::Finalize => match fault {
                    Fault::Tail => I::WriteFailed(OtaError::FlashWriteFailed),
                    Fault::Digest => I::Digest([0; 32]),
                    _ => I::Digest(SHA),
                },
                DownloadAction::Abort(_) => {
                    assert!(prepared, "abort without a write session");
                    aborted = true;
                    I::Aborted
                }
                DownloadAction::Activate(_) => match fault {
                    Fault::Activate => I::Activated(Err(OtaError::FlashWriteFailed)),
                    _ => I::Activated(Ok(())),
                },
                DownloadAction::Done(result) => {
                    if prepared && result.is_err() && !matches!(fault, Fault::Activate) {
                        assert!(aborted, "{fault:?}: an error after prepare must abort");
                    }
                    return (result, steps, aborted);
                }
                DownloadAction::Ignored => panic!("ignored"),
            };
        }
    }

    #[test]
    fn every_path_reaches_done_within_max_steps() {
        let len = 1_000;
        let faults = [
            Fault::None,
            Fault::Connect,
            Fault::Prepare,
            Fault::Read(1),
            Fault::Read(5),
            Fault::Write(3),
            Fault::ShortBody(4),
            Fault::Digest,
            Fault::Tail,
            Fault::Activate,
            Fault::Deadline(1),
            Fault::Deadline(4),
            Fault::Deadline(9),
            Fault::Deadline(30),
        ];
        for chunk in [1, 7, 16, 17, 64, 1_000, 4_096] {
            for (fault, chip) in faults.into_iter().flat_map(|f| [(f, false), (f, true)]) {
                let (result, steps, aborted) = run(len, chunk, fault, chip);
                assert!(
                    steps <= DownloadMachine::max_steps(len, chunk),
                    "{fault:?} chunk {chunk} chip {chip}: {steps} steps"
                );
                let always_fails = matches!(
                    fault,
                    Fault::Connect
                        | Fault::Prepare
                        | Fault::Read(1)
                        | Fault::Digest
                        | Fault::Tail
                        | Fault::Activate
                        | Fault::Deadline(1)
                );
                if matches!(fault, Fault::None) {
                    assert_eq!(result, Ok(()), "chunk {chunk}");
                }
                if always_fails {
                    assert!(result.is_err(), "{fault:?} chunk {chunk}");
                }
                assert!(!(aborted && result.is_ok()));
            }
        }
    }

    const CANCELLED: OtaError = OtaError::Cancelled;

    #[test]
    fn a_cancel_before_connect_ends_without_any_access() {
        let mut m = machine(Deadline::none());
        assert_eq!(m.cancel(), CancelRequest::Accepted);
        assert_eq!(kind(&m.poll(I::Begin, ms(0))), K::Done(Err(CANCELLED)));
        assert_eq!(m.cancel(), CancelRequest::TooLate);
    }

    #[test]
    fn a_cancel_during_connect_never_prepares() {
        for answer in [headers(10), I::ConnectFailed(OtaError::ServerUnreachable)] {
            let mut m = machine(Deadline::none());
            assert_eq!(kind(&m.poll(I::Begin, ms(0))), K::Connect);
            assert_eq!(m.cancel(), CancelRequest::Accepted);
            assert_eq!(
                kind(&m.poll(answer, ms(1))),
                K::Done(Err(CANCELLED)),
                "{answer:?}"
            );
        }
    }

    #[test]
    fn a_cancel_during_prepare_aborts_an_open_session() {
        let mut m = machine(Deadline::none());
        m.poll(I::Begin, ms(0));
        m.poll(headers(10), ms(0));
        assert_eq!(m.cancel(), CancelRequest::Accepted);
        assert_eq!(kind(&m.poll(I::Prepared, ms(1))), K::Abort(CANCELLED));
        assert_eq!(kind(&m.poll(I::Aborted, ms(1))), K::Done(Err(CANCELLED)));

        let mut m = machine(Deadline::none());
        m.poll(I::Begin, ms(0));
        m.poll(headers(10), ms(0));
        m.cancel();
        let a = m.poll(I::PrepareFailed(OtaError::FlashWriteFailed), ms(1));
        assert_eq!(kind(&a), K::Done(Err(CANCELLED)), "nothing to abort");
    }

    #[test]
    fn a_cancel_mid_read_aborts_after_the_read() {
        for answer in [I::Read(4), I::ReadFailed(OtaError::ServerUnreachable)] {
            let mut m = prepared(Deadline::none(), 10);
            assert_eq!(m.cancel(), CancelRequest::Accepted);
            assert_eq!(
                kind(&m.poll(answer, ms(1))),
                K::Abort(CANCELLED),
                "{answer:?}"
            );
            assert_eq!(kind(&m.poll(I::Aborted, ms(1))), K::Done(Err(CANCELLED)));
        }
    }

    #[test]
    fn a_cancel_mid_write_aborts_after_the_write() {
        for answer in [I::Written, I::WriteFailed(OtaError::FlashWriteFailed)] {
            let mut m = prepared(Deadline::none(), 10);
            assert_eq!(kind(&m.poll(I::Read(4), ms(1))), K::Write(4));
            assert_eq!(m.cancel(), CancelRequest::Accepted);
            assert_eq!(
                kind(&m.poll(answer, ms(1))),
                K::Abort(CANCELLED),
                "{answer:?}"
            );
            assert_eq!(kind(&m.poll(I::Aborted, ms(1))), K::Done(Err(CANCELLED)));
        }
    }

    #[test]
    fn a_cancel_right_before_activation_withholds_the_permit() {
        let digests = [
            I::Digest(SHA),
            I::Digest([0; 32]),
            I::WriteFailed(OtaError::FlashWriteFailed),
        ];
        for answer in digests {
            let mut m = prepared(Deadline::after(ms(50)), 10);
            m.poll(I::Read(10), ms(1));
            assert_eq!(kind(&m.poll(I::Written, ms(1))), K::Finalize);
            assert_eq!(m.cancel(), CancelRequest::Accepted);
            assert_eq!(
                kind(&m.poll(answer, ms(50))),
                K::Abort(CANCELLED),
                "{answer:?}: wins over a mismatch, a failed tail and the deadline"
            );
            assert_eq!(kind(&m.poll(I::Aborted, ms(51))), K::Done(Err(CANCELLED)));
        }
    }

    #[test]
    fn a_cancel_during_an_abort_still_ends_as_cancelled() {
        let mut m = prepared(Deadline::none(), 10);
        let a = m.poll(I::ReadFailed(OtaError::ServerUnreachable), ms(1));
        assert_eq!(kind(&a), K::Abort(OtaError::ServerUnreachable));
        assert_eq!(m.cancel(), CancelRequest::Accepted);
        assert_eq!(kind(&m.poll(I::Aborted, ms(1))), K::Done(Err(CANCELLED)));
    }

    #[test]
    fn a_cancel_during_or_after_activation_is_too_late() {
        let mut m = prepared(Deadline::none(), 10);
        m.poll(I::Read(10), ms(1));
        m.poll(I::Written, ms(1));
        assert_eq!(kind(&m.poll(I::Digest(SHA), ms(1))), K::Activate);
        assert_eq!(m.cancel(), CancelRequest::TooLate);
        assert_eq!(
            kind(&m.poll(I::Activated(Ok(())), ms(1))),
            K::Done(Ok(())),
            "the image still activates"
        );
        assert_eq!(m.cancel(), CancelRequest::TooLate);

        let mut failed = machine(Deadline::none());
        failed.poll(I::Begin, ms(0));
        failed.poll(I::ConnectFailed(OtaError::ServerUnreachable), ms(0));
        assert_eq!(failed.cancel(), CancelRequest::TooLate, "already over");
    }

    #[test]
    fn a_cancelled_machine_still_ignores_out_of_phase_input() {
        let mut m = prepared(Deadline::none(), 10);
        m.cancel();
        assert_eq!(kind(&m.poll(I::Written, ms(1))), K::Ignored);
        assert_eq!(kind(&m.poll(I::Aborted, ms(1))), K::Ignored);
        assert_eq!(kind(&m.poll(I::Read(4), ms(1))), K::Abort(CANCELLED));
    }

    #[test]
    fn the_gate_closes_with_the_permit_and_with_the_end() {
        let mut gate = CancelGate::new();
        let mut m = prepared(Deadline::none(), 10);
        gate.poll(&mut m, I::Read(10), ms(1)).unwrap();
        gate.poll(&mut m, I::Written, ms(1)).unwrap();
        let a = gate.poll(&mut m, I::Digest(SHA), ms(1)).unwrap();
        assert_eq!(kind(&a), K::Activate);
        assert_eq!(gate.request(), CancelRequest::TooLate);
        let a = gate.poll(&mut m, I::Activated(Ok(())), ms(1)).unwrap();
        assert_eq!(kind(&a), K::Done(Ok(())));

        let mut gate = CancelGate::new();
        let mut m = machine(Deadline::none());
        gate.poll(&mut m, I::Begin, ms(0)).unwrap();
        let a = gate
            .poll(&mut m, I::ConnectFailed(OtaError::ServerUnreachable), ms(0))
            .unwrap();
        assert_eq!(kind(&a), K::Done(Err(OtaError::ServerUnreachable)));
        assert_eq!(gate.request(), CancelRequest::TooLate);

        let mut gate = CancelGate::new();
        let mut m = machine(Deadline::none());
        assert!(gate.poll(&mut m, I::Written, ms(0)).is_err());
        assert_eq!(
            gate.request(),
            CancelRequest::TooLate,
            "a loop error ends it"
        );
    }

    #[test]
    fn a_retrying_gate_stays_open_after_a_failed_attempt() {
        let mut gate = CancelGate::for_retries();
        let mut m = machine(Deadline::none());
        gate.poll(&mut m, I::Begin, ms(0)).unwrap();
        let a = gate
            .poll(&mut m, I::ConnectFailed(OtaError::ServerUnreachable), ms(0))
            .unwrap();
        assert_eq!(kind(&a), K::Done(Err(OtaError::ServerUnreachable)));
        assert!(!gate.is_requested());
        assert_eq!(gate.request(), CancelRequest::Accepted);

        let mut next = machine(Deadline::none());
        let a = gate.poll(&mut next, I::Begin, ms(0)).unwrap();
        assert_eq!(
            kind(&a),
            K::Done(Err(OtaError::Cancelled)),
            "the request carries over"
        );
        assert_eq!(gate.request(), CancelRequest::Accepted);
        assert!(gate.close());
        assert_eq!(gate.request(), CancelRequest::TooLate);

        let mut gate = CancelGate::for_retries();
        let mut m = machine(Deadline::none());
        assert!(gate.poll(&mut m, I::Written, ms(0)).is_err());
        assert_eq!(gate.request(), CancelRequest::Accepted, "a loop error too");
    }

    #[test]
    fn a_retrying_gate_closes_with_the_permit_and_the_success() {
        let mut gate = CancelGate::for_retries();
        let mut m = prepared(Deadline::none(), 10);
        gate.poll(&mut m, I::Read(10), ms(1)).unwrap();
        gate.poll(&mut m, I::Written, ms(1)).unwrap();
        let a = gate.poll(&mut m, I::Digest(SHA), ms(1)).unwrap();
        assert_eq!(kind(&a), K::Activate);
        assert_eq!(gate.request(), CancelRequest::TooLate);
    }

    #[test]
    fn close_reports_whether_a_cancel_was_accepted_first() {
        let mut gate = CancelGate::new();
        assert!(!gate.close());
        assert_eq!(gate.request(), CancelRequest::TooLate);

        let mut gate = CancelGate::new();
        assert_eq!(gate.request(), CancelRequest::Accepted);
        assert!(gate.close());
        assert!(gate.close(), "closing again repeats the answer");
    }

    /// Runs a download through a gate and requests a cancel just before poll number `at`.
    ///
    /// Returns (answer, result, prepared, aborted).
    fn run_cancelled(
        len: u32,
        chunk: usize,
        at: usize,
        chip: bool,
    ) -> (CancelRequest, Result<(), OtaError>, bool, bool) {
        let mut gate = CancelGate::new();
        let mut m = machine(Deadline::none());
        if chip {
            m = m.with_expected_chip(ImageChip::Esp32C3);
        }
        let header = header_for(ImageChip::Esp32C3);
        let mut header_at = 0;
        let mut answer = None;
        let mut input = I::Begin;
        let mut prepared = false;
        let mut aborted = false;
        for step in 1.. {
            if step == at {
                answer = Some(gate.request());
            }
            input = match gate.poll(&mut m, input, ms(1)).expect("bounded") {
                DownloadAction::Connect { .. } => headers(u64::from(len)),
                DownloadAction::ReadHeader { max, .. } => {
                    let n = (max as usize).min(chunk);
                    header_at += n;
                    I::header_read(&header[header_at - n..header_at])
                }
                DownloadAction::Prepare { .. } => {
                    prepared = true;
                    I::Prepared
                }
                DownloadAction::Read { max, .. } => I::Read((max as usize).min(chunk)),
                DownloadAction::Write { .. } | DownloadAction::WriteHeader(_) => I::Written,
                DownloadAction::Finalize => I::Digest(SHA),
                DownloadAction::Abort(_) => {
                    aborted = true;
                    I::Aborted
                }
                DownloadAction::Activate(_) => I::Activated(Ok(())),
                DownloadAction::Done(result) => {
                    let answer = answer.unwrap_or_else(|| gate.request());
                    return (answer, result, prepared, aborted);
                }
                DownloadAction::Ignored => panic!("ignored"),
            };
        }
        unreachable!()
    }

    #[test]
    fn a_cancel_at_any_step_is_accepted_exactly_when_the_download_ends_cancelled() {
        for (len, chip) in [(10, false), (20, true)] {
            for chunk in [1, 3, 10] {
                let steps = DownloadMachine::max_steps(len, chunk);
                for at in 1..=steps + 1 {
                    let (answer, result, prepared, aborted) = run_cancelled(len, chunk, at, chip);
                    let case = alloc::format!("chip {chip} chunk {chunk} at {at}");
                    match answer {
                        CancelRequest::Accepted => {
                            assert_eq!(result, Err(CANCELLED), "{case}");
                            assert_eq!(aborted, prepared, "{case}");
                        }
                        CancelRequest::TooLate => {
                            assert_eq!(result, Ok(()), "{case}");
                        }
                    }
                }
            }
        }
    }

    /// A header for an image built for `chip`: the magic byte and the little-endian chip id at offset 12.
    fn header_for(chip: ImageChip) -> [u8; IMAGE_HEADER_LEN] {
        let mut header = [0x5A; IMAGE_HEADER_LEN];
        header[0] = 0xE9;
        header[12..14].copy_from_slice(&chip.id().to_le_bytes());
        header
    }

    fn chip_machine(deadline: Deadline) -> DownloadMachine {
        machine(deadline).with_expected_chip(ImageChip::Esp32C3)
    }

    /// A chip machine that has admitted `len` bytes and asked for the header.
    fn reading_header(deadline: Deadline, len: u64) -> DownloadMachine {
        let mut m = chip_machine(deadline);
        assert_eq!(kind(&m.poll(I::Begin, ms(0))), K::Connect);
        assert_eq!(
            kind(&m.poll(headers(len), ms(0))),
            K::ReadHeader(IMAGE_HEADER_LEN as u32)
        );
        m
    }

    #[test]
    fn the_image_chip_ids_are_the_esp_idf_values() {
        let table = [
            (ImageChip::Esp32, "esp32", 0),
            (ImageChip::Esp32S2, "esp32s2", 2),
            (ImageChip::Esp32C3, "esp32c3", 5),
            (ImageChip::Esp32S3, "esp32s3", 9),
            (ImageChip::Esp32C2, "esp32c2", 12),
            (ImageChip::Esp32C6, "esp32c6", 13),
            (ImageChip::Esp32H2, "esp32h2", 16),
            (ImageChip::Esp32P4, "esp32p4", 18),
            (ImageChip::Esp32C5, "esp32c5", 23),
        ];
        assert_eq!(table.len(), ImageChip::ALL.len());
        for (chip, target, id) in table {
            assert_eq!(chip.id(), id, "{chip:?}");
            assert_eq!(chip.target(), target, "{chip:?}");
            assert_eq!(ImageChip::from_target(target), Some(chip), "{target}");
        }
        for other in ["", "ESP32C3", "esp32c3 ", "esp32c61", "esp8266"] {
            assert_eq!(ImageChip::from_target(other), None, "{other:?}");
        }
    }

    #[test]
    fn the_header_check_reads_the_magic_and_the_little_endian_chip_id() {
        let c3 = header_for(ImageChip::Esp32C3);
        assert_eq!(check_image_header(&c3, ImageChip::Esp32C3), Ok(()));
        assert_eq!(
            check_image_header(&c3, ImageChip::Esp32),
            Err(OtaError::TargetMismatch)
        );
        let mut bad_magic = c3;
        bad_magic[0] = 0xE8;
        assert_eq!(
            check_image_header(&bad_magic, ImageChip::Esp32C3),
            Err(SHAPE)
        );
        let mut big_endian = c3;
        big_endian[12..14].copy_from_slice(&5u16.to_be_bytes());
        assert_eq!(
            check_image_header(&big_endian, ImageChip::Esp32C3),
            Err(OtaError::TargetMismatch)
        );
        let mut unknown = c3;
        unknown[12..14].copy_from_slice(&[0xFF, 0xFF]);
        assert_eq!(
            check_image_header(&unknown, ImageChip::Esp32C3),
            Err(OtaError::TargetMismatch)
        );
    }

    #[test]
    fn a_matching_chip_prepares_then_writes_the_header_first() {
        let header = header_for(ImageChip::Esp32C3);
        let mut m = reading_header(Deadline::none(), 100);
        assert_eq!(
            kind(&m.poll(I::header_read(&header), ms(1))),
            K::Prepare(100)
        );
        match m.poll(I::Prepared, ms(1)) {
            DownloadAction::WriteHeader(bytes) => assert_eq!(bytes, header),
            other => panic!("{other:?}"),
        }
        assert_eq!(kind(&m.poll(I::Written, ms(1))), K::Read(84));
        assert_eq!(kind(&m.poll(I::Read(84), ms(1))), K::Write(84));
        assert_eq!(kind(&m.poll(I::Written, ms(1))), K::Finalize);
    }

    #[test]
    fn an_image_of_exactly_the_header_finalizes_after_the_header_write() {
        let mut m = reading_header(Deadline::none(), IMAGE_HEADER_LEN as u64);
        let header = header_for(ImageChip::Esp32C3);
        m.poll(I::header_read(&header), ms(1));
        assert_eq!(kind(&m.poll(I::Prepared, ms(1))), K::WriteHeader);
        assert_eq!(kind(&m.poll(I::Written, ms(1))), K::Finalize);
    }

    #[test]
    fn a_wrong_chip_ends_before_any_prepare_or_write() {
        for other in [ImageChip::Esp32S3, ImageChip::Esp32, ImageChip::Esp32C6] {
            let mut m = reading_header(Deadline::none(), 100);
            let a = m.poll(I::header_read(&header_for(other)), ms(1));
            assert_eq!(
                kind(&a),
                K::Done(Err(OtaError::TargetMismatch)),
                "{other:?}"
            );
            assert_eq!(kind(&m.poll(I::Prepared, ms(1))), K::Ignored);
        }
    }

    #[test]
    fn a_bad_magic_is_a_shape_error_before_any_prepare() {
        let mut header = header_for(ImageChip::Esp32C3);
        header[0] = 0x7F;
        let mut m = reading_header(Deadline::none(), 100);
        assert_eq!(
            kind(&m.poll(I::header_read(&header), ms(1))),
            K::Done(Err(SHAPE))
        );
    }

    #[test]
    fn short_header_reads_keep_reading_until_sixteen_bytes() {
        let header = header_for(ImageChip::Esp32C3);
        let mut m = reading_header(Deadline::none(), 100);
        assert_eq!(
            kind(&m.poll(I::header_read(&header[..5]), ms(1))),
            K::ReadHeader(11)
        );
        assert_eq!(
            kind(&m.poll(I::header_read(&header[5..13]), ms(1))),
            K::ReadHeader(3),
            "the chip id is split across two reads"
        );
        assert_eq!(
            kind(&m.poll(I::header_read(&header[13..]), ms(1))),
            K::Prepare(100)
        );
        match m.poll(I::Prepared, ms(1)) {
            DownloadAction::WriteHeader(bytes) => assert_eq!(bytes, header),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_header_stream_that_ends_or_overruns_is_a_shape_error() {
        let header = header_for(ImageChip::Esp32C3);
        let mut m = reading_header(Deadline::none(), 100);
        m.poll(I::header_read(&header[..4]), ms(1));
        assert_eq!(
            kind(&m.poll(I::header_read(&[]), ms(1))),
            K::Done(Err(SHAPE))
        );

        let mut m = reading_header(Deadline::none(), 100);
        m.poll(I::header_read(&header[..4]), ms(1));
        let long = [0xE9; IMAGE_HEADER_LEN];
        assert_eq!(
            kind(&m.poll(I::header_read(&long[..13]), ms(1))),
            K::Done(Err(SHAPE)),
            "more than the missing twelve bytes"
        );

        let mut m = reading_header(Deadline::none(), 100);
        let oversized = [0xE9; IMAGE_HEADER_LEN + 1];
        assert_eq!(
            kind(&m.poll(I::header_read(&oversized), ms(1))),
            K::Done(Err(SHAPE))
        );
    }

    #[test]
    fn an_image_shorter_than_the_header_is_refused_at_the_headers() {
        let mut m = chip_machine(Deadline::none());
        m.poll(I::Begin, ms(0));
        assert_eq!(kind(&m.poll(headers(15), ms(0))), K::Done(Err(SHAPE)));
        // The length checks still come first.
        let mut m = chip_machine(Deadline::none());
        m.poll(I::Begin, ms(0));
        assert_eq!(
            kind(&m.poll(headers(8192), ms(0))),
            K::Done(Err(OtaError::InsufficientSpace))
        );
    }

    #[test]
    fn a_header_read_failure_ends_without_an_abort() {
        let mut m = reading_header(Deadline::none(), 100);
        let a = m.poll(I::ReadFailed(OtaError::ServerUnreachable), ms(1));
        assert_eq!(kind(&a), K::Done(Err(OtaError::ServerUnreachable)));
    }

    #[test]
    fn without_an_expected_chip_the_header_is_never_read() {
        let mut m = machine(Deadline::none());
        m.poll(I::Begin, ms(0));
        assert_eq!(kind(&m.poll(headers(10), ms(0))), K::Prepare(10));
        assert_eq!(kind(&m.poll(I::Prepared, ms(0))), K::Read(10));
        let mut m = machine(Deadline::none());
        m.poll(I::Begin, ms(0));
        m.poll(headers(10), ms(0));
        let header = header_for(ImageChip::Esp32C3);
        assert_eq!(kind(&m.poll(I::header_read(&header), ms(0))), K::Ignored);
    }

    #[test]
    fn the_header_bytes_reach_the_slot_and_the_hash_exactly_once() {
        let mut image = alloc::vec![0u8; 1_000];
        for (i, b) in image.iter_mut().enumerate() {
            *b = (i * 7 + 3) as u8;
        }
        image[..IMAGE_HEADER_LEN].copy_from_slice(&header_for(ImageChip::Esp32C3));
        let mut expected = crate::ota::StreamingVerifier::new();
        expected.update(&image);
        let expected = expected.finalize();

        for chunk in [1, 5, 16, 100, 4_096] {
            let mut m = DownloadMachine::new(Deadline::none(), ms(1_000), expected)
                .with_expected_chip(ImageChip::Esp32C3);
            let mut verifier = crate::ota::StreamingVerifier::new();
            let mut slot = alloc::vec::Vec::new();
            let mut sent = 0;
            let mut last = [0u8; 4_096];
            let mut input = I::Begin;
            let result = loop {
                input = match m.poll_bounded(input, ms(1)).expect("bounded") {
                    DownloadAction::Connect { .. } => headers(image.len() as u64),
                    DownloadAction::ReadHeader { max, .. } => {
                        let n = (max as usize).min(chunk);
                        sent += n;
                        I::header_read(&image[sent - n..sent])
                    }
                    DownloadAction::Prepare { .. } => {
                        assert_eq!(
                            sent, IMAGE_HEADER_LEN,
                            "the header is read before the erase"
                        );
                        I::Prepared
                    }
                    DownloadAction::WriteHeader(bytes) => {
                        verifier.update(&bytes);
                        slot.extend_from_slice(&bytes);
                        I::Written
                    }
                    DownloadAction::Read { max, .. } => {
                        let n = (max as usize).min(chunk);
                        last[..n].copy_from_slice(&image[sent..sent + n]);
                        sent += n;
                        I::Read(n)
                    }
                    DownloadAction::Write { n } => {
                        verifier.update(&last[..n]);
                        slot.extend_from_slice(&last[..n]);
                        I::Written
                    }
                    DownloadAction::Finalize => {
                        I::Digest(core::mem::take(&mut verifier).finalize())
                    }
                    DownloadAction::Activate(_) => I::Activated(Ok(())),
                    DownloadAction::Done(result) => break result,
                    other => panic!("{other:?}"),
                };
            };
            assert_eq!(result, Ok(()), "chunk {chunk}");
            assert_eq!(slot, image, "chunk {chunk}");
        }
    }

    #[test]
    fn a_cancel_during_the_header_phase_never_prepares() {
        let header = header_for(ImageChip::Esp32C3);
        for answer in [
            I::header_read(&header[..4]),
            I::header_read(&header),
            I::header_read(&header_for(ImageChip::Esp32S3)),
            I::ReadFailed(OtaError::ServerUnreachable),
        ] {
            let mut m = reading_header(Deadline::none(), 100);
            assert_eq!(m.cancel(), CancelRequest::Accepted);
            assert_eq!(
                kind(&m.poll(answer, ms(1))),
                K::Done(Err(CANCELLED)),
                "{answer:?}"
            );
        }
    }

    #[test]
    fn the_deadline_is_checked_during_the_header_phase() {
        let header = header_for(ImageChip::Esp32C3);
        let timeout = K::Done(Err(OtaError::DownloadTimeout));

        let mut m = reading_header(Deadline::after(ms(10)), 100);
        assert_eq!(kind(&m.poll(I::header_read(&header[..4]), ms(10))), timeout);

        let mut m = reading_header(Deadline::after(ms(10)), 100);
        assert_eq!(
            kind(&m.poll(I::header_read(&header), ms(10))),
            timeout,
            "a matching header does not prepare after the deadline"
        );

        let mut m = reading_header(Deadline::after(ms(10)), 100);
        assert_eq!(
            kind(&m.poll(I::header_read(&header_for(ImageChip::Esp32S3)), ms(10))),
            K::Done(Err(OtaError::TargetMismatch)),
            "the check's own result wins over the deadline"
        );

        let mut m = chip_machine(Deadline::after(ms(1_500)));
        m.poll(I::Begin, ms(0));
        match m.poll(headers(100), ms(900)) {
            DownloadAction::ReadHeader { max, budget } => assert_eq!((max, budget), (16, ms(600))),
            other => panic!("{other:?}"),
        }
        match m.poll(I::header_read(&header[..6]), ms(1_000)) {
            DownloadAction::ReadHeader { max, budget } => assert_eq!((max, budget), (10, ms(500))),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_failed_header_write_aborts_the_session() {
        let mut m = reading_header(Deadline::none(), 100);
        m.poll(I::header_read(&header_for(ImageChip::Esp32C3)), ms(1));
        assert_eq!(kind(&m.poll(I::Prepared, ms(1))), K::WriteHeader);
        let a = m.poll(I::WriteFailed(OtaError::FlashWriteFailed), ms(1));
        assert_eq!(kind(&a), K::Abort(OtaError::FlashWriteFailed));
        assert_eq!(
            kind(&m.poll(I::Aborted, ms(1))),
            K::Done(Err(OtaError::FlashWriteFailed))
        );
    }

    #[test]
    fn a_target_mismatch_is_reported_as_policy_1007() {
        assert_eq!(OtaError::TargetMismatch.code(), "target_mismatch");
        assert_eq!(
            crate::ota::FailReason::from(OtaError::TargetMismatch).as_str(),
            crate::ota::FailReason::TargetMismatch.as_str()
        );
    }
}
