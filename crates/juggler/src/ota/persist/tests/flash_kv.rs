//! [`FlashKv`]: the `OtaKv` contract, the on-flash fallbacks, the erase bound, a power cut at every program and erase step of real store sequences, and the same sequences against the reference backend.

use alloc::format;
use alloc::rc::Rc;
use alloc::string::{String, ToString};
use alloc::vec;
use alloc::vec::Vec;
use core::cell::RefCell;

use embedded_storage::nor_flash::{
    ErrorType, NorFlash, NorFlashError, NorFlashErrorKind, ReadNorFlash,
};

use super::support::*;
use crate::ota::persist::fault_kv::FaultKv;
use crate::ota::persist::flash_kv::{MAX_ENTRIES, MAX_SNAPSHOT_BYTES};
use crate::ota::persist::keys::KEYS;
use crate::ota::persist::{note_boot_slot, FlashKv, KvError, KvStr, OtaKv, OtaStore};
use crate::ota::wire::RollbackReason;
use crate::ota::Version;

const SECTOR: usize = 4096;

// ---- mock flash ------------------------------------------------------------

#[derive(Default)]
struct Inner {
    data: Vec<u8>,
    /// Program words and erase halves done so far; a power cut is placed on one of them.
    steps: usize,
    cut_at: Option<usize>,
    dead: bool,
    erases: usize,
    bytes_written: usize,
    /// Words programmed over bits that were already 0 where the new word has a 1.
    overwrites: usize,
    /// The next write fails before programming anything.
    fail_next_write: bool,
    /// The next write programs everything, then reports an error.
    land_then_fail_next_write: bool,
}

impl Inner {
    /// Counts one step; `true` when the power is cut on it.
    fn step(&mut self) -> bool {
        self.steps += 1;
        self.cut_at == Some(self.steps)
    }
}

/// NOR flash with `READ_SIZE = WRITE_SIZE = 4`, 4 KiB sectors, AND-programming and power-cut injection; clones share one chip.
#[derive(Clone)]
struct MockNor(Rc<RefCell<Inner>>);

#[derive(Debug)]
struct MockError(NorFlashErrorKind);

impl NorFlashError for MockError {
    fn kind(&self) -> NorFlashErrorKind {
        self.0
    }
}

impl MockNor {
    fn new(sectors: usize) -> Self {
        Self(Rc::new(RefCell::new(Inner {
            data: vec![0xFF; sectors * SECTOR],
            ..Inner::default()
        })))
    }

    fn sectors(&self) -> u32 {
        (self.0.borrow().data.len() / SECTOR) as u32
    }

    fn steps(&self) -> usize {
        self.0.borrow().steps
    }

    fn erases(&self) -> usize {
        self.0.borrow().erases
    }

    fn bytes_written(&self) -> usize {
        self.0.borrow().bytes_written
    }

    fn overwrites(&self) -> usize {
        self.0.borrow().overwrites
    }

    fn reset_counters(&self) {
        let mut inner = self.0.borrow_mut();
        inner.steps = 0;
        inner.erases = 0;
        inner.bytes_written = 0;
    }

    fn cut_at(&self, step: usize) {
        self.0.borrow_mut().cut_at = Some(step);
    }

    fn revive(&self) {
        let mut inner = self.0.borrow_mut();
        inner.dead = false;
        inner.cut_at = None;
    }

    fn kill(&self) {
        self.0.borrow_mut().dead = true;
    }

    fn fail_next_write(&self) {
        self.0.borrow_mut().fail_next_write = true;
    }

    fn land_then_fail_next_write(&self) {
        self.0.borrow_mut().land_then_fail_next_write = true;
    }

    /// Clears bits of `data[at]` (a bit flip toward the programmed state).
    fn clear_bits(&self, at: usize, mask: u8) {
        self.0.borrow_mut().data[at] &= !mask;
    }

    fn fill(&self, from: usize, to: usize, byte: u8) {
        self.0.borrow_mut().data[from..to].fill(byte);
    }

    fn byte(&self, at: usize) -> u8 {
        self.0.borrow().data[at]
    }
}

impl ErrorType for MockNor {
    type Error = MockError;
}

impl ReadNorFlash for MockNor {
    const READ_SIZE: usize = 4;

    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), MockError> {
        let inner = self.0.borrow();
        if inner.dead {
            return Err(MockError(NorFlashErrorKind::Other));
        }
        let start = offset as usize;
        if !start.is_multiple_of(Self::READ_SIZE) || !bytes.len().is_multiple_of(Self::READ_SIZE) {
            return Err(MockError(NorFlashErrorKind::NotAligned));
        }
        let src = inner
            .data
            .get(start..start + bytes.len())
            .ok_or(MockError(NorFlashErrorKind::OutOfBounds))?;
        bytes.copy_from_slice(src);
        Ok(())
    }

    fn capacity(&self) -> usize {
        self.0.borrow().data.len()
    }
}

impl NorFlash for MockNor {
    const WRITE_SIZE: usize = 4;
    const ERASE_SIZE: usize = SECTOR;

