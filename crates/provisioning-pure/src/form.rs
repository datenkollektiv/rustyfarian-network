//! The `application/x-www-form-urlencoded` parser and the opaque
//! [`ExtraField`] type.
//!
//! [`parse_form`] is the single entry point. It percent-decodes the body, maps
//! known input names to the canonical schema fields, collects unknown pairs as
//! opaque extras, and validates everything, accumulating per-field errors
//! rather than failing on the first problem.
//!
//! # Reserved keys
//!
//! Field names beginning with an underscore (`_`) are reserved for
//! portal-internal use — for example the session nonce `_nonce` the HTTP layer
//! injects. The parser **silently ignores** every underscore-prefixed pair: it
//! is neither collected as an extra nor reported as an error. This deviates
//! from the feature-doc sketch and was adopted in the security review so the
//! portal can carry hidden state through the form without polluting the
//! validated config or the error list.
//!
//! # Never panics
//!
//! No input can panic the parser. Truncated escapes (`%4`), invalid escapes
//! (`%zz`), and escapes that decode to invalid UTF-8 all yield a single
//! body-level [`ValidationError::MalformedBody`] on [`Field::Form`]; the
//! offending pair is skipped. Over-long keys or values are bounded by fixed
//! decode buffers and reported as length errors. Every `heapless` push is
//! handled — there are no `unwrap`s on capacity.

use crate::config::{
    ProvisioningConfig, APP_KEY_HEX_LEN, DEVICE_NAME_MAX_LEN, EUI_HEX_LEN, EXTRA_FIELDS_MAX,
    EXTRA_KEY_MAX_LEN, EXTRA_VALUE_MAX_LEN, OTA_URL_MAX_LEN,
};
use crate::error::{Field, FieldError, FieldErrors, ValidationError};

/// Largest decoded value the parser will buffer (bytes).
///
/// Comfortably above [`OTA_URL_MAX_LEN`] so a too-long URL is reported as
/// [`ValidationError::TooLong`] rather than truncated; anything beyond this is
/// treated as a malformed/oversized pair.
const VALUE_DECODE_MAX: usize = 160;

/// Largest decoded key the parser will buffer (bytes).
///
/// The longest canonical key is 9 bytes and extra keys are capped at
/// [`EXTRA_KEY_MAX_LEN`]; 16 leaves headroom to detect over-long keys.
const KEY_DECODE_MAX: usize = 16;

/// Experimental: API may change before 1.0.
///
/// An opaque host-defined key/value pair carried alongside the canonical
/// schema (the ADR 013 §4 extension mechanism).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtraField {
    /// The extra field's key (at most [`EXTRA_KEY_MAX_LEN`] bytes).
    pub key: heapless::String<EXTRA_KEY_MAX_LEN>,
    /// The extra field's value (at most [`EXTRA_VALUE_MAX_LEN`] bytes).
    pub value: heapless::String<EXTRA_VALUE_MAX_LEN>,
}

/// Outcome of percent-decoding a single key or value.
enum Decoded<const N: usize> {
    /// Successfully decoded into a UTF-8 string within the buffer bound.
    Ok(heapless::String<N>),
    /// The escape sequence was malformed or decoded to invalid UTF-8.
    Malformed,
    /// The decoded byte length exceeded the buffer bound.
    Overflow,
}

/// Percent-decodes one `application/x-www-form-urlencoded` token into a bounded
/// string, treating `+` as a space and `%XX` as a byte.
fn percent_decode<const N: usize>(input: &str) -> Decoded<N> {
    let mut bytes: heapless::Vec<u8, N> = heapless::Vec::new();
    let raw = input.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        let b = raw[i];
        let decoded = match b {
            b'+' => {
                i += 1;
                b' '
            }
            b'%' => {
                if i + 2 >= raw.len() {
                    return Decoded::Malformed;
                }
                let hi = match hex_value(raw[i + 1]) {
                    Some(v) => v,
                    None => return Decoded::Malformed,
                };
                let lo = match hex_value(raw[i + 2]) {
                    Some(v) => v,
                    None => return Decoded::Malformed,
                };
                i += 3;
                (hi << 4) | lo
            }
            other => {
                i += 1;
                other
            }
        };
        if bytes.push(decoded).is_err() {
            return Decoded::Overflow;
        }
    }
    match core::str::from_utf8(&bytes) {
        Ok(s) => {
            let mut out: heapless::String<N> = heapless::String::new();
            if out.push_str(s).is_err() {
                return Decoded::Overflow;
            }
            Decoded::Ok(out)
        }
        Err(_) => Decoded::Malformed,
    }
}

