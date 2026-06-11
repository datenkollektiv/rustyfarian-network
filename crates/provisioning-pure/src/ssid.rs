//! SoftAP SSID derivation.
//!
//! [`derive_softap_ssid`] produces the `{prefix}-{XXXX}` access-point name the
//! captive portal broadcasts, where `XXXX` is the uppercase hex of the AP MAC's
//! last two bytes. The MAC suffix keeps several unprovisioned devices on the
//! same bench distinguishable. The derivation lives here so it is host-testable
//! and reusable by a future `esp-hal` triad.

use core::fmt::Write;

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
}
