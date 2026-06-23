//! SoftAP SSID derivation and resolution.
//!
//! [`derive_softap_ssid`] produces the `{prefix}-{XXXX}` access-point name the
//! captive portal broadcasts, where `XXXX` is the uppercase hex of the AP MAC's
//! last two bytes. The MAC suffix keeps several unprovisioned devices on the
//! same bench distinguishable.
//!
//! [`resolve_softap_ssid`] is the single entry point used by both HAL crates:
//! `Some(override)` is validated and used verbatim; `None` falls through to
//! [`derive_softap_ssid`].  The derivation lives here so it is host-testable
//! and reusable by both the ESP-IDF and bare-metal tiers.

use core::fmt::Write;

use crate::wifi::validate_ssid;

/// The fixed suffix is `-XXXX`: one dash plus four uppercase hex characters.
const SUFFIX_LEN: usize = 5;

/// Experimental: API may change before 1.0.
///
/// Derives the SoftAP SSID `{prefix}-{XXXX}` from a prefix and the AP MAC.
///
/// `XXXX` is the uppercase hex of `mac[4]` and `mac[5]`. The result always fits
/// the 32-byte SSID limit: if `{prefix}-XXXX` would exceed 32 bytes, the
/// **prefix** is truncated on a UTF-8 character boundary so the dash and the
/// four hex characters always survive. Never panics.
pub fn derive_softap_ssid(prefix: &str, mac: &[u8; 6]) -> heapless::String<32> {
    let max_prefix = 32 - SUFFIX_LEN;
    let mut end = prefix.len().min(max_prefix);
    while end > 0 && !prefix.is_char_boundary(end) {
        end -= 1;
    }
    let truncated = &prefix[..end];

    let mut ssid: heapless::String<32> = heapless::String::new();
    let _ = ssid.push_str(truncated);
    let _ = write!(ssid, "-{:02X}{:02X}", mac[4], mac[5]);
    ssid
}

