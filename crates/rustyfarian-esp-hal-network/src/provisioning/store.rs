//! Flash-backed credential store with A/B torn-write recovery.
//!
//! The store owns two adjacent flash sectors and writes one record per save,
//! alternating between sectors. A higher monotonic sequence counter wins; a
//! corrupted higher-seq sector falls back to the standby. All public functions
//! return [`StoreError`] for any failure mode; error variants carry only
//! lengths and expectations — never input bytes — so error output cannot leak
//! credentials.

use core::fmt;

use embedded_storage::nor_flash::NorFlash;
use heapless::Vec as HVec;
use juggler::provisioning::ProvisioningConfig;

use super::record::{
    decode_record, encode_record, pad_to_write_granularity, pick_active, DecodedRecord,
    HEADER_FIXED, RECORD_LEN_OFFSET, SECTOR_SIZE, STORE_SIZE,
};

/// Maximum byte length of a single encoded record across all profiles.
///
/// Worst-case is `WifiMqttDevice` with all fields filled to their cap:
/// - Fixed header:     12 B
/// - Profile string:    9 B ("wifi_mqtt")
/// - TLV wifi_ssid:    34 B (2+32)
/// - TLV wifi_pass:    66 B (2+64)
/// - TLV ota_url:     130 B (2+128)
/// - TLV device_name:  26 B (2+24)
/// - TLV mqtt_host:    66 B (2+64)
/// - TLV mqtt_port:     4 B (2+2)
/// - TLV mqtt_user:    66 B (2+64)
/// - TLV mqtt_pass:    66 B (2+64)
/// - TLV mqtt_client:  25 B (2+23)
/// - CRC32:             4 B
///
/// Sum: 508 B. `512` is the next power-of-two ceiling.
const MAX_RECORD_LEN: usize = 512;

// ── Errors ────────────────────────────────────────────────────────────────────

/// Errors returned by [`ProvisioningStore`].
///
/// # Security invariant
///
/// Variants carry only lengths and expected values — never input bytes from a
/// record. `Debug` and any future `Display` impl must preserve this invariant
/// so error output cannot leak credential material.
///
/// The [`StoreError::Flash`] variant intentionally carries no inner error
/// detail: doing so would impose a viral `where F::Error: Debug` bound on
/// every `NorFlash` impl in the dep graph. The structured cause is observable
/// at the call site via downstream logging.
#[derive(Clone, PartialEq, Eq)]
pub enum StoreError {
    /// `total_bytes < 8192` at `open` time.
    TooSmall {
        /// The required minimum.
        required: u32,
        /// What the caller passed.
        provided: u32,
    },
    /// `base_offset` is not aligned to the flash erase granularity.
    NotAligned,
    /// The declared region `[base_offset, base_offset + total_bytes)` extends
    /// past the flash device's reported capacity. Catches misconfigured
    /// partitions on a cold path so the first `save` does not fail late with a
    /// generic flash error.
    OffsetOutOfBounds {
        /// The end of the declared region (`base_offset + total_bytes`).
        end: u32,
        /// The upper bound the region must not cross (`flash.capacity()`).
        limit: u32,
    },
    /// The `NorFlash` impl reports a non-4096 `ERASE_SIZE` — unsupported in v1.
    UnsupportedGeometry {
        /// The reported erase size.
        erase_size: u32,
    },
    /// Magic-bytes prefix did not match.
    BadMagic,
    /// Layout-version byte did not match.
    BadVersion {
        /// Version byte read from the record.
        found: u8,
        /// Version this crate writes.
        expected: u8,
    },
    /// CRC over the payload did not match the stored CRC word.
    BadCrc,
    /// Record slice is shorter than required.
    ShortRecord {
        /// Bytes required for a valid record at this point.
        need: usize,
        /// Bytes actually present.
        have: usize,
    },
    /// Profile-discriminator string did not match any known `SchemaProfile`.
    UnknownProfile {
        /// Length of the unrecognised string (lengths are safe to log; bytes
        /// are not).
        len: u8,
    },
    /// Encode buffer is smaller than the record requires.
    BufferTooSmall {
        /// Bytes required.
        need: usize,
        /// Bytes available.
        have: usize,
    },
    /// The `ProvisioningConfig` carries non-empty `extras`. The v1 record
    /// layout does not allocate TLV tags for opaque extras, so a silent drop
    /// would lose user data on `save → load`. Reject explicitly until a future
    /// layout revision adds extras support.
    ExtrasNotSupported {
        /// Number of extras present on the config.
        count: usize,
    },
    /// A required TLV field was absent from a CRC-valid record — refuse
    /// rather than synthesise an empty string or a default value, since a
    /// CRC-valid record can be missing fields if the producer was buggy or
    /// the format predates a new required tag.
    MissingRequiredField {
        /// The TLV tag of the missing field (constant from the tag table).
        tag: u8,
    },
    /// Two TLVs in the same record carry the same tag. The encoder never
    /// emits duplicates, so a duplicate in a CRC-valid record implies a
    /// buggy or adversarial producer; refuse rather than pick last-wins.
    DuplicateTag {
        /// The TLV tag that appeared more than once.
        tag: u8,
    },
    /// A TLV's value bytes are not valid UTF-8 for a field that is decoded
    /// as a string. Refusing here is the last bit of "fail closed on a
    /// malformed-but-CRC-valid record" — the encoder only ever writes
    /// validated `heapless::String` content, so an invalid-UTF-8 TLV in a
    /// CRC-valid record implies a buggy or adversarial producer.
    InvalidUtf8 {
        /// The TLV tag whose value was not valid UTF-8.
        tag: u8,
    },
    /// The underlying flash returned an error.
    ///
    /// The inner cause is intentionally dropped to keep `StoreError: Debug`
    /// without a viral `where F::Error: Debug` bound. The typed flash failure
    /// is observable at the call site via downstream logging.
    Flash,
}

