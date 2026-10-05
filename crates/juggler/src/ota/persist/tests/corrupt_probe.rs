//! `corrupt_present` is the read-only twin of `repair_corrupt`.

use super::support::*;
use crate::ota::persist::fault_kv::FaultKv;
use crate::ota::persist::{KvError, OtaStore};

const BAD: i32 = KvError::CODE_INVALID_VALUE;

fn repair_all(s: &mut OtaStore<FaultKv>) -> usize {
    let mut n = 0;
    while matches!(s.repair_corrupt(), Ok(Some(_))) {
        n += 1;
        assert!(n < 16, "repair must converge");
    }
    n
}

#[test]
fn a_clean_store_has_nothing_to_repair() {
    let kv = FaultKv::new();
    assert_eq!(store(&kv).corrupt_present(), Ok(false));
    put_counter(&kv, 5);
    put_attempt(&kv, 5, "2.0.0", "ota_1", true, true);
    put_request(&kv, 9, "operator", "ota_0");
    kv.put_u8("rb", 0)
        .put_u32("rb_id", 2)
        .put_str("rej_ver", "1.0.0");
    assert_eq!(store(&kv).corrupt_present(), Ok(false));
}

#[test]
fn every_corruptible_key_is_seen_and_repair_ends_it() {
    let keys = [
        "att_ver",
        "att_slot",
        "att_id",
        "att_boot",
        "att_act",
        "att_inv",
        "rq_from",
        "rq_id",
        "rb",
        "rb_id",
        "att_ctr",
        "att_epoch",
        "rej_ver",
    ];
    for key in keys {
        let kv = FaultKv::new();
        put_counter(&kv, 5);
        put_attempt(&kv, 5, "2.0.0", "ota_1", true, true);
        put_request(&kv, 9, "operator", "ota_0");
        kv.put_u8("rb", 0)
            .put_u32("rb_id", 2)
            .put_str("rej_ver", "1.5.0");
        kv.corrupt_with(key, BAD);
        let mut s = store(&kv);
        assert_eq!(s.corrupt_present(), Ok(true), "{key}");
        let before = kv.commits();
        assert_eq!(s.corrupt_present(), Ok(true), "{key}");
        assert_eq!(kv.commits(), before, "{key}: the probe never writes");
        assert!(repair_all(&mut s) >= 1, "{key}");
        assert_eq!(s.corrupt_present(), Ok(false), "{key}");
    }
}

#[test]
fn an_unknown_slot_label_counts_as_corrupt() {
    let kv = FaultKv::new();
    put_counter(&kv, 5);
    put_attempt(&kv, 5, "2.0.0", "ota_9", true, true);
    let mut s = store(&kv);
    assert_eq!(s.corrupt_present(), Ok(true));
    repair_all(&mut s);
    assert_eq!(s.corrupt_present(), Ok(false));

    let kv = FaultKv::new();
    put_counter(&kv, 5);
    put_request(&kv, 9, "operator", "ota_9");
    assert_eq!(store(&kv).corrupt_present(), Ok(true));
}

#[test]
fn a_transient_read_error_is_an_error_not_a_verdict() {
    let kv = FaultKv::new();
    put_counter(&kv, 5);
    kv.fail_read_at(1);
    assert!(store(&kv).corrupt_present().is_err());
}
