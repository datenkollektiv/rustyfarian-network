//! Platform-independent networking primitives.
//!
//! No ESP-IDF dependency — every function in this crate is pure Rust and
//! can be compiled and tested on any host target.
//!
//! The crate is `no_std`-compatible.
//! The default `std` feature gates the items that require the standard library
//! (`format_broker_url` and the subscriber-thread machinery); with
//! `default-features = false` only the `no_std`-safe validators and constants
//! remain, so pure consumers can pull them in without dragging in `std`.

#![cfg_attr(not(feature = "std"), no_std)]

pub mod backoff;
pub mod mqtt;
pub mod status_colors;
