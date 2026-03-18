//! Platform-independent networking primitives.
//!
//! No ESP-IDF dependency — every function in this crate is pure Rust and
//! can be compiled and tested on any host target.

pub mod backoff;
pub mod mqtt;