    fn erase(&mut self, from: u32, to: u32) -> Result<(), MockError> {
        let mut inner = self.0.borrow_mut();
        if inner.dead {
            return Err(MockError(NorFlashErrorKind::Other));
        }
        let (from, to) = (from as usize, to as usize);
        if !from.is_multiple_of(SECTOR) || !to.is_multiple_of(SECTOR) || to > inner.data.len() {
            return Err(MockError(NorFlashErrorKind::NotAligned));
        }
        for sector in (from..to).step_by(SECTOR) {
            inner.erases += 1;
            // First half, then the rest: a cut leaves a half-erased sector.
            let cut = inner.step();
            inner.data[sector..sector + SECTOR / 2].fill(0xFF);
            if cut {
                inner.dead = true;
                return Err(MockError(NorFlashErrorKind::Other));
            }
            let cut = inner.step();
            inner.data[sector + SECTOR / 2..sector + SECTOR].fill(0xFF);
            if cut {
                inner.dead = true;
                return Err(MockError(NorFlashErrorKind::Other));
            }
        }
        Ok(())
    }

    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), MockError> {
        let mut inner = self.0.borrow_mut();
        if inner.dead {
            return Err(MockError(NorFlashErrorKind::Other));
        }
        let start = offset as usize;
        if !start.is_multiple_of(Self::WRITE_SIZE)
            || !bytes.len().is_multiple_of(Self::WRITE_SIZE)
            || start + bytes.len() > inner.data.len()
        {
            return Err(MockError(NorFlashErrorKind::NotAligned));
        }
        if core::mem::take(&mut inner.fail_next_write) {
            return Err(MockError(NorFlashErrorKind::Other));
        }
        for (i, word) in bytes.chunks(4).enumerate() {
            let at = start + i * 4;
            let cut = inner.step();
            // A cut programs only the first byte of the word.
            let n = if cut { 1 } else { 4 };
            for (j, &b) in word.iter().take(n).enumerate() {
                let old = inner.data[at + j];
                if old & b != b {
                    inner.overwrites += 1;
                }
                inner.data[at + j] = old & b;
            }
            if cut {
                inner.dead = true;
                return Err(MockError(NorFlashErrorKind::Other));
            }
        }
        inner.bytes_written += bytes.len();
        if core::mem::take(&mut inner.land_then_fail_next_write) {
            return Err(MockError(NorFlashErrorKind::Other));
        }
        Ok(())
    }
}

// ---- helpers ---------------------------------------------------------------

fn open(flash: &MockNor) -> FlashKv<MockNor> {
    FlashKv::open(flash.clone(), 0, flash.sectors() * SECTOR as u32).expect("opens")
}

/// One key as any of the three types.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Any {
    U8(u8),
    U32(u32),
    Str(String),
}

fn read_any(kv: &impl OtaKv, key: &str) -> Option<Any> {
    if let Ok(Some(v)) = kv.get_u8(key) {
        return Some(Any::U8(v));
    }
    if let Ok(Some(v)) = kv.get_u32(key) {
        return Some(Any::U32(v));
    }
    if let Ok(Some(v)) = kv.get_str(key) {
        return Some(Any::Str(v.to_string()));
    }
    None
}

/// Every key of the namespace.
type State = Vec<Option<Any>>;

fn state(kv: &impl OtaKv) -> State {
    KEYS.iter().map(|key| read_any(kv, key)).collect()
}

fn s32(fill: char) -> String {
    core::iter::repeat_n(fill, 32).collect()
}

// ---- the OtaKv contract ----------------------------------------------------

#[test]
fn absent_keys_read_none_and_a_type_mismatch_is_an_error() {
    let flash = MockNor::new(2);
    let mut kv = open(&flash);
    assert_eq!(kv.get_u8("rb"), Ok(None));
    assert_eq!(kv.get_u32("rb"), Ok(None));
    assert_eq!(kv.get_str("rb"), Ok(None));
    kv.set_u8("rb", 1).expect("set");
    kv.set_u32("att_id", 7).expect("set");
    kv.set_str("att_ver", "1.2.3").expect("set");
    let mismatch = Err(KvError::CODE_TYPE_MISMATCH);
    assert_eq!(kv.get_u32("rb").map_err(|e| e.code), mismatch);
    assert_eq!(
        kv.get_str("rb").map(|_| ()).map_err(|e| e.code),
        mismatch.map(|_: Option<u32>| ())
    );
    assert_eq!(
        kv.get_u8("att_id").map(|_| ()).map_err(|e| e.code),
        mismatch.map(|_| ())
    );
    assert_eq!(
        kv.get_u8("att_ver").map(|_| ()).map_err(|e| e.code),
        mismatch.map(|_| ())
    );
    let kv = open(&flash);
    assert_eq!(kv.get_u8("rb"), Ok(Some(1)));
    assert_eq!(kv.get_u32("att_id"), Ok(Some(7)));
    assert_eq!(
        kv.get_str("att_ver").ok().flatten().as_deref(),
        Some("1.2.3")
    );
    assert_eq!(flash.overwrites(), 0);
}

#[test]
fn a_remove_of_an_absent_key_or_an_unchanged_set_writes_nothing() {
    let flash = MockNor::new(2);
    let mut kv = open(&flash);
    kv.set_u32("att_ctr", 3).expect("set");
    flash.reset_counters();
    assert_eq!(kv.remove("rq_from"), Ok(()));
    assert_eq!(kv.set_u32("att_ctr", 3), Ok(()));
    assert_eq!(flash.steps(), 0);
    assert_eq!(kv.remove("att_ctr"), Ok(()));
    assert!(flash.steps() > 0);
    assert_eq!(open(&flash).get_u32("att_ctr"), Ok(None));
}

#[test]
fn a_type_changing_set_replaces_the_key_in_one_commit() {
    let flash = MockNor::new(2);
    let mut kv = open(&flash);
    kv.set_str("rb", "x").expect("set");
    flash.reset_counters();
    kv.set_u8("rb", 1).expect("replaces");
    // One snapshot of one key in one write, no erase: 8 header, 5 entry and 4 CRC bytes, padded to 20.
    assert_eq!(flash.erases(), 0);
    assert_eq!(flash.bytes_written(), 20);
    for kv in [kv, open(&flash)] {
        assert_eq!(kv.get_u8("rb"), Ok(Some(1)));
        assert_eq!(
            kv.get_str("rb").map_err(|e| e.code),
            Err(KvError::CODE_TYPE_MISMATCH)
        );
    }
}

#[test]
fn all_keys_at_32_bytes_fit_one_snapshot() {
    let flash = MockNor::new(2);
    let mut kv = open(&flash);
    for (i, key) in KEYS.iter().enumerate() {
        kv.set_str(key, &s32(char::from(b'a' + i as u8)))
            .expect("fits");
    }
    let last = flash.bytes_written();
    flash.reset_counters();
    kv.set_str(KEYS[0], &s32('z')).expect("fits");
    assert!(flash.bytes_written() <= MAX_SNAPSHOT_BYTES);
    assert!(flash.bytes_written() >= last / KEYS.len());
    let kv = open(&flash);
    for (i, key) in KEYS.iter().enumerate().skip(1) {
        assert_eq!(
            kv.get_str(key).ok().flatten().map(|v| v.to_string()),
            Some(s32(char::from(b'a' + i as u8)))
        );
    }
    assert_eq!(
        kv.get_str(KEYS[0]).ok().flatten().map(|v| v.to_string()),
        Some(s32('z'))
    );
}

