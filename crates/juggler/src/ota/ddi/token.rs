//! The DDI target token and the [`Secret`] that holds it and the `Authorization` value built from it.

use core::fmt;

use zeroize::Zeroize;

/// Experimental: API may change before 1.0.
///
/// Shortest accepted target token in bytes ([`crate::identity::TARGET_TOKEN_MIN_LEN`]).
pub const TOKEN_MIN_LEN: usize = crate::identity::TARGET_TOKEN_MIN_LEN;
/// Experimental: API may change before 1.0.
///
/// Longest accepted target token in bytes ([`crate::identity::TARGET_TOKEN_MAX_LEN`]).
pub const TOKEN_MAX_LEN: usize = crate::identity::TARGET_TOKEN_MAX_LEN;

/// The `Authorization` scheme prefix, including the separating space.
const AUTHORIZATION_PREFIX: &str = "TargetToken ";

/// Experimental: API may change before 1.0.
///
/// Longest `Authorization` value in bytes: `TargetToken ` followed by a [`TOKEN_MAX_LEN`] token.
pub const AUTHORIZATION_MAX_LEN: usize = AUTHORIZATION_PREFIX.len() + TOKEN_MAX_LEN;

/// Experimental: API may change before 1.0.
///
/// A secret string of at most `N` bytes.
///
/// `Debug` prints `Secret(<redacted>)`, and there is deliberately no `Display` and no `Clone`.
/// The buffer is zeroed when the value is dropped.
/// That covers the value's final location only: like every Rust value, a move may leave a bitwise copy behind, and copies made from [`expose`](Self::expose) (an HTTP client's header list, say) are the caller's to keep short-lived.
/// Only this crate creates one.
pub struct Secret<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> Secret<N> {
    /// Concatenates `parts`; `None` when they exceed `N` bytes, never a truncation.
    fn from_parts(parts: &[&str]) -> Option<Self> {
        let mut secret = Self::empty();
        for part in parts {
            let end = secret.len.checked_add(part.len()).filter(|&e| e <= N)?;
            secret.buf[secret.len..end].copy_from_slice(part.as_bytes());
            secret.len = end;
        }
        Some(secret)
    }

    const fn empty() -> Self {
        Secret {
            buf: [0; N],
            len: 0,
        }
    }

    fn clear(&mut self) {
        self.buf.zeroize();
        self.len = 0;
    }

