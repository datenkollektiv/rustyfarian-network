//! Activation of a freshly written OTA slot as ONE otadata entry write.
//!
//! `OtaUpdater::activate_next_partition` writes only the new `ota_seq` and keeps
//! the `ota_state` of the otadata entry it reuses, which can be `Undefined`
//! (the bootloader treats it as valid), `Valid`, or even `Invalid`.
//! Setting `New` afterwards is a second otadata write; an error or power loss
//! between the two would leave the new selection persisted with no rollback
//! armed.
//! Here the entry is built whole instead: the new sequence number, the `New`
//! state and the CRC go into the *non-selected* otadata sector with a single
//! 32-byte write, so the previous selection stays untouched until that write
//! lands, and the bootloader never sees the new selection without `New`.
//!
//! The entry layout (`esp-bootloader-esp-idf 0.6.0`, `ota.rs`, and ESP-IDF's
//! `esp_ota_select_entry_t`) is 32 bytes: `ota_seq` (u32 LE), `seq_label`
//! (20 bytes, unused), `ota_state` (u32 LE), `crc` (u32 LE, CRC-32 of the
//! `ota_seq` bytes only), at sector offsets `0x0000` and `0x1000`.
//!
//! Kept free of ESP types so the order, the entry bytes and the restore
//! decision are checked on the host.

/// Length of one otadata select entry.
pub(super) const ENTRY_LEN: usize = 32;

/// One otadata entry as stored in flash.
pub(super) type Entry = [u8; ENTRY_LEN];

/// `ota_seq` / `ota_state` of an erased entry.
const ERASED: u32 = 0xFFFF_FFFF;

/// `esp_ota_img_states_t` `ESP_OTA_IMG_NEW`.
const STATE_NEW: u32 = 0x0;

/// `esp_ota_img_states_t` `ESP_OTA_IMG_VALID`.
pub(super) const STATE_VALID: u32 = 0x2;

/// `esp_ota_img_states_t` `ESP_OTA_IMG_INVALID`.
pub(super) const STATE_INVALID: u32 = 0x3;

/// `esp_ota_img_states_t` `ESP_OTA_IMG_ABORTED`.
const STATE_ABORTED: u32 = 0x4;