#[test]
fn a_key_beyond_the_capacity_is_refused_without_a_write() {
    let flash = MockNor::new(2);
    let mut kv = open(&flash);
    for i in 0..MAX_ENTRIES {
        kv.set_u8(&format!("k{i}"), 1).expect("fits");
    }
    flash.reset_counters();
    assert_eq!(
        kv.set_u8("one_more", 1).map_err(|e| e.code),
        Err(KvError::CODE_STORE_FULL)
    );
    assert_eq!(flash.steps(), 0);
    assert_eq!(kv.set_u8("k0", 2), Ok(()));
}

#[test]
fn over_long_non_ascii_values_and_bad_keys_are_refused() {
    let flash = MockNor::new(2);
    let mut kv = open(&flash);
    let invalid = Err(KvError::new(KvError::CODE_INVALID_VALUE));
    assert_eq!(kv.set_str("att_ver", &format!("{}x", s32('a'))), invalid);
    assert_eq!(kv.set_str("att_ver", "\u{e9}"), invalid);
    assert_eq!(kv.set_u8("", 1), invalid);
    assert_eq!(kv.set_u8("sixteen_bytes_ky", 1), invalid);
    assert_eq!(kv.set_u8("k\u{e9}", 1), invalid);
    assert_eq!(flash.steps(), 0);
    assert_eq!(kv.set_str("att_ver", &s32('a')), Ok(()));
}

#[test]
fn the_mock_rejects_misaligned_reads_like_esp_storage() {
    let mut flash = MockNor::new(2);
    let mut buf = [0u8; 8];
    assert!(flash.read(1, &mut buf[..4]).is_err());
    assert!(flash.read(0, &mut buf[..3]).is_err());
    assert!(flash.read(4, &mut buf).is_ok());
    // Every test here runs FlashKv against this mock, so FlashKv never reads unaligned.
    let mut kv = open(&flash);
    kv.set_str("att_ver", "1.0.1").expect("aligned");
    assert_eq!(
        open(&flash).get_str("att_ver").ok().flatten().as_deref(),
        Some("1.0.1")
    );
}

#[test]
fn the_region_must_be_aligned_large_enough_and_inside_the_flash() {
    let flash = MockNor::new(3);
    let code = |r: Result<FlashKv<MockNor>, KvError>| r.map(|_| ()).map_err(|e| e.code);
    let geometry = Err(KvError::CODE_GEOMETRY);
    assert_eq!(code(FlashKv::open(flash.clone(), 4, 2 * 4096)), geometry);
    assert_eq!(code(FlashKv::open(flash.clone(), 0, 4096)), geometry);
    assert_eq!(code(FlashKv::open(flash.clone(), 0, 8191)), geometry);
    assert_eq!(code(FlashKv::open(flash.clone(), 4096, 3 * 4096)), geometry);
    assert_eq!(
        code(FlashKv::open(flash.clone(), u32::MAX - 4095, 8192)),
        geometry
    );
    assert_eq!(code(FlashKv::open(flash.clone(), 4096, 2 * 4096)), Ok(()));
    flash.kill();
    assert_eq!(
        code(FlashKv::open(flash.clone(), 0, 2 * 4096)),
        Err(KvError::CODE_FLASH)
    );
}

#[test]
fn a_store_in_the_upper_sectors_leaves_the_lower_ones_alone() {
    let flash = MockNor::new(4);
    flash.fill(0, 2 * SECTOR, 0x5A);
    let mut kv = FlashKv::open(flash.clone(), 2 * SECTOR as u32, 2 * SECTOR as u32).expect("opens");
    for i in 0..200u32 {
        kv.set_u32("att_ctr", i).expect("set");
    }
    assert_eq!(flash.byte(0), 0x5A);
    assert_eq!(flash.byte(2 * SECTOR - 1), 0x5A);
    let kv = FlashKv::open(flash.clone(), 2 * SECTOR as u32, 2 * SECTOR as u32).expect("opens");
    assert_eq!(kv.get_u32("att_ctr"), Ok(Some(199)));
}

// ---- fallbacks -------------------------------------------------------------

/// A store with two snapshots in sector 0: `att_ctr` 1, then 2. Returns the offset of the second.
fn two_snapshots() -> (MockNor, usize) {
    let flash = MockNor::new(2);
    let mut kv = open(&flash);
    kv.set_u32("att_ctr", 1).expect("set");
    let second = flash.bytes_written();
    kv.set_u32("att_ctr", 2).expect("set");
    (flash, second)
}

#[test]
fn a_bad_crc_falls_back_to_the_previous_snapshot() {
    let (flash, second) = two_snapshots();
    flash.clear_bits(second + 12, 0x01);
    assert_eq!(open(&flash).get_u32("att_ctr"), Ok(Some(1)));
}

#[test]
fn a_torn_tail_falls_back_and_the_next_commit_goes_to_an_erased_sector() {
    let (flash, second) = two_snapshots();
    // A torn write: only the first words of the second snapshot made it.
    flash.fill(second + 8, second + 24, 0xFF);
    let mut kv = open(&flash);
    assert_eq!(kv.get_u32("att_ctr"), Ok(Some(1)));
    assert!(!kv.recovered_from_corruption());
    flash.reset_counters();
    kv.set_u32("att_ctr", 3).expect("set");
    assert_eq!(flash.erases(), 1);
    assert_eq!(flash.overwrites(), 0);
    assert_eq!(open(&flash).get_u32("att_ctr"), Ok(Some(3)));
}

