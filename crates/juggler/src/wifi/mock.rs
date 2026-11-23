//! [`MockLink`] — a test double for host-side unit tests.
//!
//! Enable with `features = ["mock"]` in `dev-dependencies`, or it is automatically
//! available inside `#[cfg(test)]` blocks within this crate.
//!
//! # Usage in a downstream crate
//!
//! ```toml
//! [dev-dependencies]
//! juggler = { workspace = true, features = ["mock"] }
//! ```

pub use super::link_mock::{MockLink, MockLinkError};
