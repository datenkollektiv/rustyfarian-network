//! The DDI codec error.

use core::fmt;

/// Experimental: API may change before 1.0.
///
/// Why a DDI document was rejected.
/// Carries no payload text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DdiError {
    /// The body exceeds [`MAX_RESPONSE_BYTES`](super::MAX_RESPONSE_BYTES); checked before parsing.
    TooLarge,
    /// Not UTF-8 or JSON, not an object, a required field missing, `null`, of the wrong type or duplicated, a string over its cap, an array over its count, or an unknown enum string.
    ///
    /// `line` and `column` (both 1-based) come from the JSON parser and are numbers only.
    /// When the body is rejected early for not starting with `{`, they point at its first non-whitespace byte, or 1/1 if there is none.
    Malformed {
        /// 1-based line of the problem.
        line: usize,
        /// 1-based column of the problem.
        column: usize,
    },
    /// `config.polling.sleep` is not `HH:MM:SS`, see [`parse_sleep`](super::parse_sleep).
    InvalidSleep,
    /// `id` or `cancelAction.stopId` is not 1 to 19 ASCII digits.
    InvalidActionId,
    /// `hashes.sha256` is present but not a 64-character hex digest.
    InvalidDigest,
}

impl fmt::Display for DdiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DdiError::TooLarge => f.write_str("DDI document too large"),
            DdiError::Malformed { line, column } => {
                write!(f, "DDI document malformed at line {line} column {column}")
            }
            DdiError::InvalidSleep => f.write_str("DDI poll sleep invalid"),
            DdiError::InvalidActionId => f.write_str("DDI action id invalid"),
            DdiError::InvalidDigest => f.write_str("DDI artifact digest invalid"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;

    #[test]
    fn display_is_fixed_text_with_positions_only() {
        assert_eq!(format!("{}", DdiError::TooLarge), "DDI document too large");
        assert_eq!(
            format!("{}", DdiError::Malformed { line: 2, column: 7 }),
            "DDI document malformed at line 2 column 7"
        );
        assert_eq!(
            format!("{}", DdiError::InvalidSleep),
            "DDI poll sleep invalid"
        );
        assert_eq!(
            format!("{}", DdiError::InvalidActionId),
            "DDI action id invalid"
        );
        assert_eq!(
            format!("{}", DdiError::InvalidDigest),
            "DDI artifact digest invalid"
        );
    }
}
