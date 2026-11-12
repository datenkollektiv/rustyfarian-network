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
//!    requested wait has elapsed), [`ScanInput::Failed`] (the operation failed), after
//!    [`ScanAction::SendProbe`], [`ScanInput::Sent`], or, after [`ScanAction::AwaitReply`],
//!    [`ScanInput::Reply`] for a probe reply the peer sent.
//!    To give up while the scan is running, feed [`ScanInput::Abort`] instead; when the driver
//!    is being deinitialised, feed [`ScanInput::Shutdown`].
//! 4. Repeat until [`ScanAction::Done`].
//!    Any poll after `Done` answers [`ScanAction::Ignored`] and changes nothing, so a driver
//!    loop can tell an extra poll from a genuine `Done` and treat it as an error exit
//!    (ADR 023 decision 5).
//!
//! `now` is any monotonic duration since an arbitrary epoch; only differences matter.
//! A scan driven with valid inputs reaches `Done` within [`ScanMachine::max_polls`] polls, so
//! the driver loop can bound itself: feed `Abort` once at that count, and treat
//! [`ScanTeardown::MAX_POLLS`] more polls without `Done` as a machine bug.
//!
//! ```text
//! RemovePeer -> RegisterPeer{0} -> BoostTxPower
//!   per channel: SetChannel -> SendProbe -> [AwaitReply]              (ACKed: wait for the reply)
//!     reply names the home channel: [SetChannel{home}]               -> found
//!     no reply: the first such channel becomes the candidate, next channel
//!   candidate (list or burst over, nothing found):
//!     SetChannel -> SendProbe -> AwaitReply
//!       -> [WaitUntil -> SendProbe -> AwaitReply]*                    (confirmations)
//!   RestoreTxPower -> RemovePeer
//!     found:     RegisterPeer{channel}                                -> Done(Ok)
//!     not found: [RestoreChannel{pin} -> RegisterFallbackPeer{pin}]   -> Done(Err)
//! ```
//!
//! A probe is the three-byte [`ProbeFrame`] (`0xF3`, ADR 025) with a per-scan sequence number;
//! [`ScanMachine::probe_frame`] builds it for the channel named by `SendProbe`.
//! A peer running the echo app answers with a [`ProbeReply`] that states its home channel, and
//! that reply wins: the scan ends on the stated channel without another probe (an
//! adjacent channel decodes and ACKs the probe, so the ACK alone cannot name the channel).
//! An ACK without a reply within [`ScanConfig::reply_timeout`] keeps the old meaning as a
//! fallback for peers that do not reply: the first such channel is the candidate, the sweep
//! goes on (a later reply still wins), and once the channel list or the burst budget is spent
//! the scan returns to the candidate and settles it with the confirmation rule of
//! [`ScanConfig::probe_confirmations`] and [`ScanConfig::confirmation_gap`].
//!
//! A failed registration (temporary peer after `RemovePeer`, discovered peer after the
//! teardown) takes the same best-effort `[RestoreChannel -> RegisterFallbackPeer]` path
//! and then finishes with the registration error.
//!
//! The steps from `RestoreTxPower` on are the teardown, a [`ScanTeardown`] the machine
//! delegates to.
//! [`ScanMachine::abort_teardown`] hands out the teardown the machine would run on `Abort`, a
//! small `Copy` value that a driver can keep and run on its own when it cannot feed the machine
//! any more (a dropped future).
//!
//! # Contract for the I/O side
//!
//! - [`ScanAction::SendProbe`] carries the probe timeout, which bounds the queue wait only:
//!   a probe that did not start within it is never sent and is reported as
//!   [`SendOutcome::Local`].
//!   A probe that has started is waited out however long its send callback takes (the channel
//!   must not change under a frame on the air, ADR 020 fail-closed) and reported as
//!   [`SendOutcome::Acked`] or [`SendOutcome::NoAck`] by its real MAC-layer result.
//!   Both tiers follow this rule.
//!   The payload is [`ScanMachine::probe_frame`]`(channel)` for the channel the action names.
//! - [`ScanAction::AwaitReply`] asks the driver to wait for a `0xF4` [`ProbeReply`] from the
//!   peer until the monotonic clock reaches `until`.
//!   Feed [`ScanInput::Reply`] for each reply received from the peer's MAC in the meantime:
//!   one that matches the outstanding probe (`seq` and `tx_channel`) ends the scan, one that does
//!   not answers `AwaitReply` again with the same `until` (and does not count toward the poll
//!   budget), so the wait goes on.
//!   Feed [`ScanInput::Completed`] once the deadline has passed.
//!   A reply that arrives while no `AwaitReply` is pending stands for nothing the machine can
//!   use: drop it in the driver, as feeding it is an out-of-phase input.
//! - At `Done(Ok)` the radio sits on the found channel, as before: when a reply names a channel
//!   other than the probed one, the machine issues `SetChannel` for it before the teardown, and a
//!   refused switch ends the scan not found with [`ScanFault::SetChannel`].
//! - [`ScanAction::RemovePeer`] is best effort: both `Completed` and `Failed` continue.
//! - [`ScanAction::BoostTxPower`] and [`ScanAction::RestoreTxPower`] are advisory; a driver
//!   that cannot change TX power reports `Completed` and the scan proceeds.
//!   Every scan that emitted `BoostTxPower` emits `RestoreTxPower` exactly once before `Done`
//!   (except after [`ScanInput::Shutdown`]), so the driver needs no restore of its own.
//!   The IDF tier boosts to `TxPowerLevel::Max` for the burst and restores the previous level.
//! - The peer must be registered on the driver's default [`WifiInterface`](super::WifiInterface),
//!   encryption off, exactly as the IDF tier does.
//!
//! - [`ScanInput::Abort`], or an input that does not match the previous action, ends the scan
//!   with [`ScanError::Aborted`] / [`ScanError::UnexpectedInput`].
//!   This is the machine's answer to ADR 023 decision 5 while the scan runs: an out-of-phase
//!   input fails closed (the radio is restored and the scan ends with `UnexpectedInput`) rather
//!   than being skipped, because the radio state it stands for is unknown.
//!   Only an input after `Done` is a no-op, answered with [`ScanAction::Ignored`].
//!   While the scan is running the machine first unwinds through the normal
//!   `RestoreTxPower -> RemovePeer -> fallback` sequence, so the caller needs no extra cleanup.
//!   Before [`ScanInput::Start`] it finishes at once.
//!   Once the teardown has begun, `Abort` stands for the input the teardown step expects
//!   (`Completed`), so feeding it never skips a restore step: the teardown continues and the
//!   scan ends with the result it was heading for (a found peer keeps its new pin).
//!   A driver that cancels may therefore feed `Abort` at every step, in place of the result of
//!   the last action; [`ScanInput::or_abort`] keeps a `Failed` result, which the machine must
//!   see.
//! - [`ScanInput::Shutdown`] ends the scan at once with [`ScanError::Aborted`] from any state,
//!   with no teardown: the driver is going away and the radio is deinitialised next.
//!
//! The machine never loops on its own: every invocation of [`ScanMachine::poll`] returns
//! within a bounded number of steps.

use core::time::Duration;

use super::command::PROBE_FRAME_LEN;
use super::{MacAddress, ProbeFrame, ProbeReply, ScanConfig, ScanResult, DEFAULT_SCAN_CHANNELS};
use crate::wifi::ChannelSet;

/// Outcome of one probe transmission, reported after [`ScanAction::SendProbe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendOutcome {
    /// The peer acknowledged the frame at the MAC layer.
    Acked,
    /// The frame was transmitted, but not acknowledged (or the ACK wait timed out after
    /// transmission).
    NoAck,
    /// The frame was not transmitted: queue full, no sender runner, invalid peer, driver
    /// error, or a timeout before transmission.
    Local,
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
    /// Fed during the teardown, it continues the teardown as if the step had completed.
    Abort,
    /// Result of the previous [`ScanAction::SendProbe`].
    Sent(SendOutcome),
    /// A probe reply from the peer, received during [`ScanAction::AwaitReply`].
    ///
    /// One that matches the outstanding probe ends the scan on the reply's home channel; any
    /// other is ignored and the wait continues.
    Reply(ProbeReply),
    /// The driver is being deinitialised: finish at once with [`ScanError::Aborted`] and emit
    /// no teardown, since the radio is going away and a probe may still be on the air.
    Shutdown,
}

impl ScanInput {
    /// The input to feed when the driver may have been asked to cancel: [`ScanInput::Abort`]
    /// in place of `self` if `cancel`, except that a `Failed` result and a `Shutdown` are kept.
    ///
    /// A failed registration of the found peer or of the temporary peer changes what the
    /// teardown must do, while every other result is superseded by the abort.
    /// The cancel is still seen at the next step.
    /// `Shutdown` is stronger than a cancel: it skips the teardown.
    #[must_use]
    pub fn or_abort(self, cancel: bool) -> Self {
        if cancel && !matches!(self, Self::Failed | Self::Shutdown) {
            Self::Abort
        } else {
            self
        }
    }
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
    /// Send the probe frame; `timeout` bounds its queue wait (see the
    /// [module docs](self#contract-for-the-io-side)).
    SendProbe {
        /// Channel being probed (for diagnostics; the radio is already on it).
        channel: u8,
        /// `0` for the first probe on the channel, `n` for the n-th confirmation probe.
        confirmation: u8,
        /// Maximum wait for the probe to start; a probe that did not start in time is
        /// [`SendOutcome::Local`].
        timeout: Duration,
    },
    /// Sleep until the monotonic clock reaches this instant, then report `Completed`.
    WaitUntil(Duration),
    /// Wait for a probe reply from the peer (see the
    /// [module docs](self#contract-for-the-io-side)): report each reply with
    /// [`ScanInput::Reply`], and `Completed` once the clock reaches `until`.
    AwaitReply {
        /// Deadline of the wait; [`ScanConfig::reply_timeout`] after the ACK.
        until: Duration,
    },
    /// The scan has finished.  Further polls answer [`ScanAction::Ignored`].
    Done(Result<ScanResult, ScanError>),
    /// The input arrived after [`ScanAction::Done`] and changed nothing (ADR 023 decision 5).
    /// A driver loop never sees it when it stops at `Done`; treat it as an error exit.
    Ignored,
}

/// The first I/O failure of a scan that the machine skipped over.
///
/// A skipped channel does not count as probed, so a scan that fails on every channel reports
/// `probed: 0` with no other clue.
/// The machine records only the first such failure; the driver adds the error detail it knows.
/// A channel the country does not allow is skipped without radio work and is no fault (see
/// [`ScanMachine::with_allowed_channels`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScanFault {
    /// The radio refused to switch to `channel` ([`ScanInput::Failed`] after
    /// [`ScanAction::SetChannel`]).
    SetChannel {
        /// The channel that could not be set.
        channel: u8,
    },
    /// Enqueueing a probe on `channel` failed ([`SendOutcome::Local`]), on a first probe or a
    /// confirmation probe.
    ProbeSend {
        /// The channel the probe was meant for.
        channel: u8,
    },
}

impl core::fmt::Display for ScanFault {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::SetChannel { channel } => write!(f, "set channel {channel}"),
            Self::ProbeSend { channel } => write!(f, "probe send on channel {channel}"),
        }
    }
}

/// Why a scan failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ScanError {
    /// The peer did not respond on any probed channel.
    ///
    /// `probed <= configured - not_allowed`.
    NotFound {
        /// Channels on which a probe was actually transmitted.
        probed: usize,
        /// Channels in [`ScanConfig::channels`].
        configured: usize,
        /// Entries of [`ScanConfig::channels`] the Wi-Fi country does not allow; they were
        /// skipped without radio work (see [`ScanMachine::with_allowed_channels`]).
        not_allowed: usize,
        /// The first channel switch or probe send that failed, if any.
        first_fault: Option<ScanFault>,
    },
    /// The peer replied with a home channel the Wi-Fi country does not allow, so the radio
    /// cannot follow it; the scan ended not found and the previous pin was restored.
    ///
    /// Set the country of the peer's network on this device.
    HomeNotAllowed {
        /// The home channel the peer named.
        home: u8,
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
    /// The caller aborted the scan with [`ScanInput::Abort`] or [`ScanInput::Shutdown`].
    Aborted,
}

impl core::fmt::Display for ScanError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotFound {
                probed,
                configured,
                not_allowed,
                first_fault,
            } => {
                write!(
                    f,
                    "peer not found after probing {probed} of {configured} configured channels"
                )?;
                if *not_allowed > 0 {
                    write!(f, " ({not_allowed} not allowed by the country)")?;
                }
                match first_fault {
                    Some(fault) => write!(f, " (first failure: {fault})"),
                    None => Ok(()),
                }
            }
            Self::HomeNotAllowed { home } => write!(
                f,
                "peer replied with home channel {home}, which the country does not allow"
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

/// Where a [`ScanTeardown`] stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Nothing issued yet; the next poll issues `RestoreTxPower`.
    /// `found` is the pending scan result (`Some` = channel).
    Begin {
        found: Option<u8>,
    },
    /// `RestoreTxPower` issued.
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
    /// `RestoreTxPower` issued because the scan was about to finish with the TX power still
    /// boosted; the result follows.
    RestoringBeforeFinish(Result<ScanResult, ScanError>),
    /// Nothing to restore (the scan never started); the next poll delivers the result.
    Finishing(Result<ScanResult, ScanError>),
    Finished(Result<ScanResult, ScanError>),
}