impl fmt::Debug for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StoreError::TooSmall { required, provided } => f
                .debug_struct("TooSmall")
                .field("required", required)
                .field("provided", provided)
                .finish(),
            StoreError::NotAligned => write!(f, "NotAligned"),
            StoreError::OffsetOutOfBounds { end, limit } => f
                .debug_struct("OffsetOutOfBounds")
                .field("end", end)
                .field("limit", limit)
                .finish(),
            StoreError::UnsupportedGeometry { erase_size } => f
                .debug_struct("UnsupportedGeometry")
                .field("erase_size", erase_size)
                .finish(),
            StoreError::BadMagic => write!(f, "BadMagic"),
            StoreError::BadVersion { found, expected } => f
                .debug_struct("BadVersion")
                .field("found", found)
                .field("expected", expected)
                .finish(),
            StoreError::BadCrc => write!(f, "BadCrc"),
            StoreError::ShortRecord { need, have } => f
                .debug_struct("ShortRecord")
                .field("need", need)
                .field("have", have)
                .finish(),
            StoreError::UnknownProfile { len } => {
                f.debug_struct("UnknownProfile").field("len", len).finish()
            }
            StoreError::BufferTooSmall { need, have } => f
                .debug_struct("BufferTooSmall")
                .field("need", need)
                .field("have", have)
                .finish(),
            StoreError::ExtrasNotSupported { count } => f
                .debug_struct("ExtrasNotSupported")
                .field("count", count)
                .finish(),
            StoreError::MissingRequiredField { tag } => f
                .debug_struct("MissingRequiredField")
                .field("tag", tag)
                .finish(),
            StoreError::DuplicateTag { tag } => {
                f.debug_struct("DuplicateTag").field("tag", tag).finish()
            }
            StoreError::InvalidUtf8 { tag } => {
                f.debug_struct("InvalidUtf8").field("tag", tag).finish()
            }
            StoreError::Flash => write!(f, "Flash"),
        }
    }
}

// ── Store ─────────────────────────────────────────────────────────────────────

/// Flash-backed credential store with A/B torn-write recovery.
///
/// Owns two 4 KiB sectors starting at `base_offset`. Each `save` writes the
/// new record into the inactive sector; the active sector is never overwritten
/// until the next `save` completes, so a power failure during write leaves the
/// previous record intact and recoverable.
pub struct ProvisioningStore<F: NorFlash> {
    flash: F,
    base_offset: u32,
}