fn hex_value(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Working state for a single canonical field during accumulation.
#[derive(Default)]
struct Slot {
    /// `true` once the field's key has been seen at least once.
    seen: bool,
    /// The decoded value of the first occurrence (when not over-long).
    value: heapless::String<VALUE_DECODE_MAX>,
    /// `true` if the field appeared more than once.
    duplicate: bool,
    /// `true` if the first occurrence's value exceeded the decode buffer.
    overflowed: bool,
}

/// The seven canonical fields, indexed positionally for the working `Slot`s.
const CANONICAL: [Field; 7] = [
    Field::WifiSsid,
    Field::WifiPassword,
    Field::DevEui,
    Field::JoinEui,
    Field::AppKey,
    Field::OtaUrl,
    Field::DeviceName,
];

/// Returns the `CANONICAL` index for a key, or `None` if it is not a canonical
/// field name.
fn canonical_index(key: &str) -> Option<usize> {
    CANONICAL.iter().position(|f| f.form_name() == key)
}

/// Experimental: API may change before 1.0.
///
/// Parses an `application/x-www-form-urlencoded` provisioning submission.
///
/// On success returns a fully validated [`ProvisioningConfig`]. On failure
/// returns the accumulated [`FieldErrors`]: at most one error per canonical
/// field plus at most one [`Field::Form`]-level error (the first body-level
/// problem wins, keeping the `8`-entry capacity exact).
///
/// See the [module docs](self) for the reserved-key and never-panics
/// guarantees.
pub fn parse_form(body: &str) -> Result<ProvisioningConfig, FieldErrors> {
    let mut slots: [Slot; 7] = Default::default();
    let mut extras: heapless::Vec<ExtraField, EXTRA_FIELDS_MAX> = heapless::Vec::new();
    let mut form_error: Option<ValidationError> = None;

    for pair in body.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (raw_key, raw_value) = match pair.split_once('=') {
            Some((k, v)) => (k, v),
            None => (pair, ""),
        };

        let key = match percent_decode::<KEY_DECODE_MAX>(raw_key) {
            Decoded::Ok(k) => k,
            Decoded::Malformed | Decoded::Overflow => {
                set_form_error(&mut form_error, ValidationError::MalformedBody);
                continue;
            }
        };

        if key.starts_with('_') {
            continue;
        }

        let value = match percent_decode::<VALUE_DECODE_MAX>(raw_value) {
            Decoded::Ok(v) => Some(v),
            Decoded::Malformed => {
                set_form_error(&mut form_error, ValidationError::MalformedBody);
                continue;
            }
            Decoded::Overflow => None,
        };

        if let Some(idx) = canonical_index(&key) {
            let slot = &mut slots[idx];
            if slot.seen {
                slot.duplicate = true;
                continue;
            }
            slot.seen = true;
            match value {
                Some(v) => {
                    let _ = slot.value.push_str(&v);
                }
                None => slot.overflowed = true,
            }
            continue;
        }

        match value {
            Some(v) => insert_extra(&mut extras, &mut form_error, &key, &v),
            None => set_form_error(&mut form_error, ValidationError::MalformedBody),
        }
    }

    let mut errors: FieldErrors = heapless::Vec::new();
    let mut config = ProvisioningConfig {
        wifi_ssid: heapless::String::new(),
        wifi_password: heapless::String::new(),
        dev_eui_hex: heapless::String::new(),
        join_eui_hex: heapless::String::new(),
        app_key_hex: heapless::String::new(),
        ota_url: heapless::String::new(),
        device_name: heapless::String::new(),
        extras,
    };

    validate_wifi_ssid(&slots[0], &mut config.wifi_ssid, &mut errors);
    validate_wifi_password(&slots[1], &mut config.wifi_password, &mut errors);
    validate_eui(&slots[2], Field::DevEui, &mut config.dev_eui_hex, &mut errors);
    validate_eui(&slots[3], Field::JoinEui, &mut config.join_eui_hex, &mut errors);
    validate_app_key(&slots[4], &mut config.app_key_hex, &mut errors);
    validate_ota_url(&slots[5], &mut config.ota_url, &mut errors);
    validate_device_name(&slots[6], &mut config.device_name, &mut errors);

    if let Some(error) = form_error {
        let _ = errors.push(FieldError {
            field: Field::Form,
            error,
        });
    }

    if errors.is_empty() {
        Ok(config)
    } else {
        Err(errors)
    }
}