/// The teardown of a scan: TX power, scan peer and pinned-channel fallback, in that order.
///
/// [`ScanMachine`] delegates to it once the burst is over or the scan unwinds, and
/// [`ScanMachine::abort_teardown`] hands out the one it would run on [`ScanInput::Abort`].
/// It borrows nothing, so a driver can keep a copy and run it on its own, for example from a
/// `Drop` that can no longer feed the machine.
///
/// Drive it like the machine: [`poll`](Self::poll) with the result of the previous action until
/// [`ScanAction::Done`].
/// A teardown that has issued nothing yet takes its first input as the start signal, like
/// [`ScanInput::Start`] for the machine (except [`ScanInput::Shutdown`], which ends it at once);
/// feed `Abort` there.
/// Any poll after `Done` answers [`ScanAction::Ignored`].
/// `Abort` stands for `Completed` at every step, so it never skips a restore.
/// It reaches `Done` within [`MAX_POLLS`](Self::MAX_POLLS) polls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScanTeardown {
    mac: MacAddress,
    step: Step,
    pinned: Option<u8>,
    /// The error the teardown finishes with unless a peer was found and registered.
    failure: ScanError,
    /// `BoostTxPower` was issued and `RestoreTxPower` was not.
    boosted: bool,
}

impl ScanTeardown {
    /// The most polls a teardown needs from its first poll to `Done`, that one included.
    pub const MAX_POLLS: usize = 7;

    fn new(mac: MacAddress, step: Step, pinned: Option<u8>, failure: ScanError) -> Self {
        Self {
            mac,
            step,
            pinned,
            failure,
            boosted: false,
        }
    }

    /// MAC address of the peer the scan was for.
    pub fn mac(&self) -> &MacAddress {
        &self.mac
    }

    /// The pinned channel: the previous one, or the discovered one once it is registered.
    pub fn pinned_channel(&self) -> Option<u8> {
        self.pinned
    }

    /// Whether the teardown has finished; [`poll`](Self::poll) then answers
    /// [`ScanAction::Ignored`].
    pub fn is_done(&self) -> bool {
        matches!(self.step, Step::Finished(_))
    }

    /// Advance the teardown with the result of the previous action.
    pub fn poll(&mut self, input: ScanInput) -> ScanAction {
        match (self.step, input) {
            // Out of phase (ADR 023 decision 5): the result was already delivered.
            (Step::Finished(_), _) => ScanAction::Ignored,
            (Step::Finishing(result), _) => self.finish_now(result),
            // The driver is going away: no further radio action.
            (_, ScanInput::Shutdown) => self.finish_now(Err(ScanError::Aborted)),
            (Step::Begin { found }, _) => self.restore_tx_power(found),
            // `Abort` is the result the step expects: skipping a step would leave the TX power,
            // the scan peer or the pinned-channel registration unrestored.
            (_, ScanInput::Abort) => self.poll(ScanInput::Completed),

            (Step::RestoringTxPower { found }, ScanInput::Completed | ScanInput::Failed) => {
                self.step = Step::RemovingScanPeer { found };
                ScanAction::RemovePeer
            }
            (Step::RemovingScanPeer { found }, ScanInput::Completed | ScanInput::Failed) => {
                match (found, self.pinned) {
                    (Some(channel), _) => {
                        self.step = Step::RegisteringFound { channel };
                        ScanAction::RegisterPeer { channel }
                    }
                    (None, _) => self.fall_back(),
                }
            }
            (Step::RegisteringFound { channel }, ScanInput::Completed) => {
                self.pinned = Some(channel);
                self.finish(Ok(ScanResult { channel }))
            }
            (Step::RegisteringFound { channel }, ScanInput::Failed) => {
                self.failure = ScanError::RegisterFoundPeer { channel };
                self.fall_back()
            }

            // Restoration is best effort: the scan result is the failure either way.
            (Step::RestoringChannel { pin }, ScanInput::Completed | ScanInput::Failed) => {
                self.step = Step::RegisteringFallback;
                ScanAction::RegisterFallbackPeer { channel: pin }
            }
            (Step::RegisteringFallback, ScanInput::Completed | ScanInput::Failed) => {
                self.finish(Err(self.failure))
            }
            (Step::RestoringBeforeFinish(result), ScanInput::Completed | ScanInput::Failed) => {
                self.finish_now(result)
            }

            // An unexpected input inside the teardown finishes at once, as the teardown cannot be
            // trusted to continue (a boosted TX power is still restored first).
            _ => self.finish(Err(ScanError::UnexpectedInput)),
        }
    }

    fn restore_tx_power(&mut self, found: Option<u8>) -> ScanAction {
        self.boosted = false;
        self.step = Step::RestoringTxPower { found };
        ScanAction::RestoreTxPower
    }

    /// Return the radio and the peer registration to the pinned channel, then finish with the
    /// failure; without a pin, finish at once.
    ///
    /// The scan removed the caller's peer registration up front, so without the fallback every
    /// send would fail while the pin still describes the old state.
    fn fall_back(&mut self) -> ScanAction {
        match self.pinned {
            Some(pin) => {
                self.step = Step::RestoringChannel { pin };
                ScanAction::RestoreChannel { channel: pin }
            }
            None => self.finish(Err(self.failure)),
        }
    }

    /// Finish with `result`, restoring a still-boosted TX power first.
    fn finish(&mut self, result: Result<ScanResult, ScanError>) -> ScanAction {
        if self.boosted {
            self.boosted = false;
            self.step = Step::RestoringBeforeFinish(result);
            ScanAction::RestoreTxPower
        } else {
            self.finish_now(result)
        }
    }

    fn finish_now(&mut self, result: Result<ScanResult, ScanError>) -> ScanAction {
        self.boosted = false;
        self.step = Step::Finished(result);
        ScanAction::Done(result)
    }
}

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
    /// `AwaitReply` issued after an ACK on `channels[idx]` for `probe`; `done` confirmations
    /// have ACKed so far (always `0` during the sweep).
    AwaitingReply {
        idx: usize,
        done: u8,
        probe: ProbeFrame,
        until: Duration,
    },
    /// `SetChannel { channel: home }` issued because a reply named a channel other than the
    /// probed one.
    SwitchingToHome {
        home: u8,
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
    /// The teardown runs, or has finished.
    Teardown(ScanTeardown),
}

