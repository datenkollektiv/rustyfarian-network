//! OTA reason codes: the frozen `failed` reason codes ([`FailReason`]) and the `rolled_back` reason codes ([`RollbackReason`]).
//!
//! Both are part of the base `ota` feature: the DDI feedback encoder (`ota::ddi::wire`) uses [`FailReason`] and the record store (`ota::persist`) uses [`RollbackReason`]; their serde impls need `ota-ddi`.
//! The code strings were first published on the MQTT OTA wire (removed after 0.8.0) and are stored in device records, so they must stay byte-compatible.
//!
//! # Privacy
//!
//! Nothing in this module logs, and no error value carries payload text.

mod reason;
mod rollback;

pub use reason::FailReason;
pub use rollback::RollbackReason;

/// Line and column (both 1-based) of the first byte that is not ASCII whitespace, or 1/1 when there is none.
///
/// Used for the early "not a JSON object" rejection so the position matches `serde_json`'s convention without reading any payload text.
#[cfg(feature = "ota-ddi")]
pub(crate) fn first_token_position(bytes: &[u8]) -> (usize, usize) {
    let (mut line, mut column) = (1, 1);
    for &b in bytes {
        if !b.is_ascii_whitespace() {
            return (line, column);
        }
        if b == b'\n' {
            line += 1;
            column = 1;
        } else {
            column += 1;
        }
    }
    (1, 1)
}

#[cfg(all(test, feature = "ota-ddi"))]
mod position_tests {
    use super::first_token_position;

    #[test]
    fn positions() {
        assert_eq!(first_token_position(b""), (1, 1));
        assert_eq!(first_token_position(b"   "), (1, 1));
        assert_eq!(first_token_position(b"x"), (1, 1));
        assert_eq!(first_token_position(b"  x"), (1, 3));
        assert_eq!(first_token_position(b"\n\n  [1]"), (3, 3));
    }
}