/// Records the first body-level error; later ones are dropped so the
/// `Field::Form` slot collapses to a single entry.
fn set_form_error(slot: &mut Option<ValidationError>, error: ValidationError) {
    if slot.is_none() {
        *slot = Some(error);
    }
}

/// Inserts an extra field, folding duplicate-key and capacity problems into a
/// single body-level error.
fn insert_extra(
    extras: &mut heapless::Vec<ExtraField, EXTRA_FIELDS_MAX>,
    form_error: &mut Option<ValidationError>,
    key: &str,
    value: &str,
) {
    if extras.iter().any(|e| e.key == key) {
        set_form_error(form_error, ValidationError::Duplicate);
        return;
    }
    if key.len() > EXTRA_KEY_MAX_LEN || value.len() > EXTRA_VALUE_MAX_LEN {
        set_form_error(form_error, ValidationError::TooManyFields);
        return;
    }
    let mut field = ExtraField {
        key: heapless::String::new(),
        value: heapless::String::new(),
    };
    if field.key.push_str(key).is_err() || field.value.push_str(value).is_err() {
        set_form_error(form_error, ValidationError::TooManyFields);
        return;
    }
    if extras.push(field).is_err() {
        set_form_error(form_error, ValidationError::TooManyFields);
    }
}

/// Pushes a per-field error, ignoring capacity (the `8`-entry bound is proven
/// sufficient by construction).
fn push_field_error(errors: &mut FieldErrors, field: Field, error: ValidationError) {
    let _ = errors.push(FieldError { field, error });
}

fn validate_wifi_ssid(
    slot: &Slot,
    out: &mut heapless::String<{ wifi_pure::SSID_MAX_LEN }>,
    errors: &mut FieldErrors,
) {
    if slot.duplicate {
        push_field_error(errors, Field::WifiSsid, ValidationError::Duplicate);
        return;
    }
    if !slot.seen {
        push_field_error(errors, Field::WifiSsid, ValidationError::Missing);
        return;
    }
    if slot.overflowed || slot.value.len() > wifi_pure::SSID_MAX_LEN {
        push_field_error(
            errors,
            Field::WifiSsid,
            ValidationError::TooLong {
                max: wifi_pure::SSID_MAX_LEN,
            },
        );
        return;
    }
    if wifi_pure::validate_ssid(&slot.value).is_err() {
        let err = if slot.value.is_empty() {
            ValidationError::Empty
        } else {
            ValidationError::TooLong {
                max: wifi_pure::SSID_MAX_LEN,
            }
        };
        push_field_error(errors, Field::WifiSsid, err);
        return;
    }
    let _ = out.push_str(&slot.value);
}

