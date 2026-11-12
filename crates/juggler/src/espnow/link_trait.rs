//! [`EspNowLink`] and [`ClassifySend`] — the async ESP-NOW surface both tiers implement (ADR 022).
//!
//! Firmware that is generic over `EspNowLink` runs unchanged on the ESP-IDF and the esp-hal
//! driver; only the setup code differs.
//! [`ClassifySend`] maps a tier's error onto the portable [`SendOutcome`] that
//! [`PeerLink`](super::PeerLink) and [`ScanMachine`](super::ScanMachine) consume.
//!
//! ```
//! use juggler::espnow::{ClassifySend, SendOutcome};
//!
//! enum MyError {
//!     NoAck,
//!     QueueFull,
//! }
//!
//! impl ClassifySend for MyError {
//!     fn outcome(&self) -> SendOutcome {
//!         match self {
//!             MyError::NoAck => SendOutcome::NoAck,
//!             MyError::QueueFull => SendOutcome::Local,
//!         }
//!     }
//! }
//!
//! assert_eq!(SendOutcome::of(&Ok::<(), MyError>(())), SendOutcome::Acked);
//! assert_eq!(SendOutcome::of(&Err(MyError::NoAck)), SendOutcome::NoAck);
//! ```

use core::time::Duration;

use super::{EspNowDriver, EspNowEvent, MacAddress, ScanConfig, ScanResult, SendOutcome};

/// Classifies a tier's send error as a portable [`SendOutcome`].
///
/// Only a frame that was transmitted but not acknowledged is [`SendOutcome::NoAck`]; every other
/// failure (queue full, timeout before the frame started, scan in progress, invalid peer,
/// driver error) is [`SendOutcome::Local`], because the peer never had a chance to answer.
/// The distinction matters: unacknowledged frames count toward a rescan, local failures do not.
pub trait ClassifySend {
    /// The portable outcome of the failed send this error describes.
    fn outcome(&self) -> SendOutcome;
}

impl SendOutcome {
    /// Classifies a send result: `Ok` is [`Acked`](SendOutcome::Acked), an error is whatever
    /// [`ClassifySend::outcome`] says.
    pub fn of<E: ClassifySend>(result: &Result<(), E>) -> SendOutcome {
        match result {
            Ok(()) => SendOutcome::Acked,
            Err(error) => error.outcome(),
        }
    }
}