#[test]
fn after_a_failed_write_in_a_fresh_sector_the_newest_snapshot_is_never_erased() {
    let (flash, second) = two_snapshots();
    flash.fill(second + 8, second + 24, 0xFF);
    let mut kv = open(&flash);
    // The torn tail sends the commit to sector 1, whose write fails.
    flash.fail_next_write();
    assert!(kv.set_u32("att_ctr", 3).is_err());
    // The retry erases sector 1 again, not sector 0 with the only valid snapshot; cut it there.
    flash.reset_counters();
    flash.cut_at(1);
    assert!(kv.set_u32("att_ctr", 4).is_err());
    flash.revive();
    assert_eq!(open(&flash).get_u32("att_ctr"), Ok(Some(1)));
}

#[test]
fn an_erased_sector_falls_back_to_the_snapshot_in_the_other() {
    let flash = MockNor::new(2);
    let mut kv = open(&flash);
    let mut value = 0u32;
    while flash.erases() == 0 {
        value += 1;
        kv.set_u32("att_ctr", value).expect("set");
    }
    // The newest snapshot opened sector 1; sector 0 holds the one before it.
    flash.fill(SECTOR, 2 * SECTOR, 0xFF);
    let mut kv = open(&flash);
    assert_eq!(kv.get_u32("att_ctr"), Ok(Some(value - 1)));
    kv.set_u32("att_ctr", 100).expect("set");
    assert_eq!(open(&flash).get_u32("att_ctr"), Ok(Some(100)));
    assert_eq!(flash.overwrites(), 0);
}

#[test]
fn foreign_data_opens_as_an_empty_store() {
    let flash = MockNor::new(3);
    flash.fill(0, 3 * SECTOR, 0x00);
    flash.fill(0, 4, 0xFE);
    let mut kv = open(&flash);
    assert_eq!(state(&kv), vec![None; KEYS.len()]);
    assert!(!kv.recovered_from_corruption());
    kv.set_u8("rb", 1).expect("set");
    assert_eq!(flash.overwrites(), 0);
    assert_eq!(open(&flash).get_u8("rb"), Ok(Some(1)));
}

#[test]
fn a_blank_region_and_plausible_looking_foreign_data_open_empty_without_the_flag() {
    let blank = MockNor::new(3);
    let kv = open(&blank);
    assert_eq!(state(&kv), vec![None; KEYS.len()]);
    assert!(!kv.recovered_from_corruption());

    // Every position looks like a header (format byte, in-range length) but nothing passes its CRC.
    let foreign = MockNor::new(6);
    foreign.fill(0, 6 * SECTOR, 0x01);
    let mut kv = open(&foreign);
    assert_eq!(state(&kv), vec![None; KEYS.len()]);
    assert!(!kv.recovered_from_corruption());
    kv.set_u8("rb", 1).expect("set");
    assert_eq!(foreign.overwrites(), 0);
    assert_eq!(open(&foreign).get_u8("rb"), Ok(Some(1)));
}

/// Three snapshots in sector 0 (`att_ctr` 1, 2, 3); returns the flash and the padded snapshot size.
fn three_snapshots() -> (MockNor, usize) {
    let flash = MockNor::new(2);
    let mut kv = open(&flash);
    kv.set_u32("att_ctr", 1).expect("set");
    let snapshot = flash.bytes_written();
    kv.set_u32("att_ctr", 2).expect("set");
    kv.set_u32("att_ctr", 3).expect("set");
    (flash, snapshot)
}

/// Opens, checks the newest value and the flag, then commits on: the commits survive a reopen and never program over written bits.
fn assert_recovered(flash: &MockNor) {
    let mut kv = open(flash);
    assert_eq!(kv.get_u32("att_ctr"), Ok(Some(3)));
    assert!(kv.recovered_from_corruption());
    for value in 4..300u32 {
        kv.set_u32("att_ctr", value).expect("set");
        if value % 37 == 0 {
            let reopened = open(flash);
            assert_eq!(reopened.get_u32("att_ctr"), Ok(Some(value)));
        }
    }
    assert_eq!(open(flash).get_u32("att_ctr"), Ok(Some(299)));
    assert_eq!(flash.overwrites(), 0);
}

#[test]
fn a_corrupt_first_snapshot_does_not_hide_the_later_ones() {
    let (flash, _) = three_snapshots();
    flash.clear_bits(12, 0x01);
    assert_recovered(&flash);
}

#[test]
fn a_corrupt_length_of_the_first_snapshot_does_not_hide_the_later_ones() {
    // Out of range, in range but short, and in range but spanning the next snapshots.
    for (at, byte) in [(3, 0xFF), (2, 0x10), (2, 0x40), (2, 0x00)] {
        let (flash, _) = three_snapshots();
        flash.fill(at, at + 1, byte);
        assert_recovered(&flash);
    }
}

#[test]
fn a_corrupt_middle_snapshot_keeps_the_newest_and_the_flag() {
    let (flash, snapshot) = three_snapshots();
    flash.clear_bits(snapshot + 12, 0x01);
    assert_recovered(&flash);
}

#[test]
fn a_torn_tail_after_a_corrupt_snapshot_falls_back_to_the_last_valid_one() {
    let (flash, snapshot) = three_snapshots();
    flash.clear_bits(12, 0x01);
    // Tear the third snapshot.
    flash.fill(2 * snapshot + 8, 2 * snapshot + 24, 0xFF);
    let mut kv = open(&flash);
    assert_eq!(kv.get_u32("att_ctr"), Ok(Some(2)));
    assert!(kv.recovered_from_corruption());
    kv.set_u32("att_ctr", 9).expect("set");
    assert_eq!(open(&flash).get_u32("att_ctr"), Ok(Some(9)));
    assert_eq!(flash.overwrites(), 0);
}

