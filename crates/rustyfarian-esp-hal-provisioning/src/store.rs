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
use provisioning_pure::ProvisioningConfig;

use crate::record::{
    decode_record, encode_record, pad_to_write_granularity, pick_active, DecodedRecord,
    SECTOR_SIZE, STORE_SIZE,
};

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
    /// `base_offset + 8192` would extend past `total_bytes` or the flash
    /// device's reported capacity. Catches misconfigured partitions on a cold
    /// path so the first `save` does not fail late with a generic flash error.
    OffsetOutOfBounds {
        /// The byte beyond the last byte the store would touch.
        end: u32,
        /// The upper bound the store must not cross (the smaller of
        /// `total_bytes` and `flash.capacity()`).
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
    /// Open a store over `total_bytes` of flash starting at `base_offset`.
    ///
    /// Fails if `total_bytes < 8192`, if `base_offset` is not a multiple of
    /// `F::ERASE_SIZE`, if `F::ERASE_SIZE` is not 4096, or if
    /// `base_offset + 8192` would extend past `total_bytes` or the flash
    /// device's reported capacity. The last check catches a misconfigured
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
        // Upper bound: the store must fit within both `total_bytes` and the
        // flash device's own capacity. `saturating_add` keeps the check sound
        // even if the caller hands us a near-`u32::MAX` `base_offset`.
        let end = base_offset.saturating_add(STORE_SIZE);
        let capacity = u32::try_from(flash.capacity()).unwrap_or(u32::MAX);
        let limit = total_bytes.min(capacity);
        if end > limit {
            return Err(StoreError::OffsetOutOfBounds { end, limit });
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
    fn try_read_sector(&mut self, sector_index: u32) -> Result<Option<DecodedRecord>, StoreError> {
        let mut buf = [0u8; SECTOR_SIZE];
        self.flash
            .read(self.sector_offset(sector_index), &mut buf)
            .map_err(|_| StoreError::Flash)?;
        Ok(decode_record(&buf).ok())
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
    /// `save` allocates a 4 KiB encode buffer; `try_read_sector` (called by
    /// `plan_save`) allocates another 4 KiB read buffer that is live across
    /// most of `save`. Peak stack on the hot path is therefore ~4–5 KiB
    /// before any task overhead. This matches `SECTOR_SIZE` and is acceptable
    /// for the spike but is on the Phase 2 entry-conditions list — the
    /// planned optimisation is to read only a 12-byte header prefix first to
    /// learn `record_len`, then read just `record_len` bytes into a smaller
    /// buffer. Sizing notes for `embassy::executor` tasks that own a
    /// `ProvisioningStore` should budget at least 6 KiB on top of their own
    /// requirements until that change lands. See
    /// `docs/features/esp-hal-provisioning-v1.md` Session Log (2026-06-15
    /// Phase 2 entry conditions) for the full plan.
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
    }

    impl MockFlash {
        pub(super) fn new() -> Self {
            Self {
                data: [0xFF; 8192],
                erased: [true; 2],
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
            self.data.len()
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
    use super::*;
    use heapless::String as HS;
    use provisioning_pure::{
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
            hs::<{ wifi_pure::SSID_MAX_LEN }>("my-net"),
            hs::<{ wifi_pure::PASSWORD_MAX_LEN }>("p@ssw0rd1"),
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
    fn store_open_rejects_offset_past_total_bytes() {
        // base_offset(4096) + STORE_SIZE(8192) = 12288, but caller only
        // declared total_bytes = 8192 — sector 1 would land outside the
        // declared partition. Must surface on the cold path, not on first save.
        let flash = MockFlash::new();
        let result = ProvisioningStore::open(flash, 4096, 8192);
        assert_eq!(
            result.err(),
            Some(StoreError::OffsetOutOfBounds {
                end: 12288,
                limit: 8192,
            })
        );
    }

    #[test]
    fn store_open_rejects_offset_past_flash_capacity() {
        // total_bytes claims 16 KiB, but the mock's capacity is only 8 KiB.
        // The min(total_bytes, capacity) check must catch that the store
        // would extend beyond the device itself even though `total_bytes`
        // alone would have allowed it.
        let flash = MockFlash::new(); // capacity == 8192
        let result = ProvisioningStore::open(flash, 4096, 16384);
        assert_eq!(
            result.err(),
            Some(StoreError::OffsetOutOfBounds {
                end: 12288,
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
}