/// Async ESP-NOW link surface shared by both tiers.
///
/// It extends [`EspNowDriver`] (peer registration, fire-and-forget `send`, polled `try_recv`)
/// with the acknowledged, awaitable operations an application needs.
///
/// # Semantics
///
/// * [`send_acked`](Self::send_acked) resolves `Ok` only on the MAC-layer ACK of a unicast
///   frame.
///   Its `timeout` bounds queueing and the start of the transmission: a frame that cannot start
///   within it fails with a timeout that classifies [`SendOutcome::Local`].
///   Once a frame has started it is waited out, so a late completion is never mistaken for the
///   next frame's ACK.
///   [`Duration::MAX`] means no bound.
///   A send issued while a scan runs fails at once, classified [`SendOutcome::Local`].
///   A send to the broadcast MAC is refused, classified [`SendOutcome::Local`]: broadcast frames
///   are never acknowledged, so use [`EspNowDriver::send`] for broadcast.
///   The future is cancel-safe: dropping it neither loses nor steals another call's result.
///   Fail-closed: a started frame is waited out without limit, so a send callback that never
///   fires blocks this call and every later transmission (sends and scan probes alike) until the
///   driver is dropped or shut down; callback timing is typical (milliseconds), never guaranteed.
/// * [`receive`](Self::receive) is infallible and waits for the next frame.
///   Concurrent `receive` calls are safe and each frame goes to exactly one caller; no ordering
///   among waiting callers is promised.
///   Frames above [`MAX_DATA_LEN`](super::MAX_DATA_LEN) are dropped and counted, never
///   truncated (see [`EspNowEvent::try_new`] and [`OversizeLog`](super::OversizeLog)).
/// * [`scan_for_peer`](Self::scan_for_peer) probes the configured channels for `mac`.
///   The driver keeps the discovered channel as its pin ([`pinned_channel`](Self::pinned_channel)),
///   so the caller stores nothing.
///   A second scan while one runs is refused, and so is a scan on a radio the driver does not
///   own (it must not change the channel under a connected station).
///   The scan never changes the channel under a probe on the air, so it is fail-closed too: a
///   probe whose send callback never fires blocks the scan and every later transmission until
///   the driver is dropped or shut down.
/// * [`pinned_channel`](Self::pinned_channel) is the channel the driver pinned, if any.
/// * [`current_channel`](Self::current_channel) is the channel the radio is on right now, read
///   from the Wi-Fi driver; it is what a coordinator puts into a probe reply (ADR 025).
///
/// The returned futures are not required to be `Send`, because the esp-hal ones are not
/// (ADR 022).
#[allow(async_fn_in_trait)] // futures deliberately carry no `Send` bound (ADR 022)
pub trait EspNowLink: EspNowDriver
where
    <Self as EspNowDriver>::Error: ClassifySend,
{
    /// Sends `data` to `mac` and resolves on the MAC-layer ACK.
    ///
    /// See the [trait docs](EspNowLink) for the timeout, scan and cancellation rules.
    ///
    /// Fail-closed: once the frame has started, the wait for its send callback has no limit.
    /// A callback that never fires blocks this call and every later transmission until the
    /// driver is dropped or shut down.
    async fn send_acked(
        &self,
        mac: &MacAddress,
        data: &[u8],
        timeout: Duration,
    ) -> Result<(), Self::Error>;

    /// Waits for the next received frame.
    async fn receive(&self) -> EspNowEvent;

    /// Scans the configured channels for the peer `mac` and pins the channel it answers on.
    ///
    /// Fail-closed: a probe that has started is waited out without limit (the channel must not
    /// change under it), so a send callback that never fires blocks the scan and every later
    /// transmission until the driver is dropped or shut down.
    async fn scan_for_peer(
        &self,
        mac: &MacAddress,
        config: &ScanConfig<'_>,
    ) -> Result<ScanResult, Self::Error>;

    /// The channel the driver pinned (by a scan or at start), or `None` if it has none.
    fn pinned_channel(&self) -> Option<u8>;

    /// The primary Wi-Fi channel the radio is on right now, or `None` when the driver cannot
    /// read it (radio not started, or the Wi-Fi driver refused the query).
    ///
    /// Unlike [`pinned_channel`](Self::pinned_channel) this is the live radio state, so on a
    /// caller-managed radio it follows the access point, and during a scan it follows the probes.
    /// A coordinator answers a scan probe with it as the reply's `home_channel` (ADR 025).
    fn current_channel(&self) -> Option<u8>;

    /// Stops the driver's asynchronous machinery; idempotent and never blocking.
    ///
    /// Queued and in-flight requests end with the tier's shutdown error, a running scan is
    /// aborted without a teardown (the channel, TX power and peer table stay as they are), and
    /// every later request is refused with the same error.
    /// A frame already on the air still finishes; its result is ignored.
    /// On the ESP-IDF tier this is what dropping the driver does first (the drop also joins the
    /// worker thread); on the esp-hal tier the handle is usually `'static`, so this is the only
    /// way to stop it, and the sender runner returns once it was called.
    fn shutdown(&self);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::espnow::mock::{MockEspNowLink, MockLinkError};
    use crate::espnow::PeerConfig;
    use crate::test_support::block_on;

    const PEER: MacAddress = [0x02, 0, 0, 0, 0, 0x01];

    #[test]
    fn of_maps_ok_to_acked_and_errors_through_classify() {
        assert_eq!(
            SendOutcome::of(&Ok::<(), MockLinkError>(())),
            SendOutcome::Acked
        );
        assert_eq!(
            SendOutcome::of(&Err(MockLinkError::NoAck)),
            SendOutcome::NoAck
        );
        assert_eq!(
            SendOutcome::of(&Err(MockLinkError::Local)),
            SendOutcome::Local
        );
    }

    /// One application body, generic over the link: what firmware shared between tiers does.
    ///
    /// The test below proves the generic API is usable.
    /// It is not tier-parity evidence: that is the per-tier compile checks and the shared
    /// example.
    async fn app<L>(link: &L, config: &ScanConfig<'_>) -> (SendOutcome, SendOutcome, Option<u8>)
    where
        L: EspNowLink,
        L::Error: ClassifySend,
    {
        let first = link
            .send_acked(&PEER, b"hello", Duration::from_millis(50))
            .await;
        let found = link.scan_for_peer(&PEER, config).await;
        let second = link.send_acked(&PEER, b"again", Duration::MAX).await;
        let _ = found;
        let heard = link.receive().await;
        assert_eq!(heard.payload(), b"pong");
        (
            SendOutcome::of(&first),
            SendOutcome::of(&second),
            link.pinned_channel(),
        )
    }

    #[test]
    fn one_app_both_tiers_espnow() {
        let link = MockEspNowLink::new();
        link.add_peer(&PeerConfig::new(PEER)).unwrap();
        link.script_send(Err(MockLinkError::NoAck));
        link.script_scan(Ok(ScanResult { channel: 6 }));
        link.queue_rx_event(EspNowEvent::new(PEER, b"pong"));

        let (first, second, pin) = block_on(app(&link, &ScanConfig::for_peer()));

        assert_eq!(first, SendOutcome::NoAck);
        assert_eq!(second, SendOutcome::Acked);
        assert_eq!(pin, Some(6));
        let sent = link.sent_messages();
        assert_eq!(sent.len(), 2, "a transmitted but unacked frame was sent");
        assert_eq!(sent[1].1, b"again");
        assert_eq!(link.last_send_timeout(), Some(Duration::MAX));
    }

    #[test]
    fn local_failure_is_not_recorded_as_sent() {
        let link = MockEspNowLink::new();
        link.script_send(Err(MockLinkError::Local));
        let result = block_on(link.send_acked(&PEER, b"x", Duration::ZERO));
        assert_eq!(SendOutcome::of(&result), SendOutcome::Local);
        assert!(link.sent_messages().is_empty());
    }

    #[test]
    fn shutdown_is_idempotent_through_the_trait() {
        fn stop<L: EspNowLink>(link: &L)
        where
            L::Error: ClassifySend,
        {
            link.shutdown();
            link.shutdown();
        }
        let link = MockEspNowLink::new();
        assert!(!link.is_shut_down());
        stop(&link);
        assert!(link.is_shut_down());
    }

    #[test]
    fn failed_scan_keeps_the_pin() {
        let link = MockEspNowLink::new();
        link.set_pinned_channel(Some(3));
        link.script_scan(Err(MockLinkError::Local));
        let result = block_on(link.scan_for_peer(&PEER, &ScanConfig::for_peer()));
        assert!(result.is_err());
        assert_eq!(link.pinned_channel(), Some(3));
    }
}
