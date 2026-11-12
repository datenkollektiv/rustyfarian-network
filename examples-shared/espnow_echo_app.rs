//! One ESP-NOW coordinator application for both tiers (ADR 022, parity plan Step 5).
//!
//! `idf_c3_espnow_coordinator` and `hal_c6_espnow_sta_coordinator` include this file with
//! `#[path = "../../../examples-shared/espnow_echo_app.rs"] mod espnow_echo_app;`.
//! It is bench-only: both crates exclude their coordinator example from the published package.
//!
//! The app answers a scan probe (`0xF3`, [`ProbeFrame`]) with a [`ProbeReply`] (`0xF4`) that
//! names the channel the radio is on (ADR 025), and echoes every other payload back to
//! its sender verbatim.
//! Unknown senders are registered as peers on the current channel first.
//!
//! The file compiles under the esp-hal example's `#![no_std]`, so it uses `core`,
//! `heapless`, `embassy-time` and `embassy-futures`, never `std`.

use core::cell::Cell;

use embassy_futures::join::join;
use embassy_time::{Duration, Timer};
use juggler::espnow::{
    ClassifySend, EspNowLink, MacAddress, PeerConfig, ProbeFrame, ProbeReply, SendOutcome,
};

/// Counters line period.
pub const STATS_SECS: u64 = 10;
/// Bound on queueing and the start of a reply; a probe reply must arrive within the scanner's
/// `reply_timeout` (50 ms by default), so a reply that cannot start in time is not worth sending.
pub const REPLY_TIMEOUT: core::time::Duration = core::time::Duration::from_millis(100);
/// Senders the app remembers as registered peers; the oldest is forgotten when the list is full.
const KNOWN_PEERS: usize = 8;

/// Tier-supplied knobs the shared app cannot derive from the traits.
pub struct EchoAppConfig<'a> {
    /// Driver counter of frames above `MAX_DATA_LEN` dropped by the receive path.
    pub oversize_dropped: &'a dyn Fn() -> u32,
    /// Tier heap reporter; the value is logged in the counters line.
    pub free_heap: fn() -> usize,
}

/// Counters shared by the receive loop and the stats loop; both run on one executor thread.
#[derive(Default)]
struct Counters {
    rx: Cell<u32>,
    reply_acked: Cell<u32>,
    reply_failed: Cell<u32>,
}

impl Counters {
    fn bump(cell: &Cell<u32>) {
        cell.set(cell.get().wrapping_add(1));
    }
}

/// Runs the coordinator application until the process ends.
pub async fn run<L>(link: &L, cfg: &EchoAppConfig<'_>)
where
    L: EspNowLink,
    L::Error: ClassifySend,
{
    let counters = Counters::default();
    join(rx_loop(link, &counters), stats_loop(link, cfg, &counters)).await;
}

/// Receives every frame, answers probes with the home channel and echoes the rest.
async fn rx_loop<L>(link: &L, counters: &Counters)
where
    L: EspNowLink,
    L::Error: ClassifySend,
{
    let mut known: heapless::Vec<MacAddress, KNOWN_PEERS> = heapless::Vec::new();
    loop {
        let event = link.receive().await;
        Counters::bump(&counters.rx);

        // Unicast needs a peer entry; channel 0 follows the radio's current channel.
        // A sender the list forgot (more than `KNOWN_PEERS` senders) is still registered and
        // both tiers answer `add_peer` with an "exists" error, so the send below decides: a peer
        // that is really missing fails there and is counted and logged as a failed reply.
        if !known.contains(&event.mac) {
            if let Err(e) = link.add_peer(&PeerConfig::new(event.mac)) {
                log::debug!(
                    "[app] add_peer {} failed: {:?}; sending anyway",
                    MacText(&event.mac),
                    e
                );
            }
            if known.is_full() {
                known.remove(0);
            }
            let _ = known.push(event.mac);
        }

        let reply_frame;
        let reply: &[u8] = match ProbeFrame::parse(event.payload()) {
            Some(probe) => {
                let home = link.current_channel().unwrap_or(0);
                reply_frame = ProbeReply::answering(&probe, home).encode();
                &reply_frame
            }
            None => event.payload(),
        };

        let result = link.send_acked(&event.mac, reply, REPLY_TIMEOUT).await;
        match SendOutcome::of(&result) {
            SendOutcome::Acked => Counters::bump(&counters.reply_acked),
            SendOutcome::NoAck => Counters::bump(&counters.reply_failed),
            SendOutcome::Local => {
                Counters::bump(&counters.reply_failed);
                if let Err(e) = result {
                    log::warn!("[app] reply to {} failed: {:?}", MacText(&event.mac), e);
                }
            }
        }
    }
}

/// Logs the counters and the radio channel every [`STATS_SECS`].
async fn stats_loop<L>(link: &L, cfg: &EchoAppConfig<'_>, counters: &Counters)
where
    L: EspNowLink,
    L::Error: ClassifySend,
{
    loop {
        Timer::after(Duration::from_secs(STATS_SECS)).await;
        log::info!(
            "[app] rx={} reply_acked={} reply_failed={} oversize_dropped={} channel={:?} heap_free={} B",
            counters.rx.get(),
            counters.reply_acked.get(),
            counters.reply_failed.get(),
            (cfg.oversize_dropped)(),
            link.current_channel(),
            (cfg.free_heap)()
        );
    }
}

/// `aa:bb:cc:dd:ee:ff` rendering of a MAC for the log lines.
struct MacText<'a>(&'a MacAddress);

impl core::fmt::Display for MacText<'_> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let m = self.0;
        write!(
            f,
            "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
            m[0], m[1], m[2], m[3], m[4], m[5]
        )
    }
}
