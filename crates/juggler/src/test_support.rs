//! Host-test fixtures shared across domain modules.

extern crate std;

/// This process's Wi-Fi test key, generated once on first use.
///
/// Derived from OS entropy rather than written as a literal, so no fixed key
/// material exists in the source — CodeQL's interprocedural
/// `rust/hard-coded-cryptographic-value` query follows a literal through any
/// helper or constant, and spelling it as bytes or chars would only hide it
/// from the analyzer. The one copy here replaces the per-module duplicates, so
/// a future change to the pattern happens in one place.
///
/// The result is always 16 lowercase hex digits, so it fits every length
/// bound the tests exercise (WPA2 minimum 8, `PASSWORD_MAX_LEN`) on every run.
pub(crate) fn test_psk() -> &'static str {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::sync::OnceLock;

    static PSK: OnceLock<alloc::string::String> = OnceLock::new();
    PSK.get_or_init(|| {
        alloc::format!("{:016x}", {
            let mut hasher = RandomState::new().build_hasher();
            hasher.write_u8(0);
            hasher.finish()
        })
    })
    .as_str()
}

/// This process's DDI target token fixture, generated once on first use: 32
/// lowercase hex digits from OS entropy, for the same CodeQL reason as
/// [`test_psk`].
pub(crate) fn test_token() -> &'static str {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};
    use std::sync::OnceLock;

    static TOKEN: OnceLock<alloc::string::String> = OnceLock::new();
    TOKEN
        .get_or_init(|| {
            let half = |seed: u8| {
                let mut hasher = RandomState::new().build_hasher();
                hasher.write_u8(seed);
                hasher.finish()
            };
            alloc::format!("{:016x}{:016x}", half(3), half(4))
        })
        .as_str()
}

// The whole module is already `cfg(test)`.
mod tests {
    use super::{test_psk, test_token};

    #[test]
    fn test_token_is_a_stable_32_digit_lowercase_hex_target_token() {
        let token = test_token();
        assert!(crate::identity::is_target_token(token));
        assert_eq!(token.len(), 32);
        assert_eq!(test_token(), token, "generated once per process");
    }

    #[test]
    fn test_psk_is_a_stable_16_digit_lowercase_hex_wpa2_passphrase() {
        let psk = test_psk();
        assert_eq!(psk.len(), 16, "within the WPA2 passphrase range 8..=63");
        assert!(psk
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        assert_eq!(test_psk(), psk, "generated once per process");
    }
}

/// Polls `future` to completion on the current thread with a no-op waker.
///
/// Enough for the test doubles, which wake themselves while pending; a future
/// that never becomes ready spins forever, so tests only await ready paths.
#[cfg(any(feature = "mqtt", feature = "espnow"))]
pub(crate) fn block_on<F: core::future::Future>(future: F) -> F::Output {
    let mut future = core::pin::pin!(future);
    let mut cx = core::task::Context::from_waker(core::task::Waker::noop());
    loop {
        if let core::task::Poll::Ready(output) = future.as_mut().poll(&mut cx) {
            return output;
        }
    }
}
