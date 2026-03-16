//! Platform-independent primitives shared by `rustyfarian-esp-idf-mqtt`
//! and `rustyfarian-esp-idf-wifi`.
//!
//! No ESP-IDF dependency — every function in this crate is pure Rust and
//! can be compiled and tested on any host target.

pub mod backoff;
pub mod mqtt;