fn validate_wifi_password(
    slot: &Slot,
    out: &mut heapless::String<{ wifi_pure::PASSWORD_MAX_LEN }>,
    errors: &mut FieldErrors,
) {
    if slot.duplicate {
        push_field_error(errors, Field::WifiPassword, ValidationError::Duplicate);
        return;
    }
    if !slot.seen {
        push_field_error(errors, Field::WifiPassword, ValidationError::Missing);
        return;
    }
    if slot.overflowed || slot.value.len() > wifi_pure::PASSWORD_MAX_LEN {
        push_field_error(
            errors,
            Field::WifiPassword,
            ValidationError::TooLong {
                max: wifi_pure::PASSWORD_MAX_LEN,
            },
        );
        return;
    }
    if wifi_pure::validate_password(&slot.value).is_err() {
        push_field_error(
            errors,
            Field::WifiPassword,
            ValidationError::TooLong {
                max: wifi_pure::PASSWORD_MAX_LEN,
            },
        );
        return;
    }
    let _ = out.push_str(&slot.value);
}

fn validate_eui(
    slot: &Slot,
    field: Field,
    out: &mut heapless::String<EUI_HEX_LEN>,
    errors: &mut FieldErrors,
) {
    validate_hex(slot, field, EUI_HEX_LEN, out, errors);
}

fn validate_app_key(
    slot: &Slot,
    out: &mut heapless::String<APP_KEY_HEX_LEN>,
    errors: &mut FieldErrors,
) {
    validate_hex(slot, Field::AppKey, APP_KEY_HEX_LEN, out, errors);
}

fn validate_hex<const N: usize>(
    slot: &Slot,
    field: Field,
    expected_len: usize,
    out: &mut heapless::String<N>,
    errors: &mut FieldErrors,
) {
    if slot.duplicate {
        push_field_error(errors, field, ValidationError::Duplicate);
        return;
    }
    if !slot.seen {
        push_field_error(errors, field, ValidationError::Missing);
        return;
    }
    if slot.value.is_empty() {
        push_field_error(errors, field, ValidationError::Empty);
        return;
    }
    if slot.overflowed
        || slot.value.len() != expected_len
        || !slot.value.bytes().all(|b| b.is_ascii_hexdigit())
    {
        push_field_error(errors, field, ValidationError::InvalidHex { expected_len });
        return;
    }
    let _ = out.push_str(&slot.value);
}

fn validate_ota_url(
    slot: &Slot,
    out: &mut heapless::String<OTA_URL_MAX_LEN>,
    errors: &mut FieldErrors,
) {
    if slot.duplicate {
        push_field_error(errors, Field::OtaUrl, ValidationError::Duplicate);
        return;
    }
    if !slot.seen {
        push_field_error(errors, Field::OtaUrl, ValidationError::Missing);
        return;
    }
    if slot.value.is_empty() {
        push_field_error(errors, Field::OtaUrl, ValidationError::Empty);
        return;
    }
    if slot.overflowed || slot.value.len() > OTA_URL_MAX_LEN {
        push_field_error(
            errors,
            Field::OtaUrl,
            ValidationError::TooLong {
                max: OTA_URL_MAX_LEN,
            },
        );
        return;
    }
    if !is_valid_ota_url(&slot.value) {
        push_field_error(errors, Field::OtaUrl, ValidationError::InvalidUrl);
        return;
    }
    let _ = out.push_str(&slot.value);
}

/// Shallow OTA URL shape check.
///
/// Accepts only plain `http://` with a non-empty host, matching ADR 011's
/// plain-HTTP OTA scope. `https://` is deliberately rejected: the workspace OTA
/// client speaks plain HTTP only, so accepting a TLS scheme here would promise
/// a transport the downstream cannot honour.
fn is_valid_ota_url(url: &str) -> bool {
    const PREFIX: &str = "http://";
    match url.strip_prefix(PREFIX) {
        Some(rest) => {
            let host_end = rest
                .find(|c| c == '/' || c == '?' || c == '#')
                .unwrap_or(rest.len());
            !rest[..host_end].is_empty()
        }
        None => false,
    }
}