/// Which part of the burst the machine is in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// Walking the channel list; an ACK without a reply only records a candidate.
    Sweep,
    /// Back on the candidate after the list or the burst budget ended; the confirmation rule
    /// settles it, and leaving the channel ends the scan.
    Candidate,
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
    first_fault: Option<ScanFault>,
    burst_expired: bool,
    /// `BoostTxPower` was issued and the teardown has not taken it over yet.
    boosted: bool,
    /// Calls of [`poll_bounded`](Self::poll_bounded) so far.
    polls: usize,
    phase: Phase,
    /// Index of the first channel that ACKed without a reply.
    candidate: Option<usize>,
    /// Sequence number of the last probe issued (`0` before the first).
    seq: u8,
    /// The last encoded probe, for the deprecated [`probe_data`](Self::probe_data).
    last_probe: [u8; PROBE_FRAME_LEN],
    /// The channels the Wi-Fi country allows; others are skipped.
    allowed: ChannelSet,
    /// Entries of the channel list `allowed` excludes.
    not_allowed: usize,
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
            first_fault: None,
            burst_expired: false,
            boosted: false,
            polls: 0,
            phase: Phase::Sweep,
            candidate: None,
            seq: 0,
            last_probe: ProbeFrame {
                seq: 0,
                tx_channel: 0,
            }
            .encode(),
            allowed: ChannelSet::ALL,
            not_allowed: 0,
        }
    }

    /// Restricts the scan to the channels the Wi-Fi country allows (default
    /// [`ChannelSet::ALL`]).
    ///
    /// Both tiers pass the set the radio reported after applying the country.
    /// A channel of [`ScanConfig::channels`] outside `allowed` is skipped without a
    /// [`ScanAction::SetChannel`] (never a [`ScanFault`]) and counted in
    /// [`ScanError::NotFound`]'s `not_allowed`; an explicit list naming one is logged once per
    /// scan at `warn`.
    /// When no listed channel is allowed, [`ScanInput::Start`] finishes not found without any
    /// radio work.
    /// A reply naming a home channel outside `allowed` ends the scan with
    /// [`ScanError::HomeNotAllowed`] through the normal not-found teardown.
    pub fn with_allowed_channels(mut self, allowed: ChannelSet) -> Self {
        self.allowed = allowed;
        self.not_allowed = self
            .config
            .channels
            .iter()
            .filter(|&&channel| !allowed.contains(channel))
            .count();
        self
    }

    /// The channels the scan may probe (see [`with_allowed_channels`](Self::with_allowed_channels)).
    pub fn allowed_channels(&self) -> ChannelSet {
        self.allowed
    }

    /// Entries of [`ScanConfig::channels`] the country does not allow.
    pub fn not_allowed(&self) -> usize {
        self.not_allowed
    }

    /// Continues the probe sequence of the previous scan: the first probe gets `seq + 1`.
    ///
    /// A fresh machine starts at `0`, so every scan would issue `(seq 1, channels[0])` first,
    /// and a reply to the previous scan's first probe that sat unread in the radio's queue
    /// would match it.
    /// Pass the [`seq`](Self::seq) of the previous scan, as both tiers do, and the pair never
    /// repeats until the counter wraps.
    pub fn with_first_seq(mut self, seq: u8) -> Self {
        self.seq = seq;
        self.last_probe = ProbeFrame { seq, tx_channel: 0 }.encode();
        self
    }

    /// Sequence number of the last probe issued (the start value before the first).
    ///
    /// Persist this after [`ScanAction::Done`] and pass it to the next machine through
    /// [`with_first_seq`](Self::with_first_seq).
    pub fn seq(&self) -> u8 {
        self.seq
    }

    /// MAC address of the peer being scanned for.
    pub fn mac(&self) -> &MacAddress {
        &self.mac
    }

    /// The pinned channel: the previous one until a scan succeeds, the discovered one after.
    ///
    /// Persist this after [`ScanAction::Done`] and pass it to the next [`ScanMachine::new`].
    pub fn pinned_channel(&self) -> Option<u8> {
        match self.state {
            State::Teardown(teardown) => teardown.pinned_channel(),
            _ => self.pinned,
        }
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

    /// The first channel switch or probe send that failed so far in this scan.
    pub fn first_fault(&self) -> Option<ScanFault> {
        self.first_fault
    }

    /// The most polls a scan driven with valid inputs needs, the one that returns
    /// [`ScanAction::Done`] included.
    ///
    /// Three setup steps, three per configured channel (channel switch, probe, reply wait),
    /// `3 + 3 * probe_confirmations` for the candidate phase (channel switch, probe and reply
    /// wait, then a gap, a probe and a reply wait per confirmation), one channel switch to a
    /// home channel a reply named, and at most [`ScanTeardown::MAX_POLLS`] for the teardown.
    /// An ignored reply (stale `seq` or other `tx_channel`) is answered with the same
    /// `AwaitReply` and not counted by [`poll_bounded`](Self::poll_bounded).
    /// A driver loop that reaches this count without `Done` feeds [`ScanInput::Abort`] once;
    /// the teardown then needs at most `ScanTeardown::MAX_POLLS` more.
    pub fn max_polls(&self) -> usize {
        const SETUP_POLLS: usize = 3;
        const PER_CHANNEL: usize = 3;
        const HOME_SWITCH: usize = 1;
        let candidate = 3 + 3 * usize::from(self.config.probe_confirmations);
        self.config
            .channels
            .len()
            .saturating_mul(PER_CHANNEL)
            .saturating_add(SETUP_POLLS + candidate + HOME_SWITCH + ScanTeardown::MAX_POLLS)
    }

    /// The teardown the machine would run if it were fed [`ScanInput::Abort`] now.
    ///
    /// Before [`ScanInput::Start`] it finishes on its first poll (nothing was touched); while
    /// the scan
    /// runs it unwinds TX power, scan peer and the pinned-channel fallback and finishes with
    /// [`ScanError::Aborted`]; inside the teardown it is the teardown itself.
    /// Feed it `Abort` at every step, exactly as the machine would be fed.
    pub fn abort_teardown(&self) -> ScanTeardown {
        match self.state {
            State::Idle => ScanTeardown::new(
                self.mac,
                Step::Finishing(Err(ScanError::Aborted)),
                self.pinned,
                ScanError::Aborted,
            ),
            State::RemovingStale
            | State::RegisteringTemp
            | State::Boosting
            | State::SettingChannel { .. }
            | State::Probing { .. }
            | State::AwaitingReply { .. }
            | State::SwitchingToHome { .. }
            | State::Gap { .. }
            | State::Confirming { .. } => {
                self.teardown(Step::Begin { found: None }, ScanError::Aborted)
            }
            State::Teardown(teardown) => teardown,
        }
    }

    /// Logs a failed channel switch or probe send with the driver's error in `detail`.
    ///
    /// Call it before feeding the failure: the first fault of a scan is logged at `warn`, the
    /// repeats at `debug`, so both tiers report a scan that probed nothing the same way.
    pub fn log_fault(&self, detail: core::fmt::Arguments<'_>) {
        if self.first_fault.is_none() {
            log::warn!("ESP-NOW scan: {detail}");
        } else {
            log::debug!("ESP-NOW scan: {detail}");
        }
    }

    /// The probe payload to transmit on [`ScanAction::SendProbe`]`{ channel, .. }`: the
    /// [`ProbeFrame`] with the scan's current sequence number and `channel` as `tx_channel`.
    ///
    /// The sequence number advances with every `SendProbe` the machine issues, so a reply is
    /// matched to the probe it answers.
    pub fn probe_frame(&self, channel: u8) -> [u8; PROBE_FRAME_LEN] {
        ProbeFrame {
            seq: self.seq,
            tx_channel: channel,
        }
        .encode()
    }

    /// The last probe frame the machine issued (the start-sequence frame for channel 0 before
    /// the first probe).
    ///
    /// Kept for one release for drivers written against the payload of
    /// [`ScanConfig::probe_data`]; the scan now sends the `0xF3` frame.
    /// Unlike that payload, the slice borrows the machine, so read it after each `SendProbe`
    /// instead of holding it across `poll`.
    #[deprecated(
        since = "0.7.0",
        note = "the scan sends the 0xF3 probe frame; use `probe_frame(channel)` for the channel `SendProbe` names"
    )]
    pub fn probe_data(&self) -> &[u8] {
        &self.last_probe
    }

    /// [`poll`](Self::poll) under the poll budget of ADR 023 decision 6.
    ///
    /// Counts its own calls, except one whose [`ScanInput::Reply`] was ignored (the answer is
    /// the pending [`ScanAction::AwaitReply`] again), since it advanced nothing and the wait it
    /// belongs to is bounded by its deadline.
    /// The call that reaches [`max_polls`](Self::max_polls) substitutes [`ScanInput::or_abort`]`(true)` for `input`, so a scan that never finishes unwinds through its teardown.
    /// Once the teardown budget of [`ScanTeardown::MAX_POLLS`] polls is spent as well, the machine is broken.
    /// Use it instead of `poll` in a driver loop, and do not mix the two.
    ///
    /// # Errors
    ///
    /// [`ScanError::UnexpectedInput`] when the scan has not reached [`ScanAction::Done`] within `max_polls() + ScanTeardown::MAX_POLLS` calls.
    pub fn poll_bounded(
        &mut self,
        now: Duration,
        input: ScanInput,
    ) -> Result<ScanAction, ScanError> {
        let max_polls = self.max_polls();
        let input = if self.polls == max_polls {
            log::warn!("ESP-NOW scan: not finished after {max_polls} steps; aborting");
            input.or_abort(true)
        } else if self.polls >= max_polls + ScanTeardown::MAX_POLLS {
            log::error!("ESP-NOW scan: the machine did not finish its teardown; giving up");
            return Err(ScanError::UnexpectedInput);
        } else {
            input
        };
        let action = self.poll(now, input);
        let ignored_reply =
            matches!(input, ScanInput::Reply(_)) && matches!(action, ScanAction::AwaitReply { .. });
        if !ignored_reply {
            self.polls += 1;
        }
        Ok(action)
    }

    /// Advance the machine.  See the [module docs](self) for the protocol.
    pub fn poll(&mut self, now: Duration, input: ScanInput) -> ScanAction {
        match (self.state, input) {
            (State::Teardown(mut teardown), _) => {
                let action = teardown.poll(input);
                self.state = State::Teardown(teardown);
                action
            }
            (_, ScanInput::Shutdown) => {
                self.enter(Step::Begin { found: None }, ScanError::Aborted, input)
            }
            (_, ScanInput::Abort) => self.unwind(ScanError::Aborted),

            (State::Idle, ScanInput::Start) => {
                if self.not_allowed > 0 {
                    self.log_not_allowed();
                    if self.not_allowed == self.config.channels.len() {
                        // Nothing to probe: finish without touching the radio.
                        let error = self.not_found();
                        return self.enter(
                            Step::Finishing(Err(error)),
                            error,
                            ScanInput::Completed,
                        );
                    }
                }
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
                self.boosted = true;
                ScanAction::BoostTxPower
            }
            // Nothing boosted and no scan peer registered: straight to the pinned-channel
            // fallback.
            (State::RegisteringTemp, ScanInput::Failed) => self.enter(
                Step::RemovingScanPeer { found: None },
                ScanError::RegisterTempPeer,
                ScanInput::Completed,
            ),

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
            (State::SettingChannel { idx }, ScanInput::Failed) => {
                self.record_fault(ScanFault::SetChannel {
                    channel: self.config.channels[idx],
                });
                self.leave_channel(now, idx)
            }

            (State::Probing { idx }, ScanInput::Sent(outcome)) => match outcome {
                SendOutcome::Local => {
                    self.record_fault(ScanFault::ProbeSend {
                        channel: self.config.channels[idx],
                    });
                    self.leave_channel(now, idx)
                }
                SendOutcome::NoAck => {
                    self.count_probed();
                    self.leave_channel(now, idx)
                }
                SendOutcome::Acked => {
                    self.count_probed();
                    self.await_reply(now, idx, 0)
                }
            },

            // The reply wins: the peer states its home channel, so no probe on it proves more.
            (State::AwaitingReply { probe, until, .. }, ScanInput::Reply(reply)) => {
                if reply.seq == probe.seq
                    && reply.tx_channel == probe.tx_channel
                    && (1..=14).contains(&reply.home_channel)
                {
                    self.found_at(reply.home_channel, probe.tx_channel)
                } else {
                    log::debug!(
                        "ESP-NOW scan: ignoring probe reply {reply:?} (outstanding probe {probe:?})"
                    );
                    ScanAction::AwaitReply { until }
                }
            }
            // No reply: an ACK alone only nominates the channel until the sweep is
            // over; back on the candidate, the confirmation rule settles it.
            (State::AwaitingReply { idx, done, .. }, ScanInput::Completed) => match self.phase {
                Phase::Sweep => {
                    if self.candidate.is_none() {
                        self.candidate = Some(idx);
                    }
                    self.next_channel(now, idx + 1)
                }
                Phase::Candidate => self.after_ack(now, idx, done),
            },

            (State::SwitchingToHome { home }, ScanInput::Completed) => self.found(home),
            // The stated home channel is not usable here: the peer stays unreachable.
            (State::SwitchingToHome { home }, ScanInput::Failed) => {
                self.record_fault(ScanFault::SetChannel { channel: home });
                self.finish_not_found()
            }

            (State::Gap { idx, done }, ScanInput::Completed) => {
                self.state = State::Confirming { idx, done };
                self.send_probe(idx, done + 1)
            }
            (State::Confirming { idx, done }, ScanInput::Sent(outcome)) => match outcome {
                SendOutcome::Acked => self.await_reply(now, idx, done + 1),
                // Unconfirmed: the peer was only passing through (roaming).
                SendOutcome::NoAck => self.leave_channel(now, idx),
                SendOutcome::Local => {
                    self.record_fault(ScanFault::ProbeSend {
                        channel: self.config.channels[idx],
                    });
                    self.leave_channel(now, idx)
                }
            },

            _ => self.unwind(ScanError::UnexpectedInput),
        }
    }

    /// The teardown at `step`, finishing with `failure` unless a peer is found.
    fn teardown(&self, step: Step, failure: ScanError) -> ScanTeardown {
        let mut teardown = ScanTeardown::new(self.mac, step, self.pinned, failure);
        teardown.boosted = self.boosted;
        teardown
    }

    /// Hand over to the teardown at `step` and feed it `input`.
    fn enter(&mut self, step: Step, failure: ScanError, input: ScanInput) -> ScanAction {
        let mut teardown = self.teardown(step, failure);
        self.boosted = false;
        let action = teardown.poll(input);
        self.state = State::Teardown(teardown);
        action
    }

    /// Give up with `error`.
    ///
    /// While the scan is running, unwinds through the normal teardown so TX power, the
    /// scan peer and the pinned-channel fallback are handled.
    /// Before `Start` there is nothing to unwind: finish at once.
    fn unwind(&mut self, error: ScanError) -> ScanAction {
        match self.state {
            State::Idle => self.enter(Step::Finishing(Err(error)), error, ScanInput::Completed),
            _ => self.enter(Step::Begin { found: None }, error, ScanInput::Abort),
        }
    }

    /// Keep only the first fault of the scan.
    fn record_fault(&mut self, fault: ScanFault) {
        self.first_fault.get_or_insert(fault);
    }

    /// Issue the next probe on `channels[idx]`, advancing the sequence number.
    fn send_probe(&mut self, idx: usize, confirmation: u8) -> ScanAction {
        let channel = self.config.channels[idx];
        self.seq = self.seq.wrapping_add(1);
        self.last_probe = self.probe_frame(channel);
        ScanAction::SendProbe {
            channel,
            confirmation,
            timeout: self.config.probe_timeout,
        }
    }

    /// Wait for the reply to the probe just ACKed on `channels[idx]`.
    fn await_reply(&mut self, now: Duration, idx: usize, done: u8) -> ScanAction {
        let until = now.saturating_add(self.config.reply_timeout);
        self.state = State::AwaitingReply {
            idx,
            done,
            probe: ProbeFrame {
                seq: self.seq,
                tx_channel: self.config.channels[idx],
            },
            until,
        };
        ScanAction::AwaitReply { until }
    }

    /// A probe went out on a new channel; confirmation and candidate probes are not counted.
    fn count_probed(&mut self) {
        if self.phase == Phase::Sweep {
            self.probed += 1;
        }
    }

    /// The scan is done with `channels[idx]` without a result: the next channel during the
    /// sweep, the end of the scan once back on the candidate.
    fn leave_channel(&mut self, now: Duration, idx: usize) -> ScanAction {
        match self.phase {
            Phase::Sweep => self.next_channel(now, idx + 1),
            Phase::Candidate => self.finish_not_found(),
        }
    }

    /// Logs the channels of the list the country does not allow, once per scan: at `warn` for
    /// an explicit list, at `debug` for [`DEFAULT_SCAN_CHANNELS`] (expected under the world
    /// default).
    fn log_not_allowed(&self) {
        let skipped = self
            .config
            .channels
            .iter()
            .filter(|&&channel| !self.allowed.contains(channel))
            .fold(ChannelSet::EMPTY, |set, &channel| set.with(channel));
        if self.config.channels == DEFAULT_SCAN_CHANNELS.as_slice() {
            log::debug!(
                "ESP-NOW scan: skipping channels {skipped} (country allows {})",
                self.allowed
            );
        } else {
            log::warn!(
                "ESP-NOW scan: skipping {} listed channel(s) {skipped} not allowed by the country (allowed {})",
                self.not_allowed,
                self.allowed
            );
        }
    }

    /// Finish on `channel`: the teardown registers the peer there.
    fn found(&mut self, channel: u8) -> ScanAction {
        self.enter(
            Step::Begin {
                found: Some(channel),
            },
            self.not_found(),
            ScanInput::Completed,
        )
    }

    /// A reply named `home` while the radio is on `probed`: finish there, switching first when
    /// the two differ so the radio sits on the found channel at `Done`.
    fn found_at(&mut self, home: u8, probed: u8) -> ScanAction {
        if !self.allowed.contains(home) {
            log::warn!(
                "ESP-NOW scan: peer replied on channel {probed} with home channel {home}, which the country does not allow (allowed {})",
                self.allowed
            );
            return self.enter(
                Step::Begin { found: None },
                ScanError::HomeNotAllowed { home },
                ScanInput::Completed,
            );
        }
        if home == probed {
            self.found(home)
        } else {
            log::debug!("ESP-NOW scan: peer replied on channel {probed}, home channel {home}");
            self.state = State::SwitchingToHome { home };
            ScanAction::SetChannel { channel: home }
        }
    }

    /// Finish with [`ScanError::NotFound`].
    fn finish_not_found(&mut self) -> ScanAction {
        self.enter(
            Step::Begin { found: None },
            self.not_found(),
            ScanInput::Completed,
        )
    }

    /// The error a scan that found nothing finishes with.
    fn not_found(&self) -> ScanError {
        ScanError::NotFound {
            probed: self.probed,
            configured: self.config.channels.len(),
            not_allowed: self.not_allowed,
            first_fault: self.first_fault,
        }
    }

    /// The reply wait after an ACK on the candidate `channels[idx]` ended with `done`
    /// confirmations collected: the confirmation rule decides.
    fn after_ack(&mut self, now: Duration, idx: usize, done: u8) -> ScanAction {
        if done >= self.config.probe_confirmations {
            self.found(self.config.channels[idx])
        } else {
            self.state = State::Gap { idx, done };
            ScanAction::WaitUntil(now.saturating_add(self.config.confirmation_gap))
        }
    }

    /// Move to the first allowed channel from `channels[idx]` on, or end the sweep when the list or the burst budget is exhausted:
    /// back to the candidate if one was recorded, otherwise not found.
    fn next_channel(&mut self, now: Duration, idx: usize) -> ScanAction {
        // A channel the country does not allow is skipped without radio work.
        let mut idx = idx;
        while let Some(&channel) = self.config.channels.get(idx) {
            if self.allowed.contains(channel) {
                break;
            }
            idx += 1;
        }
        let burst_over = now.saturating_sub(self.burst_start) >= self.config.burst_timeout;
        match self.config.channels.get(idx) {
            Some(&channel) if !burst_over => {
                self.state = State::SettingChannel { idx };
                ScanAction::SetChannel { channel }
            }
            other => {
                self.burst_expired = burst_over && other.is_some();
                if self.burst_expired {
                    log::debug!(
                        "Burst timeout {:?} reached after {} channels; stopping scan",
                        self.config.burst_timeout,
                        self.probed
                    );
                }
                match self.candidate {
                    Some(candidate) => {
                        let channel = self.config.channels[candidate];
                        log::debug!(
                            "ESP-NOW scan: no reply on any channel; confirming the ACK on channel {channel}"
                        );
                        self.phase = Phase::Candidate;
                        self.state = State::SettingChannel { idx: candidate };
                        ScanAction::SetChannel { channel }
                    }
                    None => self.finish_not_found(),
                }
            }
        }
    }
}

