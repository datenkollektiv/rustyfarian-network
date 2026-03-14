//! Platform-independent primitives shared by `rustyfarian-esp-idf-mqtt`
//! and `rustyfarian-esp-idf-wifi`.
//!
//! No ESP-IDF dependency — every function in this crate is pure Rust and
//! can be compiled and tested on any host target.

pub mod backoff;
pub mod mqtt;

#[deprecated(since = "0.2.0", note = "depend on `wifi-pure` directly")]
pub mod wifi;
