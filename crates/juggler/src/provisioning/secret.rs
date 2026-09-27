//! Internal helper for scrubbing secret [`heapless::String`] buffers before
//! their memory is released.
//!
//! See bug 003 (`docs/bugs/archive/003-provisioning-config-no-drop-scrub-2026-09-27.md`):
//! the Wi-Fi password, MQTT password, LoRaWAN AppKey, and extra-field values a
//! [`ProvisioningConfig`](crate::provisioning::ProvisioningConfig) carries must
//! not linger as readable bytes in freed stack or heap memory once the value
//! holding them is dropped.

use zeroize::Zeroize;

/// Overwrites the *full backing capacity* of `s` with zeros, then clears it.
///
/// `heapless::String<N>` does not implement [`Zeroize`], so this reaches into
/// its backing `heapless::Vec<u8, N>` via `as_mut_vec`. Zeroing only
/// `s.len()` bytes would miss any secret bytes left over in the buffer's
/// unused tail from an earlier, longer write that reused the same storage
/// (for example a shorter password overwriting a longer one in a reused stack
/// slot), so this first grows the vector to its full capacity `N` with
/// zero-filled bytes, zeroizes the resulting `N`-byte slice with a volatile,
/// compiler-fence-backed write (the `[u8]` impl of [`Zeroize`]), and finally
/// clears the string back to an empty, valid value.
pub(crate) fn scrub<const N: usize>(s: &mut heapless::String<N>) {
    // SAFETY: `as_mut_vec` only requires that the bytes be valid UTF-8 by the
    // time the returned `&mut Vec` borrow ends and `s` is used as a `String`
    // again. The intermediate states below (the original bytes padded with
    // zero bytes, then all-zero) are never read back as a `str`; the final
    // `clear()` leaves the string at length 0, which is trivially valid UTF-8.
    let vec = unsafe { s.as_mut_vec() };
    // Grow to the full capacity so every byte the buffer can ever hold is
    // covered by the zeroize pass below, not just the bytes currently in use.
    // `N` is this vector's own capacity, so this cannot fail; the whole
    // full-capacity argument rests on it, so a violation must not pass silently.
    let grown = vec.resize_default(N);
    debug_assert!(grown.is_ok(), "scrub: resize to own capacity failed");
    vec.as_mut_slice().zeroize();
    vec.clear();
}

#[cfg(test)]
mod tests {
    use super::scrub;

    #[test]
    fn scrub_zeroes_full_capacity_including_stale_tail() {
        const N: usize = 16;
        let mut s: heapless::String<N> = heapless::String::new();
        s.push_str("abcdefghijklmnop").unwrap();
        // Shrink the live length; the old bytes stay in the unused tail.
        s.truncate(3);
        let base = s.as_ptr();
        // SAFETY: `base` points at `s`'s inline `N`-byte buffer, and every
        // byte of it was initialised by the `push_str` above.
        let before = unsafe { core::slice::from_raw_parts(base, N) };
        assert_eq!(
            &before[3..],
            b"defghijklmnop",
            "fixture must leave a stale tail"
        );

        scrub(&mut s);

        assert!(s.is_empty());
        assert_eq!(s.as_ptr(), base, "scrub must not move the buffer");
        // SAFETY: as above; `scrub` wrote all `N` bytes.
        let after = unsafe { core::slice::from_raw_parts(base, N) };
        assert!(
            after.iter().all(|&b| b == 0),
            "stale bytes survived: {after:?}"
        );
    }
}