    /// The secret text, for the one place that must send it (an HTTP header); never log it.
    pub fn expose(&self) -> &str {
        // The bytes are whole `&str` parts copied end to end, so they are always valid UTF-8.
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

impl<const N: usize> Drop for Secret<N> {
    fn drop(&mut self) {
        self.clear();
    }
}

impl<const N: usize> fmt::Debug for Secret<N> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// Experimental: API may change before 1.0.
///
/// The per-device DDI target token: [`TOKEN_MIN_LEN`] to [`TOKEN_MAX_LEN`] lowercase hex digits (`[0-9a-f]{32,64}`).
///
/// A value exists only after [`parse`](Self::parse) accepted it, so every use is a validated use, and no CR, LF or other byte outside the grammar can reach a header.
/// The token is held in a [`Secret`]: `Debug` prints `DdiToken(Secret(<redacted>))`, there is no `Display` and no `Clone`, and the buffer is zeroed on drop.
///
/// # Examples
///
/// ```
/// use juggler::ota::ddi::DdiToken;
///
/// let token = DdiToken::parse("0123456789abcdef0123456789abcdef").unwrap();
/// assert_eq!(
///     token.authorization().expose(),
///     "TargetToken 0123456789abcdef0123456789abcdef"
/// );
/// assert!(DdiToken::parse("0123456789ABCDEF0123456789ABCDEF").is_err());
/// ```
#[derive(Debug)]
pub struct DdiToken(Secret<TOKEN_MAX_LEN>);

impl DdiToken {
    /// Accepts `text` only when it is [`TOKEN_MIN_LEN`] to [`TOKEN_MAX_LEN`] lowercase hex digits ([`crate::identity::is_target_token`], the rule the provisioning portal shares); anything else is refused, never trimmed or truncated.
    pub fn parse(text: &str) -> Result<Self, DdiTokenError> {
        if !(TOKEN_MIN_LEN..=TOKEN_MAX_LEN).contains(&text.len()) {
            return Err(DdiTokenError::InvalidLength);
        }
        if !crate::identity::is_target_token(text) {
            return Err(DdiTokenError::InvalidCharacter);
        }
        Secret::from_parts(&[text])
            .map(DdiToken)
            .ok_or(DdiTokenError::InvalidLength)
    }

    /// The `Authorization` header value, `TargetToken <token>`, held in its own [`Secret`].
    pub fn authorization(&self) -> Secret<AUTHORIZATION_MAX_LEN> {
        let value = Secret::from_parts(&[AUTHORIZATION_PREFIX, self.0.expose()]);
        // The prefix plus a token of at most `TOKEN_MAX_LEN` bytes fits by construction.
        debug_assert!(value.is_some(), "authorization: value exceeds its bound");
        value.unwrap_or(Secret::empty())
    }
}

/// Experimental: API may change before 1.0.
///
/// Why a target token was refused.
/// Carries no input text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DdiTokenError {
    /// Shorter than [`TOKEN_MIN_LEN`] or longer than [`TOKEN_MAX_LEN`] bytes.
    InvalidLength,
    /// A byte other than `0-9` or `a-f`.
    InvalidCharacter,
}

impl fmt::Display for DdiTokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DdiTokenError::InvalidLength => f.write_str("DDI token length invalid"),
            DdiTokenError::InvalidCharacter => f.write_str("DDI token character invalid"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;
    use alloc::string::String;

    fn hex(len: usize) -> String {
        "0123456789abcdef".chars().cycle().take(len).collect()
    }

    #[test]
    fn length_bounds() {
        assert_eq!(
            DdiToken::parse(&hex(31)).unwrap_err(),
            DdiTokenError::InvalidLength
        );
        assert!(DdiToken::parse(&hex(32)).is_ok());
        assert!(DdiToken::parse(&hex(64)).is_ok());
        assert_eq!(
            DdiToken::parse(&hex(65)).unwrap_err(),
            DdiTokenError::InvalidLength
        );
        assert_eq!(
            DdiToken::parse("").unwrap_err(),
            DdiTokenError::InvalidLength
        );
    }

    #[test]
    fn refuses_uppercase_and_non_hex() {
        let upper = hex(32).to_uppercase();
        assert_eq!(
            DdiToken::parse(&upper).unwrap_err(),
            DdiTokenError::InvalidCharacter
        );
        let g = format!("{}g", hex(31));
        assert_eq!(
            DdiToken::parse(&g).unwrap_err(),
            DdiTokenError::InvalidCharacter
        );
    }

    #[test]
    fn refuses_cr_lf_and_space_anywhere() {
        for bad in ['\r', '\n', ' ', '\t', '\0'] {
            for at in [0, 16, 32] {
                let mut text = hex(32);
                text.insert(at, bad);
                assert_eq!(
                    DdiToken::parse(&text).unwrap_err(),
                    DdiTokenError::InvalidCharacter,
                    "{bad:?} at {at}"
                );
            }
        }
        let header_injection = format!("{}\r\nX-Evil: 1", hex(32));
        assert!(DdiToken::parse(&header_injection).is_err());
    }

    #[test]
    fn authorization_value() {
        let token = DdiToken::parse(&hex(64)).unwrap();
        let auth = token.authorization();
        assert_eq!(auth.expose(), format!("TargetToken {}", hex(64)));
        assert_eq!(auth.expose().len(), AUTHORIZATION_MAX_LEN);
    }

    #[test]
    fn debug_is_redacted() {
        let token = DdiToken::parse(&hex(32)).unwrap();
        let debug = format!("{token:?}");
        assert_eq!(debug, "DdiToken(Secret(<redacted>))");
        assert_eq!(format!("{:?}", token.authorization()), "Secret(<redacted>)");
        let pretty = format!("{token:#?}");
        assert!(!pretty.contains("0123"), "{pretty}");
    }

    #[test]
    fn secret_refuses_overflow_instead_of_truncating() {
        assert!(Secret::<4>::from_parts(&["ab", "cd"]).is_some());
        assert!(Secret::<4>::from_parts(&["ab", "cde"]).is_none());
    }

    /// Tests cover the clearing implementation shared with Drop; no post-drop memory is inspected.
    #[test]
    fn clear_zeroes_the_token() {
        let mut token = DdiToken::parse(&hex(64)).unwrap();
        token.0.clear();
        assert_eq!(token.0.buf, [0; TOKEN_MAX_LEN]);
        assert_eq!(token.0.len, 0);
    }

    #[test]
    fn clear_zeroes_the_authorization_value() {
        let token = DdiToken::parse(&hex(64)).unwrap();
        let mut auth = token.authorization();
        auth.clear();
        assert_eq!(auth.buf, [0; AUTHORIZATION_MAX_LEN]);
        assert_eq!(auth.len, 0);
    }
}
