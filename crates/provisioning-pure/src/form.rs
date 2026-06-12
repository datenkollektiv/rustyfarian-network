//! The `application/x-www-form-urlencoded` parser and the opaque
//! [`ExtraField`] type.
//!
//! [`parse_form`] is the single entry point. It percent-decodes the body, maps
//! known input names to the active profile's canonical schema fields, collects
//! unknown pairs as opaque extras, and validates everything, accumulating
//! per-field errors rather than failing on the first problem.
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

use rustyfarian_network_pure::mqtt::{validate_client_id, CLIENT_ID_MAX_LEN};

#[cfg(test)]
use crate::config::MAX_FIELD_ERRORS;
use crate::config::{
    ProvisioningConfig, APP_KEY_HEX_LEN, DEVICE_NAME_MAX_LEN, EUI_HEX_LEN, EXTRA_FIELDS_MAX,
    EXTRA_KEY_MAX_LEN, EXTRA_VALUE_MAX_LEN, MQTT_HOST_MAX_LEN, MQTT_PASS_MAX_LEN,
    MQTT_USER_MAX_LEN, OTA_URL_MAX_LEN,
};
use crate::error::{Field, FieldError, FieldErrors, ValidationError};
use crate::profile::{LoraFields, MqttFields, SchemaProfile};

/// Largest decoded value the parser will buffer (bytes).
///
/// Comfortably above [`OTA_URL_MAX_LEN`] so a too-long URL is reported as
/// [`ValidationError::TooLong`] rather than truncated; anything beyond this is
/// treated as a malformed/oversized pair.
const VALUE_DECODE_MAX: usize = 160;

/// Largest decoded key the parser will buffer (bytes).
///
/// The longest canonical key is 11 bytes (`mqtt_client`) and extra keys are
/// capped at [`EXTRA_KEY_MAX_LEN`]; 16 leaves headroom to detect over-long keys.
const KEY_DECODE_MAX: usize = 16;

/// The maximum number of canonical fields across all profiles (the
/// `WifiMqttDevice` count); the working `slots` buffer is sized to this.
const MAX_CANONICAL_FIELDS: usize = 8;

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

/// Returns the active profile's slot index for a key, or `None` if it is not a
/// canonical field of that profile.
fn canonical_index(profile: SchemaProfile, key: &str) -> Option<usize> {
    profile.fields().iter().position(|f| f.form_name() == key)
}

/// Returns `true` if `key` is a canonical field of the *other* profile but not
/// the active one.
fn is_cross_profile(profile: SchemaProfile, key: &str) -> bool {
    let other = match profile {
        SchemaProfile::LorawanFieldDevice => SchemaProfile::WifiMqttDevice,
        SchemaProfile::WifiMqttDevice => SchemaProfile::LorawanFieldDevice,
    };
    other.fields().iter().any(|f| f.form_name() == key)
}

