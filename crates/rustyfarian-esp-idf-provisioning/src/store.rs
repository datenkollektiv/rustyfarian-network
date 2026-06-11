//! NVS-backed persistence for committed provisioning configuration.
//!
//! [`ProvisioningStore`] owns a single ESP-IDF NVS namespace (`rf_prov`) and is
//! the host boot path's interface to it: [`is_provisioned`](ProvisioningStore::is_provisioned)
//! and [`load`](ProvisioningStore::load) run on every normal boot, and
//! [`erase_all`](ProvisioningStore::erase_all) backs the host's factory-reset
//! trigger.
//! The portal session drives [`save`](ProvisioningStore::save) once, on a
//! valid commit.
//!
//! # Plaintext storage
//!
//! Values are stored in plaintext. Flash / NVS encryption is a partition
//! concern owned by the host firmware, not this crate.
//!
//! # Layout
//!
//! Single namespace `rf_prov` with one key per canonical field plus a
//! `schema_ver` (`u8`, value `1`) guard. Opaque extras are stored as `x_{name}`
//! with `name` capped at 13 bytes so the `x_` prefix keeps the total under the
//! 15-byte NVS key limit. Because `EspNvs` on `esp-idf-svc 0.52` cannot
//! enumerate keys portably, an `extras_idx` key holds the comma-joined extra
//! names so [`erase_all`](ProvisioningStore::erase_all) can find and remove
//! them. This deviates from the feature-doc sketch's pure one-key-per-field
//! layout — the deviation is recorded in the Session Log.

use anyhow::Context as _;

use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};

use provisioning_pure::ProvisioningConfig;

/// NVS namespace holding every provisioning value.
const NAMESPACE: &str = "rf_prov";

/// Current on-flash layout version. Written last on every commit.
const SCHEMA_VERSION: u8 = 1;

/// The `schema_ver` guard key.
const KEY_SCHEMA_VER: &str = "schema_ver";
/// Wi-Fi SSID key.
const KEY_WIFI_SSID: &str = "wifi_ssid";
/// Wi-Fi password key.
const KEY_WIFI_PASS: &str = "wifi_pass";
/// LoRaWAN DevEUI key.
const KEY_DEV_EUI: &str = "lora_dev_eui";
/// LoRaWAN JoinEUI key.
const KEY_JOIN_EUI: &str = "lora_join_eui";
/// LoRaWAN AppKey key.
const KEY_APP_KEY: &str = "lora_app_key";
/// OTA URL key.
const KEY_OTA_URL: &str = "ota_url";
/// Device name key.
const KEY_DEV_NAME: &str = "dev_name";
/// Comma-joined index of the extra-field names currently stored.
const KEY_EXTRAS_IDX: &str = "extras_idx";

/// The canonical string keys, used by [`ProvisioningStore::erase_all`].
const CANONICAL_KEYS: [&str; 7] = [
    KEY_WIFI_SSID,
    KEY_WIFI_PASS,
    KEY_DEV_EUI,
    KEY_JOIN_EUI,
    KEY_APP_KEY,
    KEY_OTA_URL,
    KEY_DEV_NAME,
];

/// Stack buffer size for a single `get_str` read.
///
/// Every stored value is below this bound (the longest, the OTA URL, is capped
/// at 128 bytes by `provisioning-pure`).
const READ_BUF_LEN: usize = 256;

/// Maximum bytes the `extras_idx` value may occupy.
///
/// Eight extras of at most 13 name bytes plus seven separators is 111 bytes;
/// 256 leaves comfortable headroom and matches [`READ_BUF_LEN`].
const EXTRAS_IDX_MAX: usize = 256;