/// Bitwise CRC-32 (reflected, polynomial `0x04C11DB7`, initial register `0`,
/// final XOR `0xFFFFFFFF`), the checksum of `esp_rom_crc32_le(UINT32_MAX, ..)`.
fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0u32;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 != 0 {
                (crc >> 1) ^ 0xEDB8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

/// Builds the entry the bootloader expects for `seq` in `state`.
pub(super) fn encode_entry(seq: u32, state: u32) -> Entry {
    let mut entry = [0xFF; ENTRY_LEN];
    entry[0..4].copy_from_slice(&seq.to_le_bytes());
    entry[24..28].copy_from_slice(&state.to_le_bytes());
    entry[28..32].copy_from_slice(&crc32(&seq.to_le_bytes()).to_le_bytes());
    entry
}

/// The `(sequence, state)` of `entry` when the bootloader would select it.
///
/// An erased entry, an entry with a bad CRC (for instance a torn write) and an
/// entry in state `Invalid` or `Aborted` all count as unused.
/// Any other state, including `ESP_OTA_IMG_UNDEFINED` (`0xFFFF_FFFF`, written by
/// `set_new_state_otadata` in `esp_ota_ops.c` when rollback is disabled), is
/// selected.
/// This mirrors `bootloader_common_ota_select_invalid` (ESP-IDF v5.3.3,
/// `bootloader_common_loader.c`): unusable only when `ota_seq == UINT32_MAX`,
/// the CRC does not match, or the state is `INVALID` or `ABORTED`.
/// After a bootloader rollback the highest sequence is therefore an `Aborted`
/// entry that must not be treated as the selection.
fn valid_entry(entry: &Entry) -> Option<(u32, u32)> {
    let seq = u32::from_le_bytes([entry[0], entry[1], entry[2], entry[3]]);
    let state = u32::from_le_bytes([entry[24], entry[25], entry[26], entry[27]]);
    let crc = u32::from_le_bytes([entry[28], entry[29], entry[30], entry[31]]);
    if seq == ERASED
        || state == STATE_INVALID
        || state == STATE_ABORTED
        || crc != crc32(&seq.to_le_bytes())
    {
        return None;
    }
    Some((seq, state))
}

/// Whether the boot is a rollback boot: the bootloader selected an entry, and the OTHER sector
/// holds an intact entry in state `Aborted` or `Invalid` (the image that was rolled back).
///
/// Mirrors `esp_ota_get_last_invalid_partition`, which the ESP-IDF tier reads.
/// An erased or torn other entry is not a rollback, and neither is a boot that selects nothing.
pub(super) fn rolled_back(sectors: &[Entry; 2]) -> bool {
    let Some((selected, _)) = selected_sector(sectors) else {
        return false;
    };
    let other = &sectors[1 - selected];
    let seq = u32::from_le_bytes([other[0], other[1], other[2], other[3]]);
    let state = u32::from_le_bytes([other[24], other[25], other[26], other[27]]);
    let crc = u32::from_le_bytes([other[28], other[29], other[30], other[31]]);
    seq != ERASED
        && crc == crc32(&seq.to_le_bytes())
        && (state == STATE_INVALID || state == STATE_ABORTED)
}

/// The otadata sector the bootloader selects and its sequence number.
///
/// The higher valid sequence wins; on a tie sector 0 does
/// (`bootloader_common_select_otadata`, `condition == otadata[0].ota_seq`).
fn selected_sector(sectors: &[Entry; 2]) -> Option<(usize, u32)> {
    match [valid_entry(&sectors[0]), valid_entry(&sectors[1])] {
        [None, None] => None,
        [Some((s0, _)), None] => Some((0, s0)),
        [None, Some((s1, _))] => Some((1, s1)),
        [Some((s0, _)), Some((s1, _))] => Some(if s0 >= s1 { (0, s0) } else { (1, s1) }),
    }
}

/// The OTA app number (0-based) the bootloader boots, or `None` when it
/// selects no entry (factory or first boot).
///
/// `Ota::current_app_partition` of `esp-bootloader-esp-idf 0.6.0` takes the
/// highest raw sequence and ignores the state, so after a bootloader rollback
/// (`[seq 1 Valid, seq 2 Aborted]`) it names the aborted slot.
/// Here the entry is the one [`selected_sector`] picks: `(seq - 1) % ota_count`.
pub(super) fn selected_app(sectors: &[Entry; 2], ota_count: usize) -> Option<u8> {
    let (_, seq) = selected_sector(sectors)?;
    let n = u32::try_from(ota_count)
        .ok()
        .filter(|n| (1..=16).contains(n))?;
    // Wraps like `(ota_seq - 1) % ota_app_count` in `bootloader_utility.c`.
    u8::try_from(seq.wrapping_sub(1) % n).ok()
}

/// Why an activation cannot be planned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PlanError {
    /// The partition table has no OTA app slots, or more than the 16 the format allows.
    SlotCount,
    /// The slot to activate is not one of the OTA app slots.
    SlotOutOfRange,
    /// The slot to activate is already the selected one.
    AlreadySelected,
    /// The sequence number would overflow.
    SequenceOverflow,
}

/// Where and what to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Plan {
    /// Index (0 or 1) of the otadata sector to write: the non-selected one.
    pub sector: usize,
    /// The entry to write there.
    pub entry: Entry,
}

/// Plans the single write that selects OTA slot `next_app` in state `New`.
///
/// `sectors` are the raw entries of the two otadata sectors, `ota_count` the
/// number of OTA app slots.
/// The sequence rule is the crate's: the selected slot is `(seq - 1) % ota_count`
/// of the highest valid sequence, and the new sequence is the smallest one above
/// it that selects `next_app`.
pub(super) fn plan_activation(
    sectors: &[Entry; 2],
    ota_count: usize,
    next_app: u8,
) -> Result<Plan, PlanError> {
    if ota_count == 0 || ota_count > 16 {
        return Err(PlanError::SlotCount);
    }
    if usize::from(next_app) >= ota_count {
        return Err(PlanError::SlotOutOfRange);
    }
    let n = ota_count as u32;

    // The sector the bootloader treats as selected; the other one is
    // overwritten (it may hold an Aborted or Invalid entry).
    let selected = selected_sector(sectors);
    let base = selected.map_or(0, |(_, seq)| seq);
    let sector = if matches!(selected, Some((0, _))) {
        1
    } else {
        0
    };

    let inc = if selected.is_none() {
        u32::from(next_app) + 1
    } else {
        if base.wrapping_sub(1) % n == u32::from(next_app) {
            return Err(PlanError::AlreadySelected);
        }
        // Searched without the wrap: `(base + inc - 1) % n` must select
        // `next_app`; a modular difference from `base - 1` is wrong at
        // `base == 0` when `n` does not divide 2^32.
        (1..=n)
            .find(|i| base.wrapping_add(*i).wrapping_sub(1) % n == u32::from(next_app))
            .ok_or(PlanError::SequenceOverflow)?
    };
    let seq = base
        .checked_add(inc)
        .filter(|s| *s != ERASED)
        .ok_or(PlanError::SequenceOverflow)?;

    Ok(Plan {
        sector,
        entry: encode_entry(seq, STATE_NEW),
    })
}