impl<F: NorFlash> ProvisioningStore<F> {
    /// Open a store over a `total_bytes` region of flash starting at
    /// `base_offset`.
    ///
    /// `total_bytes` is the size of the partition region the store occupies,
    /// measured from `base_offset` — it is *not* an absolute extent from flash
    /// offset 0. A caller hands the store the size of the dedicated partition
    /// (e.g. `8192` for a two-sector store at a 3 MiB offset on a 4 MiB
    /// device).
    ///
    /// Fails if `total_bytes < 8192`, if `base_offset` is not a multiple of
    /// `F::ERASE_SIZE`, if `F::ERASE_SIZE` is not 4096, or if the declared
    /// region `[base_offset, base_offset + total_bytes)` would extend past the
    /// flash device's reported capacity. The last check catches a misconfigured
    /// partition table on a cold path so the first `save` does not fail late
    /// with a generic flash error from beyond the partition bound.
    pub fn open(flash: F, base_offset: u32, total_bytes: u32) -> Result<Self, StoreError> {
        if total_bytes < STORE_SIZE {
            return Err(StoreError::TooSmall {
                required: STORE_SIZE,
                provided: total_bytes,
            });
        }
        let erase_size = F::ERASE_SIZE as u32;
        if erase_size != SECTOR_SIZE as u32 {
            return Err(StoreError::UnsupportedGeometry { erase_size });
        }
        if !base_offset.is_multiple_of(erase_size) {
            return Err(StoreError::NotAligned);
        }
        // Upper bound: the declared partition region `[base_offset,
        // base_offset + total_bytes)` must fit within the flash device's own
        // capacity. `total_bytes` is the size of the region the store lives in
        // (measured from `base_offset`), not an absolute extent from offset 0,
        // so the region end is `base_offset + total_bytes`. `saturating_add`
        // keeps the check sound even if the caller hands us a near-`u32::MAX`
        // `base_offset`. The minimum-size guarantee (`total_bytes >=
        // STORE_SIZE`) is enforced above, so a region that fits also leaves
        // room for the two sectors the store actually touches.
        let end = base_offset.saturating_add(total_bytes);
        let capacity = u32::try_from(flash.capacity()).unwrap_or(u32::MAX);
        if end > capacity {
            return Err(StoreError::OffsetOutOfBounds {
                end,
                limit: capacity,
            });
        }
        Ok(Self { flash, base_offset })
    }

    fn sector_offset(&self, sector_index: u32) -> u32 {
        self.base_offset + sector_index * SECTOR_SIZE as u32
    }

    /// Read one sector from flash and decode it, if it contains a valid record.
    ///
    /// Returns `Ok(None)` for any decode failure (blank flash, torn write,
    /// magic / version mismatch, CRC mismatch, unknown profile) so the active
    /// sector arbitration can fall back to the standby. Flash read errors are
    /// surfaced as `Err(StoreError::Flash)`.
    ///
    /// # Two-step bounded read
    ///
    /// Instead of reading the full 4 KiB sector, this method performs two
    /// targeted reads:
    ///
    /// 1. Read the 12-byte fixed header to extract `record_len`.
    /// 2. Read exactly `record_len` bytes (≤ `MAX_RECORD_LEN`) into a
    ///    [`MAX_RECORD_LEN`]-byte stack buffer and hand that slice to
    ///    `decode_record`.
    ///
    /// This cuts the transient stack contribution of each `try_read_sector`
    /// call from 4 KiB to ≤ 512 B (`MAX_RECORD_LEN`).
    fn try_read_sector(&mut self, sector_index: u32) -> Result<Option<DecodedRecord>, StoreError> {
        let base = self.sector_offset(sector_index);

        // Step 1 — read the fixed header (12 bytes) to learn `record_len`.
        let mut header = [0u8; HEADER_FIXED];
        self.flash
            .read(base, &mut header)
            .map_err(|_| StoreError::Flash)?;

        // Extract `record_len` from the header.  `decode_record` will re-check
        // magic / version / bounds, so we do not need to validate here — if the
        // header is corrupt `record_len` may be nonsense and the subsequent
        // bounded read will cover a partial / wrong slice, but `decode_record`
        // will return `Err` on any structural violation.
        let record_len =
            u16::from_le_bytes([header[RECORD_LEN_OFFSET], header[RECORD_LEN_OFFSET + 1]]) as usize;

        // Clamp to a sensible maximum so a corrupt `record_len` field cannot
        // request a read larger than our stack buffer.
        let read_len = record_len.min(MAX_RECORD_LEN);

        // Step 2 — read exactly `read_len` bytes from the sector head.
        let mut buf = [0u8; MAX_RECORD_LEN];
        self.flash
            .read(base, &mut buf[..read_len])
            .map_err(|_| StoreError::Flash)?;

        Ok(decode_record(&buf[..read_len]).ok())
    }

    /// Returns `true` if either sector contains a valid record.
    pub fn is_provisioned(&mut self) -> Result<bool, StoreError> {
        let a = self.try_read_sector(0)?;
        let b = self.try_read_sector(1)?;
        Ok(pick_active(a, b).is_some())
    }

    /// Read the active record, if any.
    pub fn load(&mut self) -> Result<Option<ProvisioningConfig>, StoreError> {
        let a = self.try_read_sector(0)?;
        let b = self.try_read_sector(1)?;
        Ok(pick_active(a, b).map(|rec| rec.config))
    }

