//! ESP-NOW command frame parsing — tag-byte envelope with zero-copy payload.
//!
//! Every ESP-NOW command is a single-byte tag followed by an optional payload.
//! Tags are partitioned into two ranges:
//!
//! - **System tags** (`0xF0..=0xFF`) — reserved for infrastructure commands
//!   (`Ping`, `SelfTest`, `Identify`). Unrecognised system tags parse as
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
}
