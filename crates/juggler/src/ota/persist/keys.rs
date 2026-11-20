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
/// `str`: DDI action id of the action record, canonical `i64` digits ([`is_canonical_action_id`]).
pub const DDI_ACT: &str = "ddi_act";
/// `u8`: stage of the DDI action record; the commit marker, written last and removed first.
pub const DDI_STAGE: &str = "ddi_stage";
/// `u32`: attempt id bound to the DDI action, absent for an action that never started an attempt.
pub const DDI_ATT: &str = "ddi_att";
/// `u32`: verdict code of the DDI action, 0 for success or a profile code 1001 to 1007.
pub const DDI_CODE: &str = "ddi_code";
/// `str`: first feedback detail of the verdict.
pub const DDI_DET0: &str = "ddi_det0";
/// `str`: second feedback detail of the verdict.
pub const DDI_DET1: &str = "ddi_det1";
/// Every key of the namespace.
#[cfg(test)]
pub(crate) const KEYS: [&str; 22] = [
    RB, RB_WHY, RB_ID, ATT_CTR, ATT_EPOCH, ATT_ID, ATT_BOOT, ATT_VER, ATT_SLOT, ATT_ACT, ATT_INV,
    ATT_WHY, RQ_ID, RQ_WHY, RQ_FROM, REJ_VER, DDI_ACT, DDI_STAGE, DDI_ATT, DDI_CODE, DDI_DET0,
    DDI_DET1,
];

/// `ddi_stage`: the action was admitted and its attempt (if `ddi_att` matches `att_id`) is running.
pub const STAGE_RUNNING: u8 = 1;
/// `ddi_stage`: the terminal verdict awaits delivery.
pub const STAGE_VERDICT_PENDING: u8 = 2;
/// `ddi_stage`: the verdict was delivered (or the server no longer knows the action); never re-applied.
pub const STAGE_CLOSED: u8 = 3;

pub use crate::ota::action_id::is_canonical_action_id;

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
                "rej_ver",
                "ddi_act",
                "ddi_stage",
                "ddi_att",
                "ddi_code",
                "ddi_det0",
                "ddi_det1"
            ]
        );
    }

    #[test]
    fn canonical_action_ids_are_i64_digits_without_leading_zeros() {
        for id in ["0", "1", "42", "1234567890123456789", "9223372036854775807"] {
            assert!(is_canonical_action_id(id), "{id}");
        }
        for id in [
            "",
            "00",
            "012",
            "9223372036854775808",
            "9999999999999999999",
            "12345678901234567890",
            "+1",
            "-1",
            " 1",
            "1 ",
            "1a",
            "\u{661}",
        ] {
            assert!(!is_canonical_action_id(id), "{id:?}");
        }
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