/// Decide whether a TX-power read-back confirms the restore after a scan.
///
/// `saved` is the level read before [`ScanAction::BoostTxPower`] (`None` when the
/// save failed, so no restore was attempted) and `read_back` is the level read after
/// [`ScanAction::RestoreTxPower`] (`None` when the read failed).
/// Returns `true` when there is nothing to verify or the levels match, and `false`
/// only when both are known and differ, so the caller can log the mismatch.
#[must_use]
pub const fn restore_matches(saved: Option<i8>, read_back: Option<i8>) -> bool {
    match (saved, read_back) {
        (Some(s), Some(r)) => s == r,
        _ => true,
    }
}

/// Check the TX-power read-back after [`ScanAction::RestoreTxPower`] and log a mismatch.
///
/// Both tiers call this with the level saved before the boost and the level read from the
/// radio afterwards, so the warning reads the same everywhere.
/// The restore is advisory: the scan result is never changed.
/// Returns the value of [`restore_matches`].
pub fn warn_on_restore_mismatch(saved: Option<i8>, read_back: Option<i8>) -> bool {
    let ok = restore_matches(saved, read_back);
    if !ok {
        log::warn!(
            "ESP-NOW scan: TX power not restored (saved {saved:?}, radio reports {read_back:?} quarter dBm)"
        );
    }
    ok
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
        /// Adjacent channels on which the peer's radio still decodes and ACKs a probe.
        acks_also_on: Vec<u8>,
        /// The peer runs the echo app and answers a decoded probe with a `ProbeReply`.
        replies: bool,
        /// Channels in `acks_also_on` on which the scanning radio also decodes the reply;
        /// on `home` it always does.
        decodes_reply_on: Vec<u8>,
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
        /// A peer on `channel` that answers probes with a reply (the echo app).
        fn on(channel: u8) -> Self {
            Self {
                home: Some(channel),
                acks_also_on: Vec::new(),
                replies: true,
                decodes_reply_on: Vec::new(),
                refuse: Vec::new(),
                acks_before_silence: None,
                roams_to: None,
                fail_register: None,
                fail_send: None,
            }
        }
        /// A peer on `channel` that only ACKs at the MAC layer (no echo app).
        fn legacy(channel: u8) -> Self {
            Self {
                replies: false,
                ..Self::on(channel)
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
        AwaitReply(D),
    }

    struct Run {
        actions: Vec<(D, Seen)>,
        /// `(channel, confirmation)` of every `SendProbe`.
        probe_log: Vec<(u8, u8)>,
        /// The decoded probe frame of every `SendProbe`.
        frames: Vec<ProbeFrame>,
        result: Result<ScanResult, ScanError>,
        pinned: Option<u8>,
        end: D,
        /// `poll` calls until `Done`, that one included.
        polls: usize,
        /// [`ScanMachine::max_polls`] of the machine at the start.
        max_polls: usize,
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
    /// an ACK arrives after 5 ms and a reply 2 ms after the ACK.
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
        let mut frames: Vec<ProbeFrame> = Vec::new();
        let mut input = ScanInput::Start;
        let max_polls = m.max_polls();
        for polls in 1..10_000 {
            let action = m.poll(now, input);
            match action {
                ScanAction::Done(result) => {
                    return Run {
                        actions,
                        probe_log,
                        frames,
                        result,
                        pinned: m.pinned_channel(),
                        end: now,
                        polls,
                        max_polls,
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
                    let frame = ProbeFrame::parse(&m.probe_frame(channel)).expect("a 0xF3 probe");
                    assert_eq!(frame.tx_channel, channel);
                    frames.push(frame);
                    actions.push((now, Seen::Probe(timeout)));
                    let n = probes;
                    probes += 1;
                    if peer.fail_send == Some(n) {
                        now += op_cost;
                        input = ScanInput::Sent(SendOutcome::Local);
                        continue;
                    }
                    let silent = peer.acks_before_silence.is_some_and(|k| acks >= k);
                    let decoded =
                        home == Some(radio_channel) || peer.acks_also_on.contains(&radio_channel);
                    if decoded && !silent {
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
                ScanAction::AwaitReply { until } => {
                    actions.push((now, Seen::AwaitReply(until)));
                    assert!(until >= now);
                    let heard = home == Some(radio_channel)
                        || peer.decodes_reply_on.contains(&radio_channel);
                    match (peer.replies && heard, home, frames.last()) {
                        (true, Some(home), Some(probe)) => {
                            now += D::from_millis(2);
                            input = ScanInput::Reply(ProbeReply::answering(probe, home));
                        }
                        _ => {
                            now = until;
                            input = ScanInput::Completed;
                        }
                    }
                }
                ScanAction::Ignored => panic!("input ignored before Done"),
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
        // Adjusted for the probe reply: the reply ends the scan, no confirmation probe.
        let r = run(machine(ScanConfig::for_peer(), None), &Peer::on(1), D::ZERO);
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
                Seen::AwaitReply(D::from_secs(1000) + D::from_millis(5) + D::from_millis(50)),
                Seen::Restore,
                Seen::Remove,
                Seen::Register(1),
            ]
        );
    }

    #[test]
    fn found_on_channel_6() {
        let r = run(machine(ScanConfig::for_peer(), None), &Peer::on(6), D::ZERO);
        assert_eq!(r.result, Ok(ScanResult { channel: 6 }));
        assert_eq!(r.channels_set(), [1, 2, 3, 4, 5, 6]);
        assert_eq!(r.pinned, Some(6));
        // 5 misses + 1 hit; the reply replaces the confirmation probe.
        assert_eq!(r.probes(), 6);
        assert_eq!(r.seen().last(), Some(&Seen::Register(6)));
    }

    // ── probe reply ─────────────────────────────────────────────

    // Hand mutation checks (2026-11-11, `just test-espnow`), each applied, run and reverted:
    // - seq match: `reply.seq == probe.seq` dropped from the reply arm -> 454/2:
    //   `stale_or_foreign_replies_are_ignored_and_the_wait_continues` and
    //   `ignored_replies_do_not_consume_the_poll_budget` (a seq+1 reply finishes the scan).
    // - candidate fallback: `self.candidate = Some(idx)` removed -> 443/13, among them
    //   `legacy_ack_only_peer_is_found_through_the_candidate_and_confirmation_path` and
    //   `burst_budget_ending_with_a_candidate_still_runs_the_candidate_phase` (`NotFound`).
    // - home_channel use: `found_at(probe.tx_channel, ..)` in place of `reply.home_channel` ->
    //   454/2: `adjacent_ack_whose_reply_names_the_home_channel_finds_it_at_once` (pins 10) and
    //   `reply_home_channel_that_the_radio_refuses_ends_the_scan_not_found`.

    #[test]
    fn adjacent_ack_without_a_decodable_reply_is_outrun_by_the_reply_on_the_home_channel() {
        // Channel 10 decodes and ACKs the probe, but the reply is only heard on 11.
        let peer = Peer {
            acks_also_on: alloc::vec![10],
            ..Peer::on(11)
        };
        let r = run(machine(ScanConfig::for_peer(), None), &peer, D::ZERO);
        assert_eq!(r.result, Ok(ScanResult { channel: 11 }));
        assert_eq!(r.pinned, Some(11));
        assert_eq!(
            r.channels_set(),
            (1..=11).collect::<Vec<u8>>(),
            "the candidate on 10 is never revisited"
        );
        assert_eq!(r.probe_log[r.probe_log.len() - 2..], [(10, 0), (11, 0)]);
        assert_eq!(
            r.actions
                .iter()
                .filter(|(_, s)| matches!(s, Seen::AwaitReply(_)))
                .count(),
            2
        );
    }

    #[test]
    fn adjacent_ack_whose_reply_names_the_home_channel_finds_it_at_once() {
        let peer = Peer {
            acks_also_on: alloc::vec![10],
            decodes_reply_on: alloc::vec![10],
            ..Peer::on(11)
        };
        let r = run(machine(ScanConfig::for_peer(), None), &peer, D::ZERO);
        assert_eq!(r.result, Ok(ScanResult { channel: 11 }));
        assert_eq!(r.pinned, Some(11));
        assert_eq!(r.probes(), 10, "no probe on 11: the reply named it");
        let seen = r.seen();
        assert_eq!(
            seen[seen.len() - 4..],
            [
                Seen::SetChannel(11),
                Seen::Restore,
                Seen::Remove,
                Seen::Register(11)
            ],
            "the radio is moved to the stated home channel before the teardown"
        );
    }

    #[test]
    fn legacy_ack_only_peer_is_found_through_the_candidate_and_confirmation_path() {
        let r = run(
            machine(ScanConfig::for_peer(), None),
            &Peer::legacy(6),
            D::ZERO,
        );
        assert_eq!(r.result, Ok(ScanResult { channel: 6 }));
        assert_eq!(r.pinned, Some(6));
        // The whole list first, then back to the candidate.
        assert_eq!(r.channels_set(), (1..=13).chain([6]).collect::<Vec<u8>>());
        assert_eq!(r.probe_log[r.probe_log.len() - 2..], [(6, 0), (6, 1)]);
        assert_eq!(
            r.probes(),
            15,
            "13 sweep probes, one candidate probe, one confirmation"
        );
        assert_eq!(
            r.seen()
                .iter()
                .filter(|s| matches!(s, Seen::Wait(_)))
                .count(),
            1
        );
    }

    #[test]
    fn a_reply_during_the_candidate_phase_also_wins() {
        let cfg = ScanConfig::for_peer().with_channels(&[6]);
        let mut m = machine(cfg, None);
        let t = D::from_secs(5);
        m.poll(t, ScanInput::Start);
        m.poll(t, ScanInput::Completed);
        m.poll(t, ScanInput::Completed);
        assert_eq!(
            m.poll(t, ScanInput::Completed),
            ScanAction::SetChannel { channel: 6 }
        );
        assert!(matches!(
            m.poll(t, ScanInput::Completed),
            ScanAction::SendProbe { .. }
        ));
        assert!(matches!(
            m.poll(t, ScanInput::Sent(SendOutcome::Acked)),
            ScanAction::AwaitReply { .. }
        ));
        // No reply in the sweep: the list is over, back to the candidate.
        assert_eq!(
            m.poll(t, ScanInput::Completed),
            ScanAction::SetChannel { channel: 6 }
        );
        assert!(matches!(
            m.poll(t, ScanInput::Completed),
            ScanAction::SendProbe {
                channel: 6,
                confirmation: 0,
                ..
            }
        ));
        let probe = ProbeFrame::parse(&m.probe_frame(6)).unwrap();
        assert!(matches!(
            m.poll(t, ScanInput::Sent(SendOutcome::Acked)),
            ScanAction::AwaitReply { .. }
        ));
        // The reply arrives this time: found without the confirmation probe.
        assert_eq!(
            m.poll(t, ScanInput::Reply(ProbeReply::answering(&probe, 6))),
            ScanAction::RestoreTxPower
        );
        assert_eq!(m.poll(t, ScanInput::Completed), ScanAction::RemovePeer);
        assert_eq!(
            m.poll(t, ScanInput::Completed),
            ScanAction::RegisterPeer { channel: 6 }
        );
        assert_eq!(
            m.poll(t, ScanInput::Completed),
            ScanAction::Done(Ok(ScanResult { channel: 6 }))
        );
    }

    #[test]
    fn stale_or_foreign_replies_are_ignored_and_the_wait_continues() {
        let cfg = ScanConfig::for_peer().with_channels(&[6]);
        let mut m = machine(cfg, None);
        let t = D::from_secs(5);
        m.poll(t, ScanInput::Start);
        m.poll(t, ScanInput::Completed);
        m.poll(t, ScanInput::Completed);
        m.poll(t, ScanInput::Completed);
        assert!(matches!(
            m.poll(t, ScanInput::Completed),
            ScanAction::SendProbe { channel: 6, .. }
        ));
        let probe = ProbeFrame::parse(&m.probe_frame(6)).unwrap();
        let until = t + D::from_millis(50);
        assert_eq!(
            m.poll(t, ScanInput::Sent(SendOutcome::Acked)),
            ScanAction::AwaitReply { until }
        );
        let stale = ProbeReply {
            seq: probe.seq.wrapping_add(1),
            ..ProbeReply::answering(&probe, 11)
        };
        let foreign = ProbeReply {
            tx_channel: 7,
            ..ProbeReply::answering(&probe, 11)
        };
        let out_of_range = ProbeReply::answering(&probe, 0);
        for reply in [stale, foreign, out_of_range] {
            assert_eq!(
                m.poll(t + D::from_millis(10), ScanInput::Reply(reply)),
                ScanAction::AwaitReply { until },
                "{reply:?} must not end the wait"
            );
        }
        // The deadline passes: the channel is the candidate and the list is over.
        assert_eq!(
            m.poll(until, ScanInput::Completed),
            ScanAction::SetChannel { channel: 6 }
        );
        assert_eq!(m.pinned_channel(), None);
    }

    #[test]
    fn ignored_replies_do_not_consume_the_poll_budget() {
        let cfg = ScanConfig::for_peer().with_channels(&[6]);
        let mut m = machine(cfg, None);
        let t = D::ZERO;
        let mut action = m.poll_bounded(t, ScanInput::Start).unwrap();
        while !matches!(action, ScanAction::SendProbe { .. }) {
            action = m.poll_bounded(t, ScanInput::Completed).unwrap();
        }
        let probe = ProbeFrame::parse(&m.probe_frame(6)).unwrap();
        m.poll_bounded(t, ScanInput::Sent(SendOutcome::Acked))
            .unwrap();
        let polls = m.polls;
        let stale = ProbeReply {
            seq: probe.seq.wrapping_add(1),
            ..ProbeReply::answering(&probe, 6)
        };
        for _ in 0..(m.max_polls() * 2) {
            assert!(matches!(
                m.poll_bounded(t, ScanInput::Reply(stale)).unwrap(),
                ScanAction::AwaitReply { .. }
            ));
        }
        assert_eq!(m.polls, polls, "ignored replies are not counted");
        // A matching reply is counted and finishes the scan.
        assert_eq!(
            m.poll_bounded(t, ScanInput::Reply(ProbeReply::answering(&probe, 6)))
                .unwrap(),
            ScanAction::RestoreTxPower
        );
        assert_eq!(m.polls, polls + 1);
    }

    #[test]
    fn reply_home_channel_that_the_radio_refuses_ends_the_scan_not_found() {
        let peer = Peer {
            acks_also_on: alloc::vec![10],
            decodes_reply_on: alloc::vec![10],
            refuse: alloc::vec![11],
            ..Peer::on(11)
        };
        let r = run(machine(ScanConfig::for_peer(), Some(3)), &peer, D::ZERO);
        assert_eq!(
            r.result,
            Err(ScanError::NotFound {
                probed: 10,
                configured: 13,
                not_allowed: 0,
                first_fault: Some(ScanFault::SetChannel { channel: 11 })
            })
        );
        assert_eq!(r.pinned, Some(3));
        let seen = r.seen();
        assert_eq!(
            seen[seen.len() - 2..],
            [Seen::RestoreChannel(3), Seen::RegisterFallback(3)]
        );
    }

    // ── Wi-Fi country (allowed channels) ─────────────────────────────────

    // Hand mutation checks (2026-11-11, `just test-espnow`), each applied, run and reverted:
    // - skip test: `allowed.contains(channel)` negated in `next_channel` -> many fail, among
    //   them `an_explicit_list_probes_only_its_allowed_channels`;
    // - `not_allowed` count: always 0 in `with_allowed_channels` -> 4 fail, among them
    //   `world_country_with_the_default_list_probes_only_1_to_11`;
    // - home guard: the `!self.allowed.contains(home)` early return in `found_at` removed ->
    //   `a_reply_naming_a_forbidden_home_channel_never_switches_to_it`;
    // - table: `01` moved to the 1-13 row in `Country::channels` -> 9 fail, among them the
    //   forbidden-channel regression above.

    fn world() -> ChannelSet {
        crate::wifi::Country::WORLD.channels()
    }

    #[test]
    fn world_country_with_the_default_list_probes_only_1_to_11() {
        // The default list names 12 and 13, which the world default forbids.
        let m = machine(ScanConfig::for_peer(), Some(3)).with_allowed_channels(world());
        assert_eq!(m.not_allowed(), 2);
        let r = run(m, &Peer::absent(), D::ZERO);
        assert_eq!(r.channels_set(), (1..=11).collect::<Vec<u8>>());
        assert_eq!(
            r.result,
            Err(ScanError::NotFound {
                probed: 11,
                configured: 13,
                not_allowed: 2,
                first_fault: None
            })
        );
        assert_eq!(r.pinned, Some(3));
        assert!(r.polls <= r.max_polls);
    }

    #[test]
    fn an_explicit_list_probes_only_its_allowed_channels() {
        let cfg = ScanConfig::for_peer().with_channels(&[12, 13, 6]);
        let r = run(
            machine(cfg, None).with_allowed_channels(world()),
            &Peer::on(6),
            D::ZERO,
        );
        assert_eq!(r.result, Ok(ScanResult { channel: 6 }));
        assert_eq!(r.channels_set(), [6]);
        assert_eq!(r.probe_log, [(6, 0)]);
    }

    #[test]
    fn an_explicit_list_with_a_forbidden_tail_reports_it_as_not_allowed() {
        let cfg = ScanConfig::for_peer().with_channels(&[6, 12, 13]);
        let r = run(
            machine(cfg, None).with_allowed_channels(world()),
            &Peer::absent(),
            D::ZERO,
        );
        assert_eq!(r.channels_set(), [6]);
        assert_eq!(
            r.result,
            Err(ScanError::NotFound {
                probed: 1,
                configured: 3,
                not_allowed: 2,
                first_fault: None
            })
        );
    }

    #[test]
    fn a_list_with_no_allowed_channel_does_no_radio_work() {
        let cfg = ScanConfig::for_peer().with_channels(&[12, 13]);
        let r = run(
            machine(cfg, Some(4)).with_allowed_channels(world()),
            &Peer::on(12),
            D::ZERO,
        );
        assert!(r.seen().is_empty(), "no radio action: {:?}", r.seen());
        assert_eq!(
            r.result,
            Err(ScanError::NotFound {
                probed: 0,
                configured: 2,
                not_allowed: 2,
                first_fault: None
            })
        );
        assert_eq!(r.pinned, Some(4));
        assert_eq!(r.polls, 1);
    }

    #[test]
    fn a_reply_naming_a_forbidden_home_channel_never_switches_to_it() {
        // The peer's network runs on 13; a probe on 11 leaks across and is answered.
        let peer = Peer {
            acks_also_on: alloc::vec![11],
            decodes_reply_on: alloc::vec![11],
            ..Peer::on(13)
        };
        let r = run(
            machine(ScanConfig::for_peer(), Some(3)).with_allowed_channels(world()),
            &peer,
            D::ZERO,
        );
        assert_eq!(r.result, Err(ScanError::HomeNotAllowed { home: 13 }));
        assert!(!r.channels_set().contains(&13));
        assert!(!r.channels_set().contains(&12));
        assert_eq!(r.pinned, Some(3));
        let seen = r.seen();
        assert_eq!(
            seen[seen.len() - 2..],
            [Seen::RestoreChannel(3), Seen::RegisterFallback(3)]
        );
        assert_eq!(
            seen.iter().filter(|s| **s == Seen::Restore).count(),
            1,
            "the TX power is restored"
        );
    }

    #[test]
    fn the_same_reply_is_followed_when_the_country_allows_the_home_channel() {
        let peer = Peer {
            acks_also_on: alloc::vec![11],
            decodes_reply_on: alloc::vec![11],
            ..Peer::on(13)
        };
        let de = crate::wifi::Country::parse("DE").unwrap().channels();
        let r = run(
            machine(ScanConfig::for_peer(), Some(3)).with_allowed_channels(de),
            &peer,
            D::ZERO,
        );
        assert_eq!(r.result, Ok(ScanResult { channel: 13 }));
        assert_eq!(r.pinned, Some(13));
    }

    #[test]
    fn the_default_allowed_set_changes_nothing() {
        let m = machine(ScanConfig::for_peer(), None);
        assert_eq!(m.allowed_channels(), ChannelSet::ALL);
        assert_eq!(m.not_allowed(), 0);
        let r = run(m, &Peer::absent(), D::ZERO);
        assert_eq!(r.channels_set(), (1..=13).collect::<Vec<u8>>());
    }

    #[test]
    fn burst_budget_ending_with_a_candidate_still_runs_the_candidate_phase() {
        let cfg = ScanConfig::for_peer().with_burst_timeout(D::from_millis(1));
        let r = run(machine(cfg, None), &Peer::legacy(1), D::ZERO);
        assert_eq!(r.result, Ok(ScanResult { channel: 1 }));
        assert_eq!(r.channels_set(), [1, 1], "the sweep ends after one channel");
        assert_eq!(r.probe_log, [(1, 0), (1, 0), (1, 1)]);
    }

    #[test]
    fn probe_frames_carry_an_increasing_sequence_and_the_probed_channel() {
        let r = run(
            machine(ScanConfig::for_peer(), None),
            &Peer::legacy(6),
            D::ZERO,
        );
        let seqs: Vec<u8> = r.frames.iter().map(|f| f.seq).collect();
        assert_eq!(seqs, (1..=15).collect::<Vec<u8>>());
        let channels: Vec<u8> = r.frames.iter().map(|f| f.tx_channel).collect();
        assert_eq!(channels, (1..=13).chain([6, 6]).collect::<Vec<u8>>());
        assert!(r
            .frames
            .iter()
            .all(|f| f.encode()[0] == super::super::TAG_PROBE));
    }

    #[test]
    fn a_scan_seeded_with_the_previous_sequence_continues_it() {
        let first = run(
            machine(ScanConfig::for_peer().with_channels(&[6]), None),
            &Peer::legacy(6),
            D::ZERO,
        );
        let last_seq = first.frames.last().map(|f| f.seq);
        assert_eq!(
            last_seq,
            Some(3),
            "sweep probe, candidate probe, one confirmation"
        );
        let second = run(
            machine(ScanConfig::for_peer().with_channels(&[6]), None).with_first_seq(3),
            &Peer::legacy(6),
            D::ZERO,
        );
        let seqs: Vec<u8> = second.frames.iter().map(|f| f.seq).collect();
        assert_eq!(seqs, [4, 5, 6]);
        // Mutation check: a stale reply to the previous scan's first probe no longer matches.
        let mut m = machine(ScanConfig::for_peer().with_channels(&[6]), None).with_first_seq(3);
        assert_eq!(m.seq(), 3);
        m.poll(D::ZERO, ScanInput::Start);
        m.poll(D::ZERO, ScanInput::Completed);
        m.poll(D::ZERO, ScanInput::Completed);
        m.poll(D::ZERO, ScanInput::Completed);
        m.poll(D::ZERO, ScanInput::Completed);
        let ScanAction::AwaitReply { until } = m.poll(D::ZERO, ScanInput::Sent(SendOutcome::Acked))
        else {
            panic!("expected the reply wait");
        };
        assert_eq!(m.seq(), 4);
        let stale = ProbeReply {
            seq: 1,
            tx_channel: 6,
            home_channel: 11,
        };
        assert_eq!(
            m.poll(D::ZERO, ScanInput::Reply(stale)),
            ScanAction::AwaitReply { until }
        );
    }

    #[test]
    #[allow(deprecated)]
    fn probe_data_returns_the_last_encoded_frame() {
        let mut m = machine(ScanConfig::for_peer().with_channels(&[6]), None);
        assert_eq!(m.probe_data(), &[super::super::TAG_PROBE, 0, 0]);
        m.poll(D::ZERO, ScanInput::Start);
        m.poll(D::ZERO, ScanInput::Completed);
        m.poll(D::ZERO, ScanInput::Completed);
        m.poll(D::ZERO, ScanInput::Completed);
        assert!(matches!(
            m.poll(D::ZERO, ScanInput::Completed),
            ScanAction::SendProbe { .. }
        ));
        assert_eq!(m.probe_data(), &m.probe_frame(6));
        assert_eq!(m.probe_data(), &[super::super::TAG_PROBE, 1, 6]);
    }

    #[test]
    fn found_on_channel_11() {
        let r = run(
            machine(ScanConfig::for_peer(), None),
            &Peer::on(11),
            D::ZERO,
        );
        assert_eq!(r.result, Ok(ScanResult { channel: 11 }));
        assert_eq!(r.channels_set(), (1..=11).collect::<Vec<u8>>());
    }

    #[test]
    fn found_on_last_default_channel_13() {
        let r = run(
            machine(ScanConfig::for_peer(), None),
            &Peer::on(13),
            D::ZERO,
        );
        assert_eq!(r.result, Ok(ScanResult { channel: 13 }));
    }

    // ── not found ─────────────────────────────────────────────────────────

    #[test]
    fn not_found_without_previous_pin_leaves_peer_removed() {
        let r = run(
            machine(ScanConfig::for_peer(), None),
            &Peer::absent(),
            D::ZERO,
        );
        assert_eq!(
            r.result,
            Err(ScanError::NotFound {
                probed: 13,
                configured: 13,
                not_allowed: 0,
                first_fault: None
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
            machine(ScanConfig::for_peer(), Some(9)),
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
        let first = run(machine(ScanConfig::for_peer(), None), &Peer::on(6), D::ZERO);
        let second = run(
            machine(ScanConfig::for_peer(), first.pinned),
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
        let r = run(machine(ScanConfig::for_peer(), Some(9)), &peer, D::ZERO);
        assert!(matches!(
            r.result,
            Err(ScanError::NotFound {
                probed: 12,
                configured: 13,
                not_allowed: 0,
                first_fault: Some(ScanFault::SetChannel { channel: 9 })
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
    fn zero_confirmations_settle_the_legacy_candidate_without_a_confirmation_probe() {
        // Adjusted for the probe reply: an ACK-only hit is settled after the sweep.
        let cfg = ScanConfig::for_peer().with_probe_confirmations(0);
        let r = run(machine(cfg, None), &Peer::legacy(3), D::ZERO);
        assert_eq!(r.result, Ok(ScanResult { channel: 3 }));
        assert_eq!(
            r.probes(),
            14,
            "13 sweep probes and one candidate probe, no confirmation probe"
        );
        assert_eq!(r.channels_set(), (1..=13).chain([3]).collect::<Vec<u8>>());
        assert!(!r.seen().iter().any(|s| matches!(s, Seen::Wait(_))));
    }

    #[test]
    fn confirmation_requires_every_extra_probe_and_waits_the_gap_each_time() {
        // Adjusted for the probe reply: confirmations run in the candidate phase of a legacy peer.
        let gap = D::from_millis(200);
        let cfg = ScanConfig::for_peer()
            .with_probe_confirmations(3)
            .with_confirmation_gap(gap);
        let r = run(machine(cfg, None), &Peer::legacy(1), D::ZERO);
        assert_eq!(r.result, Ok(ScanResult { channel: 1 }));
        assert_eq!(
            r.probes(),
            17,
            "13 sweep probes, the candidate probe and three confirmations"
        );
        let waits: Vec<_> = r
            .actions
            .iter()
            .filter_map(|(at, s)| match s {
                Seen::Wait(until) => Some(*until - *at),
                _ => None,
            })
            .collect();
        assert_eq!(waits, [gap, gap, gap]);
        // Gaps separate the candidate probes: each starts one reply wait plus one gap after
        // the previous ACK.
        let probe_times: Vec<D> = r
            .actions
            .iter()
            .filter(|(_, s)| matches!(s, Seen::Probe(_)))
            .map(|(t, _)| *t)
            .collect();
        for w in probe_times[probe_times.len() - 4..].windows(2) {
            assert_eq!(
                w[1] - w[0],
                D::from_millis(5) + super::super::DEFAULT_REPLY_TIMEOUT + gap
            );
        }
    }

    #[test]
    fn roaming_peer_that_acks_once_is_rejected_and_scan_moves_on() {
        // Adjusted for the probe reply: the single ACK is re-probed as the candidate.
        let peer = Peer {
            acks_before_silence: Some(1),
            ..Peer::legacy(6)
        };
        let r = run(machine(ScanConfig::for_peer(), None), &peer, D::ZERO);
        assert_eq!(
            r.result,
            Err(ScanError::NotFound {
                probed: 13,
                configured: 13,
                not_allowed: 0,
                first_fault: None
            }),
            "the candidate probe is not counted as a probed channel"
        );
        assert_eq!(r.channels_set(), (1..=13).chain([6]).collect::<Vec<u8>>());
        assert_eq!(r.pinned, None);
    }

    #[test]
    fn probe_actions_carry_channel_and_confirmation_index() {
        // Adjusted for the probe reply: the confirmations follow the sweep.
        let cfg = ScanConfig::for_peer().with_probe_confirmations(2);
        let r = run(machine(cfg, None), &Peer::legacy(2), D::ZERO);
        let expected: Vec<(u8, u8)> = (1..=13)
            .map(|c| (c, 0))
            .chain([(2, 0), (2, 1), (2, 2)])
            .collect();
        assert_eq!(r.probe_log, expected);
    }

    #[test]
    fn peer_unconfirmed_on_one_channel_is_found_on_a_later_one() {
        // Adjusted for the probe reply: the transit ACK on 3 becomes the candidate and the
        // reply on 8 wins before it is settled.
        let peer = Peer {
            roams_to: Some(8),
            ..Peer::on(3)
        };
        let r = run(machine(ScanConfig::for_peer(), None), &peer, D::ZERO);
        assert_eq!(r.result, Ok(ScanResult { channel: 8 }));
        assert_eq!(r.channels_set(), (1..=8).collect::<Vec<u8>>());
        assert_eq!(r.pinned, Some(8));
        assert_eq!(r.probe_log[r.probe_log.len() - 2..], [(7, 0), (8, 0)]);
        assert_eq!(r.seen().last(), Some(&Seen::Register(8)));
    }

    #[test]
    fn legacy_peer_that_roamed_after_its_first_ack_is_not_found_this_scan() {
        // The first ACK-only hit is the only candidate; the later home is left to a rescan.
        let peer = Peer {
            roams_to: Some(8),
            ..Peer::legacy(3)
        };
        let r = run(machine(ScanConfig::for_peer(), None), &peer, D::ZERO);
        assert_eq!(
            r.result,
            Err(ScanError::NotFound {
                probed: 13,
                configured: 13,
                not_allowed: 0,
                first_fault: None
            })
        );
        assert_eq!(r.channels_set(), (1..=13).chain([3]).collect::<Vec<u8>>());
        assert_eq!(r.pinned, None);
    }

    #[test]
    fn confirmation_send_error_rejects_the_channel() {
        // Adjusted for the probe reply: probe 0 is the sweep, 1 the candidate, 2 its confirmation.
        let peer = Peer {
            fail_send: Some(2),
            ..Peer::legacy(1)
        };
        let cfg = ScanConfig::for_peer().with_channels(&[1]);
        let r = run(machine(cfg, None), &peer, D::ZERO);
        assert_eq!(
            r.result,
            Err(ScanError::NotFound {
                probed: 1,
                configured: 1,
                not_allowed: 0,
                first_fault: Some(ScanFault::ProbeSend { channel: 1 })
            })
        );
    }

    #[test]
    fn reply_wait_and_confirmation_gap_are_added_to_the_event_time_not_the_epoch() {
        // Adjusted for the probe reply: the gap follows the reply wait of a candidate ACK.
        let cfg = ScanConfig::for_peer()
            .with_channels(&[1])
            .with_confirmation_gap(D::from_millis(150));
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
            ScanAction::AwaitReply {
                until: ack_at + D::from_millis(50)
            }
        );
        let timed_out = ack_at + D::from_millis(61);
        assert_eq!(
            m.poll(timed_out, ScanInput::Completed),
            ScanAction::SetChannel { channel: 1 },
            "list over: back to the candidate"
        );
        assert!(matches!(
            m.poll(timed_out, ScanInput::Completed),
            ScanAction::SendProbe { .. }
        ));
        let ack2_at = timed_out + D::from_millis(9);
        assert_eq!(
            m.poll(ack2_at, ScanInput::Sent(SendOutcome::Acked)),
            ScanAction::AwaitReply {
                until: ack2_at + D::from_millis(50)
            }
        );
        let timed_out2 = ack2_at + D::from_millis(50);
        assert_eq!(
            m.poll(timed_out2, ScanInput::Completed),
            ScanAction::WaitUntil(timed_out2 + D::from_millis(150))
        );
    }

    #[test]
    fn huge_clock_values_saturate_instead_of_overflowing() {
        let cfg = ScanConfig::for_peer();
        let mut m = machine(cfg, None);
        let now = D::MAX;
        m.poll(now, ScanInput::Start);
        m.poll(now, ScanInput::Completed);
        m.poll(now, ScanInput::Completed);
        m.poll(now, ScanInput::Completed);
        m.poll(now, ScanInput::Completed);
        assert_eq!(
            m.poll(now, ScanInput::Sent(SendOutcome::Acked)),
            ScanAction::AwaitReply { until: D::MAX }
        );
    }

    // ── burst timeout ─────────────────────────────────────────────────────

    #[test]
    fn burst_timeout_stops_before_the_next_channel() {
        let cfg = ScanConfig::for_peer()
            .with_probe_timeout(D::from_millis(100))
            .with_burst_timeout(D::from_millis(250));
        let r = run(machine(cfg, None), &Peer::absent(), D::ZERO);
        // Channels start at t=0, 100, 200; at t=300 the budget is spent.
        assert_eq!(r.channels_set(), [1, 2, 3]);
        assert_eq!(
            r.result,
            Err(ScanError::NotFound {
                probed: 3,
                configured: 13,
                not_allowed: 0,
                first_fault: None
            })
        );
        assert_eq!(r.end - D::from_secs(1000), D::from_millis(300));
    }

    #[test]
    fn burst_timeout_boundary_is_inclusive() {
        let cfg = ScanConfig::for_peer()
            .with_probe_timeout(D::from_millis(100))
            .with_burst_timeout(D::from_millis(200));
        let r = run(machine(cfg, None), &Peer::absent(), D::ZERO);
        assert_eq!(r.channels_set(), [1, 2], "elapsed == budget stops the scan");
    }

    #[test]
    fn zero_burst_timeout_probes_nothing() {
        let cfg = ScanConfig::for_peer().with_burst_timeout(D::ZERO);
        let r = run(machine(cfg, None), &Peer::on(1), D::ZERO);
        assert_eq!(
            r.result,
            Err(ScanError::NotFound {
                probed: 0,
                configured: 13,
                not_allowed: 0,
                first_fault: None
            })
        );
        assert_eq!(r.probes(), 0);
        assert!(r.channels_set().is_empty());
    }

    #[test]
    fn burst_budget_does_not_interrupt_confirmation_of_a_channel() {
        let cfg = ScanConfig::for_peer().with_burst_timeout(D::from_millis(1));
        let r = run(machine(cfg, None), &Peer::on(1), D::ZERO);
        assert_eq!(r.result, Ok(ScanResult { channel: 1 }));
    }

    #[test]
    fn burst_clock_starts_after_registration_and_boost() {
        // Slow driver operations before the loop must not eat into the burst budget.
        let cfg = ScanConfig::for_peer()
            .with_probe_timeout(D::from_millis(100))
            .with_burst_timeout(D::from_millis(150));
        let r = run(machine(cfg, None), &Peer::absent(), D::from_millis(40));
        assert_eq!(r.channels_set(), [1, 2]);
    }

    // ── channel lists ─────────────────────────────────────────────────────

    #[test]
    fn custom_channel_list_is_probed_in_order() {
        let list = [11, 1, 6];
        let cfg = ScanConfig::for_peer().with_channels(&list);
        let r = run(machine(cfg, None), &Peer::on(6), D::ZERO);
        assert_eq!(r.result, Ok(ScanResult { channel: 6 }));
        assert_eq!(r.channels_set(), [11, 1, 6]);
    }

    #[test]
    fn peer_outside_custom_list_is_not_found() {
        let list = [1, 6, 11];
        let cfg = ScanConfig::for_peer().with_channels(&list);
        let r = run(machine(cfg, None), &Peer::on(5), D::ZERO);
        assert_eq!(
            r.result,
            Err(ScanError::NotFound {
                probed: 3,
                configured: 3,
                not_allowed: 0,
                first_fault: None
            })
        );
    }

    #[test]
    fn empty_channel_list_is_not_found_without_probing() {
        let cfg = ScanConfig::for_peer().with_channels(&[]);
        let r = run(machine(cfg, None), &Peer::on(1), D::ZERO);
        assert_eq!(
            r.result,
            Err(ScanError::NotFound {
                probed: 0,
                configured: 0,
                not_allowed: 0,
                first_fault: None
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
        let r = run(machine(ScanConfig::for_peer(), None), &peer, D::ZERO);
        assert_eq!(r.result, Ok(ScanResult { channel: 3 }));
        assert_eq!(r.channels_set(), [1, 2, 3]);
        // Adjusted for the probe reply: one probe, the reply replaces the confirmation.
        assert_eq!(r.probes(), 1, "no probe on the refused channels");

        let peer = Peer {
            refuse: alloc::vec![12, 13],
            ..Peer::absent()
        };
        let r = run(machine(ScanConfig::for_peer(), None), &peer, D::ZERO);
        assert_eq!(
            r.result,
            Err(ScanError::NotFound {
                probed: 11,
                configured: 13,
                not_allowed: 0,
                first_fault: Some(ScanFault::SetChannel { channel: 12 })
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
        let cfg = ScanConfig::for_peer().with_channels(&list);
        let r = run(machine(cfg, None), &peer, D::ZERO);
        assert_eq!(
            r.result,
            Err(ScanError::NotFound {
                probed: 1,
                configured: 2,
                not_allowed: 0,
                first_fault: Some(ScanFault::ProbeSend { channel: 1 })
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
        let r = run(machine(ScanConfig::for_peer(), Some(4)), &peer, D::ZERO);
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
        let r = run(machine(ScanConfig::for_peer(), None), &peer, D::ZERO);
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
        let r = run(machine(ScanConfig::for_peer(), Some(4)), &peer, D::ZERO);
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
        let r = run(machine(ScanConfig::for_peer(), Some(2)), &peer, D::ZERO);
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
            let mut m = machine(ScanConfig::for_peer(), None);
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
            let r = run(machine(ScanConfig::for_peer(), None), &peer, D::ZERO);
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
        let mut m = machine(ScanConfig::for_peer().with_channels(&[1]), None);
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

    // ── TX power read-back ────────────────────────────────────────────────

    #[test]
    fn restore_matches_accepts_an_equal_read_back() {
        assert!(restore_matches(Some(78), Some(78)));
    }

    #[test]
    fn restore_matches_rejects_a_level_left_boosted() {
        assert!(!restore_matches(Some(34), Some(80)));
    }

    #[test]
    fn restore_matches_has_nothing_to_verify_without_a_saved_or_read_level() {
        assert!(restore_matches(None, Some(80)));
        assert!(restore_matches(Some(34), None));
        assert!(restore_matches(None, None));
    }

    // ── burst expiry flag ─────────────────────────────────────────────────

    #[test]
    fn burst_expired_is_set_only_when_channels_were_left_unprobed() {
        let cfg = ScanConfig::for_peer()
            .with_probe_timeout(D::from_millis(100))
            .with_burst_timeout(D::from_millis(250));
        let mut m = machine(cfg, None);
        let r = run(m.clone(), &Peer::absent(), D::ZERO);
        assert_eq!(r.channels_set(), [1, 2, 3]);
        drive_to_done(&mut m, D::from_millis(100));
        assert!(m.burst_expired());
        assert_eq!(m.probed(), 3);

        let mut m = machine(ScanConfig::for_peer().with_channels(&[1]), None);
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
        let mut m = machine(ScanConfig::for_peer(), Some(9));
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
        let mut m = machine(ScanConfig::for_peer(), None);
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

    /// Feeds `Abort` at the point where the scan is in `target` teardown state, returning the
    /// actions that follow until `Done`.
    fn actions_after_abort(mut m: ScanMachine<'_>, steps_before: usize) -> Vec<ScanAction> {
        let mut out = Vec::new();
        let mut action = m.poll(D::ZERO, ScanInput::Start);
        for _ in 0..steps_before {
            action = m.poll(D::ZERO, ScanInput::Completed);
        }
        out.push(action);
        loop {
            action = m.poll(D::ZERO, ScanInput::Abort);
            out.push(action);
            if matches!(action, ScanAction::Done(_)) {
                return out;
            }
        }
    }

    #[test]
    fn abort_during_the_register_temp_peer_fallback_still_registers_the_fallback_peer() {
        let mut m = machine(ScanConfig::for_peer(), Some(9));
        m.poll(D::ZERO, ScanInput::Start);
        m.poll(D::ZERO, ScanInput::Completed);
        assert_eq!(
            m.poll(D::ZERO, ScanInput::Failed),
            ScanAction::RestoreChannel { channel: 9 }
        );
        assert_eq!(
            m.poll(D::ZERO, ScanInput::Abort),
            ScanAction::RegisterFallbackPeer { channel: 9 }
        );
        assert_eq!(
            m.poll(D::ZERO, ScanInput::Abort),
            ScanAction::Done(Err(ScanError::RegisterTempPeer))
        );
    }

    #[test]
    fn abort_in_every_teardown_state_continues_the_teardown() {
        // Pinned, aborted while boosting: every later step is fed `Abort`.
        let seen = actions_after_abort(machine(ScanConfig::for_peer(), Some(9)), 2);
        assert_eq!(
            seen,
            [
                ScanAction::BoostTxPower,
                ScanAction::RestoreTxPower,
                ScanAction::RemovePeer,
                ScanAction::RestoreChannel { channel: 9 },
                ScanAction::RegisterFallbackPeer { channel: 9 },
                ScanAction::Done(Err(ScanError::Aborted)),
            ]
        );
        // Unpinned: no channel to restore.
        let seen = actions_after_abort(machine(ScanConfig::for_peer(), None), 2);
        assert_eq!(
            seen,
            [
                ScanAction::BoostTxPower,
                ScanAction::RestoreTxPower,
                ScanAction::RemovePeer,
                ScanAction::Done(Err(ScanError::Aborted)),
            ]
        );
    }

    #[test]
    fn poll_bounded_aborts_at_max_polls() {
        let drive_to_probe = |m: &mut ScanMachine<'_>| {
            let mut action = m.poll_bounded(D::ZERO, ScanInput::Start).unwrap();
            while !matches!(action, ScanAction::SendProbe { .. }) {
                action = m.poll_bounded(D::ZERO, ScanInput::Completed).unwrap();
            }
        };
        let mut before = machine(ScanConfig::for_peer(), None);
        drive_to_probe(&mut before);
        before.polls = before.max_polls() - 1;
        assert_eq!(
            before.poll_bounded(D::ZERO, ScanInput::Sent(SendOutcome::NoAck)),
            Ok(ScanAction::SetChannel { channel: 2 })
        );

        let mut at = machine(ScanConfig::for_peer(), None);
        drive_to_probe(&mut at);
        at.polls = at.max_polls();
        assert_eq!(
            at.poll_bounded(D::ZERO, ScanInput::Sent(SendOutcome::NoAck)),
            Ok(ScanAction::RestoreTxPower)
        );
    }

    #[test]
    fn poll_bounded_gives_up_after_the_teardown_budget() {
        let mut m = machine(ScanConfig::for_peer(), None);
        let allowed = m.max_polls() + ScanTeardown::MAX_POLLS;
        let mut ok = 0;
        let mut result = Ok(ScanAction::Ignored);
        for _ in 0..allowed + 10 {
            result = m.poll_bounded(D::ZERO, ScanInput::Completed);
            if result.is_err() {
                break;
            }
            ok += 1;
        }
        assert_eq!(result, Err(ScanError::UnexpectedInput));
        assert_eq!(ok, allowed);
    }

    #[test]
    fn poll_bounded_leaves_a_valid_scan_alone() {
        let mut m = machine(ScanConfig::for_peer(), None);
        let mut input = ScanInput::Start;
        let mut polls = 0;
        loop {
            polls += 1;
            assert!(polls <= m.max_polls());
            match m.poll_bounded(D::ZERO, input).unwrap() {
                ScanAction::Done(result) => {
                    assert!(matches!(result, Err(ScanError::NotFound { .. })));
                    break;
                }
                ScanAction::SendProbe { .. } => input = ScanInput::Sent(SendOutcome::NoAck),
                _ => input = ScanInput::Completed,
            }
        }
    }

    #[test]
    fn abort_after_the_peer_was_found_keeps_the_new_pin() {
        let mut config = ScanConfig::for_peer();
        config.probe_confirmations = 0;
        let mut m = machine(config, Some(9));
        m.poll(D::ZERO, ScanInput::Start);
        m.poll(D::ZERO, ScanInput::Completed);
        m.poll(D::ZERO, ScanInput::Completed);
        assert_eq!(
            m.poll(D::ZERO, ScanInput::Completed),
            ScanAction::SetChannel { channel: 1 }
        );
        m.poll(D::ZERO, ScanInput::Completed);
        // Adjusted for the probe reply: the ACK alone no longer finishes, the reply does.
        let probe = ProbeFrame::parse(&m.probe_frame(1)).unwrap();
        assert!(matches!(
            m.poll(D::ZERO, ScanInput::Sent(SendOutcome::Acked)),
            ScanAction::AwaitReply { .. }
        ));
        assert_eq!(
            m.poll(D::ZERO, ScanInput::Reply(ProbeReply::answering(&probe, 1))),
            ScanAction::RestoreTxPower
        );
        assert_eq!(m.poll(D::ZERO, ScanInput::Abort), ScanAction::RemovePeer);
        assert_eq!(
            m.poll(D::ZERO, ScanInput::Abort),
            ScanAction::RegisterPeer { channel: 1 }
        );
        assert_eq!(
            m.poll(D::ZERO, ScanInput::Abort),
            ScanAction::Done(Ok(ScanResult { channel: 1 }))
        );
        assert_eq!(m.pinned_channel(), Some(1));
    }

    #[test]
    fn or_abort_replaces_every_result_but_failed() {
        assert_eq!(ScanInput::Completed.or_abort(false), ScanInput::Completed);
        assert_eq!(ScanInput::Completed.or_abort(true), ScanInput::Abort);
        assert_eq!(
            ScanInput::Sent(SendOutcome::Acked).or_abort(true),
            ScanInput::Abort
        );
        assert_eq!(ScanInput::Failed.or_abort(true), ScanInput::Failed);
        assert_eq!(ScanInput::Shutdown.or_abort(true), ScanInput::Shutdown);
    }

    #[test]
    fn abort_before_start_finishes_immediately() {
        let mut m = machine(ScanConfig::for_peer(), None);
        assert_eq!(
            m.poll(D::ZERO, ScanInput::Abort),
            ScanAction::Done(Err(ScanError::Aborted))
        );
    }

    #[test]
    fn unexpected_input_mid_scan_unwinds_through_teardown() {
        let mut m = machine(ScanConfig::for_peer(), Some(4));
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
        let mut m = machine(ScanConfig::for_peer().with_channels(&[]), None);
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

        // With the TX power still boosted, the terminal restore step comes first.
        let mut t = ScanTeardown::new(
            MAC,
            Step::RemovingScanPeer { found: None },
            None,
            ScanError::Aborted,
        );
        t.boosted = true;
        assert_eq!(
            t.poll(ScanInput::Sent(SendOutcome::Acked)),
            ScanAction::RestoreTxPower
        );
        assert!(!t.is_done());
        assert_eq!(
            t.poll(ScanInput::Completed),
            ScanAction::Done(Err(ScanError::UnexpectedInput))
        );
        assert!(t.is_done());
    }

    /// Drives a fresh machine with plain answers (probes not ACKed unless on `home`), feeding
    /// `disturbance` in place of the answer at poll `at`; returns every action up to `Done`.
    fn disturbed(
        cfg: ScanConfig<'_>,
        pin: Option<u8>,
        home: Option<u8>,
        at: usize,
        disturbance: ScanInput,
    ) -> Vec<ScanAction> {
        let mut m = machine(cfg, pin);
        let mut out = Vec::new();
        let mut input = ScanInput::Start;
        let mut channel = 0;
        for poll in 0..1_000 {
            if poll == at {
                input = disturbance;
            }
            let action = m.poll(D::ZERO, input);
            out.push(action);
            input = match action {
                ScanAction::Done(_) => return out,
                ScanAction::SetChannel { channel: c } => {
                    channel = c;
                    ScanInput::Completed
                }
                ScanAction::SendProbe { .. } if home == Some(channel) => {
                    ScanInput::Sent(SendOutcome::Acked)
                }
                ScanAction::SendProbe { .. } => ScanInput::Sent(SendOutcome::NoAck),
                _ => ScanInput::Completed,
            };
        }
        panic!("scan did not finish");
    }

    #[test]
    fn tx_power_restored_before_every_done() {
        let list = [1, 6];
        let disturbances = [
            ScanInput::Abort,
            ScanInput::Failed,
            ScanInput::Start,
            ScanInput::Sent(SendOutcome::Acked),
        ];
        for pin in [None, Some(9)] {
            for home in [None, Some(6)] {
                for at in 0..20 {
                    for disturbance in disturbances {
                        let cfg = ScanConfig::for_peer().with_channels(&list);
                        let seen = disturbed(cfg, pin, home, at, disturbance);
                        let case = (pin, home, at, disturbance);
                        assert!(matches!(seen.last(), Some(ScanAction::Done(_))), "{case:?}");
                        let boosts = seen
                            .iter()
                            .filter(|a| **a == ScanAction::BoostTxPower)
                            .count();
                        let restores = seen
                            .iter()
                            .filter(|a| **a == ScanAction::RestoreTxPower)
                            .count();
                        assert!(boosts <= 1, "{case:?}: {seen:?}");
                        assert!(restores <= 1, "{case:?}: {seen:?}");
                        if let Some(boost) =
                            seen.iter().position(|a| *a == ScanAction::BoostTxPower)
                        {
                            let restore = seen
                                .iter()
                                .position(|a| *a == ScanAction::RestoreTxPower)
                                .unwrap_or_else(|| panic!("{case:?}: no restore in {seen:?}"));
                            assert!(boost < restore, "{case:?}: {seen:?}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn shutdown_finishes_at_once_without_teardown() {
        let list = [1, 6];
        for pin in [None, Some(9)] {
            for home in [None, Some(6)] {
                let cfg = ScanConfig::for_peer().with_channels(&list);
                let undisturbed = disturbed(cfg, pin, home, usize::MAX, ScanInput::Start);
                for at in 0..undisturbed.len() {
                    let seen = disturbed(cfg, pin, home, at, ScanInput::Shutdown);
                    let case = (pin, home, at);
                    // Everything before the shutdown is the plain scan; the shutdown itself
                    // yields `Done` with no further action.
                    assert_eq!(seen[..at], undisturbed[..at], "{case:?}");
                    assert_eq!(
                        seen[at..],
                        [ScanAction::Done(Err(ScanError::Aborted))],
                        "{case:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn shutdown_after_done_is_ignored() {
        let mut m = machine(ScanConfig::for_peer().with_channels(&[1]), None);
        let result = drive_with(&mut m, |a| {
            matches!(a, ScanAction::SendProbe { .. }).then_some(ScanInput::Sent(SendOutcome::NoAck))
        });
        let pin = m.pinned_channel();
        assert_eq!(m.poll(D::ZERO, ScanInput::Shutdown), ScanAction::Ignored);
        assert_eq!(m.pinned_channel(), pin);
        assert!(result.is_err());
    }

    #[test]
    fn shutdown_before_start_finishes_and_keeps_the_pin() {
        let mut m = machine(ScanConfig::for_peer(), Some(4));
        assert_eq!(
            m.poll(D::ZERO, ScanInput::Shutdown),
            ScanAction::Done(Err(ScanError::Aborted))
        );
        assert_eq!(m.pinned_channel(), Some(4));
        assert_eq!(m.poll(D::ZERO, ScanInput::Completed), ScanAction::Ignored);
    }

    #[test]
    fn every_scripted_scan_finishes_within_max_polls() {
        let list = [11, 1, 6];
        let configs = [
            ScanConfig::for_peer(),
            ScanConfig::for_peer().with_channels(&list),
            ScanConfig::for_peer().with_probe_confirmations(0),
            ScanConfig::for_peer().with_probe_confirmations(3),
            ScanConfig::for_peer()
                .with_channels(&list)
                .with_probe_confirmations(2),
        ];
        let peers = || {
            [
                Peer::absent(),
                Peer::on(1),
                Peer::on(6),
                Peer::on(13),
                Peer::legacy(1),
                Peer::legacy(6),
                Peer::legacy(13),
                // The found peer's registration fails: the longest teardown.
                Peer {
                    fail_register: Some(1),
                    ..Peer::on(13)
                },
                Peer {
                    fail_register: Some(1),
                    ..Peer::legacy(13)
                },
                Peer {
                    fail_register: Some(1),
                    ..Peer::on(6)
                },
                Peer {
                    fail_register: Some(0),
                    ..Peer::on(1)
                },
                Peer {
                    roams_to: Some(6),
                    ..Peer::on(1)
                },
                Peer {
                    roams_to: Some(6),
                    ..Peer::legacy(1)
                },
                // Adjacent-channel ACKs, with and without a decodable reply.
                Peer {
                    acks_also_on: alloc::vec![10, 12],
                    ..Peer::on(11)
                },
                Peer {
                    acks_also_on: alloc::vec![10, 12],
                    decodes_reply_on: alloc::vec![10],
                    ..Peer::on(11)
                },
                Peer {
                    acks_also_on: alloc::vec![10, 12],
                    decodes_reply_on: alloc::vec![10],
                    fail_register: Some(1),
                    ..Peer::on(11)
                },
                Peer {
                    acks_also_on: alloc::vec![10, 12],
                    ..Peer::legacy(11)
                },
                Peer {
                    acks_also_on: alloc::vec![12, 13],
                    decodes_reply_on: alloc::vec![13],
                    ..Peer::on(11)
                },
            ]
        };
        for cfg in configs {
            for pin in [None, Some(9)] {
                for peer in peers() {
                    let r = run(machine(cfg, pin), &peer, D::ZERO);
                    assert!(
                        r.polls <= r.max_polls,
                        "{} polls > max {} ({:?})",
                        r.polls,
                        r.max_polls,
                        r.result
                    );
                }
            }
        }
    }

    // ── abort teardown ────────────────────────────────────────────────────

    /// Drives a teardown with `Abort` until `Done` (or `Ignored`, for a finished one).
    fn drive_teardown(mut t: ScanTeardown) -> Vec<ScanAction> {
        let mut out = Vec::new();
        for _ in 0..ScanTeardown::MAX_POLLS {
            let action = t.poll(ScanInput::Abort);
            out.push(action);
            if matches!(action, ScanAction::Done(_) | ScanAction::Ignored) {
                return out;
            }
        }
        panic!("teardown exceeded MAX_POLLS: {out:?}");
    }

    #[test]
    fn abort_teardown_matches_abort_unwind() {
        let list = [1, 6];
        for pin in [None, Some(9)] {
            for home in [None, Some(6)] {
                for steps in 0..20 {
                    let cfg = ScanConfig::for_peer().with_channels(&list);
                    let mut m = machine(cfg, pin);
                    let mut channel = 0;
                    let mut input = ScanInput::Start;
                    let mut finished = false;
                    for _ in 0..steps {
                        let action = m.poll(D::ZERO, input);
                        input = match action {
                            ScanAction::Done(_) => {
                                finished = true;
                                break;
                            }
                            ScanAction::SetChannel { channel: c } => {
                                channel = c;
                                ScanInput::Completed
                            }
                            ScanAction::SendProbe { .. } if home == Some(channel) => {
                                ScanInput::Sent(SendOutcome::Acked)
                            }
                            ScanAction::SendProbe { .. } => ScanInput::Sent(SendOutcome::NoAck),
                            _ => ScanInput::Completed,
                        };
                    }
                    let snapshot = m.abort_teardown();
                    let mut via_machine = Vec::new();
                    // Feeding `Abort` to the machine in place of the pending result.
                    let mut machine_copy = m.clone();
                    for _ in 0..ScanTeardown::MAX_POLLS {
                        let action = machine_copy.poll(D::ZERO, ScanInput::Abort);
                        via_machine.push(action);
                        if matches!(action, ScanAction::Done(_) | ScanAction::Ignored) {
                            break;
                        }
                    }
                    let via_teardown = drive_teardown(snapshot);
                    assert_eq!(
                        via_teardown, via_machine,
                        "pin {pin:?} home {home:?} steps {steps} finished {finished}"
                    );
                    assert_eq!(snapshot.pinned_channel(), m.pinned_channel());
                }
            }
        }
    }

    #[test]
    fn abort_teardown_before_start_is_empty() {
        let m = machine(ScanConfig::for_peer(), Some(9));
        let t = m.abort_teardown();
        assert!(!t.is_done());
        assert_eq!(t.mac(), &MAC);
        assert_eq!(t.pinned_channel(), Some(9));
        assert_eq!(
            drive_teardown(t),
            [ScanAction::Done(Err(ScanError::Aborted))]
        );
    }

    #[test]
    fn abort_teardown_between_stale_remove_and_temp_register_re_registers_the_pin() {
        let mut m = machine(ScanConfig::for_peer(), Some(9));
        assert_eq!(m.poll(D::ZERO, ScanInput::Start), ScanAction::RemovePeer);
        // The caller's registration on channel 9 is gone; the teardown must put it back.
        assert_eq!(
            drive_teardown(m.abort_teardown()),
            [
                ScanAction::RestoreTxPower,
                ScanAction::RemovePeer,
                ScanAction::RestoreChannel { channel: 9 },
                ScanAction::RegisterFallbackPeer { channel: 9 },
                ScanAction::Done(Err(ScanError::Aborted)),
            ]
        );
        assert_eq!(
            m.poll(D::ZERO, ScanInput::Completed),
            ScanAction::RegisterPeer { channel: 0 }
        );
        assert_eq!(
            drive_teardown(m.abort_teardown())[2..4],
            [
                ScanAction::RestoreChannel { channel: 9 },
                ScanAction::RegisterFallbackPeer { channel: 9 },
            ]
        );
    }

    #[test]
    fn a_teardown_copy_runs_independently_of_the_machine() {
        let mut m = machine(ScanConfig::for_peer().with_channels(&[1]), None);
        m.poll(D::ZERO, ScanInput::Start);
        m.poll(D::ZERO, ScanInput::Completed);
        m.poll(D::ZERO, ScanInput::Completed);
        let snapshot = m.abort_teardown();
        // The machine runs to its end; the copy still unwinds from the earlier point.
        drive_with_from(&mut m, ScanInput::Completed);
        assert!(m.abort_teardown().is_done());
        assert_eq!(
            drive_teardown(snapshot),
            [
                ScanAction::RestoreTxPower,
                ScanAction::RemovePeer,
                ScanAction::Done(Err(ScanError::Aborted)),
            ]
        );
    }

    // ── protocol misuse ───────────────────────────────────────────────────

    #[test]
    fn input_before_start_is_rejected() {
        let mut m = machine(ScanConfig::for_peer(), None);
        assert_eq!(
            m.poll(D::ZERO, ScanInput::Completed),
            ScanAction::Done(Err(ScanError::UnexpectedInput))
        );
    }

    #[test]
    fn done_is_sticky() {
        let list = [1];
        let cfg = ScanConfig::for_peer().with_channels(&list);
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
        let pin = m.pinned_channel();
        // ADR 023 decision 5: every input after `Done` is an explicit no-op.
        for input in [
            ScanInput::Start,
            ScanInput::Completed,
            ScanInput::Failed,
            ScanInput::Sent(SendOutcome::Acked),
            ScanInput::Reply(ProbeReply {
                seq: 1,
                tx_channel: 1,
                home_channel: 1,
            }),
            ScanInput::Abort,
            ScanInput::Shutdown,
        ] {
            assert_eq!(m.poll(D::ZERO, input), ScanAction::Ignored, "{input:?}");
        }
        assert_eq!(m.pinned_channel(), pin);
        assert!(m.abort_teardown().is_done());
    }

    #[test]
    fn a_finished_teardown_ignores_every_input() {
        let mut m = machine(ScanConfig::for_peer().with_channels(&[1]), Some(3));
        m.poll(D::ZERO, ScanInput::Start);
        let mut t = m.abort_teardown();
        let actions = drive_teardown(t);
        assert_eq!(
            actions.last(),
            Some(&ScanAction::Done(Err(ScanError::Aborted)))
        );
        for _ in 0..actions.len() {
            t.poll(ScanInput::Abort);
        }
        assert!(t.is_done());
        let pin = t.pinned_channel();
        for input in [
            ScanInput::Start,
            ScanInput::Completed,
            ScanInput::Failed,
            ScanInput::Abort,
            ScanInput::Shutdown,
        ] {
            assert_eq!(t.poll(input), ScanAction::Ignored, "{input:?}");
        }
        assert_eq!(t.pinned_channel(), pin);
    }

    // ── accessors and Display ─────────────────────────────────────────────

    #[test]
    fn accessors_expose_peer_probe_and_pin() {
        let m = machine(ScanConfig::for_peer(), Some(7));
        assert_eq!(m.mac(), &MAC);
        assert_eq!(
            m.probe_frame(6),
            ProbeFrame {
                seq: 0,
                tx_channel: 6
            }
            .encode()
        );
        assert_eq!(m.pinned_channel(), Some(7));
    }

    /// Drive `m` to completion; `step` answers an action, `None` means `Completed`.
    fn drive_with(
        m: &mut ScanMachine<'_>,
        mut step: impl FnMut(&ScanAction) -> Option<ScanInput>,
    ) -> Result<ScanResult, ScanError> {
        let mut input = ScanInput::Start;
        for _ in 0..1_000 {
            let action = m.poll(D::ZERO, input);
            if let ScanAction::Done(result) = action {
                return result;
            }
            input = step(&action).unwrap_or(ScanInput::Completed);
        }
        panic!("scan did not finish");
    }

    /// Drive `m` from its current state to completion, starting with `input`.
    fn drive_with_from(m: &mut ScanMachine<'_>, mut input: ScanInput) {
        for _ in 0..1_000 {
            input = match m.poll(D::ZERO, input) {
                ScanAction::Done(_) => return,
                ScanAction::SendProbe { .. } => ScanInput::Sent(SendOutcome::NoAck),
                _ => ScanInput::Completed,
            };
        }
        panic!("scan did not finish");
    }

    #[test]
    fn first_fault_is_none_when_every_channel_just_did_not_ack() {
        let mut m = machine(ScanConfig::for_peer(), None);
        let r = drive_with(&mut m, |a| {
            matches!(a, ScanAction::SendProbe { .. }).then_some(ScanInput::Sent(SendOutcome::NoAck))
        });
        assert!(matches!(
            r,
            Err(ScanError::NotFound {
                probed: 13,
                first_fault: None,
                ..
            })
        ));
        assert_eq!(m.first_fault(), None);
    }

    #[test]
    fn first_fault_records_a_failed_set_channel() {
        let list = [1, 6];
        let cfg = ScanConfig::for_peer().with_channels(&list);
        let mut m = machine(cfg, None);
        let r = drive_with(&mut m, |a| match a {
            ScanAction::SetChannel { .. } => Some(ScanInput::Failed),
            _ => None,
        });
        assert_eq!(
            r,
            Err(ScanError::NotFound {
                probed: 0,
                configured: 2,
                not_allowed: 0,
                first_fault: Some(ScanFault::SetChannel { channel: 1 })
            })
        );
        assert_eq!(m.first_fault(), Some(ScanFault::SetChannel { channel: 1 }));
    }

    #[test]
    fn first_fault_records_a_probe_send_error() {
        let list = [1, 6];
        let cfg = ScanConfig::for_peer().with_channels(&list);
        let mut m = machine(cfg, None);
        let r = drive_with(&mut m, |a| {
            matches!(a, ScanAction::SendProbe { .. }).then_some(ScanInput::Sent(SendOutcome::Local))
        });
        assert_eq!(
            r,
            Err(ScanError::NotFound {
                probed: 0,
                configured: 2,
                not_allowed: 0,
                first_fault: Some(ScanFault::ProbeSend { channel: 1 })
            })
        );
    }

    #[test]
    fn first_fault_records_a_confirmation_probe_error() {
        let list = [6];
        let cfg = ScanConfig::for_peer()
            .with_channels(&list)
            .with_probe_confirmations(1);
        let mut m = machine(cfg, None);
        let r = drive_with(&mut m, |a| match a {
            ScanAction::SendProbe {
                confirmation: 0, ..
            } => Some(ScanInput::Sent(SendOutcome::Acked)),
            ScanAction::SendProbe { .. } => Some(ScanInput::Sent(SendOutcome::Local)),
            _ => None,
        });
        assert_eq!(
            r,
            Err(ScanError::NotFound {
                probed: 1,
                configured: 1,
                not_allowed: 0,
                first_fault: Some(ScanFault::ProbeSend { channel: 6 })
            })
        );
    }

    #[test]
    fn only_the_first_fault_is_kept() {
        let list = [1, 6, 11];
        let cfg = ScanConfig::for_peer().with_channels(&list);
        let mut m = machine(cfg, None);
        let r = drive_with(&mut m, |a| match a {
            ScanAction::SetChannel { channel: 1 } => Some(ScanInput::Failed),
            ScanAction::SendProbe { .. } => Some(ScanInput::Sent(SendOutcome::Local)),
            _ => None,
        });
        let first = Some(ScanFault::SetChannel { channel: 1 });
        assert_eq!(m.first_fault(), first);
        assert_eq!(
            r,
            Err(ScanError::NotFound {
                probed: 0,
                configured: 3,
                not_allowed: 0,
                first_fault: first
            })
        );
    }

    #[test]
    fn scan_error_display_is_informative() {
        use alloc::string::ToString;
        assert_eq!(
            ScanError::NotFound {
                probed: 3,
                configured: 13,
                not_allowed: 0,
                first_fault: None
            }
            .to_string(),
            "peer not found after probing 3 of 13 configured channels"
        );
        assert_eq!(
            ScanError::NotFound {
                probed: 0,
                configured: 13,
                not_allowed: 0,
                first_fault: Some(ScanFault::ProbeSend { channel: 1 })
            }
            .to_string(),
            "peer not found after probing 0 of 13 configured channels \
             (first failure: probe send on channel 1)"
        );
        assert!(ScanError::NotFound {
            probed: 0,
            configured: 13,
            not_allowed: 0,
            first_fault: Some(ScanFault::SetChannel { channel: 4 })
        }
        .to_string()
        .ends_with("(first failure: set channel 4)"));
        assert_eq!(
            ScanError::NotFound {
                probed: 11,
                configured: 13,
                not_allowed: 2,
                first_fault: None
            }
            .to_string(),
            "peer not found after probing 11 of 13 configured channels \
             (2 not allowed by the country)"
        );
        assert_eq!(
            ScanError::HomeNotAllowed { home: 13 }.to_string(),
            "peer replied with home channel 13, which the country does not allow"
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
