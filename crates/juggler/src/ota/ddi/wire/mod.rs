//! DDI JSON codec: the hawkBit DDI v1 poll, deployment and cancel documents a device reads, the feedback and configData bodies it writes, and the profile codes it reports.
//!
//! Every parser is strict and pure: a size check before parsing, a first-byte check that the body is a JSON object, and bounded strings and arrays.
//! Unknown fields and links are ignored, because HAL+JSON carries `self` links and more; missing or `null` required fields are errors.
//! A chunk's `part` and `name` and every metadata entry whose key is not `downgrade` or `chip` are ignored too, as the fleet DDI profile requires.
//!
//! # Limits
//!
//! - A poll, deployment or cancel body larger than [`MAX_RESPONSE_BYTES`] (8192) is rejected before any parsing.
//! - A document carries at most [`MAX_CHUNKS`] chunks, [`MAX_ARTIFACTS`] artifacts and [`MAX_METADATA`] `downgrade` or `chip` metadata entries per chunk; one more is an error, never a truncation.
//!   Metadata entries with another key are skipped without being counted or capped.
//! - Every descriptive string has a byte cap, see the `MAX_*_BYTES` constants; a longer string is an error.
//!   `id`, `stopId`, `sleep` and `sha256` have no cap inside the parser: they are bounded only by the body limit and validated afterwards, so an over-long one reports `InvalidActionId`, `InvalidSleep` or `InvalidDigest` instead of `Malformed`.
//! - An encoded feedback body never exceeds [`MAX_FEEDBACK_BYTES`] (4096); an over-limit feedback is refused, never truncated.
//! - A configData body is built only from a [`Chip`], a [`Board`] and a [`Version`](crate::ota::Version), each checked against its profile grammar when it is made.
//!
//! # Privacy
//!
//! Nothing in this module logs, and no error value carries payload text.
//! A link may carry `user:password@` or a token, so [`Href`] has a redacting `Debug` and no `Display`.
//! A `Malformed { line, column }` position reveals lengths, never content.
//! An [`ActionId`] is digits only and safe to log.
//!
//! # Consumer order
//!
//! [`PollResponse::parse`], then [`DeploymentBase::parse`] or [`CancelAction::parse`] on the linked resource, admission, download, and finally [`Feedback::encode`] for the result.
//! [`ConfigData::encode`] builds the body of the once-per-boot configData PUT.
//! A parsed deployment is only syntax-checked: admission (a usable digest, a download link of the right origin, a version that parses) comes afterwards.

mod bounded;
mod cancel;
mod code;
mod config_data;
mod deployment;
mod error;
mod feedback;
mod poll;

pub use bounded::Href;
pub use cancel::CancelAction;
pub use code::ProfileCode;
pub use config_data::{Board, Chip, ConfigData, ConfigDataError};
pub use deployment::{ActionId, Artifact, Chunk, DeploymentBase, Metadata, UpdateMode};
pub use error::DdiError;
pub use feedback::{Execution, Feedback, FeedbackError, Finished};
pub use poll::{parse_sleep, PollResponse};

pub(in crate::ota::ddi) use poll::digits;

use serde::de::DeserializeOwned;

use crate::ota::wire::first_token_position;

/// Experimental: API may change before 1.0.
///
/// Largest accepted poll, deployment or cancel body in bytes.
/// A larger body is rejected before any parsing.
pub const MAX_RESPONSE_BYTES: usize = 8192;
/// Experimental: API may change before 1.0.
///
/// Largest encoded feedback body in bytes.
pub const MAX_FEEDBACK_BYTES: usize = 4096;
/// Experimental: API may change before 1.0.
///
/// Most chunks in a deployment.
pub const MAX_CHUNKS: usize = 4;
/// Experimental: API may change before 1.0.
///
/// Most artifacts in one chunk.
pub const MAX_ARTIFACTS: usize = 4;
/// Experimental: API may change before 1.0.
///
/// Most `downgrade` and `chip` metadata entries in one chunk; entries with another key are not counted.
pub const MAX_METADATA: usize = 8;
/// Experimental: API may change before 1.0.
///
/// Longest accepted link in bytes.
pub const MAX_HREF_BYTES: usize = 512;
/// Experimental: API may change before 1.0.
///
/// Longest accepted artifact file name in bytes.
pub const MAX_FILENAME_BYTES: usize = 128;
/// Experimental: API may change before 1.0.
///
/// Longest accepted chunk version in bytes.
pub const MAX_VERSION_BYTES: usize = 32;
/// Experimental: API may change before 1.0.
///
/// Longest accepted `downgrade` or `chip` metadata value in bytes.
pub const MAX_METADATA_VALUE_BYTES: usize = 128;
/// Experimental: API may change before 1.0.
///
/// Most detail strings in one feedback.
pub const MAX_DETAILS: usize = 10;
/// Experimental: API may change before 1.0.
///
/// Longest feedback detail string in bytes.
pub const MAX_DETAIL_BYTES: usize = 256;
/// Experimental: API may change before 1.0.
///
/// Most digits in an action id.
pub const MAX_ACTION_ID_DIGITS: usize = crate::ota::action_id::MAX_DIGITS;
/// Experimental: API may change before 1.0.
///
/// Longest [`Chip`] in bytes.
pub const MAX_CHIP_BYTES: usize = 32;
/// Experimental: API may change before 1.0.
///
/// Longest [`Board`] in bytes.
pub const MAX_BOARD_BYTES: usize = 32;

/// Size check, object check, then strict JSON parse into a private raw type.
///
/// serde's derived struct deserializer also accepts the positional array form, so the first non-whitespace byte must be `{`.
fn parse_object<T: DeserializeOwned>(body: &[u8]) -> Result<T, DdiError> {
    if body.len() > MAX_RESPONSE_BYTES {
        return Err(DdiError::TooLarge);
    }
    if body.iter().find(|b| !b.is_ascii_whitespace()) != Some(&b'{') {
        let (line, column) = first_token_position(body);
        return Err(DdiError::Malformed { line, column });
    }
    serde_json::from_slice(body).map_err(|e| DdiError::Malformed {
        line: e.line().max(1),
        column: e.column().max(1),
    })
}