/// Raw access to the two otadata sectors.
pub(super) trait OtadataSectors {
    /// The error of a failed access.
    type Error;

    /// Reads the entry at the start of sector `index` (0 or 1).
    fn read_entry(&mut self, index: usize) -> Result<Entry, Self::Error>;

    /// Writes the entry at the start of sector `index` (0 or 1).
    fn write_entry(&mut self, index: usize, entry: &Entry) -> Result<(), Self::Error>;
}

/// Why the state of the selected entry could not be set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StateCause<E> {
    /// Reading the otadata failed; nothing was written.
    Read(E),
    /// The bootloader selects no entry (factory or first boot); nothing was written.
    NothingSelected,
    /// Writing the entry failed.
    Write(E),
    /// Reading the entry back after the write failed.
    VerifyRead(E),
    /// The entry did not read back as written.
    Verify,
}

/// A failed state change: the original cause and the restore outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct StateFailure<E> {
    /// The original error.
    pub cause: StateCause<E>,
    /// Whether the touched sector was put back.
    pub restore: RestoreOutcome,
}

/// Sets the `ota_state` of the entry the bootloader selects, with one write
/// and a read-back.
///
/// `OtaUpdater::set_current_ota_state` picks the highest sequence even when
/// that entry is `Aborted` or `Invalid`, which after a rollback marks the
/// aborted image `Valid`.
/// The sequence number and CRC are kept; only the state word changes.
///
/// An entry that already holds `state` is left alone (ESP-IDF's
/// `esp_ota_current_ota_is_workable` does the same): the write rewrites the
/// *selected* sector read-modify-erase-write, so every needless call would open
/// a power-cut window in which the bootloader falls back to the other sector.
/// On a write error or a read-back mismatch the previous bytes are written back
/// and read back, and the failure carries the original cause and that outcome.
pub(super) fn set_selected_state<S: OtadataSectors>(
    sectors: &mut S,
    state: u32,
) -> Result<(), StateFailure<S::Error>> {
    fn fail<E>(cause: StateCause<E>, restore: RestoreOutcome) -> StateFailure<E> {
        StateFailure { cause, restore }
    }
    let first = sectors
        .read_entry(0)
        .map_err(|e| fail(StateCause::Read(e), RestoreOutcome::NotNeeded))?;
    let second = sectors
        .read_entry(1)
        .map_err(|e| fail(StateCause::Read(e), RestoreOutcome::NotNeeded))?;
    let (index, _) = selected_sector(&[first, second])
        .ok_or_else(|| fail(StateCause::NothingSelected, RestoreOutcome::NotNeeded))?;
    let previous = if index == 0 { first } else { second };
    if previous[24..28] == state.to_le_bytes() {
        return Ok(());
    }
    let mut entry = previous;
    entry[24..28].copy_from_slice(&state.to_le_bytes());

    let cause = match sectors.write_entry(index, &entry) {
        Err(e) => StateCause::Write(e),
        Ok(()) => match sectors.read_entry(index) {
            Ok(back) if back == entry => return Ok(()),
            Ok(_) => StateCause::Verify,
            Err(e) => StateCause::VerifyRead(e),
        },
    };

    let restored = sectors.write_entry(index, &previous).is_ok()
        && sectors.read_entry(index).ok() == Some(previous);
    let restore = if restored {
        RestoreOutcome::Restored
    } else {
        RestoreOutcome::Failed
    };
    Err(fail(cause, restore))
}

/// What happened to the sector a failed activation touched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RestoreOutcome {
    /// Nothing had been written, so there was nothing to restore.
    NotNeeded,
    /// The previous bytes were written back and read back equal.
    Restored,
    /// The previous bytes could not be written back or did not read back
    /// equal; the sector may hold a partial entry.
    /// The bootloader still ignores an entry whose CRC fails, and the
    /// selected sector was never touched.
    Failed,
}

