//! Host tests of the record store, run by `just test-ota`.

mod boot_flow;
mod corrupt_probe;
mod counter_loss;
mod crash_matrix;
mod errors;
mod fail_closed;
mod failed_download;
mod fault_kv_model;
mod self_heal;
mod sequences;
mod stored_states;
mod support;