/// Experimental: API may change before 1.0.
///
/// A fully loaded provisioning configuration as owned `std` strings.
///
/// Unlike [`provisioning_pure::ProvisioningConfig`] (the parse-time, `no_std`
/// type), this is the *read-back* view the host boot path consumes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredConfig {
    /// Wi-Fi SSID.
    pub wifi_ssid: String,
    /// Wi-Fi password (empty for an open network).
    pub wifi_password: String,
    /// LoRaWAN DevEUI as a 16-character MSB-first hex string.
    pub dev_eui_hex: String,
    /// LoRaWAN JoinEUI as a 16-character MSB-first hex string.
    pub join_eui_hex: String,
    /// LoRaWAN AppKey as a 32-character hex string.
    pub app_key_hex: String,
    /// OTA update URL.
    pub ota_url: String,
    /// Device name.
    pub device_name: String,
    /// Opaque host-defined extras, in stored order.
    pub extras: Vec<(String, String)>,
}

/// Experimental: API may change before 1.0.
///
/// NVS-backed store for committed provisioning configuration.
///
/// Open it once per boot with [`ProvisioningStore::open`]; it owns a read/write
/// handle to the `rf_prov` namespace for its lifetime.
pub struct ProvisioningStore {
    nvs: EspNvs<NvsDefault>,
}