/// The first thing that went wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Cause<E> {
    /// Reading the otadata failed; nothing was written.
    Read(E),
    /// The activation cannot be planned; nothing was written.
    Plan(PlanError),
    /// Writing the new entry failed.
    Write(E),
    /// The new entry did not read back as written.
    Verify,
}

/// A failed activation: the original cause and the restore outcome.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ActivationFailure<E> {
    /// The original error.
    pub cause: Cause<E>,
    /// Whether the touched sector was put back.
    pub restore: RestoreOutcome,
}

/// Selects OTA slot `next_app` and arms the bootloader's rollback for it with
/// one entry write, verifying it by read-back.
///
/// On a write error or a read-back mismatch it tries to put the previous bytes
/// of the written sector back and reports the original error together with the
/// restore outcome.
pub(super) fn activate_as_new<S: OtadataSectors>(
    sectors: &mut S,
    ota_count: usize,
    next_app: u8,
) -> Result<(), ActivationFailure<S::Error>> {
    fn fail<E>(cause: Cause<E>, restore: RestoreOutcome) -> ActivationFailure<E> {
        ActivationFailure { cause, restore }
    }
    let first = sectors
        .read_entry(0)
        .map_err(|e| fail(Cause::Read(e), RestoreOutcome::NotNeeded))?;
    let second = sectors
        .read_entry(1)
        .map_err(|e| fail(Cause::Read(e), RestoreOutcome::NotNeeded))?;
    let before = [first, second];
    let plan = plan_activation(&before, ota_count, next_app)
        .map_err(|e| fail(Cause::Plan(e), RestoreOutcome::NotNeeded))?;

    let cause = match sectors.write_entry(plan.sector, &plan.entry) {
        Err(e) => Cause::Write(e),
        Ok(()) => match sectors.read_entry(plan.sector) {
            Ok(back) if back == plan.entry => return Ok(()),
            _ => Cause::Verify,
        },
    };

    let previous = before[plan.sector];
    let restored = sectors.write_entry(plan.sector, &previous).is_ok()
        && sectors.read_entry(plan.sector).ok() == Some(previous);
    let restore = if restored {
        RestoreOutcome::Restored
    } else {
        RestoreOutcome::Failed
    };
    Err(fail(cause, restore))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Golden entries from `esp-bootloader-esp-idf 0.6.0` `ota.rs` tests.
    const COUNT_1_UNDEFINED: Entry = [
        1, 0, 0, 0, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
        255, 255, 255, 255, 255, 255, 255, 255, 154, 152, 67, 71,
    ];
    const COUNT_1_VALID: Entry = [
        1, 0, 0, 0, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
        255, 255, 255, 255, 2, 0, 0, 0, 154, 152, 67, 71,
    ];
    const COUNT_2_NEW: Entry = [
        2, 0, 0, 0, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
        255, 255, 255, 255, 0, 0, 0, 0, 116, 55, 246, 85,
    ];
    const COUNT_3_PENDING: Entry = [
        3, 0, 0, 0, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255,
        255, 255, 255, 255, 1, 0, 0, 0, 17, 80, 74, 237,
    ];
    const ERASED_ENTRY: Entry = [0xFF; ENTRY_LEN];

    #[test]
    fn entries_match_the_crates_golden_bytes() {
        assert_eq!(encode_entry(2, 0), COUNT_2_NEW);
        assert_eq!(encode_entry(1, 2), COUNT_1_VALID);
        assert_eq!(encode_entry(1, ERASED), COUNT_1_UNDEFINED);
        assert_eq!(encode_entry(3, 1), COUNT_3_PENDING);
    }

    #[test]
    fn selecting_ota_1_from_a_valid_ota_0_writes_the_second_sector_as_new() {
        let plan = plan_activation(&[COUNT_1_VALID, ERASED_ENTRY], 2, 1).unwrap();
        assert_eq!(plan.sector, 1);
        assert_eq!(plan.entry, COUNT_2_NEW);
    }

    #[test]
    fn the_first_ota_from_the_factory_image_selects_ota_0() {
        let plan = plan_activation(&[ERASED_ENTRY, ERASED_ENTRY], 2, 0).unwrap();
        assert_eq!(plan.sector, 0);
        assert_eq!(plan.entry, encode_entry(1, 0));
    }

    #[test]
    fn the_older_sector_is_overwritten_and_the_sequence_keeps_growing() {
        let plan = plan_activation(&[COUNT_3_PENDING, COUNT_2_NEW], 2, 1).unwrap();
        assert_eq!(plan.sector, 1);
        assert_eq!(plan.entry, encode_entry(4, 0));
        let plan = plan_activation(&[COUNT_1_VALID, COUNT_2_NEW], 2, 0).unwrap();
        assert_eq!(plan.sector, 0);
        assert_eq!(plan.entry, encode_entry(3, 0));
    }

    #[test]
    fn a_stale_state_in_the_reused_sector_never_reaches_the_new_entry() {
        let stale_invalid = encode_entry(1, 3);
        let plan = plan_activation(&[stale_invalid, COUNT_2_NEW], 2, 0).unwrap();
        assert_eq!(plan.entry, encode_entry(3, 0));
    }

    #[test]
    fn a_torn_sector_counts_as_unused() {
        let mut torn = COUNT_2_NEW;
        torn[28] ^= 1;
        let plan = plan_activation(&[COUNT_1_VALID, torn], 2, 1).unwrap();
        assert_eq!(plan.sector, 1);
        assert_eq!(plan.entry, COUNT_2_NEW);
    }

    #[test]
    fn after_a_rollback_the_aborted_sector_is_overwritten_not_selected() {
        for dead in [3, 4] {
            let plan = plan_activation(&[encode_entry(1, 2), encode_entry(2, dead)], 2, 1)
                .unwrap_or_else(|e| panic!("state {dead}: {e:?}"));
            assert_eq!(plan.sector, 1);
            assert_eq!(plan.entry, encode_entry(2, 0));
        }
    }

    #[test]
    fn a_higher_sequence_in_a_dead_state_does_not_raise_the_base() {
        let plan = plan_activation(&[encode_entry(3, 4), encode_entry(2, 2)], 2, 0).unwrap();
        assert_eq!(plan.sector, 0);
        assert_eq!(plan.entry, encode_entry(3, 0));
    }

    #[test]
    fn two_dead_entries_select_nothing_and_start_over() {
        let plan = plan_activation(&[encode_entry(1, 4), encode_entry(2, 3)], 2, 1).unwrap();
        assert_eq!(plan.entry, encode_entry(2, 0));
    }

    #[test]
    fn a_sequence_tie_selects_sector_zero_like_the_bootloader() {
        let plan = plan_activation(&[encode_entry(1, 2), encode_entry(1, 2)], 2, 1).unwrap();
        assert_eq!(plan.sector, 1);
    }

    #[test]
    fn plans_refuse_what_cannot_be_selected() {
        assert_eq!(
            plan_activation(&[COUNT_1_VALID, ERASED_ENTRY], 2, 0),
            Err(PlanError::AlreadySelected)
        );
        assert_eq!(
            plan_activation(&[COUNT_1_VALID, ERASED_ENTRY], 2, 2),
            Err(PlanError::SlotOutOfRange)
        );
        assert_eq!(
            plan_activation(&[COUNT_1_VALID, ERASED_ENTRY], 0, 0),
            Err(PlanError::SlotCount)
        );
    }

    /// Two sectors that record every write.
    struct Flash {
        sectors: [Entry; 2],
        writes: usize,
        /// Fail the first write; `torn` lands its sequence bytes first.
        fail_first_write: bool,
        torn: bool,
        /// Store the first write without its state (a flash that lies).
        lie: bool,
        /// Fail every write after the first (the restore).
        fail_restore: bool,
        /// Fail the first read that follows a write (the read-back).
        fail_read_after_write: bool,
    }

    impl Flash {
        fn new(sectors: [Entry; 2]) -> Self {
            Self {
                sectors,
                writes: 0,
                fail_first_write: false,
                torn: false,
                lie: false,
                fail_restore: false,
                fail_read_after_write: false,
            }
        }

        /// What the bootloader would select: the highest valid sequence and its state.
        fn selection(&self) -> Option<(u32, u32)> {
            match (valid_entry(&self.sectors[0]), valid_entry(&self.sectors[1])) {
                (None, None) => None,
                (Some(a), None) | (None, Some(a)) => Some(a),
                (Some(a), Some(b)) => Some(if a.0 > b.0 { a } else { b }),
            }
        }
    }

    impl OtadataSectors for Flash {
        type Error = &'static str;

        fn read_entry(&mut self, index: usize) -> Result<Entry, Self::Error> {
            if self.fail_read_after_write && self.writes == 1 {
                self.fail_read_after_write = false;
                return Err("read failed");
            }
            Ok(self.sectors[index])
        }

        fn write_entry(&mut self, index: usize, entry: &Entry) -> Result<(), Self::Error> {
            self.writes += 1;
            if self.fail_restore && self.writes >= 2 {
                return Err("restore failed");
            }
            if self.fail_first_write && self.writes == 1 {
                if self.torn {
                    self.sectors[index][0..4].copy_from_slice(&entry[0..4]);
                }
                return Err("write failed");
            }
            self.sectors[index] = *entry;
            if self.lie && self.writes == 1 {
                self.sectors[index][24..28].copy_from_slice(&ERASED.to_le_bytes());
            }
            Ok(())
        }
    }

    #[test]
    fn activation_is_one_entry_write() {
        let mut flash = Flash::new([COUNT_1_VALID, ERASED_ENTRY]);
        assert_eq!(activate_as_new(&mut flash, 2, 1), Ok(()));
        assert_eq!(flash.writes, 1);
        assert_eq!(flash.sectors, [COUNT_1_VALID, COUNT_2_NEW]);
        assert_eq!(flash.selection(), Some((2, 0)));
    }

    #[test]
    fn no_interruption_point_selects_the_new_slot_without_the_new_state() {
        for torn in [false, true] {
            let mut flash = Flash::new([COUNT_1_VALID, ERASED_ENTRY]);
            flash.fail_first_write = true;
            flash.torn = torn;
            let before = flash.selection();
            let failure = activate_as_new(&mut flash, 2, 1).unwrap_err();
            assert_eq!(failure.cause, Cause::Write("write failed"));
            let after = flash.selection();
            assert!(
                after == before || after == Some((2, 0)),
                "selection {after:?} after an interrupted write (torn: {torn})"
            );
        }
    }

    #[test]
    fn a_failed_write_is_restored_and_the_original_error_returned() {
        let mut flash = Flash::new([COUNT_1_VALID, ERASED_ENTRY]);
        flash.fail_first_write = true;
        flash.torn = true;
        let failure = activate_as_new(&mut flash, 2, 1).unwrap_err();
        assert_eq!(failure.cause, Cause::Write("write failed"));
        assert_eq!(failure.restore, RestoreOutcome::Restored);
        assert_eq!(flash.sectors, [COUNT_1_VALID, ERASED_ENTRY]);
        assert_eq!(flash.selection(), Some((1, 2)));
    }

    #[test]
    fn a_failed_restore_is_reported_not_hidden() {
        let mut flash = Flash::new([COUNT_1_VALID, ERASED_ENTRY]);
        flash.fail_first_write = true;
        flash.torn = true;
        flash.fail_restore = true;
        let failure = activate_as_new(&mut flash, 2, 1).unwrap_err();
        assert_eq!(failure.cause, Cause::Write("write failed"));
        assert_eq!(failure.restore, RestoreOutcome::Failed);
        // The selected sector was never touched and the torn entry has no CRC.
        assert_eq!(flash.selection(), Some((1, 2)));
    }

    #[test]
    fn a_write_that_reads_back_wrong_is_undone() {
        let mut flash = Flash::new([COUNT_1_VALID, ERASED_ENTRY]);
        flash.lie = true;
        let failure = activate_as_new(&mut flash, 2, 1).unwrap_err();
        assert_eq!(failure.cause, Cause::Verify);
        assert_eq!(failure.restore, RestoreOutcome::Restored);
        assert_eq!(flash.sectors, [COUNT_1_VALID, ERASED_ENTRY]);
    }

    #[test]
    fn a_state_change_targets_the_selected_entry_not_the_aborted_one() {
        let mut flash = Flash::new([encode_entry(1, 2), encode_entry(2, 4)]);
        assert_eq!(set_selected_state(&mut flash, STATE_VALID), Ok(()));
        assert_eq!(flash.sectors, [encode_entry(1, 2), encode_entry(2, 4)]);
        let mut flash = Flash::new([encode_entry(1, 1), encode_entry(2, 4)]);
        assert_eq!(set_selected_state(&mut flash, STATE_VALID), Ok(()));
        assert_eq!(flash.sectors, [encode_entry(1, 2), encode_entry(2, 4)]);
        assert_eq!(flash.writes, 1);
    }

    #[test]
    fn rollback_marks_the_running_entry_invalid_and_the_bootloader_falls_back() {
        let mut flash = Flash::new([encode_entry(1, 2), encode_entry(2, 1)]);
        assert_eq!(set_selected_state(&mut flash, STATE_INVALID), Ok(()));
        assert_eq!(flash.sectors, [encode_entry(1, 2), encode_entry(2, 3)]);
        assert_eq!(flash.selection(), Some((1, 2)));
        assert_eq!(set_selected_state(&mut flash, STATE_VALID), Ok(()));
        assert_eq!(flash.sectors[1], encode_entry(2, 3));
    }

    #[test]
    fn a_state_change_without_a_selection_writes_nothing() {
        let mut flash = Flash::new([ERASED_ENTRY, encode_entry(1, 4)]);
        assert_eq!(
            set_selected_state(&mut flash, STATE_VALID)
                .unwrap_err()
                .cause,
            StateCause::NothingSelected
        );
        assert_eq!(flash.writes, 0);
    }

    #[test]
    fn a_state_write_that_reads_back_wrong_is_reported() {
        let mut flash = Flash::new([encode_entry(1, 1), ERASED_ENTRY]);
        flash.lie = true;
        assert_eq!(
            set_selected_state(&mut flash, STATE_VALID)
                .unwrap_err()
                .cause,
            StateCause::Verify
        );
    }

    #[test]
    fn the_selected_app_skips_an_aborted_highest_sequence() {
        assert_eq!(
            selected_app(&[encode_entry(1, 2), encode_entry(2, 4)], 2),
            Some(0)
        );
        assert_eq!(
            selected_app(&[encode_entry(1, 2), encode_entry(2, 3)], 2),
            Some(0)
        );
        assert_eq!(
            selected_app(&[encode_entry(1, 2), encode_entry(2, 1)], 2),
            Some(1)
        );
    }

    #[test]
    fn the_selected_app_of_a_tie_is_sector_zero() {
        assert_eq!(
            selected_app(&[encode_entry(1, 2), encode_entry(1, 2)], 2),
            Some(0)
        );
        assert_eq!(
            selected_app(&[encode_entry(3, 2), encode_entry(3, 2)], 2),
            Some(0)
        );
    }

    #[test]
    fn the_selected_app_is_none_without_a_usable_entry() {
        assert_eq!(selected_app(&[ERASED_ENTRY, ERASED_ENTRY], 2), None);
        assert_eq!(
            selected_app(&[encode_entry(1, 4), encode_entry(2, 3)], 2),
            None
        );
    }

    #[test]
    fn marking_an_already_valid_entry_writes_nothing() {
        let mut flash = Flash::new([encode_entry(1, 2), ERASED_ENTRY]);
        assert_eq!(set_selected_state(&mut flash, STATE_VALID), Ok(()));
        assert_eq!(flash.writes, 0);
    }

    #[test]
    fn a_failed_state_write_is_restored_and_the_original_error_returned() {
        let mut flash = Flash::new([encode_entry(1, 2), encode_entry(2, 1)]);
        flash.fail_first_write = true;
        flash.torn = true;
        let failure = set_selected_state(&mut flash, STATE_VALID).unwrap_err();
        assert_eq!(failure.cause, StateCause::Write("write failed"));
        assert_eq!(failure.restore, RestoreOutcome::Restored);
        assert_eq!(flash.sectors, [encode_entry(1, 2), encode_entry(2, 1)]);
    }

    #[test]
    fn a_state_write_that_reads_back_wrong_is_undone() {
        let mut flash = Flash::new([encode_entry(1, 1), ERASED_ENTRY]);
        flash.lie = true;
        let failure = set_selected_state(&mut flash, STATE_VALID).unwrap_err();
        assert_eq!(failure.cause, StateCause::Verify);
        assert_eq!(failure.restore, RestoreOutcome::Restored);
        assert_eq!(flash.sectors, [encode_entry(1, 1), ERASED_ENTRY]);
    }

    #[test]
    fn a_failed_state_restore_is_reported_not_hidden() {
        let mut flash = Flash::new([encode_entry(1, 1), ERASED_ENTRY]);
        flash.fail_first_write = true;
        flash.fail_restore = true;
        let failure = set_selected_state(&mut flash, STATE_VALID).unwrap_err();
        assert_eq!(failure.cause, StateCause::Write("write failed"));
        assert_eq!(failure.restore, RestoreOutcome::Failed);
    }

    #[test]
    fn a_read_back_error_keeps_its_own_cause() {
        let mut flash = Flash::new([encode_entry(1, 1), ERASED_ENTRY]);
        flash.fail_read_after_write = true;
        let failure = set_selected_state(&mut flash, STATE_VALID).unwrap_err();
        assert_eq!(failure.cause, StateCause::VerifyRead("read failed"));
    }

    #[test]
    fn an_undefined_state_entry_is_selected_like_the_bootloader_does() {
        assert_eq!(
            selected_app(&[encode_entry(1, ERASED), ERASED_ENTRY], 2),
            Some(0)
        );
        let mut flash = Flash::new([encode_entry(2, ERASED), ERASED_ENTRY]);
        assert_eq!(set_selected_state(&mut flash, STATE_VALID), Ok(()));
        assert_eq!(flash.sectors, [encode_entry(2, STATE_VALID), ERASED_ENTRY]);
        let plan = plan_activation(&[encode_entry(1, ERASED), ERASED_ENTRY], 2, 1).unwrap();
        assert_eq!(plan.sector, 1);
        assert_eq!(plan.entry, encode_entry(2, 0));
    }

    #[test]
    fn a_zero_sequence_wraps_like_the_bootloader_instead_of_panicking() {
        assert_eq!(
            selected_app(&[encode_entry(0, 2), ERASED_ENTRY], 2),
            Some((u32::MAX % 2) as u8)
        );
        let plan = plan_activation(&[encode_entry(0, 2), ERASED_ENTRY], 2, 0).unwrap();
        assert_eq!(plan.sector, 1);
    }

    #[test]
    fn a_planned_sequence_selects_the_requested_slot_across_the_zero_wrap() {
        for n in [2usize, 3, 4, 5, 7] {
            for next_app in 0..n as u8 {
                let sectors = [encode_entry(0, 2), ERASED_ENTRY];
                let current = selected_app(&sectors, n).unwrap();
                match plan_activation(&sectors, n, next_app) {
                    Err(PlanError::AlreadySelected) => assert_eq!(current, next_app),
                    Ok(plan) => {
                        let mut after = sectors;
                        after[plan.sector] = plan.entry;
                        assert_eq!(
                            selected_app(&after, n),
                            Some(next_app),
                            "n={n} next_app={next_app}"
                        );
                    }
                    Err(e) => panic!("n={n} next_app={next_app}: {e:?}"),
                }
            }
        }
    }

    #[test]
    fn a_rollback_boot_is_an_intact_aborted_or_invalid_entry_next_to_the_selected_one() {
        let torn = {
            let mut e = encode_entry(2, STATE_ABORTED);
            e[28] ^= 0xFF;
            e
        };
        for (sectors, expected, why) in [
            ([encode_entry(1, STATE_VALID), ERASED_ENTRY], false, "fresh"),
            (
                [encode_entry(1, STATE_VALID), encode_entry(2, STATE_ABORTED)],
                true,
                "aborted by the bootloader",
            ),
            (
                [encode_entry(2, STATE_INVALID), encode_entry(1, STATE_VALID)],
                true,
                "invalid by rollback()",
            ),
            (
                [encode_entry(1, STATE_VALID), torn],
                false,
                "torn other entry",
            ),
            (
                [encode_entry(2, STATE_ABORTED), ERASED_ENTRY],
                false,
                "nothing selected",
            ),
            (
                [encode_entry(1, STATE_VALID), encode_entry(2, STATE_NEW)],
                false,
                "new update pending",
            ),
        ] {
            assert_eq!(rolled_back(&sectors), expected, "{why}");
        }
    }

    #[test]
    fn an_unplannable_activation_writes_nothing() {
        let mut flash = Flash::new([COUNT_1_VALID, ERASED_ENTRY]);
        let failure = activate_as_new(&mut flash, 2, 0).unwrap_err();
        assert_eq!(failure.cause, Cause::Plan(PlanError::AlreadySelected));
        assert_eq!(failure.restore, RestoreOutcome::NotNeeded);
        assert_eq!(flash.writes, 0);
    }
}