#[test]
fn a_valid_snapshot_after_garbage_keeps_its_sector_until_a_newer_one_lands() {
    let (flash, snapshot) = three_snapshots();
    flash.clear_bits(12, 0x01);
    // The rest of sector 0 is dirty, so the next commit rotates to sector 1.
    flash.fill(3 * snapshot, SECTOR, 0x00);
    let mut kv = open(&flash);
    assert_eq!(kv.get_u32("att_ctr"), Ok(Some(3)));
    assert!(kv.recovered_from_corruption());
    // A power cut during that rotation leaves the newest snapshot in sector 0 intact.
    flash.reset_counters();
    flash.cut_at(1);
    assert!(kv.set_u32("att_ctr", 4).is_err());
    flash.revive();
    assert_eq!(open(&flash).get_u32("att_ctr"), Ok(Some(3)));
    assert_eq!(flash.erases(), 1);
    kv.set_u32("att_ctr", 5).expect("set");
    assert_eq!(open(&flash).get_u32("att_ctr"), Ok(Some(5)));
    assert_eq!(flash.overwrites(), 0);
}

/// The finding is stored by now: it reads `true` on a reopen, and `false` on the one after the acknowledgement.
fn assert_lost_until_acknowledged(flash: &MockNor) {
    let mut kv = open(flash);
    assert!(kv.newer_record_lost(), "the finding survives the reboot");
    kv.acknowledge_record_loss().expect("acknowledge");
    assert!(!kv.newer_record_lost());
    assert!(!open(flash).newer_record_lost());
}

#[test]
fn a_damaged_newest_snapshot_is_reported_until_it_is_acknowledged() {
    let (flash, snapshot) = three_snapshots();
    flash.clear_bits(2 * snapshot + 12, 0x01);
    let mut kv = open(&flash);
    assert_eq!(kv.get_u32("att_ctr"), Ok(Some(2)), "the older state");
    assert!(kv.newer_record_lost());
    assert!(OtaKv::newer_record_lost(&kv));
    // The commit that removes the damage (the boot-slot note of a real boot) stores the finding with it.
    kv.set_u32("att_ctr", 9).expect("set");
    assert!(kv.newer_record_lost(), "this boot keeps the finding");
    // Reboots and further commits, a sector rotation included, keep it.
    let mut kv = open(&flash);
    assert!(kv.newer_record_lost());
    flash.reset_counters();
    let mut value = 9;
    while flash.erases() < 2 {
        value += 1;
        kv.set_u32("att_ctr", value).expect("set");
    }
    let mut kv = open(&flash);
    assert_eq!(kv.get_u32("att_ctr"), Ok(Some(value)));
    assert!(kv.newer_record_lost());
    let before = state(&kv);
    OtaKv::acknowledge_record_loss(&mut kv).expect("acknowledge");
    assert!(!kv.newer_record_lost());
    let reopened = open(&flash);
    assert!(!reopened.newer_record_lost());
    assert_eq!(state(&reopened), before, "no key changed");
    assert_eq!(flash.overwrites(), 0);
}

#[test]
fn the_stored_finding_is_no_key_of_the_store() {
    let (flash, snapshot) = three_snapshots();
    flash.clear_bits(2 * snapshot + 12, 0x01);
    let mut kv = open(&flash);
    let marker = "\u{1}lost";
    assert_eq!(kv.get_u8(marker), Ok(None));
    assert_eq!(
        kv.set_u8(marker, 0).map_err(|e| e.code),
        Err(KvError::CODE_INVALID_VALUE)
    );
    kv.remove(marker).expect("an absent key");
    assert!(kv.newer_record_lost());
    kv.set_u32("att_ctr", 9).expect("set");
    assert_lost_until_acknowledged(&flash);
    // Without a finding the acknowledgement commits nothing.
    let mut kv = open(&flash);
    flash.reset_counters();
    kv.acknowledge_record_loss().expect("acknowledge");
    assert_eq!(flash.bytes_written(), 0);
}

#[test]
fn a_retry_after_a_failed_write_in_a_fresh_sector_never_skips_a_sector() {
    let flash = MockNor::new(3);
    let mut kv = open(&flash);
    let mut value = 0;
    while flash.erases() < 1 {
        value += 1;
        kv.set_u32("att_ctr", value).expect("set");
    }
    // The newest snapshot is the first of sector 1; a failed append sends the next commit to sector 2, whose write fails too.
    flash.fail_next_write();
    assert!(kv.set_u32("att_ctr", 100).is_err());
    flash.fail_next_write();
    assert!(kv.set_u32("att_ctr", 101).is_err());
    // The retry lands in sector 2 again, not two sectors on in sector 0.
    kv.set_u32("att_ctr", 102).expect("set");
    assert_ne!(flash.byte(2 * SECTOR), 0xFF);
    assert_eq!(open(&flash).get_u32("att_ctr"), Ok(Some(102)));
    // So its later damage is found where `open` looks for a lost snapshot.
    flash.clear_bits(2 * SECTOR + 12, 0x01);
    let kv = open(&flash);
    assert_eq!(kv.get_u32("att_ctr"), Ok(Some(value)), "the stale state");
    assert!(kv.newer_record_lost());
    assert_eq!(flash.overwrites(), 0);
}

#[test]
fn a_damaged_only_snapshot_opens_empty_and_is_reported() {
    let flash = MockNor::new(2);
    open(&flash).set_u32("att_ctr", 1).expect("set");
    flash.clear_bits(12, 0x01);
    let mut kv = open(&flash);
    assert_eq!(state(&kv), vec![None; KEYS.len()]);
    assert!(kv.newer_record_lost());
    kv.set_u8("rb", 1).expect("set");
    assert_lost_until_acknowledged(&flash);
    assert_eq!(flash.overwrites(), 0);
}

#[test]
fn a_damaged_first_snapshot_of_a_fresh_sector_is_reported() {
    let flash = MockNor::new(2);
    let mut kv = open(&flash);
    let mut value = 0;
    while flash.erases() < 1 {
        value += 1;
        kv.set_u32("att_ctr", value).expect("set");
    }
    // The newest snapshot is the only one of sector 1.
    flash.clear_bits(SECTOR + 12, 0x01);
    let mut kv = open(&flash);
    assert_eq!(kv.get_u32("att_ctr"), Ok(Some(value - 1)));
    assert!(kv.newer_record_lost());
    kv.set_u32("att_ctr", 7).expect("set");
    assert_lost_until_acknowledged(&flash);
}

