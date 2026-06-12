//! Structured provisioning error types.
//!
//! The HTTP layer matches on these enums to highlight the offending form input
//! and render a message without `alloc`. Both [`Field`] and [`ValidationError`]
//! implement [`core::fmt::Display`].

use core::fmt;

use crate::config::MAX_FIELD_ERRORS;

/// Experimental: API may change before 1.0.
///
/// Identifies which provisioning form field an error refers to.
///
/// The seven canonical variants correspond to the HTML inputs of the
/// four-field provisionable schema (Wi-Fi credentials, LoRaWAN OTAA keys, OTA
/// URL, device name). [`Field::Form`] carries body-level problems that are not
/// attributable to a single input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    /// Wi-Fi SSID (`wifi_ssid`).
    WifiSsid,
    /// Wi-Fi password (`wifi_pass`).
    WifiPassword,
    /// LoRaWAN DevEUI (`dev_eui`).
    DevEui,
    /// LoRaWAN JoinEUI / AppEUI (`join_eui`).
    JoinEui,
    /// LoRaWAN AppKey (`app_key`).
    AppKey,
    /// OTA update URL (`ota_url`).
    OtaUrl,
    /// Human-readable device name (`dev_name`).
    DeviceName,
    /// Body-level problem with no single owning input (malformed body,
    /// duplicate extra key, too many extra fields).
    Form,
}

impl Field {
    /// Experimental: API may change before 1.0.
    ///
    /// Returns the HTML input `name` attribute for this field.
    ///
    /// This is the single source of truth shared by the portal HTML, the
    /// [`parse_form`](crate::parse_form) parser, and the host tests. The seven
    /// canonical fields return their real input name; [`Field::Form`] has no
    /// real input and returns the sentinel `"_form"` (an underscore-prefixed
    /// name, which the parser reserves and never treats as a submitted value).
    pub fn form_name(self) -> &'static str {
        match self {
            Field::WifiSsid => "wifi_ssid",
            Field::WifiPassword => "wifi_pass",
            Field::DevEui => "dev_eui",
            Field::JoinEui => "join_eui",
            Field::AppKey => "app_key",
            Field::OtaUrl => "ota_url",
            Field::DeviceName => "dev_name",
            Field::Form => "_form",
        }
    }
}

impl fmt::Display for Field {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.form_name())
    }
}

/// Experimental: API may change before 1.0.
///
/// The reason a provisioning form field (or the body as a whole) was rejected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    /// A required field's key was absent from the body.
    Missing,
    /// The field's key was present but its value was empty.
    Empty,
    /// The same canonical key appeared more than once.
    Duplicate,
    /// The value exceeded the field's maximum length.
    TooLong {
        /// The maximum number of bytes the field accepts.
        max: usize,
    },
    /// The value was shorter than the field's minimum length.
    ///
    /// Currently emitted for `wifi_pass` when a non-empty password is below
    /// the WPA2-Personal floor (`wifi_pure::AP_PASSWORD_MIN_LEN`). An empty
    /// password means "open network" and is not rejected.
    TooShort {
        /// The minimum number of bytes the field requires when non-empty.
        min: usize,
    },
    /// The value was not exactly `expected_len` hexadecimal characters.
    InvalidHex {
        /// The exact hex-character length the field requires.
        expected_len: usize,
    },
    /// The OTA URL failed its shape check (scheme, host, or length).
    InvalidUrl,
    /// The request body could not be percent-decoded into valid UTF-8.
    MalformedBody,
    /// More opaque extra fields were submitted than [`EXTRA_FIELDS_MAX`](crate::EXTRA_FIELDS_MAX).
    TooManyFields,
}

impl fmt::Display for ValidationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ValidationError::Missing => f.write_str("required field is missing"),
            ValidationError::Empty => f.write_str("field must not be empty"),
            ValidationError::Duplicate => f.write_str("field appears more than once"),
            ValidationError::TooLong { max } => {
                write!(f, "field exceeds maximum length of {max} bytes")
            }
            ValidationError::TooShort { min } => {
                write!(f, "field must be at least {min} bytes when not empty")
            }
            ValidationError::InvalidHex { expected_len } => {
                write!(
                    f,
                    "field must be exactly {expected_len} hexadecimal characters"
                )
            }
            ValidationError::InvalidUrl => f.write_str("OTA URL is malformed"),
            ValidationError::MalformedBody => f.write_str("request body is malformed"),
            ValidationError::TooManyFields => f.write_str("too many extra fields"),
        }
    }
}

/// Experimental: API may change before 1.0.
///
/// A single field-attributed validation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldError {
    /// The field the error refers to.
    pub field: Field,
    /// The reason the field was rejected.
    pub error: ValidationError,
}

impl fmt::Display for FieldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.field, self.error)
    }
}

/// Experimental: API may change before 1.0.
///
/// The accumulated set of validation failures returned by
/// [`parse_form`](crate::parse_form).
///
/// The capacity of [`MAX_FIELD_ERRORS`] is exact: at most one error per
/// canonical field (seven) plus at most one [`Field::Form`]-level error.
pub type FieldErrors = heapless::Vec<FieldError, MAX_FIELD_ERRORS>;
