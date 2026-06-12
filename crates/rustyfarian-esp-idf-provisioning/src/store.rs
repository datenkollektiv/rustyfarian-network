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
//! Single namespace `rf_prov` with one key per canonical field, a `profile`
//! discriminator (`"lorawan"` / `"wifi_mqtt"`), and a `schema_ver` (`u8`, value
//! `2`) guard. Opaque extras are stored as `x_{name}` with `name` capped at 13
//! bytes so the `x_` prefix keeps the total under the 15-byte NVS key limit.
//! Because `EspNvs` on `esp-idf-svc 0.52` cannot enumerate keys portably, an
//! `extras_idx` key holds the comma-joined extra names so
//! [`erase_all`](ProvisioningStore::erase_all) can find and remove them. This
//! deviates from the feature-doc sketch's pure one-key-per-field layout — the
//! deviation is recorded in the Session Log.
//!
//! # Reserved keys
//!
//! The canonical field keys, the `profile` discriminator, the `schema_ver`
//! guard, and the `extras_idx` bookkeeping key are reserved (ADR 014 §4). Host
//! extensions must use the `x_*` extras prefix so they never collide with a
//! current or future canonical key.
//!
//! # Profiles and the v1 → v2 migration
//!
//! [`SCHEMA_VERSION`] is `2`. Two profiles share this namespace
//! ([`provisioning_pure::SchemaProfile`]): `LorawanFieldDevice` writes the
//! LoRaWAN keys (`lora_dev_eui` / `lora_join_eui` / `lora_app_key`),
//! `WifiMqttDevice` writes the MQTT keys (`mqtt_host` / `mqtt_port` /
//! `mqtt_user` / `mqtt_pass` / `mqtt_client`); each writes only its active
//! group, and the Core (`wifi_ssid` / `wifi_pass` / `dev_name`) and OTA
//! (`ota_url`) keys are common to both. [`load`](ProvisioningStore::load) reads
//! the `profile` key first; a `schema_ver == 1` record (or any record with the
//! `profile` key absent) is read as
//! [`SchemaProfile::LorawanFieldDevice`](provisioning_pure::SchemaProfile::LorawanFieldDevice),
//! so beekeeper-class devices provisioned under v1 are never re-provisioned.

use anyhow::Context as _;

use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};

use provisioning_pure::{ProvisioningConfig, SchemaProfile};

/// NVS namespace holding every provisioning value.
const NAMESPACE: &str = "rf_prov";

/// Current on-flash layout version. Written last on every commit.
///
/// Bumped from `1` to `2` when the second provisioning profile landed (ADR
/// 014). A `schema_ver == 1` record predates the `profile` discriminator and is
/// read as the LoRaWAN profile (see the module docs).
const SCHEMA_VERSION: u8 = 2;

/// The `schema_ver` guard key.
const KEY_SCHEMA_VER: &str = "schema_ver";
/// Profile discriminator key (`"lorawan"` / `"wifi_mqtt"`).
const KEY_PROFILE: &str = "profile";
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
/// MQTT broker host key.
const KEY_MQTT_HOST: &str = "mqtt_host";
/// MQTT broker port key (stored as a string per ADR 014 §4).
const KEY_MQTT_PORT: &str = "mqtt_port";
/// MQTT username key.
const KEY_MQTT_USER: &str = "mqtt_user";
/// MQTT password key (secret).
const KEY_MQTT_PASS: &str = "mqtt_pass";
/// MQTT client-ID key.
const KEY_MQTT_CLIENT: &str = "mqtt_client";
/// OTA URL key.
const KEY_OTA_URL: &str = "ota_url";
/// Device name key.
const KEY_DEV_NAME: &str = "dev_name";
/// Comma-joined index of the extra-field names currently stored.
const KEY_EXTRAS_IDX: &str = "extras_idx";

