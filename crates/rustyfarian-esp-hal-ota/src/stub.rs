//! Host-build stub.
//!
//! Compiled when no chip + embassy features are enabled.
//! Provides type placeholders so `cargo check -p rustyfarian-esp-hal-ota`
//! (no features) succeeds on the host.
//!
//! The stub mirrors the *type names* of the real implementation but does not
//! mirror their signatures — the real `new` requires the `FLASH` peripheral
//! and `fetch_and_apply` requires `embassy_net::tcp::TcpSocket`, neither of
//! which are available without the chip + embassy features. Any code that
//! calls the real surface without those features fails to compile, which is
//! the desired behaviour: the stub is a typecheck convenience, not a runtime
//! substitute. Mirrors the pattern used by `rustyfarian-esp-hal-wifi`.

/// Experimental: API may change before 1.0.
///
/// Configuration for [`EspHalOtaManager`].
#[derive(Debug, Clone, Copy)]
pub struct OtaManagerConfig {
    /// HTTP connection + read timeout in seconds.
    pub timeout_secs: u64,
}

/// Experimental: API may change before 1.0.
///
/// Bare-metal OTA manager placeholder.
///
/// Compiled when chip + `embassy` features are not active. The real type
/// (with `new`, `fetch_and_apply`, `mark_valid`, `rollback`) lives in
/// `manager.rs` and is gated on those features.
#[derive(Debug, Default)]
pub struct EspHalOtaManager;

impl EspHalOtaManager {
    /// Stub constructor that mirrors the real type's surface for typechecks.
    ///
    /// Real OTA usage requires both a chip feature (`esp32c3`, `esp32c6`,
    /// `esp32`) **and** the `embassy` feature.
    pub fn new() -> Self {
        Self
    }
}
