//! Every error type of the record store is a `core::error::Error` that `anyhow` accepts, with its source chain.

use alloc::boxed::Box;
use core::error::Error;

use crate::ota::persist::{
    BootFault, CorruptRecord, HardwareReadError, KvError, NoteBootError, StoreError,
};

/// The bound of `anyhow::Error::from`.
fn anyhow_ready<E: Error + Send + Sync + 'static>(_: &E) {}

#[test]
fn every_error_type_is_a_boxable_error_that_anyhow_accepts() {
    let kv = KvError::new(5);
    let corrupt = CorruptRecord::UnknownAttemptSlotLabel;
    let hardware = HardwareReadError::Read(7);
    let store = StoreError::Kv(kv);
    let note = NoteBootError::Hardware(hardware);
    let fault = BootFault::Store(StoreError::Corrupt(corrupt));
    anyhow_ready(&kv);
    anyhow_ready(&corrupt);
    anyhow_ready(&hardware);
    anyhow_ready(&store);
    anyhow_ready(&note);
    anyhow_ready(&fault);
    let boxed: [Box<dyn Error>; 6] = [
        Box::new(kv),
        Box::new(corrupt),
        Box::new(hardware),
        Box::new(store),
        Box::new(note),
        Box::new(fault),
    ];
    for e in &boxed {
        assert!(!alloc::format!("{e}").is_empty());
    }
}

#[test]
fn wrappers_expose_their_inner_error_as_source() {
    let kv = KvError::new(5);
    let store = StoreError::Kv(kv);
    assert_eq!(store.source().unwrap().downcast_ref::<KvError>(), Some(&kv));
    let corrupt = StoreError::Corrupt(CorruptRecord::UnknownRequestSlotLabel);
    assert_eq!(
        corrupt.source().unwrap().downcast_ref::<CorruptRecord>(),
        Some(&CorruptRecord::UnknownRequestSlotLabel)
    );
    assert!(StoreError::ReportPending.source().is_none());
    assert!(StoreError::InvalidSlot.source().is_none());

    let hardware = HardwareReadError::Busy;
    let fault = BootFault::Hardware(hardware);
    assert_eq!(
        fault.source().unwrap().downcast_ref::<HardwareReadError>(),
        Some(&hardware)
    );
    let fault = BootFault::Store(store);
    assert_eq!(
        fault.source().unwrap().downcast_ref::<StoreError>(),
        Some(&store)
    );
    let note = NoteBootError::Store(store);
    assert_eq!(
        note.source().unwrap().downcast_ref::<StoreError>(),
        Some(&store)
    );
    let note = NoteBootError::Hardware(hardware);
    assert_eq!(
        note.source().unwrap().downcast_ref::<HardwareReadError>(),
        Some(&hardware)
    );
    assert!(kv.source().is_none() && hardware.source().is_none());
}
