//! ESP-NOW command frame parsing — tag-byte envelope with zero-copy payload.
//!
//! Every ESP-NOW command is a single-byte tag followed by an optional payload.
//! Tags are partitioned into two ranges:
//!
//! - **System tags** (`0xF0..=0xFF`) — reserved for infrastructure commands
//!   (`Ping`, `SelfTest`, `Identify`) and the scan probe pair
//!   ([`TAG_PROBE`] / [`TAG_PROBE_REPLY`], see [`ProbeFrame`] and
//!   [`ProbeReply`]). Unrecognised system tags parse as
//!   [`SystemCommand::Unknown`] and are reserved for future use.
//! - **Module tags** (`0x01..=0xEF`) — available for application-specific
//!   commands defined by each peripheral module.
//! - Tag `0x00` is reserved and never used. [`parse_frame`] still accepts
//!   it so lower-level inspection works, but both [`is_system_tag`] and
//!   [`is_module_tag`] return `false`; callers should reject it.
//!
//! `CommandFrame` borrows from the payload slice — no heap, no allocator.
//! The sender MAC address is intentionally excluded (ADR 010); transport
//! metadata is threaded as a sidecar at the dispatch site.

/// A parsed command frame borrowing from a raw ESP-NOW payload.
///
/// The frame is a single tag byte followed by zero or more payload bytes.
/// Use [`parse_frame`] to construct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandFrame<'a> {
    /// Command tag identifying the operation.
    pub tag: u8,
    /// Payload bytes following the tag (may be empty).
    pub payload: &'a [u8],
}

/// Parses a raw ESP-NOW payload into a [`CommandFrame`].
///
/// Returns `None` if `data` is empty (if no tag byte is present).
pub fn parse_frame(data: &[u8]) -> Option<CommandFrame<'_>> {
    let (&tag, payload) = data.split_first()?;
    Some(CommandFrame { tag, payload })
}

/// Returns `true` if `tag` falls in the system range (`0xF0..=0xFF`).
pub fn is_system_tag(tag: u8) -> bool {
    tag >= 0xF0
}

/// Returns `true` if `tag` falls in the module range (`0x01..=0xEF`).
pub fn is_module_tag(tag: u8) -> bool {
    (0x01..=0xEF).contains(&tag)
}

// ─── System commands ───────────────────────────────────────────────────────

/// System-level command tags.
pub const TAG_PING: u8 = 0xF0;
/// System-level command tag for self-test.
pub const TAG_SELF_TEST: u8 = 0xF1;
/// System-level command tag for identify.
pub const TAG_IDENTIFY: u8 = 0xF2;
/// System-level command tag for a scan probe (ADR 025, see [`ProbeFrame`]).
pub const TAG_PROBE: u8 = 0xF3;
/// System-level command tag for a scan probe reply (ADR 025, see [`ProbeReply`]).
pub const TAG_PROBE_REPLY: u8 = 0xF4;

/// A parsed system command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemCommand {
    /// Health probe — peer responds with a pong.
    Ping,
    /// Request a self-test result from the peripheral.
    SelfTest,
    /// Request module type and version identification.
    Identify,
    /// An unrecognised system tag (reserved for future use).
    Unknown(u8),
}

/// Parses a system command from a [`CommandFrame`].
///
/// Returns `None` if the frame's tag is not in the system range.
pub fn parse_system_command(frame: &CommandFrame<'_>) -> Option<SystemCommand> {
    if !is_system_tag(frame.tag) {
        return None;
    }
    Some(match frame.tag {
        TAG_PING => SystemCommand::Ping,
        TAG_SELF_TEST => SystemCommand::SelfTest,
        TAG_IDENTIFY => SystemCommand::Identify,
        other => SystemCommand::Unknown(other),
    })
}

// ─── Response helpers ──────────────────────────────────────────────────────

/// Pong response payload: tag `0xF0`, status `0x01`.
pub const PONG_RESPONSE: [u8; 2] = [TAG_PING, 0x01];

/// Self-test pass response: tag `0xF1`, result `0x00`.
pub const SELF_TEST_PASS: [u8; 2] = [TAG_SELF_TEST, 0x00];

/// Self-test fail response: tag `0xF1`, result `0x01`.
pub const SELF_TEST_FAIL: [u8; 2] = [TAG_SELF_TEST, 0x01];