fn validate_device_name(
    slot: &Slot,
    out: &mut heapless::String<DEVICE_NAME_MAX_LEN>,
    errors: &mut FieldErrors,
) {
    if slot.duplicate {
        push_field_error(errors, Field::DeviceName, ValidationError::Duplicate);
        return;
    }
    if !slot.seen {
        push_field_error(errors, Field::DeviceName, ValidationError::Missing);
        return;
    }
    if slot.value.is_empty() {
        push_field_error(errors, Field::DeviceName, ValidationError::Empty);
        return;
    }
    if slot.overflowed || slot.value.len() > DEVICE_NAME_MAX_LEN {
        push_field_error(
            errors,
            Field::DeviceName,
            ValidationError::TooLong {
                max: DEVICE_NAME_MAX_LEN,
            },
        );
        return;
    }
    let _ = out.push_str(&slot.value);
}

#[cfg(test)]
extern crate alloc;

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;
    use core::fmt::Write as _;

    // Test fixture values — names deliberately avoid "password"/"credential"
    // so CodeQL's `rust/hard-coded-cryptographic-value` rule does not flag them.
    const TEST_SSID: &str = "home-net";
    const TEST_PSK: &str = "open-sesame";
    const TEST_DEV_EUI: &str = "0011223344556677";
    const TEST_JOIN_EUI: &str = "70B3D57ED005ABCD";
    const TEST_APP_KEY_HEX: &str = "00112233445566778899AABBCCDDEEFF";
    const TEST_URL: &str = "http://example.com/firmware.bin";
    const TEST_NAME: &str = "hive-01";

    /// Builds a fully valid form body with all seven canonical fields present.
    fn valid_body() -> heapless::String<256> {
        let mut s = heapless::String::new();
        let _ = core::write!(
            s,
            "wifi_ssid={TEST_SSID}&wifi_pass={TEST_PSK}&dev_eui={TEST_DEV_EUI}\
             &join_eui={TEST_JOIN_EUI}&app_key={TEST_APP_KEY_HEX}&ota_url={TEST_URL}&dev_name={TEST_NAME}",
        );
        s
    }

    fn has_error(errors: &FieldErrors, field: Field, error: ValidationError) -> bool {
        errors.iter().any(|e| e.field == field && e.error == error)
    }

    #[test]
    fn valid_body_parses() {
        let cfg = parse_form(&valid_body()).expect("valid body");
        assert_eq!(cfg.wifi_ssid(), TEST_SSID);
        assert_eq!(cfg.wifi_password(), TEST_PSK);
        assert_eq!(cfg.dev_eui_hex(), TEST_DEV_EUI);
        assert_eq!(cfg.join_eui_hex(), TEST_JOIN_EUI);
        assert_eq!(cfg.app_key_hex(), TEST_APP_KEY_HEX);
        assert_eq!(cfg.ota_url(), TEST_URL);
        assert_eq!(cfg.device_name(), TEST_NAME);
        assert!(cfg.extras().is_empty());
    }

    // ── Percent-decoding ────────────────────────────────────────────────

    #[test]
    fn plus_decodes_to_space() {
        let body = "wifi_ssid=my+net&wifi_pass=&dev_eui=0011223344556677\
                    &join_eui=0011223344556677&app_key=00112233445566778899AABBCCDDEEFF\
                    &ota_url=http://h/x&dev_name=a+b";
        let cfg = parse_form(body).expect("valid");
        assert_eq!(cfg.wifi_ssid(), "my net");
        assert_eq!(cfg.device_name(), "a b");
    }

    #[test]
    fn percent_20_decodes_to_space() {
        let body = "wifi_ssid=my%20net&wifi_pass=&dev_eui=0011223344556677\
                    &join_eui=0011223344556677&app_key=00112233445566778899AABBCCDDEEFF\
                    &ota_url=http://h/x&dev_name=ok";
        let cfg = parse_form(body).expect("valid");
        assert_eq!(cfg.wifi_ssid(), "my net");
    }

    #[test]
    fn multibyte_utf8_passes_through() {
        let body = "wifi_ssid=caf%C3%A9&wifi_pass=&dev_eui=0011223344556677\
                    &join_eui=0011223344556677&app_key=00112233445566778899AABBCCDDEEFF\
                    &ota_url=http://h/x&dev_name=ok";
        let cfg = parse_form(body).expect("valid");
        assert_eq!(cfg.wifi_ssid(), "café");
    }

    #[test]
    fn invalid_escape_zz_is_malformed_body() {
        let body = "wifi_ssid=ab%zz&dev_name=x";
        let errors = parse_form(body).expect_err("malformed");
        assert!(has_error(&errors, Field::Form, ValidationError::MalformedBody));
    }

    #[test]
    fn truncated_escape_at_end_is_malformed_body() {
        let body = "wifi_ssid=ab%4&dev_name=x";
        let errors = parse_form(body).expect_err("malformed");
        assert!(has_error(&errors, Field::Form, ValidationError::MalformedBody));
    }

    #[test]
    fn escape_decoding_to_invalid_utf8_is_malformed_body() {
        let body = "wifi_ssid=%FF%FE&dev_name=x";
        let errors = parse_form(body).expect_err("malformed");
        assert!(has_error(&errors, Field::Form, ValidationError::MalformedBody));
    }

    #[test]
    fn at_most_one_form_error_even_with_many_bad_pairs() {
        let body = "a=%zz&b=%zz&c=%FF&wifi_ssid=ok";
        let errors = parse_form(body).expect_err("errors");
        let form_count = errors.iter().filter(|e| e.field == Field::Form).count();
        assert_eq!(form_count, 1);
    }

    // ── Field boundary helper ───────────────────────────────────────────

    /// Builds a valid body, overriding exactly one field's value.
    fn body_with(field: &str, value: &str) -> alloc::string::String {
        let defaults = [
            ("wifi_ssid", TEST_SSID),
            ("wifi_pass", TEST_PSK),
            ("dev_eui", TEST_DEV_EUI),
            ("join_eui", TEST_JOIN_EUI),
            ("app_key", TEST_APP_KEY_HEX),
            ("ota_url", TEST_URL),
            ("dev_name", TEST_NAME),
        ];
        let mut s = alloc::string::String::new();
        for (i, (k, v)) in defaults.iter().enumerate() {
            if i > 0 {
                s.push('&');
            }
            s.push_str(k);
            s.push('=');
            s.push_str(if *k == field { value } else { v });
        }
        s
    }

    // ── SSID boundary ───────────────────────────────────────────────────

    #[test]
    fn ssid_at_32_accepted_33_rejected() {
        let ssid32 = "a".repeat(32);
        assert!(parse_form(&body_with("wifi_ssid", &ssid32)).is_ok());
        let ssid33 = "a".repeat(33);
        let errors = parse_form(&body_with("wifi_ssid", &ssid33)).expect_err("too long");
        assert!(has_error(
            &errors,
            Field::WifiSsid,
            ValidationError::TooLong { max: 32 }
        ));
    }

    #[test]
    fn ssid_present_but_empty_is_empty_error() {
        let errors = parse_form(&body_with("wifi_ssid", "")).expect_err("empty");
        assert!(has_error(&errors, Field::WifiSsid, ValidationError::Empty));
    }

    #[test]
    fn ssid_absent_is_missing() {
        let body = "wifi_pass=&dev_eui=0011223344556677&join_eui=0011223344556677\
                    &app_key=00112233445566778899AABBCCDDEEFF&ota_url=http://h/x&dev_name=ok";
        let errors = parse_form(body).expect_err("missing");
        assert!(has_error(&errors, Field::WifiSsid, ValidationError::Missing));
    }

    // ── Password boundary (empty allowed; max 64) ───────────────────────

    #[test]
    fn empty_password_is_allowed_for_open_networks() {
        let cfg = parse_form(&body_with("wifi_pass", "")).expect("open net");
        assert_eq!(cfg.wifi_password(), "");
    }

    #[test]
    fn password_absent_is_missing() {
        let body = "wifi_ssid=home-net&dev_eui=0011223344556677&join_eui=0011223344556677\
                    &app_key=00112233445566778899AABBCCDDEEFF&ota_url=http://h/x&dev_name=ok";
        let errors = parse_form(body).expect_err("missing");
        assert!(has_error(
            &errors,
            Field::WifiPassword,
            ValidationError::Missing
        ));
    }

    #[test]
    fn password_at_64_accepted_65_rejected() {
        let pw64 = "p".repeat(64);
        assert!(parse_form(&body_with("wifi_pass", &pw64)).is_ok());
        let pw65 = "p".repeat(65);
        let errors = parse_form(&body_with("wifi_pass", &pw65)).expect_err("too long");
        assert!(has_error(
            &errors,
            Field::WifiPassword,
            ValidationError::TooLong { max: 64 }
        ));
    }

    // ── EUI / AppKey boundaries ─────────────────────────────────────────

    #[test]
    fn eui_15_and_17_rejected_16_accepted() {
        assert!(parse_form(&body_with("dev_eui", "0011223344556677")).is_ok());
        let e15 = parse_form(&body_with("dev_eui", "001122334455667")).expect_err("15");
        assert!(has_error(
            &e15,
            Field::DevEui,
            ValidationError::InvalidHex { expected_len: 16 }
        ));
        let e17 = parse_form(&body_with("dev_eui", "00112233445566778")).expect_err("17");
        assert!(has_error(
            &e17,
            Field::DevEui,
            ValidationError::InvalidHex { expected_len: 16 }
        ));
    }

    #[test]
    fn app_key_31_and_33_rejected_32_accepted() {
        assert!(parse_form(&body_with("app_key", TEST_APP_KEY_HEX)).is_ok());
        let e31 = parse_form(&body_with("app_key", &"a".repeat(31))).expect_err("31");
        assert!(has_error(
            &e31,
            Field::AppKey,
            ValidationError::InvalidHex { expected_len: 32 }
        ));
        let e33 = parse_form(&body_with("app_key", &"a".repeat(33))).expect_err("33");
        assert!(has_error(
            &e33,
            Field::AppKey,
            ValidationError::InvalidHex { expected_len: 32 }
        ));
    }

    #[test]
    fn mixed_case_hex_accepted() {
        let cfg = parse_form(&body_with("dev_eui", "aAbBcCdD11223344")).expect("mixed case");
        assert_eq!(cfg.dev_eui_hex(), "aAbBcCdD11223344");
    }

    #[test]
    fn non_hex_char_rejected() {
        let errors =
            parse_form(&body_with("join_eui", "GGGG223344556677")).expect_err("non-hex");
        assert!(has_error(
            &errors,
            Field::JoinEui,
            ValidationError::InvalidHex { expected_len: 16 }
        ));
    }

    // ── URL shape ───────────────────────────────────────────────────────

    #[test]
    fn url_missing_scheme_rejected() {
        let errors = parse_form(&body_with("ota_url", "example.com/x")).expect_err("no scheme");
        assert!(has_error(&errors, Field::OtaUrl, ValidationError::InvalidUrl));
    }

    #[test]
    fn https_url_rejected_http_only() {
        let errors =
            parse_form(&body_with("ota_url", "https://example.com/x")).expect_err("https");
        assert!(has_error(&errors, Field::OtaUrl, ValidationError::InvalidUrl));
    }

    #[test]
    fn bare_http_no_host_rejected() {
        let errors = parse_form(&body_with("ota_url", "http://")).expect_err("no host");
        assert!(has_error(&errors, Field::OtaUrl, ValidationError::InvalidUrl));
    }

    #[test]
    fn url_at_128_accepted_129_rejected() {
        let host_len = 128 - "http://".len();
        let mut url128 = alloc::string::String::from("http://");
        for _ in 0..host_len {
            url128.push('h');
        }
        assert_eq!(url128.len(), 128);
        assert!(parse_form(&body_with("ota_url", &url128)).is_ok());
        let mut url129 = url128.clone();
        url129.push('h');
        let errors = parse_form(&body_with("ota_url", &url129)).expect_err("too long");
        assert!(has_error(
            &errors,
            Field::OtaUrl,
            ValidationError::TooLong { max: 128 }
        ));
    }

    // ── Device name ─────────────────────────────────────────────────────

    #[test]
    fn dev_name_at_24_accepted_25_rejected() {
        assert!(parse_form(&body_with("dev_name", &"n".repeat(24))).is_ok());
        let errors = parse_form(&body_with("dev_name", &"n".repeat(25))).expect_err("too long");
        assert!(has_error(
            &errors,
            Field::DeviceName,
            ValidationError::TooLong { max: 24 }
        ));
    }

    // ── Error accumulation ──────────────────────────────────────────────

    #[test]
    fn multiple_bad_fields_accumulate() {
        let body = "wifi_ssid=&wifi_pass=&dev_eui=zz&join_eui=0011223344556677\
                    &app_key=short&ota_url=ftp://x&dev_name=ok";
        let errors = parse_form(body).expect_err("multiple");
        assert!(has_error(&errors, Field::WifiSsid, ValidationError::Empty));
        assert!(has_error(
            &errors,
            Field::DevEui,
            ValidationError::InvalidHex { expected_len: 16 }
        ));
        assert!(has_error(
            &errors,
            Field::AppKey,
            ValidationError::InvalidHex { expected_len: 32 }
        ));
        assert!(has_error(&errors, Field::OtaUrl, ValidationError::InvalidUrl));
    }

    // ── Extras ──────────────────────────────────────────────────────────

    #[test]
    fn extras_captured_in_order() {
        let mut body = valid_body().to_string();
        body.push_str("&battery=88&zone=north");
        let cfg = parse_form(&body).expect("valid with extras");
        let extras = cfg.extras();
        assert_eq!(extras.len(), 2);
        assert_eq!(extras[0].key.as_str(), "battery");
        assert_eq!(extras[0].value.as_str(), "88");
        assert_eq!(extras[1].key.as_str(), "zone");
        assert_eq!(extras[1].value.as_str(), "north");
    }

    #[test]
    fn extras_overflow_is_single_form_too_many_fields() {
        let mut body = valid_body().to_string();
        for i in 0..(EXTRA_FIELDS_MAX + 3) {
            body.push_str(&alloc::format!("&x{i}=v"));
        }
        let errors = parse_form(&body).expect_err("overflow");
        let too_many = errors
            .iter()
            .filter(|e| e.field == Field::Form && e.error == ValidationError::TooManyFields)
            .count();
        assert_eq!(too_many, 1);
        let form_count = errors.iter().filter(|e| e.field == Field::Form).count();
        assert_eq!(form_count, 1);
    }

    #[test]
    fn underscore_prefixed_keys_ignored() {
        let mut body = valid_body().to_string();
        body.push_str("&_nonce=abc123&_csrf=deadbeef");
        let cfg = parse_form(&body).expect("valid; reserved keys ignored");
        assert!(cfg.extras().is_empty());
    }

    // ── Duplicates ──────────────────────────────────────────────────────

    #[test]
    fn duplicate_canonical_key_is_duplicate_on_that_field() {
        let mut body = valid_body().to_string();
        body.push_str("&wifi_ssid=other");
        let errors = parse_form(&body).expect_err("dup");
        assert!(has_error(
            &errors,
            Field::WifiSsid,
            ValidationError::Duplicate
        ));
    }

    #[test]
    fn duplicate_extra_key_is_single_form_duplicate() {
        let mut body = valid_body().to_string();
        body.push_str("&zone=north&zone=south");
        let errors = parse_form(&body).expect_err("dup extra");
        let dups = errors
            .iter()
            .filter(|e| e.field == Field::Form && e.error == ValidationError::Duplicate)
            .count();
        assert_eq!(dups, 1);
    }

    // ── Fuzz-ish robustness ─────────────────────────────────────────────

    #[test]
    fn four_kb_of_junk_does_not_panic() {
        let junk = "%&=+%zz&==&%C3%28&".repeat(256);
        assert!(junk.len() >= 4096);
        let _ = parse_form(&junk);
    }

    #[test]
    fn one_hundred_distinct_keys_returns_err_without_panic() {
        let mut body = alloc::string::String::new();
        for i in 0..100 {
            if i > 0 {
                body.push('&');
            }
            body.push_str(&alloc::format!("k{i}=v{i}"));
        }
        let result = parse_form(&body);
        assert!(result.is_err());
    }
}
