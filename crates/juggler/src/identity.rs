//! The device identity grammar of the fleet DDI profile (§2), shared by the DDI client and the provisioning portal.
//!
//! One device has one id: it is the DDI controller id, the MQTT username and the MQTT client id.
//! `ota::ddi::ControllerId::parse`, `ota::ddi::DdiToken::parse` and the portal's `IdentityRule::Ddi` all call the predicates here, so the two features can never drift apart.
//! The module needs neither alloc nor a feature, so both tiers' portals (and any `no_std` caller) can use it; the stores do not validate on load, and `ProvisioningConfig::ddi_token` re-validates the token before use.
//!
//! Nothing here trims: a value with surrounding whitespace is refused.

/// Experimental: API may change before 1.0.
///
/// Shortest device id in bytes.
pub const DEVICE_ID_MIN_LEN: usize = 3;

/// Experimental: API may change before 1.0.
///
/// Longest device id in bytes; it fits the 23-byte MQTT client-id cap.
pub const DEVICE_ID_MAX_LEN: usize = 20;

/// Experimental: API may change before 1.0.
///
/// Longest `MacSuffixed` prefix in bytes: the prefix, one dash and four hex digits make a [`DEVICE_ID_MAX_LEN`]-byte id.
pub const DEVICE_ID_PREFIX_MAX_LEN: usize = DEVICE_ID_MAX_LEN - 5;

/// Experimental: API may change before 1.0.
///
/// Shortest target token in bytes.
pub const TARGET_TOKEN_MIN_LEN: usize = 32;

/// Experimental: API may change before 1.0.
///
/// Longest target token in bytes.
pub const TARGET_TOKEN_MAX_LEN: usize = 64;

const fn is_id_byte(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'0'..=b'9' | b'-')
}

const fn is_id_start(b: u8) -> bool {
    matches!(b, b'a'..=b'z' | b'0'..=b'9')
}

/// A non-empty run of `[a-z0-9-]` that starts with a letter or digit: the character rule of a device id and of a `MacSuffixed` prefix.
const fn is_id_text(bytes: &[u8]) -> bool {
    if bytes.is_empty() || !is_id_start(bytes[0]) {
        return false;
    }
    let mut i = 1;
    while i < bytes.len() {
        if !is_id_byte(bytes[i]) {
            return false;
        }
        i += 1;
    }
    true
}

/// Experimental: API may change before 1.0.
///
/// `^[a-z0-9][a-z0-9-]{2,19}$`: whether `text` is a device id.
///
/// ```
/// use juggler::identity::is_device_id;
///
/// assert!(is_device_id("dev-1"));
/// assert!(!is_device_id("-dev"));
/// assert!(!is_device_id("Dev1"));
/// assert!(!is_device_id("ab"));
/// ```
pub const fn is_device_id(text: &str) -> bool {
    let bytes = text.as_bytes();
    bytes.len() >= DEVICE_ID_MIN_LEN && bytes.len() <= DEVICE_ID_MAX_LEN && is_id_text(bytes)
}

/// Experimental: API may change before 1.0.
///
/// Whether `prefix` makes `{prefix}-{xxxx}` (four lowercase hex digits from the MAC) a device id for every MAC:
/// at most [`DEVICE_ID_PREFIX_MAX_LEN`] bytes of `[a-z0-9-]`, starting with a letter or digit when it is not empty.
///
/// An empty prefix passes: the bare four digits are a device id.
///
/// ```
/// use juggler::identity::is_device_id_prefix;
///
/// assert!(is_device_id_prefix("rgb-clock"));
/// assert!(!is_device_id_prefix("RGB_Clock"));
/// assert!(!is_device_id_prefix("a-sixteen-byte-p"));
/// ```
pub const fn is_device_id_prefix(prefix: &str) -> bool {
    let bytes = prefix.as_bytes();
    bytes.len() <= DEVICE_ID_PREFIX_MAX_LEN && (bytes.is_empty() || is_id_text(bytes))
}

/// Experimental: API may change before 1.0.
///
/// `[0-9a-f]{32,64}`: whether `text` is a target token.
///
/// ```
/// use juggler::identity::is_target_token;
///
/// assert!(is_target_token("0123456789abcdef0123456789abcdef"));
/// assert!(!is_target_token("0123456789ABCDEF0123456789ABCDEF"));
/// assert!(!is_target_token("0123456789abcdef0123456789abcde"));
/// ```
pub const fn is_target_token(text: &str) -> bool {
    let bytes = text.as_bytes();
    if bytes.len() < TARGET_TOKEN_MIN_LEN || bytes.len() > TARGET_TOKEN_MAX_LEN {
        return false;
    }
    let mut i = 0;
    while i < bytes.len() {
        if !matches!(bytes[i], b'0'..=b'9' | b'a'..=b'f') {
            return false;
        }
        i += 1;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    extern crate std;
    use std::string::String;

    fn repeat(c: char, n: usize) -> String {
        core::iter::repeat_n(c, n).collect()
    }

    #[test]
    fn device_id_accepts_the_profile_grammar() {
        for id in ["abc", "dev-1", "c3-lab-01", "0ab", "a-b", &repeat('a', 20)] {
            assert!(is_device_id(id), "{id}");
        }
    }

    #[test]
    fn device_id_refuses_length_case_start_and_charset() {
        for id in [
            "",
            "ab",
            &repeat('a', 21),
            "-abc",
            "Abc",
            "ab c",
            " abc",
            "abc ",
            "ab_c",
            "ab.c",
            "abé",
            "abc\n",
        ] {
            assert!(!is_device_id(id), "{id:?}");
        }
    }

    #[test]
    fn prefix_is_valid_exactly_when_every_derived_name_is_a_device_id() {
        for prefix in ["", "a", "rgb-clock", "c3-mqtt-demo", &repeat('p', 15), "a-"] {
            assert!(is_device_id_prefix(prefix), "{prefix:?}");
            let mut name = String::from(prefix);
            if !prefix.is_empty() {
                name.push('-');
            }
            name.push_str("ab12");
            assert!(is_device_id(&name), "{name}");
        }
        for prefix in [
            &repeat('p', 16),
            "RGB-clock",
            "rgb_clock",
            "-clock",
            " clock",
            "clock ",
            "é",
        ] {
            assert!(!is_device_id_prefix(prefix), "{prefix:?}");
        }
    }

    #[test]
    fn token_accepts_32_to_64_lowercase_hex_only() {
        assert!(is_target_token(&repeat('0', 32)));
        assert!(is_target_token(&repeat('f', 64)));
        assert!(!is_target_token(&repeat('0', 31)));
        assert!(!is_target_token(&repeat('0', 65)));
        assert!(!is_target_token(&repeat('A', 32)));
        assert!(!is_target_token(&repeat('g', 32)));
        let mut spaced = repeat('0', 31);
        spaced.push(' ');
        assert!(!is_target_token(&spaced));
        let mut newline = repeat('0', 32);
        newline.push('\n');
        assert!(!is_target_token(&newline));
        assert!(!is_target_token(""));
    }

    #[test]
    fn the_limits_are_the_profile_limits() {
        assert_eq!(DEVICE_ID_MIN_LEN, 3);
        assert_eq!(DEVICE_ID_MAX_LEN, 20);
        assert_eq!(DEVICE_ID_PREFIX_MAX_LEN, 15);
        assert_eq!(TARGET_TOKEN_MIN_LEN, 32);
        assert_eq!(TARGET_TOKEN_MAX_LEN, 64);
    }
}
