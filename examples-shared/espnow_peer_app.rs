//! One ESP-NOW peer application for both tiers (ADR 022, parity plan Step 7).
//!
//! `idf_c3_espnow_peer` and `hal_c3_espnow_peer` include this file with
//! `#[path = "../../../examples-shared/espnow_peer_app.rs"] mod espnow_peer_app;`.
//! It is bench-only: both crates exclude their peer example from the published package.
//!
//! The app runs on a [`PeerSession`], which owns the scans: the boot scan, the rescan after
//! 20 consecutive unacknowledged sends and the retry backoff of a failed scan (`PeerLink`).
//! Every 100 ms it sends an 8-byte frame (big-endian sequence counter, then the milliseconds
//! since boot) to the coordinator through the session and counts the ACK; every frame the
//! session hands back is counted as an echo, never logged, so the output does not distort the
//! measurement.
//! Every 10 s the counters go out as the line the bench checks match (hardware gate G-E1, 30
//! minutes at 100 ms, failures below 1 %):
//!
//! ```text
//! [espnow] sent=N acked=N failed=N fail_rate=D.DD% echoes=N channel=Some(N) oversize_dropped=N rescans=N local_failures=N
//! ```
//!
//! The session itself logs `[espnow] coordinator found on channel N` when a scan succeeds and
//! `[espnow] N sends unacknowledged - rescanning` when the threshold triggers, so this file
//! prints neither.
//! No frame goes out before the first scan has found the coordinator (the scan registers the
//! peer) or while a scan holds the radio; those ticks are skipped and not counted.
//! A send that never left the node (`SendOutcome::Local`, on esp-hal a `Timeout` included)
//! counts as failed and in `local_failures`, never toward the rescan, and is logged at `debug`
//! only.
//!
//! The file compiles under the esp-hal example's `#![no_std]`, so it uses `core`,
//! `embassy-time` and `embassy-futures`, never `std`.

use core::cell::Cell;

use embassy_futures::join::join4;
use embassy_time::{Duration, Instant, Ticker, Timer};
use juggler::espnow::{
    ClassifySend, Clock, EspNowDriver, EspNowLink, PeerLinkState, PeerSession, SendOutcome,
};

/// Send period.
pub const SEND_PERIOD_MS: u64 = 100;
/// Bound on queueing and the start of a frame; a started frame is waited out by the tier.
pub const ACK_TIMEOUT: core::time::Duration = core::time::Duration::from_millis(50);
/// Counters line period.
pub const STATS_SECS: u64 = 10;

/// Tier-supplied readers the shared app cannot derive from the session.
pub struct PeerAppConfig<'a> {
    /// The channel the driver keeps pinned (`pinned_channel()`), printed as `channel=`.
    pub pinned_channel: &'a dyn Fn() -> Option<u8>,
    /// Driver counter of frames above `MAX_DATA_LEN` dropped by the receive path.
    pub oversize_dropped: &'a dyn Fn() -> u32,
}

/// Counters shared by the send, receive and stats loops; all run on one executor thread.
#[derive(Default)]
struct Counters {
    sent: Cell<u32>,
    acked: Cell<u32>,
    echoes: Cell<u32>,
}

impl Counters {
    fn bump(cell: &Cell<u32>) {
        cell.set(cell.get().wrapping_add(1));
    }
}

/// Runs the peer application until the process ends.
///
/// `session.run()` is started here, so the caller must not run it a second time.
pub async fn run<L, C>(session: &PeerSession<'_, L, C>, cfg: &PeerAppConfig<'_>)
where
    L: EspNowLink,
    <L as EspNowDriver>::Error: ClassifySend + core::fmt::Display,
    C: Clock,
{
    let counters = Counters::default();
    join4(
        session.run(),
        tx_loop(session, &counters),
        rx_loop(session, &counters),
        stats_loop(session, cfg, &counters),
    )
    .await;
}

/// Sends one frame per period through the session and counts the outcome.
async fn tx_loop<L, C>(session: &PeerSession<'_, L, C>, counters: &Counters)
where
    L: EspNowLink,
    <L as EspNowDriver>::Error: ClassifySend + core::fmt::Display,
    C: Clock,
{
    let mut ticker = Ticker::every(Duration::from_millis(SEND_PERIOD_MS));
    let mut seq: u32 = 0;
    loop {
        ticker.next().await;
        // Not before the first find (no peer registered yet) and not while a scan holds the
        // radio; the session would refuse the send anyway (`PeerSendError::Scanning`).
        if session.scanning() || session.state() == PeerLinkState::Searching {
            continue;
        }
        seq = seq.wrapping_add(1);
        let mut frame = [0u8; 8];
        frame[..4].copy_from_slice(&seq.to_be_bytes());
        frame[4..].copy_from_slice(&(Instant::now().as_millis() as u32).to_be_bytes());

        Counters::bump(&counters.sent);
        let result = session.send(&frame, ACK_TIMEOUT).await;
        match SendOutcome::of(&result) {
            SendOutcome::Acked => Counters::bump(&counters.acked),
            SendOutcome::NoAck => {}
            SendOutcome::Local => {
                // Counted in the `local_failures` field of the counters line; logged at
                // `debug` only, because an esp-hal `Timeout` (a frame that never started
                // within `ACK_TIMEOUT`) is a local failure too and the bench checks reject
                // `[espnow] send error:` at the default level.
                if let Err(e) = result {
                    log::debug!("[espnow] send error: {}", e);
                }
            }
        }
    }
}

/// Counts the frames the coordinator echoes back (reported in the periodic stats, never logged
/// per frame); draining keeps the driver's receive queue from overflowing.
/// Probe replies never arrive here: the scan takes them before `receive` sees them.
async fn rx_loop<L, C>(session: &PeerSession<'_, L, C>, counters: &Counters)
where
    L: EspNowLink,
    <L as EspNowDriver>::Error: ClassifySend,
    C: Clock,
{
    loop {
        let _ = session.receive().await;
        Counters::bump(&counters.echoes);
    }
}

/// Logs the counters line every [`STATS_SECS`].
async fn stats_loop<L, C>(
    session: &PeerSession<'_, L, C>,
    cfg: &PeerAppConfig<'_>,
    counters: &Counters,
) where
    L: EspNowLink,
    <L as EspNowDriver>::Error: ClassifySend,
    C: Clock,
{
    loop {
        Timer::after(Duration::from_secs(STATS_SECS)).await;
        let sent = counters.sent.get();
        let acked = counters.acked.get();
        let failed = sent - acked;
        // Failure rate in hundredths of a percent, integer only.
        let per_10k = (u64::from(failed) * 10_000) / u64::from(sent.max(1));
        log::info!(
            "[espnow] sent={} acked={} failed={} fail_rate={}.{:02}% echoes={} channel={:?} oversize_dropped={} rescans={} local_failures={}",
            sent,
            acked,
            failed,
            per_10k / 100,
            per_10k % 100,
            counters.echoes.get(),
            (cfg.pinned_channel)(),
            (cfg.oversize_dropped)(),
            session.rescans(),
            session.local_failures()
        );
    }
}