#[test]
fn damage_anywhere_in_the_newest_committed_snapshot_is_reported() {
    let (probe, snapshot) = three_snapshots();
    let len = usize::from(u16::from_le_bytes([
        probe.byte(2 * snapshot + 2),
        probe.byte(2 * snapshot + 3),
    ]));
    // Format byte, entry count, both length bytes, the sequence number, the body, the CRC.
    let damage: [(usize, u8); 8] = [
        (0, 0x00),
        (1, 0xFE),
        (2, 0x00),
        (3, 0xFF),
        (4, 0x00),
        (7, 0x7F),
        (12, 0x00),
        (len - 1, 0x00),
    ];
    for (offset, byte) in damage {
        let (flash, snapshot) = three_snapshots();
        let at = 2 * snapshot + offset;
        assert_ne!(flash.byte(at), byte, "offset {offset} is not damaged");
        flash.fill(at, at + 1, byte);
        let mut kv = open(&flash);
        assert_eq!(kv.get_u32("att_ctr"), Ok(Some(2)), "offset {offset}");
        assert!(kv.newer_record_lost(), "offset {offset}");
        kv.set_u32("att_ctr", 9).expect("set");
        let reopened = open(&flash);
        assert_eq!(reopened.get_u32("att_ctr"), Ok(Some(9)));
        assert!(reopened.newer_record_lost(), "offset {offset}");
        assert_lost_until_acknowledged(&flash);
        assert_eq!(flash.overwrites(), 0);
    }
}

#[test]
fn a_write_cut_short_is_not_reported_before_its_last_word() {
    let (probe, _) = two_snapshots();
    probe.reset_counters();
    open(&probe).set_u32("att_ctr", 3).expect("set");
    let steps = probe.steps();
    assert!(steps > 2);
    for step in 1..=steps {
        let (flash, _) = two_snapshots();
        let mut kv = open(&flash);
        flash.reset_counters();
        flash.cut_at(step);
        let committed = kv.set_u32("att_ctr", 3).is_ok();
        flash.revive();
        let mut kv = open(&flash);
        let value = kv.get_u32("att_ctr").expect("reads");
        assert!(value == Some(2) || value == Some(3), "step {step}");
        if committed {
            assert_eq!(value, Some(3), "step {step}");
        }
        if step < steps {
            assert_eq!(
                value,
                Some(2),
                "step {step}: the first word is written last"
            );
            assert!(!kv.newer_record_lost(), "step {step}");
        }
        kv.set_u32("att_ctr", 9).expect("set");
        let reopened = open(&flash);
        assert_eq!(reopened.get_u32("att_ctr"), Ok(Some(9)));
        // Only a cut inside the last word may read as a loss, and that finding is then kept like any other.
        if step < steps {
            assert!(!reopened.newer_record_lost(), "step {step}");
        }
        assert_eq!(flash.overwrites(), 0);
    }
}

#[test]
fn older_damage_and_a_blank_region_are_not_reported() {
    // Damage in front of the adopted snapshot lost nothing newer.
    for at in [0, 12] {
        let (flash, snapshot) = three_snapshots();
        for at in [at, snapshot + at] {
            flash.clear_bits(at, 0x01);
        }
        let kv = open(&flash);
        assert_eq!(kv.get_u32("att_ctr"), Ok(Some(3)));
        assert!(!kv.newer_record_lost());
    }
    let blank = MockNor::new(3);
    assert!(!open(&blank).newer_record_lost());
}

#[test]
fn foreign_data_is_reported_and_erased_by_the_first_commit() {
    let flash = MockNor::new(4);
    flash.fill(SECTOR, 4 * SECTOR, 0x5A);
    let mut kv = open(&flash);
    assert_eq!(state(&kv), vec![None; KEYS.len()]);
    assert!(kv.newer_record_lost());
    flash.reset_counters();
    kv.set_u8("rb", 1).expect("set");
    assert_eq!(flash.erases(), 3, "every sector that was not blank");
    let reopened = open(&flash);
    assert_eq!(reopened.get_u8("rb"), Ok(Some(1)));
    assert_lost_until_acknowledged(&flash);
    assert_eq!(flash.overwrites(), 0);
}

#[test]
fn a_failed_write_keeps_the_state_and_the_next_commit_lands() {
    let (flash, _) = two_snapshots();
    let mut kv = open(&flash);
    flash.fail_next_write();
    assert_eq!(
        kv.set_u32("att_ctr", 3).map_err(|e| e.code),
        Err(KvError::CODE_FLASH)
    );
    assert_eq!(kv.get_u32("att_ctr"), Ok(Some(2)));
    kv.set_u32("att_ctr", 4).expect("set");
    assert_eq!(open(&flash).get_u32("att_ctr"), Ok(Some(4)));
    assert_eq!(flash.overwrites(), 0);
}

#[test]
fn a_write_that_landed_despite_its_error_never_outranks_the_next_one() {
    for sectors in [2, 3] {
        let flash = MockNor::new(sectors);
        let mut kv = open(&flash);
        for round in 0..40u32 {
            flash.land_then_fail_next_write();
            assert!(kv.set_u32("att_ctr", 2 * round + 1).is_err());
            kv.set_u32("att_ctr", 2 * round + 2).expect("set");
            assert_eq!(open(&flash).get_u32("att_ctr"), Ok(Some(2 * round + 2)));
        }
        assert_eq!(flash.overwrites(), 0);
    }
}

// ---- wear ------------------------------------------------------------------

#[test]
fn erases_stay_within_the_documented_bound() {
    for sectors in [2, 6] {
        let flash = MockNor::new(sectors);
        let mut kv = open(&flash);
        kv.set_str("att_ver", "12.34.56").expect("set");
        kv.set_str("rej_ver", "12.34.55").expect("set");
        flash.reset_counters();
        let n = 1000u32;
        let mut most = 0;
        for i in 0..n {
            let before = flash.erases();
            kv.set_u32("att_ctr", i | 0x0100_0000).expect("set");
            most = most.max(flash.erases() - before);
        }
        assert!(most <= 1, "a commit erases at most one sector");
        let snapshot = flash.bytes_written() / n as usize;
        let per_sector = SECTOR / snapshot;
        assert!(flash.erases() <= n as usize / per_sector + 1);
        assert_eq!(flash.overwrites(), 0);
        assert_eq!(
            open(&flash).get_u32("att_ctr"),
            Ok(Some((n - 1) | 0x0100_0000))
        );
    }
}

