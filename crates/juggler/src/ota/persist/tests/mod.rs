//! Host tests of the record store, run by `just test-ota`.

mod boot_flow;
mod corrupt_probe;
mod counter_loss;
mod crash_matrix;
mod ddi_record;
#[cfg(feature = "ota-ddi")]
mod ddi_verdict;
mod errors;
mod fail_closed;
mod failed_download;
mod fault_kv_model;
#[cfg(feature = "ota-flash-kv")]
mod flash_kv;
mod self_heal;
mod sequences;
mod stored_states;
mod support;