/// Every canonical value key plus the `profile` discriminator, used by
/// [`ProvisioningStore::erase_all`] so a reset clears both profiles' groups
/// regardless of which one the device was provisioned under.
const CANONICAL_KEYS: [&str; 13] = [
    KEY_PROFILE,
    KEY_WIFI_SSID,
    KEY_WIFI_PASS,
    KEY_DEV_EUI,
    KEY_JOIN_EUI,
    KEY_APP_KEY,
    KEY_MQTT_HOST,
    KEY_MQTT_PORT,
    KEY_MQTT_USER,
    KEY_MQTT_PASS,
    KEY_MQTT_CLIENT,
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
///
/// The [`profile`](Self::profile) discriminator says which group is populated.
/// For [`SchemaProfile::LorawanFieldDevice`] the LoRaWAN fields
/// (`dev_eui_hex` / `join_eui_hex` / `app_key_hex`) carry the validated
/// credentials and the MQTT fields are empty / `None`; for
/// [`SchemaProfile::WifiMqttDevice`] the inverse holds. Hosts match on
/// `profile` rather than probing which group happens to be populated, mirroring
/// [`ProvisioningConfig::profile`](provisioning_pure::ProvisioningConfig::profile).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredConfig {
    /// The profile this record was provisioned under.
    pub profile: SchemaProfile,
    /// Wi-Fi SSID.
    pub wifi_ssid: String,
    /// Wi-Fi password (empty for an open network).
    pub wifi_password: String,
    /// LoRaWAN DevEUI as a 16-character MSB-first hex string (empty for the
    /// `WifiMqttDevice` profile).
    pub dev_eui_hex: String,
    /// LoRaWAN JoinEUI as a 16-character MSB-first hex string (empty for the
    /// `WifiMqttDevice` profile).
    pub join_eui_hex: String,
    /// LoRaWAN AppKey as a 32-character hex string (empty for the
    /// `WifiMqttDevice` profile).
    pub app_key_hex: String,
    /// MQTT broker host (empty for the `LorawanFieldDevice` profile).
    pub mqtt_host: String,
    /// MQTT broker port (`0` for the `LorawanFieldDevice` profile).
    pub mqtt_port: u16,
    /// MQTT username, or `None` for an anonymous connection / the
    /// `LorawanFieldDevice` profile.
    pub mqtt_user: Option<String>,
    /// MQTT password, or `None` when none was supplied / the
    /// `LorawanFieldDevice` profile.
    pub mqtt_pass: Option<String>,
    /// MQTT client ID, or `None` when the host derives one at boot / the
    /// `LorawanFieldDevice` profile.
    pub mqtt_client: Option<String>,
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
    /// without. The SSID lives in the Core group common to both profiles, so
    /// this test is profile-independent and unchanged from v1.
    pub fn is_provisioned(&self) -> anyhow::Result<bool> {
        let schema = self
            .nvs
            .get_u8(KEY_SCHEMA_VER)
            .context("failed to read schema_ver")?;
        if schema.is_none() {
            return Ok(false);
        }
        Ok(self.read_str(KEY_WIFI_SSID)?.is_some())
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Loads the stored configuration, or `None` if the device is not
    /// provisioned.
    ///
    /// Secrets (`wifi_password`, `app_key_hex`, `mqtt_pass`) are read verbatim
    /// because the host boot path needs them to join networks and brokers. The
    /// portal must never emit them into HTML — that rule is enforced in the
    /// portal, not by crippling this loader.
    ///
    /// The `profile` discriminator is read first. An absent `profile` key
    /// (a v1 record, or one written by `schema_ver == 1` firmware) is read as
    /// [`SchemaProfile::LorawanFieldDevice`], the documented v1 → v2 migration
    /// that re-provisions no device. Only the active profile's group is read;
    /// the other group's fields are left empty / `None`. A stored `mqtt_port`
    /// that is absent, empty, or not a valid `u16` is surfaced as a load error
    /// rather than silently defaulting, since a `WifiMqttDevice` device cannot
    /// connect without it.
    pub fn load(&self) -> anyhow::Result<Option<StoredConfig>> {
        if !self.is_provisioned()? {
            return Ok(None);
        }

        let profile = match self.read_str(KEY_PROFILE)? {
            Some(s) => SchemaProfile::from_nvs_str(&s)
                .with_context(|| format!("unrecognised stored profile '{s}'"))?,
            None => SchemaProfile::LorawanFieldDevice,
        };

        let wifi_ssid = self.read_str(KEY_WIFI_SSID)?.unwrap_or_default();
        let wifi_password = self.read_str(KEY_WIFI_PASS)?.unwrap_or_default();
        let ota_url = self.read_str(KEY_OTA_URL)?.unwrap_or_default();
        let device_name = self.read_str(KEY_DEV_NAME)?.unwrap_or_default();

        let mut dev_eui_hex = String::new();
        let mut join_eui_hex = String::new();
        let mut app_key_hex = String::new();
        let mut mqtt_host = String::new();
        let mut mqtt_port: u16 = 0;
        let mut mqtt_user = None;
        let mut mqtt_pass = None;
        let mut mqtt_client = None;

        match profile {
            SchemaProfile::LorawanFieldDevice => {
                dev_eui_hex = self.read_str(KEY_DEV_EUI)?.unwrap_or_default();
                join_eui_hex = self.read_str(KEY_JOIN_EUI)?.unwrap_or_default();
                app_key_hex = self.read_str(KEY_APP_KEY)?.unwrap_or_default();
            }
            SchemaProfile::WifiMqttDevice => {
                mqtt_host = self.read_str(KEY_MQTT_HOST)?.unwrap_or_default();
                let port_str = self
                    .read_str(KEY_MQTT_PORT)?
                    .context("provisioned as wifi_mqtt but mqtt_port key is absent")?;
                mqtt_port = port_str
                    .parse::<u16>()
                    .with_context(|| format!("stored mqtt_port '{port_str}' is not a valid u16"))?;
                mqtt_user = self.read_str(KEY_MQTT_USER)?;
                mqtt_pass = self.read_str(KEY_MQTT_PASS)?;
                mqtt_client = self.read_str(KEY_MQTT_CLIENT)?;
            }
        }

        let mut extras = Vec::new();
        for name in self.extra_names()? {
            let key = extra_key(&name);
            if let Some(value) = self.read_str(&key)? {
                extras.push((name, value));
            }
        }

        Ok(Some(StoredConfig {
            profile,
            wifi_ssid,
            wifi_password,
            dev_eui_hex,
            join_eui_hex,
            app_key_hex,
            mqtt_host,
            mqtt_port,
            mqtt_user,
            mqtt_pass,
            mqtt_client,
            ota_url,
            device_name,
            extras,
        }))
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Writes a validated configuration to NVS in one logical commit.
    ///
    /// Only the active profile's group keys are written; the other group's keys
    /// and any absent optional MQTT keys (`mqtt_user` / `mqtt_pass` /
    /// `mqtt_client`) are *removed*, so a later [`load`](Self::load) round-trips
    /// an absent optional as `None` and never reads a stale value left by a
    /// previous commit under a different profile or auth shape.
    ///
    /// The `profile` discriminator is written before the `schema_ver` guard,
    /// and the `schema_ver` guard is written **last** overall, so a power loss
    /// mid-commit never leaves the namespace looking provisioned with partial
    /// data or an unknown profile: until the guard lands,
    /// [`is_provisioned`](Self::is_provisioned) returns `false`. SSID length and
    /// the profile are logged for diagnostics; secret *values* are never logged.
    pub fn save(&mut self, config: &ProvisioningConfig) -> anyhow::Result<()> {
        let profile = config.profile();
        log::info!(
            "Persisting provisioning config (profile={}, ssid len={}, extras={})",
            profile.as_str(),
            config.wifi_ssid().len(),
            config.extras().len(),
        );

        // Read the previous extras index *before* we overwrite anything, so we
        // can remove keys this commit no longer references. Without this,
        // shrinking the extras set would leak stale `x_*` values into NVS —
        // invisible to `load()` (which trusts the index) but still occupying
        // flash and retaining whatever value was last written.
        let previous_extras = self.extra_names().unwrap_or_default();

        self.set_str(KEY_WIFI_SSID, config.wifi_ssid())?;
        self.set_str(KEY_WIFI_PASS, config.wifi_password())?;
        self.set_str(KEY_OTA_URL, config.ota_url())?;
        self.set_str(KEY_DEV_NAME, config.device_name())?;

        self.write_profile_group(config)?;

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

        for name in &previous_extras {
            let still_present = config
                .extras()
                .iter()
                .any(|e| e.key.as_str() == name.as_str());
            if !still_present {
                let key = extra_key(name);
                // Best-effort cleanup — we do not fail the commit on a removal
                // error because the new state (canonical keys + new index) is
                // already valid. But the whole point of this loop is to
                // prevent stale retention, so a removal failure means we did
                // not achieve that intent; surface it at `warn` so the
                // operator can investigate (NVS exhaustion, hardware fault).
                if let Err(e) = self.nvs.remove(&key) {
                    log::warn!("failed to remove stale extra '{key}': {e:?}");
                }
            }
        }

        self.set_str(KEY_EXTRAS_IDX, &index)?;

        // The `profile` discriminator is written before the `schema_ver` guard
        // so a torn write never reads as provisioned with an unknown profile
        // (ADR 014 §4); `schema_ver` remains the very last write overall.
        self.set_str(KEY_PROFILE, profile.as_str())?;

        self.nvs
            .set_u8(KEY_SCHEMA_VER, SCHEMA_VERSION)
            .context("failed to write schema_ver")?;

        Ok(())
    }

    /// Writes the active profile's group keys and removes the inactive group's
    /// keys (and any absent optional MQTT keys), so [`load`](Self::load)
    /// round-trips an absent optional as `None` and never returns a stale value
    /// from a previous commit under a different profile or auth shape.
    fn write_profile_group(&mut self, config: &ProvisioningConfig) -> anyhow::Result<()> {
        match config.profile() {
            SchemaProfile::LorawanFieldDevice => {
                let lora = config
                    .lora()
                    .context("LorawanFieldDevice config missing its LoRaWAN group")?;
                self.set_str(KEY_DEV_EUI, lora.dev_eui_hex())?;
                self.set_str(KEY_JOIN_EUI, lora.join_eui_hex())?;
                self.set_str(KEY_APP_KEY, lora.app_key_hex())?;
                self.remove_mqtt_keys();
            }
            SchemaProfile::WifiMqttDevice => {
                let mqtt = config
                    .mqtt()
                    .context("WifiMqttDevice config missing its MQTT group")?;
                self.set_str(KEY_MQTT_HOST, mqtt.host())?;
                // `mqtt_port` is stored as a string (ADR 014 §4), reusing the
                // single `set_str` / `read_str` value path; it is parsed back to
                // a `u16` in `load`.
                self.set_str(KEY_MQTT_PORT, &mqtt.port().to_string())?;
                self.set_or_remove(KEY_MQTT_USER, mqtt.username())?;
                self.set_or_remove(KEY_MQTT_PASS, mqtt.password())?;
                self.set_or_remove(KEY_MQTT_CLIENT, mqtt.client_id())?;
                self.remove_lora_keys();
            }
        }
        Ok(())
    }

    /// Writes `value` when `Some`, or removes the key when `None`, so an absent
    /// optional field never leaves a stale value behind.
    fn set_or_remove(&mut self, key: &str, value: Option<&str>) -> anyhow::Result<()> {
        match value {
            Some(v) => self.set_str(key, v),
            None => {
                let _ = self.nvs.remove(key);
                Ok(())
            }
        }
    }

    /// Removes the LoRaWAN group keys (best-effort), used when committing a
    /// `WifiMqttDevice` record over a previous LoRaWAN one.
    fn remove_lora_keys(&mut self) {
        for key in [KEY_DEV_EUI, KEY_JOIN_EUI, KEY_APP_KEY] {
            let _ = self.nvs.remove(key);
        }
    }

    /// Removes the MQTT group keys (best-effort), used when committing a
    /// `LorawanFieldDevice` record over a previous `WifiMqttDevice` one.
    fn remove_mqtt_keys(&mut self) {
        for key in [
            KEY_MQTT_HOST,
            KEY_MQTT_PORT,
            KEY_MQTT_USER,
            KEY_MQTT_PASS,
            KEY_MQTT_CLIENT,
        ] {
            let _ = self.nvs.remove(key);
        }
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Removes every provisioning key, returning the namespace to an
    /// unprovisioned state.
    ///
    /// Removal order is the inverse of [`save`](Self::save): the `schema_ver`
    /// guard goes first so an interrupted erase also reads as unprovisioned.
    /// Indexed extras are removed via the `extras_idx` key, then the index
    /// itself, then the canonical keys — which include the `profile`
    /// discriminator and both profiles' group keys, so the reset is complete
    /// regardless of which profile the device was provisioned under.
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
            Some(idx) if !idx.is_empty() => Ok(idx.split(',').map(|s| s.to_string()).collect()),
            _ => Ok(Vec::new()),
        }
    }
}

/// Builds the prefixed NVS key for an extra field name.
fn extra_key(name: &str) -> String {
    format!("x_{name}")
}
