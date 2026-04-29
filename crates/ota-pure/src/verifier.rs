//! Streaming SHA-256 verifier and fixed-size hex encoding/decoding.

use sha2::{Digest, Sha256};

use crate::OtaError;

/// Experimental: API may change before 1.0.
///
/// Streaming SHA-256 verifier for large firmware images.
///
/// Feed chunks via [`update`](StreamingVerifier::update) as they arrive from
/// the network or flash, then call [`finalize`](StreamingVerifier::finalize)
/// to obtain the 32-byte digest.
/// The image is never held in RAM in its entirety.
pub struct StreamingVerifier {
    hasher: Sha256,
}

impl StreamingVerifier {
    /// Experimental: API may change before 1.0.
    ///
    /// Create a new streaming verifier.
    pub fn new() -> Self {
        Self {
            hasher: Sha256::new(),
        }
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Feed the next chunk of data into the running hash.
    pub fn update(&mut self, data: &[u8]) {
        self.hasher.update(data);
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Consume the verifier and return the final 32-byte SHA-256 digest.
    pub fn finalize(self) -> [u8; 32] {
        let hash = self.hasher.finalize();
        let mut result = [0u8; 32];
        result.copy_from_slice(hash.as_slice());
        result
    }
}

impl Default for StreamingVerifier {
    fn default() -> Self {
        Self::new()
    }
}

/// Experimental: API may change before 1.0.
///
/// Decode a 64-character lowercase hex string into a 32-byte array.
///
/// Returns `Err(OtaError::ChecksumMismatch)` if the string is not exactly
/// 64 characters or contains non-hex characters.
pub fn hex_to_bytes(hex: &str) -> Result<[u8; 32], OtaError> {
    if hex.len() != 64 {
        return Err(OtaError::ChecksumMismatch);
    }

    let mut bytes = [0u8; 32];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let high = hex_char_to_nibble(chunk[0])?;
        let low = hex_char_to_nibble(chunk[1])?;
        bytes[i] = (high << 4) | low;
    }

    Ok(bytes)
}

/// Convert a single ASCII hex byte to its nibble value (0–15).
fn hex_char_to_nibble(c: u8) -> Result<u8, OtaError> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(OtaError::ChecksumMismatch),
    }
}

/// Experimental: API may change before 1.0.
///
/// Encode a 32-byte array as a 64-character lowercase hex string.
/// Returns a [`heapless::String<64>`] — no heap allocation required.
pub fn bytes_to_hex(bytes: &[u8; 32]) -> heapless::String<64> {
    const HEX_CHARS: &[u8; 16] = b"0123456789abcdef";
    let mut s = heapless::String::<64>::new();
    for &b in bytes {
        // Capacity is exactly 64 and we push exactly 64 chars — these cannot fail.
        s.push(HEX_CHARS[(b >> 4) as usize] as char).unwrap();
        s.push(HEX_CHARS[(b & 0x0f) as usize] as char).unwrap();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    // SHA-256 of empty string
    const EMPTY_HASH: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    // SHA-256 of "hello"
    const HELLO_HASH: &str = "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824";

    #[test]
    fn streaming_verifier_chunked_matches_reference() {
        let mut verifier = StreamingVerifier::new();
        verifier.update(b"hel");
        verifier.update(b"lo");
        let digest = verifier.finalize();
        let expected = hex_to_bytes(HELLO_HASH).unwrap();
        assert_eq!(digest, expected);
    }

    #[test]
    fn streaming_verifier_empty_input() {
        let verifier = StreamingVerifier::new();
        let digest = verifier.finalize();
        let expected = hex_to_bytes(EMPTY_HASH).unwrap();
        assert_eq!(digest, expected);
    }

    #[test]
    fn streaming_verifier_default_equals_new() {
        let mut a = StreamingVerifier::default();
        let mut b = StreamingVerifier::new();
        a.update(b"data");
        b.update(b"data");
        assert_eq!(a.finalize(), b.finalize());
    }

    #[test]
    fn hex_to_bytes_valid() {
        let bytes = hex_to_bytes(EMPTY_HASH).unwrap();
        assert_eq!(bytes[0], 0xe3);
        assert_eq!(bytes[1], 0xb0);
        assert_eq!(bytes[31], 0x55);
    }

    #[test]
    fn hex_roundtrip() {
        let bytes = hex_to_bytes(HELLO_HASH).unwrap();
        let back = bytes_to_hex(&bytes);
        assert_eq!(back.as_str(), HELLO_HASH);
    }

    #[test]
    fn bytes_to_hex_zero() {
        let bytes = [0u8; 32];
        let hex = bytes_to_hex(&bytes);
        assert_eq!(hex.len(), 64);
        assert!(hex.as_str().chars().all(|c| c == '0'));
    }

    #[test]
    fn hex_to_bytes_invalid_length() {
        assert_eq!(hex_to_bytes("abc"), Err(OtaError::ChecksumMismatch));
    }

    #[test]
    fn hex_to_bytes_invalid_char() {
        let invalid = "g3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";
        assert_eq!(hex_to_bytes(invalid), Err(OtaError::ChecksumMismatch));
    }

    #[test]
    fn hex_to_bytes_uppercase_accepted() {
        let upper = HELLO_HASH.to_uppercase();
        let a = hex_to_bytes(&upper).unwrap();
        let b = hex_to_bytes(HELLO_HASH).unwrap();
        assert_eq!(a, b);
    }
}
