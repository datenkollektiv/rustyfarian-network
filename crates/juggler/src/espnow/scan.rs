//! Clock-injected ESP-NOW peer-scan state machine.
//!
//! [`ScanMachine`] is the pure core of `scan_for_peer`.
//! It implements the channel-scan algorithm of ADR 009 and ADR 012 (shared by both tiers per
//! ADR 020) without touching a radio, a clock or a thread, so the ESP-IDF tier (blocking I/O)
//! and the esp-hal tier (embassy async I/O) drive one implementation.
//!
//! # Driving the machine
//!
//! The caller owns I/O and time.
//! It feeds [`ScanInput`] values together with the current monotonic time and performs the
//! returned [`ScanAction`]:
//!
//! 1. Call [`ScanMachine::poll`] with [`ScanInput::Start`].
//! 2. Perform the returned action.
//! 3. Report the outcome with [`ScanInput::Completed`] (the operation succeeded, or the
//!    requested wait has elapsed), [`ScanInput::Failed`] (the operation failed) or, after
//!    [`ScanAction::SendProbe`], [`ScanInput::Sent`].
//!    To give up while the scan is running, feed [`ScanInput::Abort`] instead.
//! 4. Repeat until [`ScanAction::Done`].
//!
//! `now` is any monotonic duration since an arbitrary epoch; only differences matter.
//!
//! ```text
//! RemovePeer -> RegisterPeer{0} -> BoostTxPower
//!   per channel: SetChannel -> SendProbe -> [WaitUntil -> SendProbe]*  (confirmations)
//!   RestoreTxPower -> RemovePeer
//!     found:     RegisterPeer{channel}                                -> Done(Ok)
//!     not found: [RestoreChannel{pin} -> RegisterFallbackPeer{pin}]   -> Done(Err)
//! ```
//!
//! A failed registration (temporary peer after `RemovePeer`, discovered peer after the
//! teardown) takes the same best-effort `[RestoreChannel -> RegisterFallbackPeer]` path
//! and then finishes with the registration error.
//!
//! # Contract for the I/O side
//!
//! - [`ScanAction::SendProbe`] carries the probe timeout.
//!   The caller waits at most that long for the MAC-layer ACK and reports a timeout as
//!   [`SendOutcome::NoAck`]; an enqueue failure is [`SendOutcome::Error`].
//! - [`ScanAction::RemovePeer`] is best effort: both `Completed` and `Failed` continue.
//! - [`ScanAction::BoostTxPower`] and [`ScanAction::RestoreTxPower`] are advisory; a driver
//!   that cannot change TX power reports `Completed` and the scan proceeds.
//!   The IDF tier boosts to `TxPowerLevel::Max` for the burst and restores the previous level.
//! - The peer must be registered on the driver's default [`WifiInterface`](super::WifiInterface),
//!   encryption off, exactly as the IDF tier does.
//!
//! - [`ScanInput::Abort`], or an input that does not match the previous action, ends the scan
//!   with [`ScanError::Aborted`] / [`ScanError::UnexpectedInput`].
//!   While the scan is running the machine first unwinds through the normal
//!   `RestoreTxPower -> RemovePeer -> fallback` sequence, so the caller needs no extra cleanup.
//!   Before [`ScanInput::Start`], or once the teardown has begun, it finishes at once and the
//!   caller owns whatever remains.
//!
//! The machine never loops on its own: every invocation of [`ScanMachine::poll`] returns
//! within a bounded number of steps.

use core::time::Duration;

use super::{MacAddress, ScanConfig, ScanResult};

/// Outcome of one probe transmission, reported after [`ScanAction::SendProbe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    /// The peer acknowledged the frame at the MAC layer.
    Acked,
    /// The frame was transmitted but not acknowledged, or the ACK wait timed out.
    NoAck,
    /// The frame could not be enqueued (driver error).
    Error,
}

/// Input fed to [`ScanMachine::poll`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanInput {
    /// Begin the scan.  Valid exactly once, as the first input.
    Start,
    /// The previous action succeeded (for [`ScanAction::WaitUntil`]: the deadline has passed).
    Completed,
    /// The previous action failed.
    Failed,
    /// The caller cannot continue (e.g. the ACK callback could not be registered).
    /// The machine unwinds through its normal teardown and reports [`ScanError::Aborted`].
    Abort,
    /// Result of the previous [`ScanAction::SendProbe`].
    Sent(SendOutcome),
}

/// Action the caller must perform before polling again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanAction {
    /// Deregister the peer.  Best effort.
    RemovePeer,
    /// Register the peer on `channel` (`0` = current channel).
    RegisterPeer {
        /// Wi-Fi channel, `0` for "current channel".
        channel: u8,
    },
    /// Boost TX power for the scan burst.  Advisory.
    BoostTxPower,
    /// Restore the TX power saved before [`ScanAction::BoostTxPower`].  Advisory.
    RestoreTxPower,
    /// Switch the radio to this primary channel.
    SetChannel {
        /// Wi-Fi primary channel.
        channel: u8,
    },
    /// Switch the radio back to the previously pinned channel after an unsuccessful scan.
    /// Best effort: both `Completed` and `Failed` continue.
    RestoreChannel {
        /// The previously pinned channel.
        channel: u8,
    },
    /// Register the peer on the previously pinned channel after an unsuccessful scan.
    /// Best effort: both `Completed` and `Failed` continue.
    RegisterFallbackPeer {
        /// The previously pinned channel.
        channel: u8,
    },
    /// Send the probe frame and wait at most `timeout` for the MAC-layer ACK.
    SendProbe {
        /// Channel being probed (for diagnostics; the radio is already on it).
        channel: u8,
        /// `0` for the first probe on the channel, `n` for the n-th confirmation probe.
        confirmation: u8,
        /// Maximum ACK wait; a timeout is reported as [`SendOutcome::NoAck`].
        timeout: Duration,
    },
    /// Sleep until the monotonic clock reaches this instant, then report `Completed`.
    WaitUntil(Duration),
    /// The scan has finished.  Further polls repeat this result.
    Done(Result<ScanResult, ScanError>),
}

/// Why a scan failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScanError {
    /// The peer did not respond on any probed channel.
    NotFound {
        /// Channels on which a probe was actually transmitted.
        probed: usize,
        /// Channels in [`ScanConfig::channels`].
        configured: usize,
    },
    /// The temporary peer (channel 0) could not be registered; nothing was probed.
    RegisterTempPeer,
    /// The peer was found but could not be re-registered on the discovered channel.
    RegisterFoundPeer {
        /// The channel on which the peer answered.
        channel: u8,
    },
    /// The caller fed an input that does not match the previous action.
    /// This is a driver bug, not a radio condition.
    UnexpectedInput,
    /// The caller aborted the scan with [`ScanInput::Abort`].
    Aborted,
}

