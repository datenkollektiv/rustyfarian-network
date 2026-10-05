//! OTA wire contract: the command, manifest and status JSON that an OTA
//! consumer exchanges with its backend, plus the frozen `failed` reason codes.
//!
//! Enabled by the `ota-wire` feature (implies `ota`, needs `alloc`).
//! The shapes are the first consumer's (rustyfarian-rgb-clock) and must stay byte-compatible.
//!
//! # Limits
//!
//! - A command payload larger than [`MAX_COMMAND_BYTES`] (512) is rejected before any parsing.
//! - A manifest body larger than [`MAX_MANIFEST_BYTES`] (1024) is rejected before any parsing.
//!
//! # Privacy
//!
//! Nothing in this module logs, and no error value carries payload text.
//! URLs may contain `user:password@`; a consumer logs only the fixed reason strings.
//! A `Malformed { line, column }` position reveals lengths, never content: an error just after a URL hints at the URL's length.
//!
//! # Consumer order
//!
//! `OtaCommand::parse`, fetch the manifest, `Manifest::parse`, `Manifest::check_target`, `Manifest::image_metadata`, then `decide_offer`; map each failure with `reason()` or `FailReason::from_offer`.
//!
//! `Manifest::parse` validates the URL, so a manifest with a bad URL reports `manifest_invalid` even when its target is also wrong; the target is only checked afterwards.

mod command;
mod manifest;
mod reason;
mod rollback;
mod status;
mod url;

pub use command::{CommandError, OtaCommand, MAX_COMMAND_BYTES};
pub use manifest::{Manifest, ManifestError, MAX_MANIFEST_BYTES};
pub use reason::FailReason;
pub use rollback::RollbackReason;
pub use status::{OtaStatus, StatusError};

/// Line and column (both 1-based) of the first byte that is not ASCII whitespace, or 1/1 when there is none.
///
/// Used for the early "not a JSON object" rejection so the position matches `serde_json`'s convention without reading any payload text.
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

#[cfg(test)]
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