    /// Write `config` to flash.
    ///
    /// The encode buffer is a private `heapless::Vec<u8, 4096>` allocated on
    /// this function's stack frame. The buffer is overwritten with `0xFF`
    /// before this function returns — on the success path **and** every error
    /// path — bounding the credential-bytes window on the stack. This is not
    /// equivalent to `zeroize` (Rust drop semantics and optimiser elision
    /// preclude a true scrub); the scope is per ADR 015 §5.
    ///
    /// # Stack usage
    ///
    /// `save` allocates a 4 KiB `heapless::Vec` encode buffer on this
    /// function's frame.
    /// `try_read_sector` (called twice by `plan_save`) uses a two-step
    /// bounded read — a 12-byte header read followed by a read of exactly
    /// `record_len` bytes (≤ `MAX_RECORD_LEN` = 512 B) — so the transient
    /// read-buffer contribution is ~512 B, not the 4 KiB it was before the
    /// Phase 2B optimisation.
    ///
    /// The total transient stack contribution of `save` (encode buffer + read
    /// buffer + locals) is approximately **4.6 KiB**.
    ///
    /// Combined with the HTTP task's steady-state frame (~8.3 KiB: `req_buf`
    /// 2048 + `resp_buf` 6144 + executor overhead), the worst-case stack peak
    /// during a `POST /save` request is approximately **13 KiB**.
    /// The recommended integrator HTTP-task stack is **14 KiB minimum**,
    /// leaving headroom for ISR frames and stack canary.
    ///
    /// The steady-state HTTP frame is ~8 KiB (`req_buf` 2048 + `resp_buf`
    /// 6144 + executor overhead); `POST /save` transiently adds another
    /// ~4.6 KiB for `ProvisioningStore::save`'s encode + read buffers,
    /// raising the worst-case peak to ~13 KiB.
    /// Integrators should size the spawned HTTP task with at least 14 KiB of
    /// stack to leave headroom for ISR frames and stack canary.
    ///
    /// See `docs/features/esp-hal-provisioning-v1.md` Decisions
    /// "Locked at Phase 2B implementation" for the `DEFAULT_TX_BUF = 6144`
    /// rationale.
    ///
    /// # Hardware caller pre-condition
    ///
    /// On real hardware the caller must ensure no other flash user (radio TX,
    /// DMA, partition reader) is active during the call. `&mut self` enforces
    /// same-instance exclusion; the cross-system invariant is the caller's.
    pub fn save(&mut self, config: &ProvisioningConfig) -> Result<(), StoreError> {
        // One read pass yields both the next sequence number and the target
        // sector — cuts a `save` from 4 full-sector reads + 1 write to
        // 2 + 1, and removes the small TOCTOU window between the two reads.
        let SaveTarget {
            next_seq,
            target_sector,
        } = self.plan_save()?;

        let mut buf: HVec<u8, SECTOR_SIZE> = HVec::new();
        buf.resize_default(SECTOR_SIZE).expect("SECTOR_SIZE fits");

        // Run the fallible region inside a closure so the encode-buffer
        // overwrite below runs on every exit path, including error returns
        // from `encode_record`, `flash.erase`, and `flash.write`.
        let result: Result<(), StoreError> = (|| {
            let raw_len = encode_record(config, next_seq, &mut buf)?;
            let padded_len = pad_to_write_granularity(raw_len);
            // Ensure the bytes between the record's natural end and the
            // 4-byte-aligned write boundary are 0xFF, so the AND-semantics
            // merge with the freshly-erased sector is a no-op for those bytes.
            for byte in buf[raw_len..padded_len].iter_mut() {
                *byte = 0xFF;
            }

            let offset = self.sector_offset(target_sector);
            let erase_end = offset + SECTOR_SIZE as u32;
            self.flash
                .erase(offset, erase_end)
                .map_err(|_| StoreError::Flash)?;
            self.flash
                .write(offset, &buf[..padded_len])
                .map_err(|_| StoreError::Flash)?;
            Ok(())
        })();

        // Unconditional overwrite — runs on success and on every Err above.
        for byte in buf.iter_mut() {
            *byte = 0xFF;
        }
        result
    }

    /// Consume the store and return the underlying flash device.
    ///
    /// Useful for tests that want to inspect or manipulate the flash backing
    /// directly, or for applications that want to repurpose the flash device
    /// after deprovisioning.
    pub fn into_flash(self) -> F {
        self.flash
    }

    /// Erase both sectors, returning the store to the unprovisioned state.
    pub fn erase_all(&mut self) -> Result<(), StoreError> {
        let a_start = self.sector_offset(0);
        let b_end = self.sector_offset(2);
        self.flash
            .erase(a_start, b_end)
            .map_err(|_| StoreError::Flash)?;
        Ok(())
    }