impl core::fmt::Display for ScanError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotFound { probed, configured } => write!(
                f,
                "peer not found after probing {probed} of {configured} configured channels"
            ),
            Self::RegisterTempPeer => f.write_str("failed to register temporary peer for scanning"),
            Self::RegisterFoundPeer { channel } => {
                write!(
                    f,
                    "failed to re-register peer on discovered channel {channel}"
                )
            }
            Self::UnexpectedInput => {
                f.write_str("scan driven with an input that does not match the previous action")
            }
            Self::Aborted => f.write_str("scan aborted by the driver"),
        }
    }
}

impl core::error::Error for ScanError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Idle,
    /// `RemovePeer` (stale registration) issued.
    RemovingStale,
    /// `RegisterPeer { channel: 0 }` issued.
    RegisteringTemp,
    /// `BoostTxPower` issued.
    Boosting,
    /// `SetChannel(channels[idx])` issued.
    SettingChannel {
        idx: usize,
    },
    /// First `SendProbe` on `channels[idx]` issued.
    Probing {
        idx: usize,
    },
    /// Confirmation gap `WaitUntil` issued; `done` confirmations have ACKed so far.
    Gap {
        idx: usize,
        done: u8,
    },
    /// Confirmation `SendProbe` issued; `done` confirmations have ACKed so far.
    Confirming {
        idx: usize,
        done: u8,
    },
    /// `RestoreTxPower` issued; `found` is the pending scan result (`Some` = channel).
    RestoringTxPower {
        found: Option<u8>,
    },
    /// `RemovePeer` (scan peer) issued after the burst.
    RemovingScanPeer {
        found: Option<u8>,
    },
    /// Final `RegisterPeer { channel }` issued on the discovered channel.
    RegisteringFound {
        channel: u8,
    },
    /// Fallback `RestoreChannel { channel: pin }` issued.
    RestoringChannel {
        pin: u8,
    },
    /// Fallback `RegisterFallbackPeer { channel: pin }` issued.
    RegisteringFallback,
    Finished(Result<ScanResult, ScanError>),
}

/// Pure ESP-NOW peer-scan state machine.  See the [module docs](self).
#[derive(Debug, Clone)]
pub struct ScanMachine<'a> {
    mac: MacAddress,
    config: ScanConfig<'a>,
    pinned: Option<u8>,
    state: State,
    burst_start: Duration,
    probed: usize,
    /// Why the scan is unwinding early, if it is.
    failure: Option<ScanError>,
    burst_expired: bool,
}

impl<'a> ScanMachine<'a> {
    /// Create a machine scanning for `mac`.
    ///
    /// `last_pinned` is the channel found by the previous successful scan, if any.
    /// After a failed scan the machine restores the radio and the peer registration to it
    /// so the next send can attempt delivery without an extra failure round-trip.
    pub fn new(mac: MacAddress, config: ScanConfig<'a>, last_pinned: Option<u8>) -> Self {
        Self {
            mac,
            config,
            pinned: last_pinned,
            state: State::Idle,
            burst_start: Duration::ZERO,
            probed: 0,
            failure: None,
            burst_expired: false,
        }
    }

    /// MAC address of the peer being scanned for.
    pub fn mac(&self) -> &MacAddress {
        &self.mac
    }

    /// The pinned channel: the previous one until a scan succeeds, the discovered one after.
    ///
    /// Persist this after [`ScanAction::Done`] and pass it to the next [`ScanMachine::new`].
    pub fn pinned_channel(&self) -> Option<u8> {
        self.pinned
    }

    /// Whether the burst budget cut the channel list short.
    ///
    /// Meaningful after [`ScanAction::Done`]; lets the driver log the stop reason.
    pub fn burst_expired(&self) -> bool {
        self.burst_expired
    }

    /// Number of channels on which a probe was actually transmitted so far.
    pub fn probed(&self) -> usize {
        self.probed
    }