/// Builds an identify response: tag `0xF2` + module type + version.
pub fn identify_response(module_type: u8, version: u8) -> [u8; 3] {
    [TAG_IDENTIFY, module_type, version]
}

// ─── Scan probe and reply ──────────────────────────────────────────────────

/// Encoded length of a [`ProbeFrame`] (tag, `seq`, `tx_channel`).
pub const PROBE_FRAME_LEN: usize = 3;
/// Encoded length of a [`ProbeReply`] (tag, `seq`, `tx_channel`, `home_channel`).
pub const PROBE_REPLY_LEN: usize = 4;

/// A scan probe: `[0xF3, seq, tx_channel]` (ADR 025).
///
/// The scan sends one per probed channel; `tx_channel` is the channel the
/// probe went out on, so a reply heard from an adjacent channel is
/// recognisable, and `seq` tells a stale reply from the current one.
/// Channel bytes are carried as-is; range validation is the scan machine's job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeFrame {
    /// Scan-local sequence number of the probe.
    pub seq: u8,
    /// Channel the probe was transmitted on.
    pub tx_channel: u8,
}

impl ProbeFrame {
    /// Encodes the probe as its three-byte wire form.
    pub fn encode(&self) -> [u8; PROBE_FRAME_LEN] {
        [TAG_PROBE, self.seq, self.tx_channel]
    }

    /// Parses a probe; `None` on any length other than three or a tag other than `0xF3`.
    pub fn parse(data: &[u8]) -> Option<ProbeFrame> {
        match data {
            &[TAG_PROBE, seq, tx_channel] => Some(ProbeFrame { seq, tx_channel }),
            _ => None,
        }
    }
}

/// A scan probe reply: `[0xF4, seq, tx_channel, home_channel]` (ADR 025).
///
/// A coordinator answers a [`ProbeFrame`] with the probe's `seq` and
/// `tx_channel` echoed and its own `home_channel`, so the scanning peer pins
/// the channel the coordinator states instead of the one that happened to ACK.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProbeReply {
    /// Sequence number copied from the probe.
    pub seq: u8,
    /// `tx_channel` copied from the probe.
    pub tx_channel: u8,
    /// Channel the coordinator is parked on.
    pub home_channel: u8,
}

impl ProbeReply {
    /// Builds the reply to `probe` for a coordinator parked on `home_channel`.
    pub fn answering(probe: &ProbeFrame, home_channel: u8) -> ProbeReply {
        ProbeReply {
            seq: probe.seq,
            tx_channel: probe.tx_channel,
            home_channel,
        }
    }

    /// Encodes the reply as its four-byte wire form.
    pub fn encode(&self) -> [u8; PROBE_REPLY_LEN] {
        [
            TAG_PROBE_REPLY,
            self.seq,
            self.tx_channel,
            self.home_channel,
        ]
    }

    /// Parses a reply; `None` on any length other than four or a tag other than `0xF4`.
    pub fn parse(data: &[u8]) -> Option<ProbeReply> {
        match data {
            &[TAG_PROBE_REPLY, seq, tx_channel, home_channel] => Some(ProbeReply {
                seq,
                tx_channel,
                home_channel,
            }),
            _ => None,
        }
    }
}

