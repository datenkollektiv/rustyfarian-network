//! The DDI action-id rule, shared by the wire codec (`ddi::wire::ActionId`) and the record store (`persist`), so neither depends on the other.

/// Most digits in a canonical action id: `i64::MAX` has 19.
pub(crate) const MAX_DIGITS: usize = 19;

/// Whether `text` is a DDI action id in canonical `i64` form: 1 to 19 ASCII digits, no leading zero unless the id is `0`, at most `i64::MAX`.
///
/// hawkBit action ids are Java `long` values, so this is the only spelling a server sends, and text and numeric equality agree on it.
pub fn is_canonical_action_id(text: &str) -> bool {
    let bytes = text.as_bytes();
    !bytes.is_empty()
        && bytes.len() <= MAX_DIGITS
        && bytes.iter().all(u8::is_ascii_digit)
        && (bytes[0] != b'0' || bytes.len() == 1)
        && text.parse::<i64>().is_ok()
}