    /// Single-pass replacement for the previous `next_seq` +
    /// `target_sector_for_save` pair: one read of each sector, then derive
    /// both the next sequence number and the target sector from the same
    /// observation.
    fn plan_save(&mut self) -> Result<SaveTarget, StoreError> {
        let a = self.try_read_sector(0)?;
        let b = self.try_read_sector(1)?;
        let target_sector = match (&a, &b) {
            (None, _) => 0,
            (_, None) => 1,
            (Some(a_rec), Some(b_rec)) => {
                if a_rec.seq >= b_rec.seq {
                    1
                } else {
                    0
                }
            }
        };
        let max_seq = pick_active(a, b).map(|rec| rec.seq).unwrap_or(0);
        let next_seq = max_seq.saturating_add(1);
        Ok(SaveTarget {
            next_seq,
            target_sector,
        })
    }
}

/// Result of `plan_save`: the sequence number to assign and the sector
/// (0 or 1) to overwrite.
struct SaveTarget {
    next_seq: u32,
    target_sector: u32,
}

// ── Mock NorFlash + tests ─────────────────────────────────────────────────────

#[cfg(test)]
mod test_flash_instrumented {
    extern crate alloc;
    use embedded_storage::nor_flash::{
        ErrorType, NorFlash, NorFlashError, NorFlashErrorKind, ReadNorFlash,
    };

    /// Wrapper around a byte slice that records each `read` call's `(offset,
    /// length)`.  Used to assert `try_read_sector`'s two-step bounded-read
    /// behaviour without touching real flash.
    pub(super) struct InstrumentedFlash {
        pub(super) data: [u8; 8192],
        pub(super) read_log: alloc::vec::Vec<(u32, usize)>,
    }

    impl InstrumentedFlash {
        pub(super) fn from_data(data: [u8; 8192]) -> Self {
            Self {
                data,
                read_log: alloc::vec::Vec::new(),
            }
        }
    }

    #[derive(Debug)]
    pub(super) struct IError;

    impl NorFlashError for IError {
        fn kind(&self) -> NorFlashErrorKind {
            NorFlashErrorKind::Other
        }
    }

    impl ErrorType for InstrumentedFlash {
        type Error = IError;
    }

    impl ReadNorFlash for InstrumentedFlash {
        const READ_SIZE: usize = 1;

        fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
            self.read_log.push((offset, bytes.len()));
            let start = offset as usize;
            let end = start + bytes.len();
            if end > self.data.len() {
                return Err(IError);
            }
            bytes.copy_from_slice(&self.data[start..end]);
            Ok(())
        }