    /// The probe payload to transmit on [`ScanAction::SendProbe`].
    pub fn probe_data(&self) -> &'a [u8] {
        self.config.probe_data
    }

    /// Advance the machine.  See the [module docs](self) for the protocol.
    pub fn poll(&mut self, now: Duration, input: ScanInput) -> ScanAction {
        match (self.state, input) {
            (State::Finished(result), _) => ScanAction::Done(result),
            (_, ScanInput::Abort) => self.unwind(ScanError::Aborted),

            (State::Idle, ScanInput::Start) => {
                self.state = State::RemovingStale;
                ScanAction::RemovePeer
            }

            // A stale registration may or may not exist; either way register afresh.
            (State::RemovingStale, ScanInput::Completed | ScanInput::Failed) => {
                self.state = State::RegisteringTemp;
                ScanAction::RegisterPeer { channel: 0 }
            }
            (State::RegisteringTemp, ScanInput::Completed) => {
                self.state = State::Boosting;
                ScanAction::BoostTxPower
            }
            (State::RegisteringTemp, ScanInput::Failed) => {
                self.fall_back(ScanError::RegisterTempPeer)
            }

            (State::Boosting, ScanInput::Completed | ScanInput::Failed) => {
                self.burst_start = now;
                self.next_channel(now, 0)
            }

            (State::SettingChannel { idx }, ScanInput::Completed) => {
                self.state = State::Probing { idx };
                self.send_probe(idx, 0)
            }
            // A channel the radio refuses (e.g. not allowed in the regulatory domain) is
            // skipped and does not count as probed.
            (State::SettingChannel { idx }, ScanInput::Failed) => self.next_channel(now, idx + 1),

            (State::Probing { idx }, ScanInput::Sent(outcome)) => match outcome {
                SendOutcome::Error => self.next_channel(now, idx + 1),
                SendOutcome::NoAck => {
                    self.probed += 1;
                    self.next_channel(now, idx + 1)
                }
                SendOutcome::Acked => {
                    self.probed += 1;
                    self.after_ack(now, idx, 0)
                }
            },

            (State::Gap { idx, done }, ScanInput::Completed) => {
                self.state = State::Confirming { idx, done };
                self.send_probe(idx, done + 1)
            }
            (State::Confirming { idx, done }, ScanInput::Sent(outcome)) => match outcome {
                SendOutcome::Acked => self.after_ack(now, idx, done + 1),
                // Unconfirmed: the peer was only passing through (roaming).
                SendOutcome::NoAck | SendOutcome::Error => self.next_channel(now, idx + 1),
            },

            (State::RestoringTxPower { found }, ScanInput::Completed | ScanInput::Failed) => {
                self.state = State::RemovingScanPeer { found };
                ScanAction::RemovePeer
            }
            (State::RemovingScanPeer { found }, ScanInput::Completed | ScanInput::Failed) => {
                match (found, self.pinned) {
                    (Some(channel), _) => {
                        self.state = State::RegisteringFound { channel };
                        ScanAction::RegisterPeer { channel }
                    }
                    (None, Some(pin)) => {
                        self.state = State::RestoringChannel { pin };
                        ScanAction::RestoreChannel { channel: pin }
                    }
                    (None, None) => self.finish_not_found(),
                }
            }
            (State::RegisteringFound { channel }, ScanInput::Completed) => {
                self.pinned = Some(channel);
                self.finish(Ok(ScanResult { channel }))
            }
            (State::RegisteringFound { channel }, ScanInput::Failed) => {
                self.fall_back(ScanError::RegisterFoundPeer { channel })
            }

            // Restoration is best effort: the scan result is `NotFound` either way.
            (State::RestoringChannel { pin }, ScanInput::Completed | ScanInput::Failed) => {
                self.state = State::RegisteringFallback;
                ScanAction::RegisterFallbackPeer { channel: pin }
            }
            (State::RegisteringFallback, ScanInput::Completed | ScanInput::Failed) => {
                self.finish_not_found()
            }

            _ => self.unwind(ScanError::UnexpectedInput),
        }
    }

    /// Give up with `error`.
    ///
    /// While the scan is running, unwinds through the normal teardown so TX power, the
    /// scan peer and the pinned-channel fallback are handled.
    /// Before `Start` or inside the teardown itself there is nothing to unwind: finish at once.
    fn unwind(&mut self, error: ScanError) -> ScanAction {
        match self.state {
            State::RemovingStale
            | State::RegisteringTemp
            | State::Boosting
            | State::SettingChannel { .. }
            | State::Probing { .. }
            | State::Gap { .. }
            | State::Confirming { .. } => {
                self.failure = Some(error);
                self.state = State::RestoringTxPower { found: None };
                ScanAction::RestoreTxPower
            }
            _ => self.finish(Err(error)),
        }
    }

    fn send_probe(&self, idx: usize, confirmation: u8) -> ScanAction {
        ScanAction::SendProbe {
            channel: self.config.channels[idx],
            confirmation,
            timeout: self.config.probe_timeout,
        }
    }

    /// An ACK arrived on `channels[idx]` with `done` confirmations already collected.
    fn after_ack(&mut self, now: Duration, idx: usize, done: u8) -> ScanAction {
        if done >= self.config.probe_confirmations {
            let channel = self.config.channels[idx];
            self.state = State::RestoringTxPower {
                found: Some(channel),
            };
            ScanAction::RestoreTxPower
        } else {
            self.state = State::Gap { idx, done };
            ScanAction::WaitUntil(now.saturating_add(self.config.confirmation_gap))
        }
    }

    /// Move to `channels[idx]`, or end the burst when the list or the burst budget is exhausted.
    fn next_channel(&mut self, now: Duration, idx: usize) -> ScanAction {
        let burst_over = now.saturating_sub(self.burst_start) >= self.config.burst_timeout;
        match self.config.channels.get(idx) {
            Some(&channel) if !burst_over => {
                self.state = State::SettingChannel { idx };
                ScanAction::SetChannel { channel }
            }
            other => {
                self.burst_expired = burst_over && other.is_some();
                self.state = State::RestoringTxPower { found: None };
                ScanAction::RestoreTxPower
            }
        }
    }

    /// Fail with `error` after a registration failure, restoring the pinned channel first.
    ///
    /// The scan removed the caller's peer registration up front, so without the fallback every
    /// send would fail while the pin still describes the old state.
    /// TX power and the scan peer are already handled by the time this runs.
    fn fall_back(&mut self, error: ScanError) -> ScanAction {
        self.failure = Some(error);
        match self.pinned {
            Some(pin) => {
                self.state = State::RestoringChannel { pin };
                ScanAction::RestoreChannel { channel: pin }
            }
            None => self.finish_not_found(),
        }
    }

    fn finish_not_found(&mut self) -> ScanAction {
        let error = self.failure.unwrap_or(ScanError::NotFound {
            probed: self.probed,
            configured: self.config.channels.len(),
        });
        self.finish(Err(error))
    }

    fn finish(&mut self, result: Result<ScanResult, ScanError>) -> ScanAction {
        self.state = State::Finished(result);
        ScanAction::Done(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::time::Duration as D;

    extern crate alloc;
    use alloc::vec::Vec;

    const MAC: MacAddress = [0x24, 0x6F, 0x28, 0x00, 0x00, 0x01];

    /// How the simulated peer answers probes.
    struct Peer {
        /// The channel the peer sits on; probes on other channels are not ACKed.
        home: Option<u8>,
        /// Channels the radio refuses to switch to.
        refuse: Vec<u8>,
        /// Number of ACKs the peer gives before going silent (roam simulation).
        acks_before_silence: Option<usize>,
        /// After the first ACK the peer moves to this channel (roam simulation).
        roams_to: Option<u8>,
        /// Fail registration of the n-th RegisterPeer call (0-based).
        fail_register: Option<usize>,
        /// Enqueue fails on the n-th probe (0-based).
        fail_send: Option<usize>,
    }

    impl Peer {
        fn on(channel: u8) -> Self {
            Self {
                home: Some(channel),
                refuse: Vec::new(),
                acks_before_silence: None,
                roams_to: None,
                fail_register: None,
                fail_send: None,
            }
        }
        fn absent() -> Self {
            Self {
                home: None,
                ..Self::on(0)
            }
        }
    }

    /// Everything the harness observed, in order.
    #[derive(Debug, Clone, PartialEq, Eq)]
    enum Seen {
        Remove,
        Register(u8),
        Boost,
        Restore,
        SetChannel(u8),
        RestoreChannel(u8),
        RegisterFallback(u8),
        Probe(D),
        Wait(D),
    }

    struct Run {
        actions: Vec<(D, Seen)>,
        /// `(channel, confirmation)` of every `SendProbe`.
        probe_log: Vec<(u8, u8)>,
        result: Result<ScanResult, ScanError>,
        pinned: Option<u8>,
        end: D,
    }

    impl Run {
        fn seen(&self) -> Vec<Seen> {
            self.actions.iter().map(|(_, s)| s.clone()).collect()
        }
        fn channels_set(&self) -> Vec<u8> {
            self.actions
                .iter()
                .filter_map(|(_, s)| match s {
                    Seen::SetChannel(c) => Some(*c),
                    _ => None,
                })
                .collect()
        }
        fn probes(&self) -> usize {
            self.actions
                .iter()
                .filter(|(_, s)| matches!(s, Seen::Probe(_)))
                .count()
        }
    }

    /// Drive a machine against a simulated peer on a virtual clock.
    /// Each radio operation costs `op_cost`; a NoAck costs the full probe timeout,
    /// an ACK arrives after 5 ms.
    fn run(mut m: ScanMachine<'_>, peer: &Peer, op_cost: D) -> Run {
        let probe_timeout = m.config.probe_timeout;
        let mut home = peer.home;
        let mut now = D::from_secs(1000); // arbitrary epoch
        let mut radio_channel = 0u8;
        let mut registers = 0usize;
        let mut probes = 0usize;
        let mut acks = 0usize;
        let mut actions = Vec::new();
        let mut probe_log = Vec::new();
        let mut input = ScanInput::Start;
        for _ in 0..10_000 {
            let action = m.poll(now, input);
            match action {
                ScanAction::Done(result) => {
                    return Run {
                        actions,
                        probe_log,
                        result,
                        pinned: m.pinned_channel(),
                        end: now,
                    }
                }
                ScanAction::RemovePeer => {
                    actions.push((now, Seen::Remove));
                    now += op_cost;
                    // Removing a non-existent peer fails in ESP-IDF; both are accepted.
                    input = ScanInput::Failed;
                }
                ScanAction::RegisterPeer { channel } => {
                    actions.push((now, Seen::Register(channel)));
                    now += op_cost;
                    input = if peer.fail_register == Some(registers) {
                        ScanInput::Failed
                    } else {
                        ScanInput::Completed
                    };
                    registers += 1;
                }
                ScanAction::BoostTxPower => {
                    actions.push((now, Seen::Boost));
                    input = ScanInput::Completed;
                }
                ScanAction::RestoreTxPower => {
                    actions.push((now, Seen::Restore));
                    input = ScanInput::Completed;
                }
                ScanAction::RegisterFallbackPeer { channel } => {
                    actions.push((now, Seen::RegisterFallback(channel)));
                    now += op_cost;
                    input = if peer.fail_register == Some(registers) {
                        ScanInput::Failed
                    } else {
                        ScanInput::Completed
                    };
                    registers += 1;
                }
                ScanAction::RestoreChannel { channel: c } => {
                    actions.push((now, Seen::RestoreChannel(c)));
                    now += op_cost;
                    if peer.refuse.contains(&c) {
                        input = ScanInput::Failed;
                    } else {
                        radio_channel = c;
                        input = ScanInput::Completed;
                    }
                }
                ScanAction::SetChannel { channel: c } => {
                    actions.push((now, Seen::SetChannel(c)));
                    now += op_cost;
                    if peer.refuse.contains(&c) {
                        input = ScanInput::Failed;
                    } else {
                        radio_channel = c;
                        input = ScanInput::Completed;
                    }
                }
                ScanAction::SendProbe {
                    channel,
                    confirmation,
                    timeout,
                } => {
                    assert_eq!(timeout, probe_timeout);
                    assert_eq!(channel, radio_channel, "probe carries the channel");
                    probe_log.push((channel, confirmation));
                    actions.push((now, Seen::Probe(timeout)));
                    let n = probes;
                    probes += 1;
                    if peer.fail_send == Some(n) {
                        now += op_cost;
                        input = ScanInput::Sent(SendOutcome::Error);
                        continue;
                    }
                    let silent = peer.acks_before_silence.is_some_and(|k| acks >= k);
                    if home == Some(radio_channel) && !silent {
                        acks += 1;
                        if peer.roams_to.is_some() {
                            home = peer.roams_to;
                        }
                        now += D::from_millis(5);
                        input = ScanInput::Sent(SendOutcome::Acked);
                    } else {
                        now += timeout;
                        input = ScanInput::Sent(SendOutcome::NoAck);
                    }
                }
                ScanAction::WaitUntil(t) => {
                    actions.push((now, Seen::Wait(t)));
                    assert!(t >= now);
                    now = t;
                    input = ScanInput::Completed;
                }
            }
        }
        panic!("scan did not terminate");
    }

    fn machine<'a>(cfg: ScanConfig<'a>, pin: Option<u8>) -> ScanMachine<'a> {
        ScanMachine::new(MAC, cfg, pin)
    }

    // ── found ─────────────────────────────────────────────────────────────

    #[test]
    fn found_on_channel_1_default_settings() {
        let r = run(
            machine(ScanConfig::new(b"probe"), None),
            &Peer::on(1),
            D::ZERO,
        );
        assert_eq!(r.result, Ok(ScanResult { channel: 1 }));
        assert_eq!(r.pinned, Some(1));
        assert_eq!(
            r.seen(),
            [
                Seen::Remove,
                Seen::Register(0),
                Seen::Boost,
                Seen::SetChannel(1),
                Seen::Probe(super::super::DEFAULT_PROBE_TIMEOUT),
                Seen::Wait(D::from_secs(1000) + D::from_millis(5) + D::from_millis(150)),
                Seen::Probe(super::super::DEFAULT_PROBE_TIMEOUT),
                Seen::Restore,
                Seen::Remove,
                Seen::Register(1),
            ]
        );
    }

    #[test]
    fn found_on_channel_6() {
        let r = run(machine(ScanConfig::new(b"p"), None), &Peer::on(6), D::ZERO);
        assert_eq!(r.result, Ok(ScanResult { channel: 6 }));
        assert_eq!(r.channels_set(), [1, 2, 3, 4, 5, 6]);
        assert_eq!(r.pinned, Some(6));
        // 5 misses + 2 hits.
        assert_eq!(r.probes(), 7);
        assert_eq!(r.seen().last(), Some(&Seen::Register(6)));
    }

    #[test]
    fn found_on_channel_11() {
        let r = run(machine(ScanConfig::new(b"p"), None), &Peer::on(11), D::ZERO);
        assert_eq!(r.result, Ok(ScanResult { channel: 11 }));
        assert_eq!(r.channels_set(), (1..=11).collect::<Vec<u8>>());
    }

    #[test]
    fn found_on_last_default_channel_13() {
        let r = run(machine(ScanConfig::new(b"p"), None), &Peer::on(13), D::ZERO);
        assert_eq!(r.result, Ok(ScanResult { channel: 13 }));
    }

    // ── not found ─────────────────────────────────────────────────────────

    #[test]
    fn not_found_without_previous_pin_leaves_peer_removed() {
        let r = run(
            machine(ScanConfig::new(b"p"), None),
            &Peer::absent(),
            D::ZERO,
        );
        assert_eq!(
            r.result,
            Err(ScanError::NotFound {
                probed: 13,
                configured: 13
            })
        );
        assert_eq!(r.pinned, None);
        assert_eq!(r.channels_set(), (1..=13).collect::<Vec<u8>>());
        // Ends with RestoreTxPower, RemovePeer and nothing re-registered.
        let seen = r.seen();
        assert_eq!(seen[seen.len() - 2..], [Seen::Restore, Seen::Remove]);
        assert_eq!(
            seen.iter()
                .filter(|s| matches!(s, Seen::Register(_)))
                .count(),
            1,
            "only the temporary channel-0 registration"
        );
    }

    #[test]
    fn not_found_with_previous_pin_restores_channel_then_fallback_peer() {
        let r = run(
            machine(ScanConfig::new(b"p"), Some(9)),
            &Peer::absent(),
            D::ZERO,
        );
        assert!(matches!(r.result, Err(ScanError::NotFound { .. })));
        assert_eq!(r.pinned, Some(9), "pin survives a failed scan");
        let seen = r.seen();
        assert_eq!(
            seen[seen.len() - 4..],
            [
                Seen::Restore,
                Seen::Remove,
                Seen::RestoreChannel(9),
                Seen::RegisterFallback(9)
            ]
        );
    }

    #[test]
    fn failed_rescan_after_success_restores_the_discovered_channel() {
        let first = run(machine(ScanConfig::new(b"p"), None), &Peer::on(6), D::ZERO);
        let second = run(
            machine(ScanConfig::new(b"p"), first.pinned),
            &Peer::absent(),
            D::ZERO,
        );
        let seen = second.seen();
        assert_eq!(
            seen[seen.len() - 2..],
            [Seen::RestoreChannel(6), Seen::RegisterFallback(6)]
        );
        assert_eq!(second.pinned, Some(6));
    }

    #[test]
    fn restore_failures_do_not_change_the_not_found_result() {
        let peer = Peer {
            // Register calls: 0 = temp, 1 = fallback.
            fail_register: Some(1),
            refuse: alloc::vec![9],
            ..Peer::absent()
        };
        let r = run(machine(ScanConfig::new(b"p"), Some(9)), &peer, D::ZERO);
        assert!(matches!(
            r.result,
            Err(ScanError::NotFound {
                probed: 12,
                configured: 13
            })
        ));
        let seen = r.seen();
        assert_eq!(
            seen[seen.len() - 2..],
            [Seen::RestoreChannel(9), Seen::RegisterFallback(9)],
            "both restore steps are still attempted"
        );
    }

    // ── confirmation ──────────────────────────────────────────────────────

    #[test]
    fn zero_confirmations_first_ack_wins() {
        let cfg = ScanConfig::new(b"p").with_probe_confirmations(0);
        let r = run(machine(cfg, None), &Peer::on(3), D::ZERO);
        assert_eq!(r.result, Ok(ScanResult { channel: 3 }));
        assert_eq!(
            r.probes(),
            3,
            "two misses and one hit, no confirmation probe"
        );
        assert!(!r.seen().iter().any(|s| matches!(s, Seen::Wait(_))));
    }

    #[test]
    fn confirmation_requires_every_extra_probe_and_waits_the_gap_each_time() {
        let gap = D::from_millis(200);
        let cfg = ScanConfig::new(b"p")
            .with_probe_confirmations(3)
            .with_confirmation_gap(gap);
        let r = run(machine(cfg, None), &Peer::on(1), D::ZERO);
        assert_eq!(r.result, Ok(ScanResult { channel: 1 }));
        assert_eq!(r.probes(), 4, "first probe plus three confirmations");
        let waits: Vec<_> = r
            .actions
            .iter()
            .filter_map(|(at, s)| match s {
                Seen::Wait(until) => Some(*until - *at),
                _ => None,
            })
            .collect();
        assert_eq!(waits, [gap, gap, gap]);
        // Gaps separate the probes: each probe starts one gap after the previous ACK.
        let probe_times: Vec<D> = r
            .actions
            .iter()
            .filter(|(_, s)| matches!(s, Seen::Probe(_)))
            .map(|(t, _)| *t)
            .collect();
        for w in probe_times.windows(2) {
            assert_eq!(w[1] - w[0], D::from_millis(5) + gap);
        }
    }

    #[test]
    fn roaming_peer_that_acks_once_is_rejected_and_scan_moves_on() {
        let peer = Peer {
            acks_before_silence: Some(1),
            ..Peer::on(6)
        };
        let r = run(machine(ScanConfig::new(b"p"), None), &peer, D::ZERO);
        assert_eq!(
            r.result,
            Err(ScanError::NotFound {
                probed: 13,
                configured: 13
            }),
            "the confirmation probe is not counted as a probed channel"
        );
        assert_eq!(r.channels_set(), (1..=13).collect::<Vec<u8>>());
        assert_eq!(r.pinned, None);
    }

    #[test]
    fn probe_actions_carry_channel_and_confirmation_index() {
        let cfg = ScanConfig::new(b"p").with_probe_confirmations(2);
        let r = run(machine(cfg, None), &Peer::on(2), D::ZERO);
        assert_eq!(r.probe_log, [(1, 0), (2, 0), (2, 1), (2, 2)]);
    }

    #[test]
    fn peer_unconfirmed_on_one_channel_is_found_on_a_later_one() {
        let peer = Peer {
            roams_to: Some(8),
            ..Peer::on(3)
        };
        let r = run(machine(ScanConfig::new(b"p"), None), &peer, D::ZERO);
        assert_eq!(r.result, Ok(ScanResult { channel: 8 }));
        assert_eq!(r.channels_set(), (1..=8).collect::<Vec<u8>>());
        assert_eq!(r.pinned, Some(8));
        // Channel 3: ACK, then the unconfirmed confirmation; channel 8: both ACK.
        assert!(r.probe_log.contains(&(3, 1)));
        assert_eq!(r.probe_log[r.probe_log.len() - 2..], [(8, 0), (8, 1)]);
        assert_eq!(r.seen().last(), Some(&Seen::Register(8)));
    }

    #[test]
    fn confirmation_send_error_rejects_the_channel() {
        let peer = Peer {
            fail_send: Some(1),
            ..Peer::on(1)
        };
        let r = run(machine(ScanConfig::new(b"p"), None), &peer, D::ZERO);
        assert!(matches!(r.result, Err(ScanError::NotFound { .. })));
    }

    #[test]
    fn confirmation_gap_is_added_to_the_ack_time_not_the_epoch() {
        let cfg = ScanConfig::new(b"p").with_confirmation_gap(D::from_millis(150));
        let mut m = machine(cfg, None);
        let t0 = D::from_secs(42);
        assert_eq!(m.poll(t0, ScanInput::Start), ScanAction::RemovePeer);
        assert_eq!(
            m.poll(t0, ScanInput::Failed),
            ScanAction::RegisterPeer { channel: 0 }
        );
        assert_eq!(m.poll(t0, ScanInput::Completed), ScanAction::BoostTxPower);
        assert_eq!(
            m.poll(t0, ScanInput::Completed),
            ScanAction::SetChannel { channel: 1 }
        );
        assert!(matches!(
            m.poll(t0, ScanInput::Completed),
            ScanAction::SendProbe { .. }
        ));
        let ack_at = t0 + D::from_millis(7);
        assert_eq!(
            m.poll(ack_at, ScanInput::Sent(SendOutcome::Acked)),
            ScanAction::WaitUntil(ack_at + D::from_millis(150))
        );
    }

    #[test]
    fn huge_clock_values_saturate_instead_of_overflowing() {
        let cfg = ScanConfig::new(b"p");
        let mut m = machine(cfg, None);
        let now = D::MAX;
        m.poll(now, ScanInput::Start);
        m.poll(now, ScanInput::Completed);
        m.poll(now, ScanInput::Completed);
        m.poll(now, ScanInput::Completed);
        m.poll(now, ScanInput::Completed);
        assert_eq!(
            m.poll(now, ScanInput::Sent(SendOutcome::Acked)),
            ScanAction::WaitUntil(D::MAX)
        );
    }

    // ── burst timeout ─────────────────────────────────────────────────────

    #[test]
    fn burst_timeout_stops_before_the_next_channel() {
        let cfg = ScanConfig::new(b"p")
            .with_probe_timeout(D::from_millis(100))
            .with_burst_timeout(D::from_millis(250));
        let r = run(machine(cfg, None), &Peer::absent(), D::ZERO);
        // Channels start at t=0, 100, 200; at t=300 the budget is spent.
        assert_eq!(r.channels_set(), [1, 2, 3]);
        assert_eq!(
            r.result,
            Err(ScanError::NotFound {
                probed: 3,
                configured: 13
            })
        );
        assert_eq!(r.end - D::from_secs(1000), D::from_millis(300));
    }

    #[test]
    fn burst_timeout_boundary_is_inclusive() {
        let cfg = ScanConfig::new(b"p")
            .with_probe_timeout(D::from_millis(100))
            .with_burst_timeout(D::from_millis(200));
        let r = run(machine(cfg, None), &Peer::absent(), D::ZERO);
        assert_eq!(r.channels_set(), [1, 2], "elapsed == budget stops the scan");
    }

    #[test]
    fn zero_burst_timeout_probes_nothing() {
        let cfg = ScanConfig::new(b"p").with_burst_timeout(D::ZERO);
        let r = run(machine(cfg, None), &Peer::on(1), D::ZERO);
        assert_eq!(
            r.result,
            Err(ScanError::NotFound {
                probed: 0,
                configured: 13
            })
        );
        assert_eq!(r.probes(), 0);
        assert!(r.channels_set().is_empty());
    }

    #[test]
    fn burst_budget_does_not_interrupt_confirmation_of_a_channel() {
        let cfg = ScanConfig::new(b"p").with_burst_timeout(D::from_millis(1));
        let r = run(machine(cfg, None), &Peer::on(1), D::ZERO);
        assert_eq!(r.result, Ok(ScanResult { channel: 1 }));
    }

    #[test]
    fn burst_clock_starts_after_registration_and_boost() {
        // Slow driver operations before the loop must not eat into the burst budget.
        let cfg = ScanConfig::new(b"p")
            .with_probe_timeout(D::from_millis(100))
            .with_burst_timeout(D::from_millis(150));
        let r = run(machine(cfg, None), &Peer::absent(), D::from_millis(40));
        assert_eq!(r.channels_set(), [1, 2]);
    }

    // ── channel lists ─────────────────────────────────────────────────────

    #[test]
    fn custom_channel_list_is_probed_in_order() {
        let list = [11, 1, 6];
        let cfg = ScanConfig::new(b"p").with_channels(&list);
        let r = run(machine(cfg, None), &Peer::on(6), D::ZERO);
        assert_eq!(r.result, Ok(ScanResult { channel: 6 }));
        assert_eq!(r.channels_set(), [11, 1, 6]);
    }

    #[test]
    fn peer_outside_custom_list_is_not_found() {
        let list = [1, 6, 11];
        let cfg = ScanConfig::new(b"p").with_channels(&list);
        let r = run(machine(cfg, None), &Peer::on(5), D::ZERO);
        assert_eq!(
            r.result,
            Err(ScanError::NotFound {
                probed: 3,
                configured: 3
            })
        );
    }

    #[test]
    fn empty_channel_list_is_not_found_without_probing() {
        let cfg = ScanConfig::new(b"p").with_channels(&[]);
        let r = run(machine(cfg, None), &Peer::on(1), D::ZERO);
        assert_eq!(
            r.result,
            Err(ScanError::NotFound {
                probed: 0,
                configured: 0
            })
        );
        assert_eq!(r.probes(), 0);
    }

    // ── radio refusals ────────────────────────────────────────────────────

    #[test]
    fn refused_channels_are_skipped_and_not_counted_as_probed() {
        let peer = Peer {
            refuse: alloc::vec![1, 2],
            ..Peer::on(3)
        };
        let r = run(machine(ScanConfig::new(b"p"), None), &peer, D::ZERO);
        assert_eq!(r.result, Ok(ScanResult { channel: 3 }));
        assert_eq!(r.channels_set(), [1, 2, 3]);
        assert_eq!(r.probes(), 2, "no probe on the refused channels");

        let peer = Peer {
            refuse: alloc::vec![12, 13],
            ..Peer::absent()
        };
        let r = run(machine(ScanConfig::new(b"p"), None), &peer, D::ZERO);
        assert_eq!(
            r.result,
            Err(ScanError::NotFound {
                probed: 11,
                configured: 13
            })
        );
    }

    #[test]
    fn first_probe_send_error_skips_the_channel_without_counting_it() {
        let list = [1, 6];
        let peer = Peer {
            fail_send: Some(0),
            ..Peer::absent()
        };
        let cfg = ScanConfig::new(b"p").with_channels(&list);
        let r = run(machine(cfg, None), &peer, D::ZERO);
        assert_eq!(
            r.result,
            Err(ScanError::NotFound {
                probed: 1,
                configured: 2
            })
        );
        assert_eq!(r.channels_set(), [1, 6]);
    }

    // ── registration failures ─────────────────────────────────────────────

    #[test]
    fn temp_peer_registration_failure_restores_the_pin_before_failing() {
        let peer = Peer {
            fail_register: Some(0),
            ..Peer::on(1)
        };
        let r = run(machine(ScanConfig::new(b"p"), Some(4)), &peer, D::ZERO);
        assert_eq!(r.result, Err(ScanError::RegisterTempPeer));
        assert_eq!(
            r.seen(),
            [
                Seen::Remove,
                Seen::Register(0),
                Seen::RestoreChannel(4),
                Seen::RegisterFallback(4),
            ]
        );
        assert_eq!(r.pinned, Some(4));
    }

    #[test]
    fn temp_peer_registration_failure_without_pin_finishes_at_once() {
        let peer = Peer {
            fail_register: Some(0),
            ..Peer::on(1)
        };
        let r = run(machine(ScanConfig::new(b"p"), None), &peer, D::ZERO);
        assert_eq!(r.result, Err(ScanError::RegisterTempPeer));
        assert_eq!(r.seen(), [Seen::Remove, Seen::Register(0)]);
        assert_eq!(r.pinned, None);
    }

    #[test]
    fn temp_peer_failure_keeps_error_when_the_fallback_fails() {
        let peer = Peer {
            fail_register: Some(0),
            refuse: alloc::vec![4],
            ..Peer::on(1)
        };
        let r = run(machine(ScanConfig::new(b"p"), Some(4)), &peer, D::ZERO);
        assert_eq!(r.result, Err(ScanError::RegisterTempPeer));
        assert_eq!(
            r.seen(),
            [
                Seen::Remove,
                Seen::Register(0),
                Seen::RestoreChannel(4),
                Seen::RegisterFallback(4),
            ]
        );
    }

    #[test]
    fn found_peer_registration_failure_is_reported_and_keeps_the_old_pin() {
        let peer = Peer {
            fail_register: Some(1),
            ..Peer::on(6)
        };
        let r = run(machine(ScanConfig::new(b"p"), Some(2)), &peer, D::ZERO);
        assert_eq!(r.result, Err(ScanError::RegisterFoundPeer { channel: 6 }));
        let seen = r.seen();
        assert_eq!(
            seen[seen.len() - 5..],
            [
                Seen::Restore,
                Seen::Remove,
                Seen::Register(6),
                Seen::RestoreChannel(2),
                Seen::RegisterFallback(2),
            ]
        );
        assert_eq!(r.pinned, Some(2));
    }

    #[test]
    fn stale_peer_removal_result_is_irrelevant() {
        for input in [ScanInput::Completed, ScanInput::Failed] {
            let mut m = machine(ScanConfig::new(b"p"), None);
            assert_eq!(m.poll(D::ZERO, ScanInput::Start), ScanAction::RemovePeer);
            assert_eq!(
                m.poll(D::ZERO, input),
                ScanAction::RegisterPeer { channel: 0 }
            );
        }
    }

    // ── TX power bracket ──────────────────────────────────────────────────

    #[test]
    fn tx_power_is_boosted_for_the_burst_and_restored_on_every_outcome() {
        for peer in [Peer::on(2), Peer::absent()] {
            let r = run(machine(ScanConfig::new(b"p"), None), &peer, D::ZERO);
            let seen = r.seen();
            let boost = seen.iter().position(|s| *s == Seen::Boost).unwrap();
            let restore = seen.iter().position(|s| *s == Seen::Restore).unwrap();
            let first_set = seen
                .iter()
                .position(|s| matches!(s, Seen::SetChannel(_)))
                .unwrap();
            let last_probe = seen
                .iter()
                .rposition(|s| matches!(s, Seen::Probe(_)))
                .unwrap();
            assert!(boost < first_set);
            assert!(restore > last_probe);
            assert_eq!(seen[restore + 1], Seen::Remove, "restore precedes cleanup");
            assert_eq!(seen.iter().filter(|s| **s == Seen::Boost).count(), 1);
            assert_eq!(seen.iter().filter(|s| **s == Seen::Restore).count(), 1);
        }
    }

    #[test]
    fn tx_power_failures_are_ignored() {
        let mut m = machine(ScanConfig::new(b"p").with_channels(&[1]), None);
        m.poll(D::ZERO, ScanInput::Start);
        m.poll(D::ZERO, ScanInput::Completed);
        assert_eq!(
            m.poll(D::ZERO, ScanInput::Completed),
            ScanAction::BoostTxPower
        );
        assert_eq!(
            m.poll(D::ZERO, ScanInput::Failed),
            ScanAction::SetChannel { channel: 1 }
        );
        m.poll(D::ZERO, ScanInput::Completed);
        assert_eq!(
            m.poll(D::ZERO, ScanInput::Sent(SendOutcome::NoAck)),
            ScanAction::RestoreTxPower
        );
        assert_eq!(m.poll(D::ZERO, ScanInput::Failed), ScanAction::RemovePeer);
    }

    // ── burst expiry flag ─────────────────────────────────────────────────

    #[test]
    fn burst_expired_is_set_only_when_channels_were_left_unprobed() {
        let cfg = ScanConfig::new(b"p")
            .with_probe_timeout(D::from_millis(100))
            .with_burst_timeout(D::from_millis(250));
        let mut m = machine(cfg, None);
        let r = run(m.clone(), &Peer::absent(), D::ZERO);
        assert_eq!(r.channels_set(), [1, 2, 3]);
        drive_to_done(&mut m, D::from_millis(100));
        assert!(m.burst_expired());
        assert_eq!(m.probed(), 3);

        let mut m = machine(ScanConfig::new(b"p").with_channels(&[1]), None);
        drive_to_done(&mut m, D::ZERO);
        assert!(!m.burst_expired(), "list exhausted, not the budget");
    }

    /// Feed `NoAck` / `Completed` until `Done`, advancing the clock by `step` per probe.
    fn drive_to_done(m: &mut ScanMachine<'_>, step: D) {
        let mut now = D::ZERO;
        let mut input = ScanInput::Start;
        loop {
            match m.poll(now, input) {
                ScanAction::Done(_) => return,
                ScanAction::SendProbe { .. } => {
                    now += step;
                    input = ScanInput::Sent(SendOutcome::NoAck);
                }
                _ => input = ScanInput::Completed,
            }
        }
    }

    // ── abort and protocol misuse ─────────────────────────────────────────

    #[test]
    fn abort_during_boost_unwinds_through_teardown_and_fallback() {
        let mut m = machine(ScanConfig::new(b"p"), Some(9));
        assert_eq!(m.poll(D::ZERO, ScanInput::Start), ScanAction::RemovePeer);
        m.poll(D::ZERO, ScanInput::Completed);
        assert_eq!(
            m.poll(D::ZERO, ScanInput::Completed),
            ScanAction::BoostTxPower
        );
        assert_eq!(
            m.poll(D::ZERO, ScanInput::Abort),
            ScanAction::RestoreTxPower
        );
        assert_eq!(
            m.poll(D::ZERO, ScanInput::Completed),
            ScanAction::RemovePeer
        );
        assert_eq!(
            m.poll(D::ZERO, ScanInput::Completed),
            ScanAction::RestoreChannel { channel: 9 }
        );
        assert_eq!(
            m.poll(D::ZERO, ScanInput::Completed),
            ScanAction::RegisterFallbackPeer { channel: 9 }
        );
        assert_eq!(
            m.poll(D::ZERO, ScanInput::Completed),
            ScanAction::Done(Err(ScanError::Aborted))
        );
        assert_eq!(m.pinned_channel(), Some(9));
    }

    #[test]
    fn abort_without_pin_ends_after_peer_removal() {
        let mut m = machine(ScanConfig::new(b"p"), None);
        m.poll(D::ZERO, ScanInput::Start);
        m.poll(D::ZERO, ScanInput::Completed);
        m.poll(D::ZERO, ScanInput::Completed);
        m.poll(D::ZERO, ScanInput::Completed);
        m.poll(D::ZERO, ScanInput::Completed);
        assert_eq!(
            m.poll(D::ZERO, ScanInput::Abort),
            ScanAction::RestoreTxPower
        );
        m.poll(D::ZERO, ScanInput::Completed);
        assert_eq!(
            m.poll(D::ZERO, ScanInput::Failed),
            ScanAction::Done(Err(ScanError::Aborted))
        );
    }

    #[test]
    fn abort_before_start_finishes_immediately() {
        let mut m = machine(ScanConfig::new(b"p"), None);
        assert_eq!(
            m.poll(D::ZERO, ScanInput::Abort),
            ScanAction::Done(Err(ScanError::Aborted))
        );
    }

    #[test]
    fn unexpected_input_mid_scan_unwinds_through_teardown() {
        let mut m = machine(ScanConfig::new(b"p"), Some(4));
        m.poll(D::ZERO, ScanInput::Start);
        // RemovePeer was issued; a send outcome makes no sense.
        assert_eq!(
            m.poll(D::ZERO, ScanInput::Sent(SendOutcome::Acked)),
            ScanAction::RestoreTxPower
        );
        assert_eq!(
            m.poll(D::ZERO, ScanInput::Completed),
            ScanAction::RemovePeer
        );
        assert_eq!(
            m.poll(D::ZERO, ScanInput::Completed),
            ScanAction::RestoreChannel { channel: 4 }
        );
        m.poll(D::ZERO, ScanInput::Completed);
        assert_eq!(
            m.poll(D::ZERO, ScanInput::Completed),
            ScanAction::Done(Err(ScanError::UnexpectedInput))
        );
    }

    #[test]
    fn unexpected_input_inside_teardown_finishes_immediately() {
        let mut m = machine(ScanConfig::new(b"p").with_channels(&[]), None);
        m.poll(D::ZERO, ScanInput::Start);
        m.poll(D::ZERO, ScanInput::Completed);
        m.poll(D::ZERO, ScanInput::Completed);
        assert_eq!(
            m.poll(D::ZERO, ScanInput::Completed),
            ScanAction::RestoreTxPower
        );
        assert_eq!(
            m.poll(D::ZERO, ScanInput::Sent(SendOutcome::Acked)),
            ScanAction::Done(Err(ScanError::UnexpectedInput))
        );
    }

    // ── protocol misuse ───────────────────────────────────────────────────

    #[test]
    fn input_before_start_is_rejected() {
        let mut m = machine(ScanConfig::new(b"p"), None);
        assert_eq!(
            m.poll(D::ZERO, ScanInput::Completed),
            ScanAction::Done(Err(ScanError::UnexpectedInput))
        );
    }

    #[test]
    fn done_is_sticky() {
        let list = [1];
        let cfg = ScanConfig::new(b"p").with_channels(&list);
        let mut m = machine(cfg, None);
        let r = run(m.clone(), &Peer::absent(), D::ZERO);
        let done = ScanAction::Done(r.result);
        // Drive manually to Done, then keep polling.
        let mut input = ScanInput::Start;
        let mut last = m.poll(D::ZERO, input);
        while !matches!(last, ScanAction::Done(_)) {
            input = match last {
                ScanAction::SendProbe { .. } => ScanInput::Sent(SendOutcome::NoAck),
                _ => ScanInput::Completed,
            };
            last = m.poll(D::ZERO, input);
        }
        assert_eq!(last, done);
        assert_eq!(m.poll(D::ZERO, ScanInput::Completed), done);
        assert_eq!(m.poll(D::ZERO, ScanInput::Start), done);
    }

    // ── accessors and Display ─────────────────────────────────────────────

    #[test]
    fn accessors_expose_peer_probe_and_pin() {
        let m = machine(ScanConfig::new(b"probe-frame"), Some(7));
        assert_eq!(m.mac(), &MAC);
        assert_eq!(m.probe_data(), b"probe-frame");
        assert_eq!(m.pinned_channel(), Some(7));
    }

    #[test]
    fn scan_error_display_is_informative() {
        use alloc::string::ToString;
        assert_eq!(
            ScanError::NotFound {
                probed: 3,
                configured: 13
            }
            .to_string(),
            "peer not found after probing 3 of 13 configured channels"
        );
        assert!(ScanError::RegisterTempPeer
            .to_string()
            .contains("temporary"));
        assert!(ScanError::RegisterFoundPeer { channel: 6 }
            .to_string()
            .contains("channel 6"));
        assert!(!ScanError::UnexpectedInput.to_string().is_empty());
        assert!(!ScanError::Aborted.to_string().is_empty());
    }
}
