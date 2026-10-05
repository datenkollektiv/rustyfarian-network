//! The fault model itself: the matrix is only as good as this backend's fidelity.

use super::support::*;
use crate::ota::persist::fault_kv::{FaultKv, CRASHED, INJECTED};
use crate::ota::persist::{KvError, OtaKv};

#[test]
fn values_are_typed_and_a_wrong_type_is_an_error_not_absent() {
    let mut kv = FaultKv::new();
    kv.set_u8("a", 1).unwrap();
    assert_eq!(kv.get_u8("a"), Ok(Some(1)));
    assert_eq!(
        kv.get_u32("a"),
        Err(KvError::new(KvError::CODE_TYPE_MISMATCH))
    );
    assert_eq!(
        kv.get_str("a"),
        Err(KvError::new(KvError::CODE_TYPE_MISMATCH))
    );
    assert_eq!(kv.get_u8("missing"), Ok(None));
}

#[test]
fn a_string_set_is_two_commits_and_a_remove_of_an_absent_key_is_none() {
    let mut kv = FaultKv::new();
    kv.set_str("s", "x").unwrap();
    assert_eq!(kv.commits(), 2);
    kv.remove("absent").unwrap();
    assert_eq!(kv.commits(), 2);
    kv.remove("s").unwrap();
    assert_eq!(kv.commits(), 3);
    assert_eq!(kv.ops(), strs(&["set:s", "rm:s"]));
}

#[test]
fn over_long_strings_are_rejected() {
    let mut kv = FaultKv::new();
    let long = "x".repeat(33);
    assert_eq!(
        kv.set_str("s", &long),
        Err(KvError::new(KvError::CODE_INVALID_VALUE))
    );
    assert_eq!(kv.commits(), 0);
}

#[test]
fn crash_after_applies_the_commit_then_kills_the_store_until_revived() {
    let mut kv = FaultKv::new();
    kv.crash_after(2);
    kv.set_u8("a", 1).unwrap();
    assert_eq!(kv.set_u8("b", 2), Err(KvError::new(CRASHED)));
    assert_eq!(kv.u8_of("b"), Some(2));
    assert_eq!(kv.set_u8("c", 3), Err(KvError::new(CRASHED)));
    assert!(!kv.has("c"));
    assert_eq!(kv.get_u8("a"), Err(KvError::new(CRASHED)));
    kv.revive();
    assert_eq!(kv.get_u8("b"), Ok(Some(2)));
}

#[test]
fn fail_at_does_not_apply_and_the_store_survives() {
    let mut kv = FaultKv::new();
    kv.fail_at(1);
    assert_eq!(kv.set_u8("a", 1), Err(KvError::new(INJECTED)));
    assert!(!kv.has("a"));
    kv.set_u8("a", 1).unwrap();
    assert!(kv.has("a"));
}

#[test]
fn fail_after_erase_leaves_the_string_key_absent() {
    let mut kv = FaultKv::new();
    kv.put_str("s", "old");
    kv.fail_after_erase(1);
    assert_eq!(kv.set_str("s", "new"), Err(KvError::new(INJECTED)));
    assert!(!kv.has("s"));
    assert_eq!(kv.get_str("s"), Ok(None));
}

#[test]
fn read_faults_and_corrupt_keys() {
    let kv = FaultKv::new();
    kv.put_u8("a", 1);
    kv.fail_read_at(2);
    assert_eq!(kv.get_u8("a"), Ok(Some(1)));
    assert_eq!(kv.get_u8("a"), Err(KvError::new(INJECTED)));
    kv.corrupt("a");
    assert!(kv.get_u8("a").is_err());
    kv.revive();
    assert_eq!(kv.get_u8("a"), Ok(Some(1)));
}

#[test]
fn a_set_replaces_a_value_of_another_type_in_one_commit() {
    let mut kv = FaultKv::new();
    kv.put_str("rb", "x");
    kv.corrupt_with("rb", KvError::CODE_TYPE_MISMATCH);
    assert!(kv.get_u8("rb").is_err());
    kv.set_u8("rb", 1).unwrap();
    assert_eq!(kv.commits(), 1);
    assert_eq!(kv.ops(), strs(&["set:rb"]));
    assert_eq!(kv.get_u8("rb"), Ok(Some(1)));
}
