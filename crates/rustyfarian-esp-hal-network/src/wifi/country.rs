//! Wi-Fi country (regulatory domain) for the bare-metal tier.
//!
//! The FFI adapter behind [`juggler::wifi::apply_country`]: `esp_wifi_set_country_code` and
//! `esp_wifi_get_country` from the Wi-Fi blob that `esp-radio` links (`esp-wifi-sys 0.3.0`).
//! `esp-radio` keeps its bindings private, so they are declared here, like
//! `esp_wifi_set_max_tx_power` in the parent module.
//!
//! `WifiController::new` applies `esp-radio`'s own default once (`CN`, channels 1-13,
//! manual policy); every constructor of this crate that owns the controller replaces it with
//! the configured [`Country`] (default [`Country::WORLD`], channels 1-11, 802.11d on) right
//! after, so both tiers start from the same world-safe default.
//! Re-check the struct layout below and that applied-once assumption on every `esp-radio`
//! bump.

use juggler::wifi::{apply_country, Country, CountryPort, Regulatory};

use super::WifiError;

/// `wifi_country_t` of `esp-wifi-sys 0.3.0`, identical for ESP32, ESP32-C3, ESP32-C6 and
/// ESP32-S3 (no 5 GHz channel mask on these 2.4 GHz chips).
#[repr(C)]
struct WifiCountryRaw {
    /// Country code, the third byte is the environment (`' '`, `'O'`, `'I'`, `'X'`).
    cc: [u8; 3],
    /// First allowed channel.
    schan: u8,
    /// Number of allowed channels.
    nchan: u8,
    /// Output only: the maximum TX power.
    max_tx_power: i8,
    /// `wifi_country_policy_t` (`c_uint`): 0 automatic, 1 manual.
    policy: u32,
}

const _: () = {
    assert!(core::mem::size_of::<WifiCountryRaw>() == 12);
    assert!(core::mem::align_of::<WifiCountryRaw>() == 4);
};

/// `WIFI_COUNTRY_POLICY_AUTO`.
const WIFI_COUNTRY_POLICY_AUTO: u32 = 0;

extern "C" {
    fn esp_wifi_set_country_code(
        country: *const core::ffi::c_char,
        ieee80211d_enabled: bool,
    ) -> i32;
    fn esp_wifi_get_country(country: *mut WifiCountryRaw) -> i32;
}

/// The [`CountryPort`] over the Wi-Fi blob; valid once `WifiController::new` has run.
///
/// The error is the raw `esp_err_t` (`ESP_ERR_WIFI_NOT_INIT` before the radio exists).
pub(crate) struct RadioCountry;

impl CountryPort for RadioCountry {
    type Error = i32;

    fn set_country_code(&mut self, cc: &[u8; 3], ieee80211d: bool) -> Result<(), i32> {
        // SAFETY: provided by esp-wifi-sys, always linked by esp-radio; `cc` is a valid,
        // NUL-terminated three-byte string (`Country::c_code`) that outlives the call, which
        // copies it.
        let rc = unsafe { esp_wifi_set_country_code(cc.as_ptr().cast(), ieee80211d) };
        if rc == 0 {
            Ok(())
        } else {
            Err(rc)
        }
    }

    fn country(&mut self) -> Result<Regulatory, i32> {
        let mut raw = WifiCountryRaw {
            cc: [0; 3],
            schan: 0,
            nchan: 0,
            max_tx_power: 0,
            policy: 0,
        };
        // SAFETY: provided by esp-wifi-sys, always linked by esp-radio; `raw` matches the
        // blob's `wifi_country_t` (layout asserted above) and is a valid local it writes into.
        let rc = unsafe { esp_wifi_get_country(&mut raw) };
        if rc != 0 {
            return Err(rc);
        }
        Ok(Regulatory::from_raw(
            raw.cc,
            raw.schan,
            raw.nchan,
            raw.policy == WIFI_COUNTRY_POLICY_AUTO,
        ))
    }

    /// Always `true`, so the write is never skipped: `esp-radio` runs the blob without NVS
    /// (`nvs_enable: 0`), so a write costs no flash, and the blob's AP-record struct is not
    /// worth a hand-written layout just to save one call.
    fn station_associated(&mut self) -> bool {
        true
    }
}

/// Applies `country` to the started radio and returns what the radio reports.
///
/// Every constructor of this crate that builds the Wi-Fi controller applies the country of
/// its configuration already; call this on a controller the firmware built itself
/// (`esp_radio::wifi::WifiController::new` applies `CN`, channels 1-13), or to change the
/// country later.
/// Call it after `WifiController::new` and before the first `set_config` or scan.
/// Logs one line, e.g. `[wifi] country 01, channels 1-11, 802.11d on`.
/// Same name and shape as the ESP-IDF tier's `wifi::set_country` (ADR 022).
///
/// # Errors
///
/// [`WifiError::Country`] with the driver's error code, e.g. `ESP_ERR_WIFI_NOT_INIT` before
/// the controller exists.
pub fn set_country(country: Country) -> Result<Regulatory, WifiError> {
    apply_country(&mut RadioCountry, country).map_err(WifiError::Country)
}

/// The country the radio reports right now, `None` before the controller exists.
///
/// With 802.11d on, an associated station reports the access point's country.
/// Same name and shape as the ESP-IDF tier's `wifi::regulatory` (ADR 022).
pub fn regulatory() -> Option<Regulatory> {
    RadioCountry.country().ok()
}