        fn capacity(&self) -> usize {
            self.data.len()
        }
    }

    impl NorFlash for InstrumentedFlash {
        const WRITE_SIZE: usize = 4;
        const ERASE_SIZE: usize = 4096;

        fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
            let start = from as usize;
            let end = to as usize;
            if end > self.data.len() {
                return Err(IError);
            }
            for byte in &mut self.data[start..end] {
                *byte = 0xFF;
            }
            Ok(())
        }

        fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
            let start = offset as usize;
            let end = start + bytes.len();
            if end > self.data.len() {
                return Err(IError);
            }
            for (i, &b) in bytes.iter().enumerate() {
                self.data[start + i] &= b;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod test_flash {
    use embedded_storage::nor_flash::{
        ErrorType, NorFlash, NorFlashError, NorFlashErrorKind, ReadNorFlash,
    };

    /// In-memory mock flash, sized to match one [`super::ProvisioningStore`]:
    /// two 4 KiB sectors of `0xFF` bytes initially.
    pub(super) struct MockFlash {
        pub(super) data: [u8; 8192],
        /// Per-sector erase state. `true` = freshly erased and writable;
        /// `false` = contains at least one write and must be erased again
        /// before any further write to this sector.
        pub(super) erased: [bool; 2],
        /// Capacity the mock reports from `capacity()`. Defaults to the backing
        /// `data` length; `with_reported_capacity` raises it so `open`'s
        /// region-vs-capacity check can be exercised at a high `base_offset`
        /// without a multi-MiB backing buffer (`open` never reads flash).
        reported_capacity: usize,
    }

    impl MockFlash {
        pub(super) fn new() -> Self {
            Self {
                data: [0xFF; 8192],
                erased: [true; 2],
                reported_capacity: 8192,
            }
        }

        /// A mock that reports `cap` bytes of capacity while keeping the small
        /// backing buffer. Only valid for `open`-bound tests, which do not read
        /// or write flash.
        pub(super) fn with_reported_capacity(cap: usize) -> Self {
            Self {
                data: [0xFF; 8192],
                erased: [true; 2],
                reported_capacity: cap,
            }
        }
    }

    #[derive(Debug)]
    pub(super) struct MockError(pub(super) NorFlashErrorKind);

    impl NorFlashError for MockError {
        fn kind(&self) -> NorFlashErrorKind {
            self.0
        }
    }

    impl ErrorType for MockFlash {
        type Error = MockError;
    }

    impl ReadNorFlash for MockFlash {
        const READ_SIZE: usize = 1;

        fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
            let start = offset as usize;
            let end = start + bytes.len();
            if end > self.data.len() {
                return Err(MockError(NorFlashErrorKind::OutOfBounds));
            }
            bytes.copy_from_slice(&self.data[start..end]);
            Ok(())
        }

        fn capacity(&self) -> usize {
            self.reported_capacity
        }
    }

    impl NorFlash for MockFlash {
        const WRITE_SIZE: usize = 4;
        const ERASE_SIZE: usize = 4096;

        fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
            if !(from as usize).is_multiple_of(Self::ERASE_SIZE)
                || !(to as usize).is_multiple_of(Self::ERASE_SIZE)
            {
                return Err(MockError(NorFlashErrorKind::NotAligned));
            }
            let start = from as usize;
            let end = to as usize;
            if end > self.data.len() {
                return Err(MockError(NorFlashErrorKind::OutOfBounds));
            }
            for byte in &mut self.data[start..end] {
                *byte = 0xFF;
            }
            let start_sector = start / Self::ERASE_SIZE;
            let end_sector = end / Self::ERASE_SIZE;
            for s in start_sector..end_sector {
                self.erased[s] = true;
            }
            Ok(())
        }

        fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
            if !(offset as usize).is_multiple_of(Self::WRITE_SIZE)
                || !bytes.len().is_multiple_of(Self::WRITE_SIZE)
            {
                return Err(MockError(NorFlashErrorKind::NotAligned));
            }
            let start = offset as usize;
            let end = start + bytes.len();
            if end > self.data.len() {
                return Err(MockError(NorFlashErrorKind::OutOfBounds));
            }
            let start_sector = start / Self::ERASE_SIZE;
            let end_sector = end.div_ceil(Self::ERASE_SIZE);
            for s in start_sector..end_sector {
                assert!(
                    self.erased[s],
                    "write to non-erased sector {s} — real flash would refuse this"
                );
            }
            // AND-semantics merge: catches double-writes (any 0-bit cannot
            // become 1 without an erase).
            for (i, &b) in bytes.iter().enumerate() {
                self.data[start + i] &= b;
            }
            for s in start_sector..end_sector {
                self.erased[s] = false;
            }
            Ok(())
        }
    }

    /// Variant with a non-4096 `ERASE_SIZE` — used to exercise the
    /// `UnsupportedGeometry` rejection in `ProvisioningStore::open`.
    pub(super) struct MockFlash8KiB {
        data: [u8; 16384],
    }

    impl MockFlash8KiB {
        pub(super) fn new() -> Self {
            Self {
                data: [0xFF; 16384],
            }
        }
    }

    impl ErrorType for MockFlash8KiB {
        type Error = MockError;
    }

    impl ReadNorFlash for MockFlash8KiB {
        const READ_SIZE: usize = 1;

        fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
            let start = offset as usize;
            let end = start + bytes.len();
            if end > self.data.len() {
                return Err(MockError(NorFlashErrorKind::OutOfBounds));
            }
            bytes.copy_from_slice(&self.data[start..end]);
            Ok(())
        }

        fn capacity(&self) -> usize {
            self.data.len()
        }
    }

    impl NorFlash for MockFlash8KiB {
        const WRITE_SIZE: usize = 4;
        const ERASE_SIZE: usize = 8192;

        fn erase(&mut self, _from: u32, _to: u32) -> Result<(), Self::Error> {
            Ok(())
        }

        fn write(&mut self, _offset: u32, _bytes: &[u8]) -> Result<(), Self::Error> {
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_flash::{MockFlash, MockFlash8KiB};
    use super::test_flash_instrumented::InstrumentedFlash;
    use super::*;
    use heapless::String as HS;
    use juggler::provisioning::{
        config::{DEVICE_NAME_MAX_LEN, MQTT_HOST_MAX_LEN, OTA_URL_MAX_LEN},
        profile::MqttFields,
    };

    fn hs<const N: usize>(s: &str) -> HS<N> {
        let mut h = HS::new();
        let _ = h.push_str(&s[..s.len().min(N)]);
        h
    }

    fn sample_config() -> ProvisioningConfig {
        let mqtt = MqttFields::from_storage_parts(
            hs::<MQTT_HOST_MAX_LEN>("broker.example.com"),
            1883,
            None,
            None,
            None,
        );
        ProvisioningConfig::from_storage_parts(
            hs::<{ juggler::wifi::SSID_MAX_LEN }>("my-net"),
            hs::<{ juggler::wifi::PASSWORD_MAX_LEN }>("p@ssw0rd1"),
            hs::<OTA_URL_MAX_LEN>("http://ota.example.com/fw.bin"),
            hs::<DEVICE_NAME_MAX_LEN>("test-device"),
            None,
            Some(mqtt),
        )
    }

    #[test]
    fn store_open_rejects_below_8192() {
        let flash = MockFlash::new();
        let result = ProvisioningStore::open(flash, 0, 4096);
        assert_eq!(
            result.err(),
            Some(StoreError::TooSmall {
                required: 8192,
                provided: 4096,
            })
        );

        let flash = MockFlash::new();
        assert!(ProvisioningStore::open(flash, 0, 8192).is_ok());
    }

    #[test]
    fn store_open_rejects_unaligned_base_offset() {
        let flash = MockFlash::new();
        let result = ProvisioningStore::open(flash, 1, 8192);
        assert_eq!(result.err(), Some(StoreError::NotAligned));
    }

    #[test]
    fn store_open_rejects_non_4096_erase_size() {
        let flash = MockFlash8KiB::new();
        let result = ProvisioningStore::open(flash, 0, 16384);
        assert_eq!(
            result.err(),
            Some(StoreError::UnsupportedGeometry { erase_size: 8192 })
        );
    }

    #[test]
    fn store_open_accepts_region_at_high_offset_within_capacity() {
        // Regression (hardware, 2026-06-18): the C3 example opens the store at a
        // 3 MiB partition offset on a 4 MiB device with total_bytes = 8192 (the
        // partition size). `total_bytes` is the region size measured from
        // base_offset, NOT an absolute extent from offset 0, so the bound is
        // `base_offset + total_bytes <= capacity` (3145728 + 8192 = 3153920 <=
        // 4 MiB), which must succeed. The prior `base_offset + STORE_SIZE >
        // min(total_bytes, capacity)` check rejected this at boot.
        let flash = MockFlash::with_reported_capacity(4 * 1024 * 1024);
        let result = ProvisioningStore::open(flash, 0x0030_0000, 8192);
        assert!(
            result.is_ok(),
            "high-offset open within capacity must succeed, got {:?}",
            result.err()
        );
    }

    #[test]
    fn store_open_rejects_region_past_flash_capacity() {
        // The declared region [4096, 4096 + 16384) = [4096, 20480) extends past
        // the mock's 8 KiB capacity. The region-vs-capacity check must reject it
        // on the cold path rather than failing late on the first save.
        let flash = MockFlash::new(); // capacity == 8192
        let result = ProvisioningStore::open(flash, 4096, 16384);
        assert_eq!(
            result.err(),
            Some(StoreError::OffsetOutOfBounds {
                end: 20480,
                limit: 8192,
            })
        );
    }

    #[test]
    fn store_load_returns_none_on_blank_flash_0xff() {
        let mut store = ProvisioningStore::open(MockFlash::new(), 0, 8192).unwrap();
        assert!(store.load().unwrap().is_none());
        assert!(!store.is_provisioned().unwrap());
    }

    #[test]
    fn store_save_then_load_round_trips() {
        let mut store = ProvisioningStore::open(MockFlash::new(), 0, 8192).unwrap();
        let cfg = sample_config();
        store.save(&cfg).unwrap();
        let loaded = store.load().unwrap().expect("config present after save");
        assert_eq!(loaded.wifi_ssid(), "my-net");
        assert_eq!(loaded.wifi_password(), "p@ssw0rd1");
        assert_eq!(loaded.device_name(), "test-device");
        let mqtt = loaded.mqtt().expect("MQTT group present");
        assert_eq!(mqtt.host(), "broker.example.com");
        assert_eq!(mqtt.port(), 1883);
    }

    #[test]
    fn store_erase_all_makes_is_provisioned_false() {
        let mut store = ProvisioningStore::open(MockFlash::new(), 0, 8192).unwrap();
        store.save(&sample_config()).unwrap();
        assert!(store.is_provisioned().unwrap());
        store.erase_all().unwrap();
        assert!(!store.is_provisioned().unwrap());
    }

    #[test]
    fn store_corrupt_active_sector_falls_back_to_standby() {
        // Save twice (seq=1 → A, seq=2 → B), take back the flash, corrupt B,
        // re-open, and verify load returns the seq=1 record from A.
        let mut store = ProvisioningStore::open(MockFlash::new(), 0, 8192).unwrap();
        store.save(&sample_config()).unwrap();
        store.save(&sample_config()).unwrap();
        let mut flash = store.into_flash();

        // Corrupt a byte deep in sector B's record region.
        flash.data[4096 + 50] ^= 0xFF;

        let mut store = ProvisioningStore::open(flash, 0, 8192).unwrap();
        let loaded = store.load().unwrap().expect("falls back to sector A");
        assert_eq!(loaded.wifi_ssid(), "my-net");
    }

    #[test]
    fn store_save_writes_only_target_sector() {
        // Save once (→ sector A), take back the flash, snapshot A, re-open,
        // save again (→ sector B), confirm sector A is byte-identical.
        let mut store = ProvisioningStore::open(MockFlash::new(), 0, 8192).unwrap();
        store.save(&sample_config()).unwrap();
        let flash_after_first = store.into_flash();

        let mut snapshot = [0u8; 4096];
        snapshot.copy_from_slice(&flash_after_first.data[..4096]);

        let mut store = ProvisioningStore::open(flash_after_first, 0, 8192).unwrap();
        store.save(&sample_config()).unwrap();
        let flash_after_second = store.into_flash();

        assert_eq!(&flash_after_second.data[..4096], &snapshot[..]);
    }

    /// Verifies the two-step bounded-read contract of `try_read_sector`:
    ///
    /// For each sector the method must:
    /// 1. Issue one read of exactly [`HEADER_FIXED`] (12) bytes at the sector
    ///    base to extract `record_len`.
    /// 2. Issue one read of exactly `record_len` bytes (≤ [`MAX_RECORD_LEN`])
    ///    at the same sector base.
    ///
    /// Together `plan_save` (which calls `try_read_sector` on both sectors)
    /// must produce exactly four reads: two pairs of (12-byte header,
    /// record-body) — one pair per sector — rather than the old two reads of
    /// 4096 bytes each.
    ///
    /// This test also asserts that the record decoded through the bounded read
    /// equals the one obtained by writing via the store API, confirming that
    /// `decode_record` receives the right slice.
    #[test]
    fn try_read_sector_reads_only_record_len_bytes() {
        use super::super::record::encode_record;

        let cfg = sample_config();

        // Encode the config into a SECTOR_SIZE staging buffer so we can
        // compute the exact `record_len` the decoder will see.
        let mut staging = [0u8; SECTOR_SIZE];
        let record_len = encode_record(&cfg, 1, &mut staging).unwrap();

        // Build a flash image: sector 0 contains the encoded record, sector 1
        // is blank (0xFF — blank sector so `try_read_sector(1)` returns None).
        let mut flash_data = [0xFFu8; 8192];
        flash_data[..record_len].copy_from_slice(&staging[..record_len]);

        // The instrumented flash records every (offset, length) read call.
        let flash = InstrumentedFlash::from_data(flash_data);

        // Open a store; save() calls plan_save() which calls try_read_sector()
        // on both sectors.  We want to observe the reads without going through
        // save(), so we call load() instead — it also calls try_read_sector()
        // on both sectors internally.
        let mut store = ProvisioningStore::open(flash, 0, 8192).unwrap();
        let loaded = store.load().unwrap().expect("record present in sector 0");
        assert_eq!(loaded.wifi_ssid(), "my-net");

        // Retrieve the log.
        let log = store.into_flash().read_log;

        // load() calls try_read_sector(0) then try_read_sector(1).
        // Each call issues:
        //   read #1: (sector_base,     HEADER_FIXED)  = (offset, 12)
        //   read #2: (sector_base,     record_len)    = (offset, record_len)
        //
        // Sector 0 base = 0; sector 1 base = 4096.
        // Blank sector 1: header read returns 0xFF bytes → record_len decodes
        // to 0xFFFF (65535), clamped to MAX_RECORD_LEN (512).
        assert_eq!(log.len(), 4, "expected exactly 4 flash reads, got {log:?}");

        // Sector 0, read 1: 12-byte header.
        assert_eq!(log[0], (0, HEADER_FIXED), "sector 0 header read mismatch");

        // Sector 0, read 2: exactly `record_len` bytes (not SECTOR_SIZE).
        assert_eq!(
            log[1],
            (0, record_len),
            "sector 0 body read should be record_len={record_len}, not 4096"
        );
        assert!(
            record_len <= MAX_RECORD_LEN,
            "encoded record ({record_len} B) exceeds MAX_RECORD_LEN ({MAX_RECORD_LEN} B)"
        );

        // Sector 1, read 1: 12-byte header (returns 0xFF bytes).
        assert_eq!(
            log[2],
            (4096, HEADER_FIXED),
            "sector 1 header read mismatch"
        );

        // Sector 1, read 2: clamped to MAX_RECORD_LEN (0xFFFF → 512).
        assert_eq!(
            log[3],
            (4096, MAX_RECORD_LEN),
            "sector 1 body read should be clamped to MAX_RECORD_LEN"
        );
    }
}
