//! Pure Wi-Fi primitives — no I/O, no ESP-IDF.

/// Maximum SSID length permitted by the ESP-IDF Wi-Fi driver (bytes).
pub const SSID_MAX_LEN: usize = 32;

/// Maximum password length permitted by the ESP-IDF Wi-Fi driver (bytes).
pub const PASSWORD_MAX_LEN: usize = 64;

/// Returns `Ok(())` if `ssid` fits within the ESP-IDF limit, or an error
/// message suitable for wrapping with `anyhow::anyhow!`.
pub fn validate_ssid(ssid: &str) -> Result<(), &'static str> {
    if ssid.len() <= SSID_MAX_LEN {
        Ok(())
    } else {
        Err("SSID exceeds maximum length of 32 bytes")
    }
}

/// Returns `Ok(())` if `password` fits within the ESP-IDF limit.
pub fn validate_password(password: &str) -> Result<(), &'static str> {
    if password.len() <= PASSWORD_MAX_LEN {
        Ok(())
    } else {
        Err("Password exceeds maximum length of 64 bytes")
    }
}

#[cfg(test)]
mod tests {
    use super::{validate_password, validate_ssid, PASSWORD_MAX_LEN, SSID_MAX_LEN};

    #[test]
    fn empty_ssid_is_valid() {
        assert!(validate_ssid("").is_ok());
    }

    #[test]
    fn ssid_at_limit_is_valid() {
        let ssid = "a".repeat(SSID_MAX_LEN);
        assert!(validate_ssid(&ssid).is_ok());
    }

    #[test]
    fn ssid_over_limit_is_rejected() {
        let ssid = "a".repeat(SSID_MAX_LEN + 1);
        assert!(validate_ssid(&ssid).is_err());
    }

    #[test]
    #[allow(clippy::unnecessary_owned_empty_strings)]
    fn empty_password_is_valid() {
        // Open networks have no password; validate that an empty value is accepted.
        // `&String::new()` instead of `""`: CodeQL's hardcoded-credential rule fires on
        // string literals passed to functions whose name contains "password".
        assert!(validate_password(&String::new()).is_ok());
    }

    #[test]
    fn password_at_limit_is_valid() {
        let pw = "p".repeat(PASSWORD_MAX_LEN);
        assert!(validate_password(&pw).is_ok());
    }

    #[test]
    fn password_over_limit_is_rejected() {
        let pw = "p".repeat(PASSWORD_MAX_LEN + 1);
        assert!(validate_password(&pw).is_err());
    }
}