impl ProvisioningStore {
    /// Experimental: API may change before 1.0.
    ///
    /// Opens (creating if absent) the `rf_prov` NVS namespace read/write.
    pub fn open(partition: EspDefaultNvsPartition) -> anyhow::Result<Self> {
        let nvs = EspNvs::new(partition, NAMESPACE, true)
            .context("failed to open NVS namespace 'rf_prov'")?;
        Ok(Self { nvs })
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Reports whether a complete provisioning record is present.
    ///
    /// A record counts as provisioned only when both the `schema_ver` guard and
    /// the Wi-Fi SSID are present: `schema_ver` is written *last* on a commit,
    /// so a torn write that left the SSID but not the guard reads as
    /// unprovisioned, and the SSID is the one field a real device cannot boot
    /// without.
    pub fn is_provisioned(&self) -> anyhow::Result<bool> {
        let schema = self
            .nvs
            .get_u8(KEY_SCHEMA_VER)
            .context("failed to read schema_ver")?;
        if schema.is_none() {
            return Ok(false);
        }
        let ssid_present = self
            .nvs
            .contains(KEY_WIFI_SSID)
            .context("failed to probe wifi_ssid")?;
        Ok(ssid_present)
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Loads the stored configuration, or `None` if the device is not
    /// provisioned.
    ///
    /// Secrets (`wifi_password`, `app_key_hex`) are read verbatim because the
    /// host boot path needs them to join networks. The portal must never emit
    /// them into HTML — that rule is enforced in the portal, not by crippling
    /// this loader.
    pub fn load(&self) -> anyhow::Result<Option<StoredConfig>> {
        if !self.is_provisioned()? {
            return Ok(None);
        }

        let wifi_ssid = self.read_str(KEY_WIFI_SSID)?.unwrap_or_default();
        let wifi_password = self.read_str(KEY_WIFI_PASS)?.unwrap_or_default();
        let dev_eui_hex = self.read_str(KEY_DEV_EUI)?.unwrap_or_default();
        let join_eui_hex = self.read_str(KEY_JOIN_EUI)?.unwrap_or_default();
        let app_key_hex = self.read_str(KEY_APP_KEY)?.unwrap_or_default();
        let ota_url = self.read_str(KEY_OTA_URL)?.unwrap_or_default();
        let device_name = self.read_str(KEY_DEV_NAME)?.unwrap_or_default();

        let mut extras = Vec::new();
        for name in self.extra_names()? {
            let key = extra_key(&name);
            if let Some(value) = self.read_str(&key)? {
                extras.push((name, value));
            }
        }

        Ok(Some(StoredConfig {
            wifi_ssid,
            wifi_password,
            dev_eui_hex,
            join_eui_hex,
            app_key_hex,
            ota_url,
            device_name,
            extras,
        }))
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Writes a validated configuration to NVS in one logical commit.
    ///
    /// The `schema_ver` guard is written **last** so a power loss mid-commit
    /// never leaves the namespace looking provisioned with partial data: until
    /// the guard lands, [`is_provisioned`](Self::is_provisioned) returns
    /// `false`. SSID and AppKey lengths are logged for diagnostics; secret
    /// *values* are never logged.
    pub fn save(&mut self, config: &ProvisioningConfig) -> anyhow::Result<()> {
        log::info!(
            "Persisting provisioning config (ssid len={}, app_key len={}, extras={})",
            config.wifi_ssid().len(),
            config.app_key_hex().len(),
            config.extras().len(),
        );

        self.set_str(KEY_WIFI_SSID, config.wifi_ssid())?;
        self.set_str(KEY_WIFI_PASS, config.wifi_password())?;
        self.set_str(KEY_DEV_EUI, config.dev_eui_hex())?;
        self.set_str(KEY_JOIN_EUI, config.join_eui_hex())?;
        self.set_str(KEY_APP_KEY, config.app_key_hex())?;
        self.set_str(KEY_OTA_URL, config.ota_url())?;
        self.set_str(KEY_DEV_NAME, config.device_name())?;

        let mut index = String::new();
        for (i, extra) in config.extras().iter().enumerate() {
            if i > 0 {
                index.push(',');
            }
            index.push_str(extra.key.as_str());
            self.set_str(&extra_key(extra.key.as_str()), extra.value.as_str())?;
        }
        if index.len() > EXTRAS_IDX_MAX {
            anyhow::bail!(
                "extras index too large ({} bytes, max {})",
                index.len(),
                EXTRAS_IDX_MAX
            );
        }
        self.set_str(KEY_EXTRAS_IDX, &index)?;

        self.nvs
            .set_u8(KEY_SCHEMA_VER, SCHEMA_VERSION)
            .context("failed to write schema_ver")?;

        Ok(())
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Removes every provisioning key, returning the namespace to an
    /// unprovisioned state.
    ///
    /// Removal order is the inverse of [`save`](Self::save): the `schema_ver`
    /// guard goes first so an interrupted erase also reads as unprovisioned.
    /// Indexed extras are removed via the `extras_idx` key, then the index
    /// itself, then the canonical keys.
    pub fn erase_all(&mut self) -> anyhow::Result<()> {
        let _ = self.nvs.remove(KEY_SCHEMA_VER);

        for name in self.extra_names().unwrap_or_default() {
            let _ = self.nvs.remove(&extra_key(&name));
        }
        let _ = self.nvs.remove(KEY_EXTRAS_IDX);

        for key in CANONICAL_KEYS {
            let _ = self.nvs.remove(key);
        }

        Ok(())
    }

    /// Reads one string key into an owned `String`, or `None` if absent.
    fn read_str(&self, key: &str) -> anyhow::Result<Option<String>> {
        let mut buf = [0u8; READ_BUF_LEN];
        match self
            .nvs
            .get_str(key, &mut buf)
            .with_context(|| format!("failed to read NVS key '{key}'"))?
        {
            Some(s) => Ok(Some(s.to_string())),
            None => Ok(None),
        }
    }

    /// Writes one string key.
    fn set_str(&mut self, key: &str, value: &str) -> anyhow::Result<()> {
        self.nvs
            .set_str(key, value)
            .with_context(|| format!("failed to write NVS key '{key}'"))
    }

    /// Reads the comma-joined extras index into owned names.
    fn extra_names(&self) -> anyhow::Result<Vec<String>> {
        match self.read_str(KEY_EXTRAS_IDX)? {
            Some(idx) if !idx.is_empty() => {
                Ok(idx.split(',').map(|s| s.to_string()).collect())
            }
            _ => Ok(Vec::new()),
        }
    }
}

/// Builds the prefixed NVS key for an extra field name.
fn extra_key(name: &str) -> String {
    format!("x_{name}")
}