// ─── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── parse_frame ────────────────────────────────────────────────────

    #[test]
    fn parse_frame_empty_returns_none() {
        assert!(parse_frame(&[]).is_none());
    }

    #[test]
    fn parse_frame_single_byte() {
        let frame = parse_frame(&[0xF0]).unwrap();
        assert_eq!(frame.tag, 0xF0);
        assert!(frame.payload.is_empty());
    }

    #[test]
    fn parse_frame_with_payload() {
        let frame = parse_frame(&[0x01, 0xAA, 0xBB]).unwrap();
        assert_eq!(frame.tag, 0x01);
        assert_eq!(frame.payload, &[0xAA, 0xBB]);
    }

    #[test]
    fn parse_frame_reserved_zero_tag() {
        let frame = parse_frame(&[0x00, 0x01]).unwrap();
        assert_eq!(frame.tag, 0x00);
        assert!(!is_system_tag(frame.tag));
        assert!(!is_module_tag(frame.tag));
    }

    // ── tag range predicates ───────────────────────────────────────────

    #[test]
    fn system_tag_range() {
        assert!(is_system_tag(0xF0));
        assert!(is_system_tag(0xFF));
        assert!(!is_system_tag(0xEF));
        assert!(!is_system_tag(0x00));
    }

    #[test]
    fn module_tag_range() {
        assert!(is_module_tag(0x01));
        assert!(is_module_tag(0xEF));
        assert!(!is_module_tag(0x00));
        assert!(!is_module_tag(0xF0));
    }

    #[test]
    fn tag_zero_is_neither_system_nor_module() {
        assert!(!is_system_tag(0x00));
        assert!(!is_module_tag(0x00));
    }

    // ── parse_system_command ───────────────────────────────────────────

    #[test]
    fn parse_ping() {
        let frame = parse_frame(&[TAG_PING]).unwrap();
        assert_eq!(parse_system_command(&frame), Some(SystemCommand::Ping));
    }

    #[test]
    fn parse_self_test() {
        let frame = parse_frame(&[TAG_SELF_TEST]).unwrap();
        assert_eq!(parse_system_command(&frame), Some(SystemCommand::SelfTest));
    }

    #[test]
    fn parse_identify() {
        let frame = parse_frame(&[TAG_IDENTIFY]).unwrap();
        assert_eq!(parse_system_command(&frame), Some(SystemCommand::Identify));
    }

    #[test]
    fn parse_unknown_system_tag() {
        let frame = parse_frame(&[0xF5]).unwrap();
        assert_eq!(
            parse_system_command(&frame),
            Some(SystemCommand::Unknown(0xF5))
        );
    }

    #[test]
    fn module_tag_returns_none_for_system_command() {
        let frame = parse_frame(&[0x01, 0xAA]).unwrap();
        assert!(parse_system_command(&frame).is_none());
    }

    #[test]
    fn reserved_zero_returns_none_for_system_command() {
        let frame = parse_frame(&[0x00]).unwrap();
        assert!(parse_system_command(&frame).is_none());
    }

    // ── response helpers ───────────────────────────────────────────────

    #[test]
    fn pong_response_correct() {
        assert_eq!(PONG_RESPONSE, [0xF0, 0x01]);
    }

    #[test]
    fn self_test_pass_response_correct() {
        assert_eq!(SELF_TEST_PASS, [0xF1, 0x00]);
    }

    #[test]
    fn self_test_fail_response_correct() {
        assert_eq!(SELF_TEST_FAIL, [0xF1, 0x01]);
    }

    #[test]
    fn identify_response_correct() {
        let resp = identify_response(0x42, 0x03);
        assert_eq!(resp, [0xF2, 0x42, 0x03]);
    }

    #[test]
    fn identify_response_roundtrip() {
        let resp = identify_response(0x10, 0x02);
        let frame = parse_frame(&resp).unwrap();
        assert_eq!(frame.tag, TAG_IDENTIFY);
        assert_eq!(frame.payload, &[0x10, 0x02]);
    }

    // ── response parsing roundtrips ────────────────────────────────────

    #[test]
    fn pong_response_parses_as_ping_command() {
        let frame = parse_frame(&PONG_RESPONSE).unwrap();
        assert_eq!(parse_system_command(&frame), Some(SystemCommand::Ping));
        assert_eq!(frame.payload, &[0x01]);
    }

    // ── scan probe and reply (ADR 025) ─────────────────────────────────
    //
    // Hand mutation check 2026-11-11: with the tag guard in `ProbeReply::parse`
    // relaxed to `_`, `probe_reply_rejects_wrong_tag` failed (442 passed, 1
    // failed); with `ProbeReply::encode` emitting `self.tx_channel` in the
    // `home_channel` slot, `probe_reply_wire_bytes`, `probe_reply_roundtrip`
    // and `probe_reply_answering_echoes_probe_and_states_home_channel` failed
    // (440 passed, 3 failed). Both mutations were restored.

    #[test]
    fn probe_tags_are_system_tags() {
        assert_eq!(TAG_PROBE, 0xF3);
        assert_eq!(TAG_PROBE_REPLY, 0xF4);
        assert!(is_system_tag(TAG_PROBE));
        assert!(is_system_tag(TAG_PROBE_REPLY));
    }

    #[test]
    fn probe_frame_wire_bytes() {
        let probe = ProbeFrame {
            seq: 7,
            tx_channel: 11,
        };
        assert_eq!(probe.encode(), [0xF3, 7, 11]);
    }

    #[test]
    fn probe_frame_roundtrip() {
        let probe = ProbeFrame {
            seq: 0xA5,
            tx_channel: 6,
        };
        assert_eq!(ProbeFrame::parse(&probe.encode()), Some(probe));
    }

    #[test]
    fn probe_frame_rejects_wrong_tag() {
        assert!(ProbeFrame::parse(&[TAG_PROBE_REPLY, 1, 11]).is_none());
        assert!(ProbeFrame::parse(&[TAG_PING, 1, 11]).is_none());
    }

    #[test]
    fn probe_frame_rejects_wrong_length() {
        assert!(ProbeFrame::parse(&[]).is_none());
        assert!(ProbeFrame::parse(&[TAG_PROBE]).is_none());
        assert!(ProbeFrame::parse(&[TAG_PROBE, 1]).is_none());
        assert!(ProbeFrame::parse(&[TAG_PROBE, 1, 11, 0]).is_none());
    }

    #[test]
    fn probe_frame_carries_channel_bytes_unvalidated() {
        assert_eq!(
            ProbeFrame::parse(&[TAG_PROBE, 1, 0]),
            Some(ProbeFrame {
                seq: 1,
                tx_channel: 0
            })
        );
        assert_eq!(
            ProbeFrame::parse(&[TAG_PROBE, 1, 14]),
            Some(ProbeFrame {
                seq: 1,
                tx_channel: 14
            })
        );
    }

    #[test]
    fn probe_reply_wire_bytes() {
        let reply = ProbeReply {
            seq: 7,
            tx_channel: 10,
            home_channel: 11,
        };
        assert_eq!(reply.encode(), [0xF4, 7, 10, 11]);
    }

    #[test]
    fn probe_reply_roundtrip() {
        let reply = ProbeReply {
            seq: 0x5A,
            tx_channel: 10,
            home_channel: 11,
        };
        assert_eq!(ProbeReply::parse(&reply.encode()), Some(reply));
    }

    #[test]
    fn probe_reply_answering_echoes_probe_and_states_home_channel() {
        let probe = ProbeFrame {
            seq: 3,
            tx_channel: 10,
        };
        let reply = ProbeReply::answering(&probe, 11);
        assert_eq!(
            reply,
            ProbeReply {
                seq: 3,
                tx_channel: 10,
                home_channel: 11,
            }
        );
        assert_eq!(reply.encode(), [0xF4, 3, 10, 11]);
    }

    #[test]
    fn probe_reply_rejects_wrong_tag() {
        assert!(ProbeReply::parse(&[TAG_PROBE, 1, 10, 11]).is_none());
        assert!(ProbeReply::parse(&[TAG_IDENTIFY, 1, 10, 11]).is_none());
    }

    #[test]
    fn probe_reply_rejects_wrong_length() {
        assert!(ProbeReply::parse(&[]).is_none());
        assert!(ProbeReply::parse(&[TAG_PROBE_REPLY]).is_none());
        assert!(ProbeReply::parse(&[TAG_PROBE_REPLY, 1, 10]).is_none());
        assert!(ProbeReply::parse(&[TAG_PROBE_REPLY, 1, 10, 11, 0]).is_none());
    }

    #[test]
    fn probe_reply_carries_channel_bytes_unvalidated() {
        assert_eq!(
            ProbeReply::parse(&[TAG_PROBE_REPLY, 1, 0, 14]),
            Some(ProbeReply {
                seq: 1,
                tx_channel: 0,
                home_channel: 14,
            })
        );
        assert_eq!(
            ProbeReply::parse(&[TAG_PROBE_REPLY, 1, 14, 0]),
            Some(ProbeReply {
                seq: 1,
                tx_channel: 14,
                home_channel: 0,
            })
        );
    }

    #[test]
    fn probe_frames_parse_as_frames_with_system_tags() {
        let probe = ProbeFrame {
            seq: 1,
            tx_channel: 11,
        }
        .encode();
        let frame = parse_frame(&probe).unwrap();
        assert_eq!(frame.tag, TAG_PROBE);
        assert_eq!(frame.payload, &[1, 11]);
        let reply = ProbeReply::answering(&ProbeFrame::parse(&probe).unwrap(), 11).encode();
        let frame = parse_frame(&reply).unwrap();
        assert_eq!(frame.tag, TAG_PROBE_REPLY);
        assert_eq!(frame.payload, &[1, 11, 11]);
    }
}
