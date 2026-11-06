//! [`NetworkLink`] — the station-link surface both tiers implement (ADR 022).
//!
//! Firmware that is generic over `NetworkLink` runs unchanged on the ESP-IDF
//! `WiFiManager` and the esp-hal `WifiLink`; only the setup code differs.
//! The behaviour behind it lives in [`LinkMachine`](super::LinkMachine) and
//! [`LinkHandover`](super::LinkHandover), sans-IO state machines that each tier
//! drives with a thin runner.

use super::LinkState;

/// Station link control shared by both tiers.
///
/// Hardware differences stay capabilities, not API differences: `suspend`
/// stops the radio on ESP-IDF but only disconnects on esp-hal (see
/// `docs/wifi-suspend-and-deep-sleep.md`).
///
/// The returned futures are not required to be `Send`, because the esp-hal
/// ones are not; generic code runs them on the executor that owns the link.
#[allow(async_fn_in_trait)] // futures deliberately carry no `Send` bound (ADR 022)
pub trait NetworkLink {
    /// Tier error for `suspend` and `resume`; `core::convert::Infallible` where
    /// they cannot fail.
    type Error: core::fmt::Debug;

    /// Current link state as seen by the link state machine.
    fn state(&self) -> LinkState;

    /// `true` while the station is associated ([`LinkState::Connected`]).
    fn is_connected(&self) -> bool {
        self.state() == LinkState::Connected
    }

    /// `true` once a requested suspend has taken effect.
    fn is_suspended(&self) -> bool;

    /// Associations completed since boot; a change means a new link session.
    fn connect_count(&self) -> u32;

    /// Takes the station down and stops reconnecting until [`resume`](Self::resume).
    fn suspend(&self) -> Result<(), Self::Error>;

    /// Brings the station back after [`suspend`](Self::suspend) with a fresh back-off.
    fn resume(&self) -> Result<(), Self::Error>;

    /// Completes once the station is associated and has its IP configuration.
    async fn wait_connected(&self);

    /// Completes once a requested [`suspend`](Self::suspend) has taken effect,
    /// that is when [`is_suspended`](Self::is_suspended) is `true`.
    ///
    /// Has no timeout of its own: wrap it in the executor's timeout if the
    /// caller needs a bound. On ESP-IDF the suspend finishes before
    /// `suspend` returns, so this completes at once; on esp-hal it completes
    /// when the supervisor has finished the disconnect, which is already
    /// bounded. It stays pending if nothing was suspended, or if
    /// [`resume`](Self::resume) overtakes the suspend before it completed.
    async fn wait_suspended(&self);
}