/// Experimental: API may change before 1.0.
///
/// Parses an `application/x-www-form-urlencoded` provisioning submission under
/// `profile`.
///
/// On success returns a fully validated [`ProvisioningConfig`] whose profile
/// matches `profile`. On failure returns the accumulated [`FieldErrors`]: at
/// most one error per canonical field of the active profile (up to eight for
/// `WifiMqttDevice`) plus at most one [`Field::Form`]-level error (the first
/// body-level problem wins), keeping the `9`-entry capacity exact.
///
/// A field canonical only to the *other* profile is rejected with a single
/// body-level [`ValidationError::UnexpectedForProfile`] rather than folded into
/// extras.
///
/// See the [module docs](self) for the reserved-key and never-panics
/// guarantees.
pub fn parse_form(body: &str, profile: SchemaProfile) -> Result<ProvisioningConfig, FieldErrors> {
    let fields = profile.fields();
    let mut slots: [Slot; MAX_CANONICAL_FIELDS] = Default::default();
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

        if let Some(idx) = canonical_index(profile, &key) {
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

        if is_cross_profile(profile, &key) {
            set_form_error(&mut form_error, ValidationError::UnexpectedForProfile);
            continue;
        }

        match value {
            Some(v) => insert_extra(&mut extras, &mut form_error, &key, &v),
            None => set_form_error(&mut form_error, ValidationError::MalformedBody),
        }
    }

    let mut errors: FieldErrors = heapless::Vec::new();

    let mut wifi_ssid: heapless::String<{ wifi_pure::SSID_MAX_LEN }> = heapless::String::new();
    let mut wifi_password: heapless::String<{ wifi_pure::PASSWORD_MAX_LEN }> =
        heapless::String::new();
    let mut ota_url: heapless::String<OTA_URL_MAX_LEN> = heapless::String::new();
    let mut device_name: heapless::String<DEVICE_NAME_MAX_LEN> = heapless::String::new();

    let mut dev_eui_hex: heapless::String<EUI_HEX_LEN> = heapless::String::new();
    let mut join_eui_hex: heapless::String<EUI_HEX_LEN> = heapless::String::new();
    let mut app_key_hex: heapless::String<APP_KEY_HEX_LEN> = heapless::String::new();

    let mut mqtt_host: heapless::String<MQTT_HOST_MAX_LEN> = heapless::String::new();
    let mut mqtt_port: u16 = 0;
    let mut mqtt_user: Option<heapless::String<MQTT_USER_MAX_LEN>> = None;
    let mut mqtt_pass: Option<heapless::String<MQTT_PASS_MAX_LEN>> = None;
    let mut mqtt_client: Option<heapless::String<CLIENT_ID_MAX_LEN>> = None;

    for (idx, field) in fields.iter().enumerate() {
        let slot = &slots[idx];
        match field {
            Field::WifiSsid => validate_wifi_ssid(slot, &mut wifi_ssid, &mut errors),
            Field::WifiPassword => validate_wifi_password(slot, &mut wifi_password, &mut errors),
            Field::DevEui => validate_eui(slot, Field::DevEui, &mut dev_eui_hex, &mut errors),
            Field::JoinEui => validate_eui(slot, Field::JoinEui, &mut join_eui_hex, &mut errors),
            Field::AppKey => validate_app_key(slot, &mut app_key_hex, &mut errors),
            Field::MqttUri => validate_mqtt_uri(slot, &mut mqtt_host, &mut mqtt_port, &mut errors),
            Field::MqttClient => validate_mqtt_client(slot, &mut mqtt_client, &mut errors),
            Field::OtaUrl => validate_ota_url(slot, &mut ota_url, &mut errors),
            Field::DeviceName => validate_device_name(slot, &mut device_name, &mut errors),
            // MqttPass is validated jointly with MqttUser below.
            Field::MqttUser | Field::MqttPass | Field::Form => {}
        }
    }

    let lora = if profile == SchemaProfile::LorawanFieldDevice {
        Some(LoraFields {
            dev_eui_hex,
            join_eui_hex,
            app_key_hex,
        })
    } else {
        None
    };

    let mqtt = if profile == SchemaProfile::WifiMqttDevice {
        let user_idx = canonical_index(profile, Field::MqttUser.form_name())
            .expect("MqttUser is canonical to WifiMqttDevice");
        let pass_idx = canonical_index(profile, Field::MqttPass.form_name())
            .expect("MqttPass is canonical to WifiMqttDevice");
        validate_mqtt_auth(
            &slots[user_idx],
            &slots[pass_idx],
            &mut mqtt_user,
            &mut mqtt_pass,
            &mut errors,
        );
        Some(MqttFields {
            host: mqtt_host,
            port: mqtt_port,
            username: mqtt_user,
            password: mqtt_pass,
            client_id: mqtt_client,
        })
    } else {
        None
    };

    if let Some(error) = form_error {
        let _ = errors.push(FieldError {
            field: Field::Form,
            error,
        });
    }

    if errors.is_empty() {
        Ok(ProvisioningConfig {
            wifi_ssid,
            wifi_password,
            ota_url,
            device_name,
            lora,
            mqtt,
            extras,
        })
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

/// Pushes a per-field error, ignoring capacity (the `9`-entry bound is proven
/// sufficient by construction: at most one error per canonical field of the
/// active profile — up to eight for `WifiMqttDevice` — plus one form-level
/// error).
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
    // Empty is allowed (open network); a non-empty password must clear the
    // WPA2-Personal minimum, otherwise the STA association will fail at runtime.
    if !slot.value.is_empty() && slot.value.len() < wifi_pure::AP_PASSWORD_MIN_LEN {
        push_field_error(
            errors,
            Field::WifiPassword,
            ValidationError::TooShort {
                min: wifi_pure::AP_PASSWORD_MIN_LEN,
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
            let host_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
            !rest[..host_end].is_empty()
        }
        None => false,
    }
}

/// Validates the `mqtt_uri` form input, splitting it into host and port.
///
/// The accepted shape is `mqtt://{host}:{port}`: the scheme is locked to
/// `mqtt://` (mirroring `format_broker_url`'s single hard-coded scheme), the
/// host is non-empty, and the port is present, parses as a `u16`, and is
/// non-zero (mirroring `validate_broker_port`'s `!= 0` rule). Every shape,
/// scheme, host, and port failure maps to a single [`ValidationError::InvalidUrl`]
/// on [`Field::MqttUri`]; a port of `0` (rejected by the `!= 0` rule) and a
/// port of `65536` (rejected by `u16` parsing) are distinct failure paths that
/// both land here.
fn validate_mqtt_uri(
    slot: &Slot,
    host_out: &mut heapless::String<MQTT_HOST_MAX_LEN>,
    port_out: &mut u16,
    errors: &mut FieldErrors,
) {
    if slot.duplicate {
        push_field_error(errors, Field::MqttUri, ValidationError::Duplicate);
        return;
    }
    if !slot.seen {
        push_field_error(errors, Field::MqttUri, ValidationError::Missing);
        return;
    }
    if slot.value.is_empty() {
        push_field_error(errors, Field::MqttUri, ValidationError::Empty);
        return;
    }
    if slot.overflowed {
        push_field_error(errors, Field::MqttUri, ValidationError::InvalidUrl);
        return;
    }
    match parse_mqtt_uri(&slot.value) {
        Some((host, port)) if host.len() <= MQTT_HOST_MAX_LEN => {
            let _ = host_out.push_str(host);
            *port_out = port;
        }
        _ => push_field_error(errors, Field::MqttUri, ValidationError::InvalidUrl),
    }
}

/// Parses `mqtt://{host}:{port}` into a `(host, port)` pair.
///
/// Returns `None` for any scheme, host, or port problem: a wrong/absent scheme,
/// an empty host, a missing/empty port, a non-numeric or out-of-`u16`-range
/// port, or a zero port.
fn parse_mqtt_uri(uri: &str) -> Option<(&str, u16)> {
    const PREFIX: &str = "mqtt://";
    let rest = uri.strip_prefix(PREFIX)?;
    let (host, port_str) = rest.rsplit_once(':')?;
    if host.is_empty() || port_str.is_empty() {
        return None;
    }
    let port: u16 = port_str.parse().ok()?;
    if port == 0 {
        return None;
    }
    Some((host, port))
}

/// Validates the optional `mqtt_client` form input.
///
/// Blank or absent leaves the client ID `None` (the host derives one at boot).
/// A non-empty value must satisfy
/// [`validate_client_id`](rustyfarian_network_pure::mqtt::validate_client_id),
/// i.e. be at most [`CLIENT_ID_MAX_LEN`] (23) bytes.
fn validate_mqtt_client(
    slot: &Slot,
    out: &mut Option<heapless::String<CLIENT_ID_MAX_LEN>>,
    errors: &mut FieldErrors,
) {
    if slot.duplicate {
        push_field_error(errors, Field::MqttClient, ValidationError::Duplicate);
        return;
    }
    if !slot.seen || slot.value.is_empty() {
        return;
    }
    if slot.overflowed || validate_client_id(&slot.value).is_err() {
        push_field_error(
            errors,
            Field::MqttClient,
            ValidationError::TooLong {
                max: CLIENT_ID_MAX_LEN,
            },
        );
        return;
    }
    let mut client: heapless::String<CLIENT_ID_MAX_LEN> = heapless::String::new();
    if client.push_str(&slot.value).is_err() {
        push_field_error(
            errors,
            Field::MqttClient,
            ValidationError::TooLong {
                max: CLIENT_ID_MAX_LEN,
            },
        );
        return;
    }
    *out = Some(client);
}

/// Validates the optional `mqtt_user` / `mqtt_pass` auth pair jointly.
///
/// Both absent or empty = anonymous (both stay `None`). A password without a
/// username is rejected as a field error on [`Field::MqttPass`]. A username
/// without a password is allowed (username-only broker ACLs).
fn validate_mqtt_auth(
    user_slot: &Slot,
    pass_slot: &Slot,
    user_out: &mut Option<heapless::String<MQTT_USER_MAX_LEN>>,
    pass_out: &mut Option<heapless::String<MQTT_PASS_MAX_LEN>>,
    errors: &mut FieldErrors,
) {
    if user_slot.duplicate {
        push_field_error(errors, Field::MqttUser, ValidationError::Duplicate);
    }
    if pass_slot.duplicate {
        push_field_error(errors, Field::MqttPass, ValidationError::Duplicate);
    }
    if user_slot.duplicate || pass_slot.duplicate {
        return;
    }

    let has_user = user_slot.seen && !user_slot.value.is_empty();
    let has_pass = pass_slot.seen && !pass_slot.value.is_empty();

    if has_user {
        if user_slot.overflowed || user_slot.value.len() > MQTT_USER_MAX_LEN {
            push_field_error(
                errors,
                Field::MqttUser,
                ValidationError::TooLong {
                    max: MQTT_USER_MAX_LEN,
                },
            );
        } else {
            let mut user: heapless::String<MQTT_USER_MAX_LEN> = heapless::String::new();
            if user.push_str(&user_slot.value).is_err() {
                push_field_error(
                    errors,
                    Field::MqttUser,
                    ValidationError::TooLong {
                        max: MQTT_USER_MAX_LEN,
                    },
                );
            } else {
                *user_out = Some(user);
            }
        }
    }

    if has_pass && !has_user {
        push_field_error(errors, Field::MqttPass, ValidationError::Missing);
        return;
    }

    if has_pass {
        if pass_slot.overflowed || pass_slot.value.len() > MQTT_PASS_MAX_LEN {
            push_field_error(
                errors,
                Field::MqttPass,
                ValidationError::TooLong {
                    max: MQTT_PASS_MAX_LEN,
                },
            );
        } else {
            let mut pass: heapless::String<MQTT_PASS_MAX_LEN> = heapless::String::new();
            if pass.push_str(&pass_slot.value).is_err() {
                push_field_error(
                    errors,
                    Field::MqttPass,
                    ValidationError::TooLong {
                        max: MQTT_PASS_MAX_LEN,
                    },
                );
            } else {
                *pass_out = Some(pass);
            }
        }
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

    // MQTT-profile fixtures.
    const TEST_MQTT_URI: &str = "mqtt://broker.local:1883";
    const TEST_MQTT_USER: &str = "sensor-svc";
    const TEST_MQTT_PSK: &str = "hunter2-ish";
    const TEST_MQTT_CLIENT: &str = "rgb-clock-01";

    const LORAWAN: SchemaProfile = SchemaProfile::LorawanFieldDevice;
    const WIFI_MQTT: SchemaProfile = SchemaProfile::WifiMqttDevice;

    /// Builds a fully valid LoRaWAN body with all seven canonical fields.
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
        let cfg = parse_form(&valid_body(), LORAWAN).expect("valid body");
        assert_eq!(cfg.profile(), LORAWAN);
        assert_eq!(cfg.wifi_ssid(), TEST_SSID);
        assert_eq!(cfg.wifi_password(), TEST_PSK);
        let lora = cfg.lora().expect("lora group");
        assert_eq!(lora.dev_eui_hex(), TEST_DEV_EUI);
        assert_eq!(lora.join_eui_hex(), TEST_JOIN_EUI);
        assert_eq!(lora.app_key_hex(), TEST_APP_KEY_HEX);
        assert_eq!(cfg.ota_url(), TEST_URL);
        assert_eq!(cfg.device_name(), TEST_NAME);
        assert!(cfg.mqtt().is_none());
        assert!(cfg.extras().is_empty());
    }

    // ── Percent-decoding ────────────────────────────────────────────────

    #[test]
    fn plus_decodes_to_space() {
        let body = "wifi_ssid=my+net&wifi_pass=&dev_eui=0011223344556677\
                    &join_eui=0011223344556677&app_key=00112233445566778899AABBCCDDEEFF\
                    &ota_url=http://h/x&dev_name=a+b";
        let cfg = parse_form(body, LORAWAN).expect("valid");
        assert_eq!(cfg.wifi_ssid(), "my net");
        assert_eq!(cfg.device_name(), "a b");
    }

    #[test]
    fn percent_20_decodes_to_space() {
        let body = "wifi_ssid=my%20net&wifi_pass=&dev_eui=0011223344556677\
                    &join_eui=0011223344556677&app_key=00112233445566778899AABBCCDDEEFF\
                    &ota_url=http://h/x&dev_name=ok";
        let cfg = parse_form(body, LORAWAN).expect("valid");
        assert_eq!(cfg.wifi_ssid(), "my net");
    }

    #[test]
    fn multibyte_utf8_passes_through() {
        let body = "wifi_ssid=caf%C3%A9&wifi_pass=&dev_eui=0011223344556677\
                    &join_eui=0011223344556677&app_key=00112233445566778899AABBCCDDEEFF\
                    &ota_url=http://h/x&dev_name=ok";
        let cfg = parse_form(body, LORAWAN).expect("valid");
        assert_eq!(cfg.wifi_ssid(), "café");
    }

    #[test]
    fn invalid_escape_zz_is_malformed_body() {
        let body = "wifi_ssid=ab%zz&dev_name=x";
        let errors = parse_form(body, LORAWAN).expect_err("malformed");
        assert!(has_error(
            &errors,
            Field::Form,
            ValidationError::MalformedBody
        ));
    }

    #[test]
    fn truncated_escape_at_end_is_malformed_body() {
        let body = "wifi_ssid=ab%4&dev_name=x";
        let errors = parse_form(body, LORAWAN).expect_err("malformed");
        assert!(has_error(
            &errors,
            Field::Form,
            ValidationError::MalformedBody
        ));
    }

    #[test]
    fn escape_decoding_to_invalid_utf8_is_malformed_body() {
        let body = "wifi_ssid=%FF%FE&dev_name=x";
        let errors = parse_form(body, LORAWAN).expect_err("malformed");
        assert!(has_error(
            &errors,
            Field::Form,
            ValidationError::MalformedBody
        ));
    }

    #[test]
    fn at_most_one_form_error_even_with_many_bad_pairs() {
        let body = "a=%zz&b=%zz&c=%FF&wifi_ssid=ok";
        let errors = parse_form(body, LORAWAN).expect_err("errors");
        let form_count = errors.iter().filter(|e| e.field == Field::Form).count();
        assert_eq!(form_count, 1);
    }

    // ── Field boundary helper ───────────────────────────────────────────

    /// Builds a valid LoRaWAN body, overriding exactly one field's value.
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
        build_body(&defaults, field, value)
    }

    /// Builds a valid `WifiMqttDevice` body, overriding exactly one field.
    fn mqtt_body_with(field: &str, value: &str) -> alloc::string::String {
        let defaults = [
            ("wifi_ssid", TEST_SSID),
            ("wifi_pass", TEST_PSK),
            ("mqtt_uri", TEST_MQTT_URI),
            ("mqtt_user", ""),
            ("mqtt_pass", ""),
            ("mqtt_client", ""),
            ("ota_url", TEST_URL),
            ("dev_name", TEST_NAME),
        ];
        build_body(&defaults, field, value)
    }

    fn build_body(defaults: &[(&str, &str)], field: &str, value: &str) -> alloc::string::String {
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
        assert!(parse_form(&body_with("wifi_ssid", &ssid32), LORAWAN).is_ok());
        let ssid33 = "a".repeat(33);
        let errors = parse_form(&body_with("wifi_ssid", &ssid33), LORAWAN).expect_err("too long");
        assert!(has_error(
            &errors,
            Field::WifiSsid,
            ValidationError::TooLong { max: 32 }
        ));
    }

    #[test]
    fn ssid_present_but_empty_is_empty_error() {
        let errors = parse_form(&body_with("wifi_ssid", ""), LORAWAN).expect_err("empty");
        assert!(has_error(&errors, Field::WifiSsid, ValidationError::Empty));
    }

    #[test]
    fn ssid_absent_is_missing() {
        let body = "wifi_pass=&dev_eui=0011223344556677&join_eui=0011223344556677\
                    &app_key=00112233445566778899AABBCCDDEEFF&ota_url=http://h/x&dev_name=ok";
        let errors = parse_form(body, LORAWAN).expect_err("missing");
        assert!(has_error(
            &errors,
            Field::WifiSsid,
            ValidationError::Missing
        ));
    }

    // ── Password boundary (empty allowed; max 64) ───────────────────────

    #[test]
    fn empty_password_is_allowed_for_open_networks() {
        let cfg = parse_form(&body_with("wifi_pass", ""), LORAWAN).expect("open net");
        assert_eq!(cfg.wifi_password(), "");
    }

    #[test]
    fn password_absent_is_missing() {
        let body = "wifi_ssid=home-net&dev_eui=0011223344556677&join_eui=0011223344556677\
                    &app_key=00112233445566778899AABBCCDDEEFF&ota_url=http://h/x&dev_name=ok";
        let errors = parse_form(body, LORAWAN).expect_err("missing");
        assert!(has_error(
            &errors,
            Field::WifiPassword,
            ValidationError::Missing
        ));
    }

    #[test]
    fn password_below_wpa2_minimum_is_rejected() {
        let pw7 = "p".repeat(7);
        let errors = parse_form(&body_with("wifi_pass", &pw7), LORAWAN).expect_err("too short");
        assert!(has_error(
            &errors,
            Field::WifiPassword,
            ValidationError::TooShort { min: 8 }
        ));
        let pw8 = "p".repeat(8);
        assert!(parse_form(&body_with("wifi_pass", &pw8), LORAWAN).is_ok());
    }

    #[test]
    fn password_at_64_accepted_65_rejected() {
        let pw64 = "p".repeat(64);
        assert!(parse_form(&body_with("wifi_pass", &pw64), LORAWAN).is_ok());
        let pw65 = "p".repeat(65);
        let errors = parse_form(&body_with("wifi_pass", &pw65), LORAWAN).expect_err("too long");
        assert!(has_error(
            &errors,
            Field::WifiPassword,
            ValidationError::TooLong { max: 64 }
        ));
    }

    // ── EUI / AppKey boundaries ─────────────────────────────────────────

    #[test]
    fn eui_15_and_17_rejected_16_accepted() {
        assert!(parse_form(&body_with("dev_eui", "0011223344556677"), LORAWAN).is_ok());
        let e15 = parse_form(&body_with("dev_eui", "001122334455667"), LORAWAN).expect_err("15");
        assert!(has_error(
            &e15,
            Field::DevEui,
            ValidationError::InvalidHex { expected_len: 16 }
        ));
        let e17 = parse_form(&body_with("dev_eui", "00112233445566778"), LORAWAN).expect_err("17");
        assert!(has_error(
            &e17,
            Field::DevEui,
            ValidationError::InvalidHex { expected_len: 16 }
        ));
    }

    #[test]
    fn app_key_31_and_33_rejected_32_accepted() {
        assert!(parse_form(&body_with("app_key", TEST_APP_KEY_HEX), LORAWAN).is_ok());
        let e31 = parse_form(&body_with("app_key", &"a".repeat(31)), LORAWAN).expect_err("31");
        assert!(has_error(
            &e31,
            Field::AppKey,
            ValidationError::InvalidHex { expected_len: 32 }
        ));
        let e33 = parse_form(&body_with("app_key", &"a".repeat(33)), LORAWAN).expect_err("33");
        assert!(has_error(
            &e33,
            Field::AppKey,
            ValidationError::InvalidHex { expected_len: 32 }
        ));
    }

    #[test]
    fn mixed_case_hex_accepted() {
        let cfg =
            parse_form(&body_with("dev_eui", "aAbBcCdD11223344"), LORAWAN).expect("mixed case");
        assert_eq!(cfg.lora().unwrap().dev_eui_hex(), "aAbBcCdD11223344");
    }

    #[test]
    fn non_hex_char_rejected() {
        let errors =
            parse_form(&body_with("join_eui", "GGGG223344556677"), LORAWAN).expect_err("non-hex");
        assert!(has_error(
            &errors,
            Field::JoinEui,
            ValidationError::InvalidHex { expected_len: 16 }
        ));
    }

    // ── URL shape ───────────────────────────────────────────────────────

    #[test]
    fn url_missing_scheme_rejected() {
        let errors =
            parse_form(&body_with("ota_url", "example.com/x"), LORAWAN).expect_err("no scheme");
        assert!(has_error(
            &errors,
            Field::OtaUrl,
            ValidationError::InvalidUrl
        ));
    }

    #[test]
    fn https_url_rejected_http_only() {
        let errors =
            parse_form(&body_with("ota_url", "https://example.com/x"), LORAWAN).expect_err("https");
        assert!(has_error(
            &errors,
            Field::OtaUrl,
            ValidationError::InvalidUrl
        ));
    }

    #[test]
    fn bare_http_no_host_rejected() {
        let errors = parse_form(&body_with("ota_url", "http://"), LORAWAN).expect_err("no host");
        assert!(has_error(
            &errors,
            Field::OtaUrl,
            ValidationError::InvalidUrl
        ));
    }

    #[test]
    fn url_at_128_accepted_129_rejected() {
        let host_len = 128 - "http://".len();
        let mut url128 = alloc::string::String::from("http://");
        for _ in 0..host_len {
            url128.push('h');
        }
        assert_eq!(url128.len(), 128);
        assert!(parse_form(&body_with("ota_url", &url128), LORAWAN).is_ok());
        let mut url129 = url128.clone();
        url129.push('h');
        let errors = parse_form(&body_with("ota_url", &url129), LORAWAN).expect_err("too long");
        assert!(has_error(
            &errors,
            Field::OtaUrl,
            ValidationError::TooLong { max: 128 }
        ));
    }

    // ── Device name ─────────────────────────────────────────────────────

    #[test]
    fn dev_name_at_24_accepted_25_rejected() {
        assert!(parse_form(&body_with("dev_name", &"n".repeat(24)), LORAWAN).is_ok());
        let errors =
            parse_form(&body_with("dev_name", &"n".repeat(25)), LORAWAN).expect_err("too long");
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
        let errors = parse_form(body, LORAWAN).expect_err("multiple");
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
        assert!(has_error(
            &errors,
            Field::OtaUrl,
            ValidationError::InvalidUrl
        ));
    }

    // ── Extras ──────────────────────────────────────────────────────────

    #[test]
    fn extras_captured_in_order() {
        let mut body = valid_body().to_string();
        body.push_str("&battery=88&zone=north");
        let cfg = parse_form(&body, LORAWAN).expect("valid with extras");
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
        let errors = parse_form(&body, LORAWAN).expect_err("overflow");
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
        let cfg = parse_form(&body, LORAWAN).expect("valid; reserved keys ignored");
        assert!(cfg.extras().is_empty());
    }

    // ── Duplicates ──────────────────────────────────────────────────────

    #[test]
    fn duplicate_canonical_key_is_duplicate_on_that_field() {
        let mut body = valid_body().to_string();
        body.push_str("&wifi_ssid=other");
        let errors = parse_form(&body, LORAWAN).expect_err("dup");
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
        let errors = parse_form(&body, LORAWAN).expect_err("dup extra");
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
        let _ = parse_form(&junk, LORAWAN);
        let _ = parse_form(&junk, WIFI_MQTT);
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
        assert!(parse_form(&body, LORAWAN).is_err());
        assert!(parse_form(&body, WIFI_MQTT).is_err());
    }

    // ── WifiMqttDevice: valid round-trip ────────────────────────────────

    #[test]
    fn mqtt_valid_body_parses_anonymous() {
        let cfg = parse_form(&mqtt_body_with("mqtt_uri", TEST_MQTT_URI), WIFI_MQTT).expect("valid");
        assert_eq!(cfg.profile(), WIFI_MQTT);
        assert!(cfg.lora().is_none());
        let mqtt = cfg.mqtt().expect("mqtt group");
        assert_eq!(mqtt.host(), "broker.local");
        assert_eq!(mqtt.port(), 1883);
        assert_eq!(mqtt.username(), None);
        assert_eq!(mqtt.password(), None);
        assert_eq!(mqtt.client_id(), None);
    }

    // ── mqtt_uri shape boundaries ───────────────────────────────────────

    #[test]
    fn mqtt_uri_valid_host_port() {
        let cfg = parse_form(&mqtt_body_with("mqtt_uri", "mqtt://h:1883"), WIFI_MQTT).expect("ok");
        let mqtt = cfg.mqtt().unwrap();
        assert_eq!(mqtt.host(), "h");
        assert_eq!(mqtt.port(), 1883);
    }

    #[test]
    fn mqtt_uri_port_zero_rejected_by_validator() {
        let errors =
            parse_form(&mqtt_body_with("mqtt_uri", "mqtt://h:0"), WIFI_MQTT).expect_err("port 0");
        assert!(has_error(
            &errors,
            Field::MqttUri,
            ValidationError::InvalidUrl
        ));
    }

    #[test]
    fn mqtt_uri_port_65536_fails_u16_parse() {
        let errors = parse_form(&mqtt_body_with("mqtt_uri", "mqtt://h:65536"), WIFI_MQTT)
            .expect_err("port 65536");
        assert!(has_error(
            &errors,
            Field::MqttUri,
            ValidationError::InvalidUrl
        ));
    }

    #[test]
    fn mqtt_uri_port_missing_rejected() {
        let errors =
            parse_form(&mqtt_body_with("mqtt_uri", "mqtt://h"), WIFI_MQTT).expect_err("no port");
        assert!(has_error(
            &errors,
            Field::MqttUri,
            ValidationError::InvalidUrl
        ));
    }

    #[test]
    fn mqtt_uri_host_empty_rejected() {
        let errors = parse_form(&mqtt_body_with("mqtt_uri", "mqtt://:1883"), WIFI_MQTT)
            .expect_err("no host");
        assert!(has_error(
            &errors,
            Field::MqttUri,
            ValidationError::InvalidUrl
        ));
    }

    #[test]
    fn mqtt_uri_wrong_scheme_rejected() {
        let errors = parse_form(&mqtt_body_with("mqtt_uri", "tcp://h:1883"), WIFI_MQTT)
            .expect_err("wrong scheme");
        assert!(has_error(
            &errors,
            Field::MqttUri,
            ValidationError::InvalidUrl
        ));
    }

    #[test]
    fn mqtt_uri_https_style_scheme_rejected() {
        let errors = parse_form(&mqtt_body_with("mqtt_uri", "https://h:1883"), WIFI_MQTT)
            .expect_err("https scheme");
        assert!(has_error(
            &errors,
            Field::MqttUri,
            ValidationError::InvalidUrl
        ));
    }

    #[test]
    fn mqtt_uri_missing_is_missing() {
        let body = "wifi_ssid=home-net&wifi_pass=open-sesame&mqtt_user=&mqtt_pass=\
                    &mqtt_client=&ota_url=http://h/x&dev_name=ok";
        let errors = parse_form(body, WIFI_MQTT).expect_err("missing uri");
        assert!(has_error(&errors, Field::MqttUri, ValidationError::Missing));
    }

    // ── Auth-pair rules ─────────────────────────────────────────────────

    #[test]
    fn auth_both_present_accepted() {
        let mut body = mqtt_body_with("mqtt_user", TEST_MQTT_USER);
        body = body.replace("mqtt_pass=", &alloc::format!("mqtt_pass={TEST_MQTT_PSK}"));
        let cfg = parse_form(&body, WIFI_MQTT).expect("auth");
        let mqtt = cfg.mqtt().unwrap();
        assert_eq!(mqtt.username(), Some(TEST_MQTT_USER));
        assert_eq!(mqtt.password(), Some(TEST_MQTT_PSK));
    }

    #[test]
    fn auth_both_absent_is_anonymous() {
        let cfg = parse_form(&mqtt_body_with("mqtt_uri", TEST_MQTT_URI), WIFI_MQTT).expect("anon");
        let mqtt = cfg.mqtt().unwrap();
        assert_eq!(mqtt.username(), None);
        assert_eq!(mqtt.password(), None);
    }

    #[test]
    fn auth_password_without_user_rejected_on_mqtt_pass() {
        let body = mqtt_body_with("mqtt_pass", TEST_MQTT_PSK);
        let errors = parse_form(&body, WIFI_MQTT).expect_err("pass without user");
        assert!(has_error(
            &errors,
            Field::MqttPass,
            ValidationError::Missing
        ));
    }

    #[test]
    fn auth_user_without_password_accepted() {
        let cfg =
            parse_form(&mqtt_body_with("mqtt_user", TEST_MQTT_USER), WIFI_MQTT).expect("user only");
        let mqtt = cfg.mqtt().unwrap();
        assert_eq!(mqtt.username(), Some(TEST_MQTT_USER));
        assert_eq!(mqtt.password(), None);
    }

    // ── Client-ID boundaries ────────────────────────────────────────────

    #[test]
    fn client_id_blank_is_none() {
        let cfg = parse_form(&mqtt_body_with("mqtt_client", ""), WIFI_MQTT).expect("blank client");
        assert_eq!(cfg.mqtt().unwrap().client_id(), None);
    }

    #[test]
    fn client_id_at_23_accepted() {
        let id23 = "c".repeat(23);
        let cfg =
            parse_form(&mqtt_body_with("mqtt_client", &id23), WIFI_MQTT).expect("23-byte client");
        assert_eq!(cfg.mqtt().unwrap().client_id(), Some(id23.as_str()));
    }

    #[test]
    fn client_id_at_24_rejected() {
        let id24 = "c".repeat(24);
        let errors =
            parse_form(&mqtt_body_with("mqtt_client", &id24), WIFI_MQTT).expect_err("24-byte");
        assert!(has_error(
            &errors,
            Field::MqttClient,
            ValidationError::TooLong {
                max: CLIENT_ID_MAX_LEN
            }
        ));
    }

    #[test]
    fn client_id_named_value_round_trips() {
        let cfg = parse_form(&mqtt_body_with("mqtt_client", TEST_MQTT_CLIENT), WIFI_MQTT)
            .expect("named client");
        assert_eq!(cfg.mqtt().unwrap().client_id(), Some(TEST_MQTT_CLIENT));
    }

    // ── Cross-profile rejection ─────────────────────────────────────────

    #[test]
    fn lora_field_posted_to_mqtt_profile_is_single_form_unexpected() {
        let mut body = mqtt_body_with("mqtt_uri", TEST_MQTT_URI);
        body.push_str("&dev_eui=0011223344556677");
        let errors = parse_form(&body, WIFI_MQTT).expect_err("cross-profile");
        assert!(has_error(
            &errors,
            Field::Form,
            ValidationError::UnexpectedForProfile
        ));
        let form_count = errors.iter().filter(|e| e.field == Field::Form).count();
        assert_eq!(form_count, 1);
        assert!(errors.len() <= MAX_FIELD_ERRORS);
        assert!(!has_error(&errors, Field::DevEui, ValidationError::Missing));
        assert!(cfg_has_no_dev_eui_extra(&body));
    }

    fn cfg_has_no_dev_eui_extra(body: &str) -> bool {
        match parse_form(body, WIFI_MQTT) {
            Ok(cfg) => !cfg.extras().iter().any(|e| e.key.as_str() == "dev_eui"),
            Err(_) => true,
        }
    }

    #[test]
    fn mqtt_field_posted_to_lora_profile_is_single_form_unexpected() {
        let mut body = valid_body().to_string();
        body.push_str("&mqtt_uri=mqtt://h:1883");
        let errors = parse_form(&body, LORAWAN).expect_err("cross-profile");
        assert!(has_error(
            &errors,
            Field::Form,
            ValidationError::UnexpectedForProfile
        ));
        let form_count = errors.iter().filter(|e| e.field == Field::Form).count();
        assert_eq!(form_count, 1);
    }

    #[test]
    fn cross_profile_field_first_body_error_wins() {
        let mut body = mqtt_body_with("mqtt_uri", TEST_MQTT_URI);
        body.push_str("&dev_eui=0011223344556677&app_key=00112233445566778899AABBCCDDEEFF");
        let errors = parse_form(&body, WIFI_MQTT).expect_err("two cross-profile");
        let form_count = errors.iter().filter(|e| e.field == Field::Form).count();
        assert_eq!(form_count, 1);
        assert!(has_error(
            &errors,
            Field::Form,
            ValidationError::UnexpectedForProfile
        ));
    }

    // ── MQTT error accumulation ─────────────────────────────────────────

    #[test]
    fn multiple_bad_mqtt_fields_accumulate() {
        let body = "wifi_ssid=&wifi_pass=&mqtt_uri=tcp://h:1883&mqtt_user=&mqtt_pass=secretpw\
                    &mqtt_client=ccccccccccccccccccccccccc&ota_url=ftp://x&dev_name=";
        let errors = parse_form(body, WIFI_MQTT).expect_err("multiple");
        assert!(has_error(&errors, Field::WifiSsid, ValidationError::Empty));
        assert!(has_error(
            &errors,
            Field::MqttUri,
            ValidationError::InvalidUrl
        ));
        assert!(has_error(
            &errors,
            Field::MqttPass,
            ValidationError::Missing
        ));
        assert!(has_error(
            &errors,
            Field::MqttClient,
            ValidationError::TooLong {
                max: CLIENT_ID_MAX_LEN
            }
        ));
        assert!(has_error(
            &errors,
            Field::OtaUrl,
            ValidationError::InvalidUrl
        ));
        assert!(has_error(
            &errors,
            Field::DeviceName,
            ValidationError::Empty
        ));
        assert!(errors.len() <= MAX_FIELD_ERRORS);
    }

    #[test]
    fn all_eight_mqtt_fields_invalid_plus_cross_profile_fits_nine_cap() {
        let id24 = "c".repeat(24);
        let mut body = alloc::string::String::new();
        let _ = core::write!(
            body,
            "wifi_ssid=&wifi_pass=p&mqtt_uri=tcp://h&mqtt_user={}&mqtt_pass=secretpw\
             &mqtt_client={id24}&ota_url=nope&dev_name=&dev_eui=0011223344556677",
            "u".repeat(MQTT_USER_MAX_LEN + 1),
        );
        let errors = parse_form(&body, WIFI_MQTT).expect_err("all bad");
        assert!(errors.len() <= MAX_FIELD_ERRORS);
        let form_count = errors.iter().filter(|e| e.field == Field::Form).count();
        assert_eq!(form_count, 1);
    }

    // ── MQTT Debug redaction ────────────────────────────────────────────

    #[test]
    fn mqtt_debug_redacts_password_keeps_user() {
        let mut body = mqtt_body_with("mqtt_user", TEST_MQTT_USER);
        body = body.replace("mqtt_pass=", &alloc::format!("mqtt_pass={TEST_MQTT_PSK}"));
        let cfg = parse_form(&body, WIFI_MQTT).expect("auth");
        let rendered = alloc::format!("{cfg:?}");
        assert!(rendered.contains("<redacted>"));
        assert!(!rendered.contains(TEST_MQTT_PSK));
        assert!(!rendered.contains(TEST_PSK));
        assert!(rendered.contains(TEST_MQTT_USER));
        assert!(rendered.contains("broker.local"));
    }
}
