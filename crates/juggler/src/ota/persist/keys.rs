//! NVS key names and slot-label mapping of the `ota` namespace.

use crate::ota::SlotId;

/// `u8`: 1 while a persisted `rolled_back` report awaits delivery, 0 once delivered.
pub const RB: &str = "rb";
/// `str`: reason of the persisted report, kept after delivery until the next report.
pub const RB_WHY: &str = "rb_why";
/// `u32`: attempt (or request) id of the persisted report, kept after delivery.
pub const RB_ID: &str = "rb_id";
/// `u32`: attempt id counter, bumped before every id is handed out, never removed.
pub const ATT_CTR: &str = "att_ctr";
/// `u32`: install epoch, random, created once with the first id, never removed.
pub const ATT_EPOCH: &str = "att_epoch";
/// `u32`: id of the update attempt.
pub const ATT_ID: &str = "att_id";
/// `u8`: 1 once the boot slot was switched to the attempt's slot.
pub const ATT_BOOT: &str = "att_boot";
/// `str`: promised version of the attempt; the hide marker on clear.
pub const ATT_VER: &str = "att_ver";
/// `str`: partition label of the attempt; the commit marker on write.
pub const ATT_SLOT: &str = "att_slot";
/// `u8`: 1 once the attempted image is known to have been activated.
pub const ATT_ACT: &str = "att_act";
/// `u8`: 1 when the target slot was already `Invalid` before the attempt.
pub const ATT_INV: &str = "att_inv";
/// `str`: why a rollback of the attempt started, first reason wins.
pub const ATT_WHY: &str = "att_why";
/// `u32`: event id reserved for an operator rollback without an attempt.
pub const RQ_ID: &str = "rq_id";
/// `str`: reason of that operator rollback.
pub const RQ_WHY: &str = "rq_why";
/// `str`: slot label that rollback leaves; written last as the arm marker.
pub const RQ_FROM: &str = "rq_from";
/// `str`: version last left by a rollback, refused until a different one is offered.
pub const REJ_VER: &str = "rej_ver";
/// Every key of the namespace.
#[cfg(test)]
pub(crate) const KEYS: [&str; 16] = [
    RB, RB_WHY, RB_ID, ATT_CTR, ATT_EPOCH, ATT_ID, ATT_BOOT, ATT_VER, ATT_SLOT, ATT_ACT, ATT_INV,
    ATT_WHY, RQ_ID, RQ_WHY, RQ_FROM, REJ_VER,
];

/// Slot used for a running partition that is not `ota_0` or `ota_1`.
///
/// It never equals an attempt slot, because an attempt can only be written to `ota_0` or `ota_1`.
pub const FACTORY_SLOT: SlotId = SlotId(0xFF);

/// The partition label stored for `slot`, or `None` for a slot that has no label.
pub const fn slot_label(slot: SlotId) -> Option<&'static str> {
    match slot.0 {
        0 => Some("ota_0"),
        1 => Some("ota_1"),
        0xFF => Some("factory"),
        _ => None,
    }
}

/// The slot for a stored partition label, or `None` for an unknown label.
pub fn slot_from_label(label: &str) -> Option<SlotId> {
    match label {
        "ota_0" => Some(SlotId(0)),
        "ota_1" => Some(SlotId(1)),
        "factory" => Some(FACTORY_SLOT),
        _ => None,
    }
}

/// The slot for the label of a running or update partition.
///
/// `ota_0` and `ota_1` map to their index; `factory` and any other label map to [`FACTORY_SLOT`].
/// That is not a failure: such a slot can never equal an attempt slot.
///
/// v1 assumes exactly two OTA slots: `ota_2` and up also map to [`FACTORY_SLOT`], so a partition table with more than two OTA slots is misreported.
pub fn slot_for_partition(label: &str) -> SlotId {
    slot_from_label(label).unwrap_or(FACTORY_SLOT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partition_labels_never_fail() {
        assert_eq!(slot_for_partition("ota_0"), SlotId(0));
        assert_eq!(slot_for_partition("ota_1"), SlotId(1));
        for other in ["factory", "ota_2", "", "app0"] {
            assert_eq!(slot_for_partition(other), FACTORY_SLOT);
        }
    }

    #[test]
    fn key_names_are_frozen_distinct_and_fit_nvs() {
        for (i, a) in KEYS.iter().enumerate() {
            assert!((1..=15).contains(&a.len()), "{a}");
            for b in &KEYS[i + 1..] {
                assert_ne!(a, b);
            }
        }
        assert_eq!(
            KEYS,
            [
                "rb",
                "rb_why",
                "rb_id",
                "att_ctr",
                "att_epoch",
                "att_id",
                "att_boot",
                "att_ver",
                "att_slot",
                "att_act",
                "att_inv",
                "att_why",
                "rq_id",
                "rq_why",
                "rq_from",
                "rej_ver"
            ]
        );
    }

    #[test]
    fn slot_labels_round_trip() {
        for slot in [SlotId(0), SlotId(1), FACTORY_SLOT] {
            let label = slot_label(slot).expect("labelled");
            assert_eq!(slot_from_label(label), Some(slot));
        }
        assert_eq!(slot_label(SlotId(2)), None);
        assert_eq!(slot_from_label("ota_2"), None);
        assert_eq!(slot_from_label(""), None);
    }
}