// ---- store sequences -------------------------------------------------------

/// Three update attempts that boot and complete, the third rolled back and reported.
fn lifecycle<K: OtaKv>(s: &mut OtaStore<K>) -> Vec<String> {
    let mut out = Vec::new();
    for round in 0..3u16 {
        let version = Version::new(2, round, 0);
        let slot = if round % 2 == 0 { SLOT_B } else { SLOT_A };
        let id = s.begin_attempt(version, slot, false);
        out.push(format!("{id:?}"));
        let Ok(id) = id else { return out };
        out.push(format!("{:?}", s.mark_boot_selected()));
        out.push(format!("{:?}", note_boot_slot(s, || Ok(slot))));
        if round < 2 {
            out.push(format!("{:?}", s.complete_attempt(version)));
        } else {
            out.push(format!(
                "{:?}",
                s.note_rollback(RollbackReason::HealthDeadline, slot)
            ));
            out.push(format!("{:?}", s.report_rollback(id, Some(version), false)));
            out.push(format!("{:?}", s.mark_report_delivered(id)));
            out.push(format!("{:?}", s.refuse_version(Version::new(9, 9, 9))));
        }
    }
    out
}

/// An operator rollback without an attempt, settled on the other slot.
fn operator_rollback<K: OtaKv>(s: &mut OtaStore<K>) -> Vec<String> {
    vec![
        format!("{:?}", s.note_rollback(RollbackReason::Operator, SLOT_A)),
        format!("{:?}", s.settle_request(SLOT_B)),
        format!("{:?}", s.pending_report().map(|v| format!("{v:?}"))),
        format!("{:?}", s.next_attempt_id()),
    ]
}

