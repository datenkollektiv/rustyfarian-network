//! The ESP-IDF side of the Wi-Fi country (regulatory domain).
//!
//! [`IdfCountryPort`] implements the shared [`CountryPort`] with one bindgen-checked
//! `esp_wifi_*` call per method; the read-before-write sequence, the read-back check and the
//! log line live in [`juggler::wifi::apply_country`].
//! Every call needs an initialised Wi-Fi driver (`EspWifi::new`, i.e. `esp_wifi_init`); the
//! radio does not have to be started.
//!
//! ESP-IDF stores the country in flash (NVS), so it survives a reflash; the library applies the
//! configured country on every boot, and `apply_country` skips the write when the radio already
//! reports it and the station is not associated.

use anyhow::Context as _;
use esp_idf_svc::sys::{
    esp, esp_wifi_get_country, esp_wifi_set_country_code, esp_wifi_sta_get_ap_info,
    wifi_ap_record_t, wifi_country_policy_t_WIFI_COUNTRY_POLICY_AUTO, wifi_country_t, EspError,
};
use juggler::wifi::{apply_country, Country, CountryPort, Regulatory};

/// The country calls of the ESP-IDF Wi-Fi driver.
pub(crate) struct IdfCountryPort;

impl CountryPort for IdfCountryPort {
    type Error = EspError;

    fn set_country_code(&mut self, cc: &[u8; 3], ieee80211d: bool) -> Result<(), EspError> {
        // SAFETY: plain FFI call; `cc` is a NUL-terminated 3-byte C string that lives for the
        // duration of the call, and the driver copies it.
        esp!(unsafe { esp_wifi_set_country_code(cc.as_ptr().cast(), ieee80211d) })
    }

    fn country(&mut self) -> Result<Regulatory, EspError> {
        let mut raw = wifi_country_t::default();
        // SAFETY: plain FFI call with a pointer to a live, writable `wifi_country_t`, which the
        // driver only fills; it fails cleanly before `esp_wifi_init`.
        esp!(unsafe { esp_wifi_get_country(&mut raw) })?;
        // `c_char` is `u8` on the RISC-V targets but not on every target this crate builds for.
        #[allow(clippy::unnecessary_cast)]
        let cc = raw.cc.map(|c| c as u8);
        Ok(Regulatory::from_raw(
            cc,
            raw.schan,
            raw.nchan,
            raw.policy == wifi_country_policy_t_WIFI_COUNTRY_POLICY_AUTO,
        ))
    }

    fn station_associated(&mut self) -> bool {
        let mut record = wifi_ap_record_t::default();
        // SAFETY: plain FFI call with a pointer to a live, writable `wifi_ap_record_t`, which the
        // driver only fills; it returns an error when the station is not associated or the
        // driver is not in station mode.
        esp!(unsafe { esp_wifi_sta_get_ap_info(&mut record) }).is_ok()
    }
}

/// Applies `country` to the initialised Wi-Fi driver and returns what the radio reports.
///
/// # Errors
///
/// The ESP-IDF error of the write or of the read-back, with the country as context.
pub(crate) fn apply(country: Country) -> anyhow::Result<Regulatory> {
    apply_country(&mut IdfCountryPort, country)
        .with_context(|| format!("failed to set the Wi-Fi country {country}"))
}

/// What the radio reports right now, `None` when the Wi-Fi driver is not initialised or
/// refuses the query.
pub(crate) fn read() -> Option<Regulatory> {
    match IdfCountryPort.country() {
        Ok(regulatory) => Some(regulatory),
        Err(e) => {
            log::debug!("[wifi] esp_wifi_get_country failed: {e}");
            None
        }
    }
}