/// Experimental: API may change before 1.0.
///
/// Resolves the SoftAP SSID from an optional verbatim override and the
/// `{prefix}-{MAC}` derivation.
///
/// # Behaviour
///
/// - `Some(s)` — the override path:
///   1. If `s.trim().is_empty()` returns `Err("SSID is whitespace-only")`.
///   2. Calls [`validate_ssid`](crate::wifi::validate_ssid) which rejects empty
///      strings and SSIDs longer than 32 UTF-8 bytes; propagates its `&'static str`
///      error unchanged.
///   3. Constructs a [`heapless::String<32>`] from `s` (guaranteed to fit after
///      validation) and returns `Ok`.
///
/// - `None` — returns `Ok(derive_softap_ssid(prefix, mac))`, which is infallible.
///
/// # SSID length
///
/// Length is measured in **UTF-8 bytes**, not Unicode scalar count.  A name that
/// looks short may exceed the 32-byte cap if it contains non-ASCII characters.
///
/// # Collision caveat
///
/// Using a verbatim override disables the per-device MAC suffix, so multiple
/// devices sharing the same override will share the same SSID.  The caller owns
/// uniqueness when opting out of the derived path.
pub fn resolve_softap_ssid(
    ssid_override: Option<&str>,
    prefix: &str,
    mac: &[u8; 6],
) -> Result<heapless::String<32>, &'static str> {
    match ssid_override {
        Some(s) => {
            if s.trim().is_empty() {
                return Err("SSID is whitespace-only");
            }
            validate_ssid(s)?;
            let mut out: heapless::String<32> = heapless::String::new();
            // `validate_ssid` already confirmed s.len() <= 32, so push_str
            // cannot fail here; the result is intentionally discarded.
            let _ = out.push_str(s);
            Ok(out)
        }
        None => Ok(derive_softap_ssid(prefix, mac)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_MAC: [u8; 6] = [0xDE, 0xAD, 0xBE, 0xEF, 0xAB, 0x12];

    #[test]
    fn normal_prefix_appends_uppercase_hex_suffix() {
        let ssid = derive_softap_ssid("Rustyfarian", &TEST_MAC);
        assert_eq!(ssid.as_str(), "Rustyfarian-AB12");
    }

    #[test]
    fn beekeeper_prefix() {
        let ssid = derive_softap_ssid("Beekeeper", &TEST_MAC);
        assert_eq!(ssid.as_str(), "Beekeeper-AB12");
    }

    #[test]
    fn low_bytes_are_zero_padded() {
        let mac = [0, 0, 0, 0, 0x01, 0x0F];
        let ssid = derive_softap_ssid("X", &mac);
        assert_eq!(ssid.as_str(), "X-010F");
    }

    #[test]
    fn over_long_prefix_is_truncated_to_fit_32_bytes() {
        let prefix = "a".repeat(40);
        let ssid = derive_softap_ssid(&prefix, &TEST_MAC);
        assert_eq!(ssid.as_str().len(), 32);
        assert!(ssid.as_str().ends_with("-AB12"));
        assert_eq!(&ssid.as_str()[..27], "a".repeat(27));
    }

    #[test]
    fn prefix_exactly_filling_budget_is_untouched() {
        let prefix = "p".repeat(27);
        let ssid = derive_softap_ssid(&prefix, &TEST_MAC);
        assert_eq!(ssid.as_str().len(), 32);
        assert!(ssid.as_str().starts_with(&"p".repeat(27)));
    }

    #[test]
    fn multibyte_prefix_truncation_stays_on_char_boundary() {
        let prefix = "é".repeat(20);
        let ssid = derive_softap_ssid(&prefix, &TEST_MAC);
        assert!(ssid.as_str().is_char_boundary(0));
        assert!(ssid.as_str().ends_with("-AB12"));
        assert!(ssid.as_str().len() <= 32);
        let body = ssid.as_str().strip_suffix("-AB12").unwrap();
        assert!(body.chars().all(|c| c == 'é'));
    }

    #[test]
    fn empty_prefix_yields_bare_suffix() {
        let ssid = derive_softap_ssid("", &TEST_MAC);
        assert_eq!(ssid.as_str(), "-AB12");
    }

    // ── resolve_softap_ssid tests ────────────────────────────────────────────

    #[test]
    fn resolve_single_char_override_is_accepted() {
        let result = resolve_softap_ssid(Some("A"), "prefix", &TEST_MAC);
        assert_eq!(
            result,
            Ok({
                let mut s: heapless::String<32> = heapless::String::new();
                let _ = s.push_str("A");
                s
            })
        );
        assert_eq!(result.unwrap().as_str(), "A");
    }

    #[test]
    fn resolve_exactly_32_ascii_bytes_is_accepted() {
        let name = "a".repeat(32);
        let result = resolve_softap_ssid(Some(&name), "prefix", &TEST_MAC);
        assert!(result.is_ok(), "32-byte SSID must be accepted");
        assert_eq!(result.unwrap().as_str(), name);
    }

    #[test]
    fn resolve_33_ascii_bytes_is_rejected() {
        let name = "a".repeat(33);
        let result = resolve_softap_ssid(Some(&name), "prefix", &TEST_MAC);
        assert!(result.is_err(), "33-byte SSID must be rejected");
    }

    #[test]
    fn resolve_multibyte_under_32_chars_but_over_32_bytes_is_rejected() {
        // Each 'é' is 2 UTF-8 bytes; 17 of them = 34 bytes > 32, but only 17 chars.
        let name = "é".repeat(17);
        assert!(name.chars().count() == 17, "sanity: 17 chars");
        assert!(name.len() == 34, "sanity: 34 bytes");
        let result = resolve_softap_ssid(Some(&name), "prefix", &TEST_MAC);
        assert!(
            result.is_err(),
            "multibyte string over 32 bytes must be rejected (byte count, not char count)"
        );
    }

    #[test]
    fn resolve_exactly_32_byte_multibyte_string_is_accepted() {
        // Each 'é' is 2 bytes; 16 × 'é' = 32 bytes.
        let name = "é".repeat(16);
        assert_eq!(name.len(), 32, "sanity: exactly 32 bytes");
        let result = resolve_softap_ssid(Some(&name), "prefix", &TEST_MAC);
        assert!(result.is_ok(), "32-byte multibyte SSID must be accepted");
        assert_eq!(result.unwrap().as_str(), name);
    }

    #[test]
    fn resolve_whitespace_only_override_is_rejected_with_whitespace_message() {
        let result = resolve_softap_ssid(Some("   "), "prefix", &TEST_MAC);
        assert_eq!(
            result,
            Err("SSID is whitespace-only"),
            "whitespace-only override must produce the whitespace error message"
        );
    }

    #[test]
    fn resolve_override_with_hyphens_is_accepted_verbatim() {
        let result = resolve_softap_ssid(Some("my-clock-2"), "Rustyfarian", &TEST_MAC);
        assert!(result.is_ok(), "override with hyphens must be accepted");
        assert_eq!(result.unwrap().as_str(), "my-clock-2");
    }

    #[test]
    fn resolve_none_equals_derive_softap_ssid() {
        let result = resolve_softap_ssid(None, "Rustyfarian", &TEST_MAC);
        let expected = derive_softap_ssid("Rustyfarian", &TEST_MAC);
        assert_eq!(result, Ok(expected));
    }

    #[test]
    fn resolve_none_with_empty_prefix_equals_derive_softap_ssid() {
        let result = resolve_softap_ssid(None, "", &TEST_MAC);
        let expected = derive_softap_ssid("", &TEST_MAC);
        assert_eq!(result, Ok(expected));
    }
}