/// A DDI action that installs and is delivered, then a refused one.
#[cfg(feature = "ota-ddi")]
fn ddi_actions<K: OtaKv>(s: &mut OtaStore<K>) -> Vec<String> {
    use crate::ota::ddi::wire::{ActionId, PollResponse, ProfileCode};
    use crate::ota::ddi::{AdmittedOffer, Refusal};
    use crate::ota::persist::DdiVerdict;
    use crate::ota::wire::FailReason;

    let id = |text: &str| ActionId::parse(text).expect("canonical id");
    let link = PollResponse::parse(br#"{"_links":{"deploymentBase":{"href":"http://h/fw.bin"}}}"#)
        .expect("poll")
        .deployment_base
        .expect("link");
    let offer = AdmittedOffer {
        action: id("42"),
        version: V_NEW,
        artifact: link,
        sha256: [0; 32],
        size: 1234,
        success_detail: Some(FailReason::Downgrade),
    };
    let mut out = vec![format!("{:?}", s.begin_ddi_attempt(&offer, SLOT_B, false))];
    out.push(format!("{:?}", s.mark_boot_selected()));
    out.push(format!("{:?}", s.mark_activated()));
    out.push(format!("{:?}", s.complete_attempt(V_NEW)));
    out.push(format!("{:?}", s.pending_verdict()));
    out.push(format!("{:?}", s.mark_verdict_delivered(&id("42"))));
    let refused = DdiVerdict::refused(&Refusal {
        action: id("43"),
        code: ProfileCode::Policy,
        reason: FailReason::UpToDate,
        url_case: None,
    });
    out.push(format!("{:?}", s.persist_verdict(&refused)));
    out.push(format!("{:?}", s.mark_verdict_delivered(&id("43"))));
    out
}

/// Long reason and version strings, so every snapshot is large and the log wraps often.
fn prefill(kv: &mut impl OtaKv) {
    kv.set_str("rb_why", &s32('w')).expect("set");
    kv.set_str("rej_ver", "100.200.300").expect("set");
    kv.set_u32("rb_id", 0x00AB_CDEF).expect("set");
    kv.set_u8("rb", 0).expect("set");
}

#[test]
fn store_sequences_match_the_reference_backend() {
    fn compare(name: &str, flash_run: (Vec<String>, State), fault_run: (Vec<String>, State)) {
        assert_eq!(flash_run.0, fault_run.0, "{name}: results");
        assert_eq!(flash_run.1, fault_run.1, "{name}: end state");
    }

    macro_rules! both {
        ($name:literal, $scenario:ident) => {{
            let flash = MockNor::new(2);
            let mut kv = open(&flash);
            prefill(&mut kv);
            let mut s = OtaStore::open(open(&flash), entropy);
            let out = $scenario(&mut s);
            let flash_run = (out, state(&open(&flash)));
            assert_eq!(flash.overwrites(), 0);

            let fault = FaultKv::new();
            let mut kv = fault.clone();
            prefill(&mut kv);
            let mut s = store(&fault);
            let out = $scenario(&mut s);
            compare($name, flash_run, (out, state(&fault)));
        }};
    }
    both!("lifecycle", lifecycle);
    both!("operator rollback", operator_rollback);
    #[cfg(feature = "ota-ddi")]
    both!("ddi actions", ddi_actions);
}

// ---- power cut at every step -----------------------------------------------

/// One `set_*` or `remove` of a run: the flash steps it spanned and the state on both sides.
struct Transition {
    steps: core::ops::Range<usize>,
    before: State,
    after: State,
}

/// [`FlashKv`] that logs every write as a [`Transition`].
struct Recorder {
    kv: FlashKv<MockNor>,
    flash: MockNor,
    log: Rc<RefCell<Vec<Transition>>>,
}

impl Recorder {
    fn write(
        &mut self,
        op: impl FnOnce(&mut FlashKv<MockNor>) -> Result<(), KvError>,
    ) -> Result<(), KvError> {
        let before = state(&self.kv);
        let start = self.flash.steps();
        let result = op(&mut self.kv);
        let end = self.flash.steps();
        if end > start {
            self.log.borrow_mut().push(Transition {
                steps: start..end,
                before,
                after: state(&self.kv),
            });
        }
        result
    }
}

impl OtaKv for Recorder {
    fn get_u8(&self, key: &str) -> Result<Option<u8>, KvError> {
        self.kv.get_u8(key)
    }
    fn get_u32(&self, key: &str) -> Result<Option<u32>, KvError> {
        self.kv.get_u32(key)
    }
    fn get_str(&self, key: &str) -> Result<Option<KvStr>, KvError> {
        self.kv.get_str(key)
    }
    fn set_u8(&mut self, key: &str, value: u8) -> Result<(), KvError> {
        self.write(|kv| kv.set_u8(key, value))
    }
    fn set_u32(&mut self, key: &str, value: u32) -> Result<(), KvError> {
        self.write(|kv| kv.set_u32(key, value))
    }
    fn set_str(&mut self, key: &str, value: &str) -> Result<(), KvError> {
        self.write(|kv| kv.set_str(key, value))
    }
    fn remove(&mut self, key: &str) -> Result<(), KvError> {
        self.write(|kv| kv.remove(key))
    }
}

type Scenario = fn(&mut OtaStore<Recorder>) -> Vec<String>;

/// Runs `scenario` once per flash step with the power cut on that step; after the cut the store reopens with the state from just before or just after the write in progress.
fn power_cut_matrix(sectors: usize, scenario: Scenario) -> usize {
    let world = |cut: Option<usize>| {
        let flash = MockNor::new(sectors);
        prefill(&mut open(&flash));
        flash.reset_counters();
        if let Some(step) = cut {
            flash.cut_at(step);
        }
        let log = Rc::new(RefCell::new(Vec::new()));
        let recorder = Recorder {
            kv: open(&flash),
            flash: flash.clone(),
            log: Rc::clone(&log),
        };
        let mut s = OtaStore::open(recorder, entropy);
        scenario(&mut s);
        (flash, log)
    };

    let (reference, log) = world(None);
    let total = reference.steps();
    let log = log.borrow();
    assert!(total > 0 && !log.is_empty());
    assert!(
        reference.erases() > 0,
        "the run must wrap the log at least once"
    );
    for step in 1..=total {
        let (flash, _) = world(Some(step));
        flash.revive();
        let reopened = state(&open(&flash));
        let t = log
            .iter()
            .find(|t| t.steps.contains(&(step - 1)))
            .expect("every step belongs to a write");
        assert!(
            reopened == t.before || reopened == t.after,
            "cut at step {step} of {total}: {reopened:?}"
        );
        assert_eq!(flash.overwrites(), 0, "cut at step {step}");
    }
    total
}

#[test]
fn a_power_cut_at_any_step_of_the_attempt_lifecycle_keeps_one_side_of_the_write() {
    assert!(power_cut_matrix(2, lifecycle::<Recorder>) > 1000);
    power_cut_matrix(3, lifecycle::<Recorder>);
}

#[test]
fn a_power_cut_at_any_step_of_an_operator_rollback_keeps_one_side_of_the_write() {
    power_cut_matrix(2, |s| {
        let mut out = lifecycle(s);
        out.extend(operator_rollback(s));
        out
    });
}

#[cfg(feature = "ota-ddi")]
#[test]
fn a_stored_records_unreadable_verdict_acknowledges_the_lost_record() {
    use crate::ota::ddi::wire::ActionId;
    use crate::ota::persist::DdiVerdict;
    let (flash, snapshot) = three_snapshots();
    flash.clear_bits(2 * snapshot + 12, 0x01);
    // Boot one: the boot-slot note replaces the damaged snapshot, then the device resets before any report.
    let mut kv = open(&flash);
    assert!(kv.newer_record_lost());
    kv.set_u32("att_ctr", 9).expect("set");
    // Boot two still owes the report; storing the 1005 verdict discharges it.
    let mut store = OtaStore::open(open(&flash), || 7);
    assert!(store.newer_record_lost());
    let verdict = DdiVerdict::records_unreadable(ActionId::parse("7").expect("id"));
    store.persist_verdict(&verdict).expect("verdict");
    assert!(!store.newer_record_lost());
    assert!(!open(&flash).newer_record_lost());
}

#[cfg(feature = "ota-ddi")]
#[test]
fn a_delivered_records_unreadable_verdict_acknowledges_a_finding_left_by_a_power_cut() {
    use crate::ota::ddi::wire::ActionId;
    use crate::ota::persist::DdiVerdict;
    let damaged = || {
        let (flash, snapshot) = three_snapshots();
        flash.clear_bits(2 * snapshot + 12, 0x01);
        flash.reset_counters();
        flash
    };
    let verdict = DdiVerdict::records_unreadable(ActionId::parse("7").expect("id"));
    let probe = damaged();
    OtaStore::open(open(&probe), || 7)
        .persist_verdict(&verdict)
        .expect("verdict");
    // The acknowledgement is the last commit: cut its last step, after the verdict is stored.
    let flash = damaged();
    let mut store = OtaStore::open(open(&flash), || 7);
    flash.cut_at(probe.steps());
    assert!(store.persist_verdict(&verdict).is_err());
    flash.revive();
    let mut store = OtaStore::open(open(&flash), || 7);
    assert!(store.newer_record_lost(), "verdict and finding both stored");
    assert_eq!(
        store.pending_verdict().expect("reads").map(|v| v.action),
        Some(verdict.action.clone())
    );
    // The rebooted worker delivers the queued verdict on its normal path.
    store
        .mark_verdict_delivered(&verdict.action)
        .expect("delivered");
    assert!(!store.newer_record_lost());
    assert!(!open(&flash).newer_record_lost());
}

#[cfg(feature = "ota-ddi")]
#[test]
fn a_power_cut_at_any_step_of_a_ddi_action_keeps_one_side_of_the_write() {
    power_cut_matrix(2, |s| {
        let mut out = ddi_actions(s);
        out.extend(lifecycle(s));
        out
    });
}
