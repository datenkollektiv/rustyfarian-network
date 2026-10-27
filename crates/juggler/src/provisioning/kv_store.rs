//! Key-value persistence for committed provisioning configuration.
//!
//! [`KvProvisioningStore`] holds every record rule of the provisioning
//! namespace — key names, read and write order, the torn-write guard and the
//! v1 → v2 profile migration — as pure code over the key-level
//! [`ProvisioningKv`] trait, so the on-flash layout is host-tested.
//! A platform crate supplies only the backend; on ESP-IDF that is the NVS
//! namespace [`NVS_NAMESPACE`], wrapped by
//! `rustyfarian_esp_idf_network::provisioning::ProvisioningStore`.
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
//! [`erase_all`](KvProvisioningStore::erase_all) can find and remove them. This
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
//! The on-flash schema version is `2`. Two profiles share this namespace
//! ([`SchemaProfile`]): `LorawanFieldDevice` writes the
//! LoRaWAN keys (`lora_dev_eui` / `lora_join_eui` / `lora_app_key`),
//! `WifiMqttDevice` writes the MQTT keys (`mqtt_host` / `mqtt_port` /
//! `mqtt_user` / `mqtt_pass` / `mqtt_client`); each writes only its active
//! group, and the Core (`wifi_ssid` / `wifi_pass` / `dev_name`) and OTA
//! (`ota_url`) keys are common to both. [`load`](KvProvisioningStore::load) reads
//! the `profile` key first; a `schema_ver == 1` record (or any record with the
//! `profile` key absent) is read as
//! [`SchemaProfile::LorawanFieldDevice`],
//! so beekeeper-class devices provisioned under v1 are never re-provisioned.

use anyhow::Context as _;

use crate::provisioning::{
    ExtraField, LoraFields, MqttFields, ProvisioningConfig, SchemaProfile, EXTRA_FIELDS_MAX,
};

/// The NVS namespace holding every provisioning value.
pub const NVS_NAMESPACE: &str = "rf_prov";

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
/// [`KvProvisioningStore::erase_all`] so a reset clears both profiles' groups
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

/// Maximum bytes the `extras_idx` value may occupy.
///
/// Eight extras of at most 13 name bytes plus seven separators is 111 bytes;
/// 256 leaves comfortable headroom and matches the ESP-IDF backend's per-key
/// read buffer size.
const EXTRAS_IDX_MAX: usize = 256;

/// Experimental: API may change before 1.0.
///
/// Typed key-value access to the provisioning namespace.
///
/// Contract:
/// - `Ok(None)` means no value of the requested type is stored under `key`
///   (a backend may report a key of another type as `None`); a backend
///   failure is `Err`;
/// - `remove` of an absent key is `Ok`;
/// - `set_*` of an existing key replaces its value;
/// - `get_str` returns the whole stored value; for a portal-validated config
///   the longest is the OTA URL (at most 128 bytes).
///
/// Reads take `&self`, writes `&mut self`; the owner serialises access.
pub trait ProvisioningKv {
    /// The backend's error, chained into the store's `anyhow` errors.
    type Error: std::error::Error + Send + Sync + 'static;

    /// Reads a `u8`.
    fn get_u8(&self, key: &str) -> Result<Option<u8>, Self::Error>;
    /// Writes a `u8`.
    fn set_u8(&mut self, key: &str, value: u8) -> Result<(), Self::Error>;
    /// Reads a string.
    fn get_str(&self, key: &str) -> Result<Option<String>, Self::Error>;
    /// Writes a string.
    fn set_str(&mut self, key: &str, value: &str) -> Result<(), Self::Error>;
    /// Removes a key; an absent key is `Ok`.
    fn remove(&mut self, key: &str) -> Result<(), Self::Error>;
}

/// Experimental: API may change before 1.0.
///
/// Store for committed provisioning configuration over a [`ProvisioningKv`]
/// backend.
///
/// Every method returns `anyhow::Result`; the backend's error is chained
/// under a context line naming the key. No error names a stored value or its
/// length.
pub struct KvProvisioningStore<K: ProvisioningKv> {
    kv: K,
}

impl<K: ProvisioningKv> KvProvisioningStore<K> {
    /// Experimental: API may change before 1.0.
    ///
    /// Wraps `kv`; performs no reads or writes.
    pub fn new(kv: K) -> Self {
        Self { kv }
    }

    /// The backend, for host-test inspection.
    #[cfg(test)]
    pub(crate) fn kv(&self) -> &K {
        &self.kv
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
    ///
    /// # Errors
    ///
    /// Returns `Err` on a backend read error.
    pub fn is_provisioned(&self) -> anyhow::Result<bool> {
        let schema = self
            .kv
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
    /// that re-provisions no device. Only the active profile's group is read
    /// and returned (`lora()` or `mqtt()`); the other group is `None`. A stored
    /// `mqtt_port` that is absent, empty, or not a valid `u16` is surfaced as a
    /// load error rather than silently defaulting, since a `WifiMqttDevice`
    /// device cannot connect without it.
    ///
    /// # Key layout
    ///
    /// The key layout (ADR 014 §4) is unchanged from 0.6.x: the same keys
    /// are read in the same order, so a record written by 0.6.x firmware loads
    /// unchanged. An empty stored `ota_url` reads as
    /// [`ProvisioningConfig::ota_url`] `== None`.
    ///
    /// # Errors
    ///
    /// Returns `Err` on a backend read error, an unrecognised `profile`, an
    /// invalid `mqtt_port`, a stored value longer than its field cap (the
    /// error names the key, never the value), or more than
    /// [`EXTRA_FIELDS_MAX`] stored extras.
    pub fn load(&self) -> anyhow::Result<Option<ProvisioningConfig>> {
        if !self.is_provisioned()? {
            return Ok(None);
        }

        let profile = match self.read_str(KEY_PROFILE)? {
            Some(s) => SchemaProfile::from_nvs_str(&s)
                .with_context(|| format!("unrecognised stored profile '{s}'"))?,
            None => SchemaProfile::LorawanFieldDevice,
        };

        let wifi_ssid = self.read_capped(KEY_WIFI_SSID)?;
        let wifi_password = self.read_capped(KEY_WIFI_PASS)?;
        let ota_url = self.read_capped(KEY_OTA_URL)?;
        let device_name = self.read_capped(KEY_DEV_NAME)?;

        let mut lora = None;
        let mut mqtt = None;

        match profile {
            SchemaProfile::LorawanFieldDevice => {
                let dev_eui_hex = self.read_capped(KEY_DEV_EUI)?;
                let join_eui_hex = self.read_capped(KEY_JOIN_EUI)?;
                let app_key_hex = self.read_capped(KEY_APP_KEY)?;
                lora = Some(LoraFields::from_storage_parts(
                    dev_eui_hex,
                    join_eui_hex,
                    app_key_hex,
                ));
            }
            SchemaProfile::WifiMqttDevice => {
                let host = self.read_capped(KEY_MQTT_HOST)?;
                let port_str = self
                    .read_str(KEY_MQTT_PORT)?
                    .context("provisioned as wifi_mqtt but mqtt_port key is absent")?;
                let port = port_str
                    .parse::<u16>()
                    .with_context(|| format!("stored mqtt_port '{port_str}' is not a valid u16"))?;
                let username = self.read_capped_opt(KEY_MQTT_USER)?;
                let password = self.read_capped_opt(KEY_MQTT_PASS)?;
                let client_id = self.read_capped_opt(KEY_MQTT_CLIENT)?;
                mqtt = Some(MqttFields::from_storage_parts(
                    host, port, username, password, client_id,
                ));
            }
        }

        let mut extras = Vec::new();
        for name in self.extra_names()? {
            let key = extra_key(&name);
            if let Some(value) = self.read_str(&key)? {
                extras.push(ExtraField {
                    key: capped(KEY_EXTRAS_IDX, &name)?,
                    value: capped(&key, &value)?,
                });
            }
        }
        if extras.len() > EXTRA_FIELDS_MAX {
            anyhow::bail!(
                "{} stored extras exceed the cap of {}",
                extras.len(),
                EXTRA_FIELDS_MAX
            );
        }

        Ok(Some(ProvisioningConfig::from_storage_parts(
            wifi_ssid,
            wifi_password,
            ota_url,
            device_name,
            lora,
            mqtt,
            // `heapless::Vec`'s `FromIterator` panics past its capacity; the
            // `take` bounds the iterator to the cap, so no panic path remains
            // on the boot path even without the `bail!` above.
            extras.into_iter().take(EXTRA_FIELDS_MAX).collect(),
        )))
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Writes a validated configuration in one logical commit.
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
    ///
    /// # Errors
    ///
    /// Returns `Err` on a backend write error, an extras index longer than
    /// 256 bytes, or a config whose active profile group is missing.
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
        self.set_str(KEY_OTA_URL, config.ota_url().unwrap_or(""))?;
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
                if let Err(e) = self.kv.remove(&key) {
                    log::warn!("failed to remove stale extra '{key}': {e:?}");
                }
            }
        }

        self.set_str(KEY_EXTRAS_IDX, &index)?;

        // The `profile` discriminator is written before the `schema_ver` guard
        // so a torn write never reads as provisioned with an unknown profile
        // (ADR 014 §4); `schema_ver` remains the very last write overall.
        self.set_str(KEY_PROFILE, profile.as_str())?;

        self.kv
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
                let _ = self.kv.remove(key);
                Ok(())
            }
        }
    }

    /// Removes the LoRaWAN group keys (best-effort), used when committing a
    /// `WifiMqttDevice` record over a previous LoRaWAN one.
    fn remove_lora_keys(&mut self) {
        for key in [KEY_DEV_EUI, KEY_JOIN_EUI, KEY_APP_KEY] {
            let _ = self.kv.remove(key);
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
            let _ = self.kv.remove(key);
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
    ///
    /// # Errors
    ///
    /// Never returns `Err` today: every removal is best-effort. The `Result`
    /// is kept so a stricter erase needs no signature change.
    pub fn erase_all(&mut self) -> anyhow::Result<()> {
        let _ = self.kv.remove(KEY_SCHEMA_VER);

        for name in self.extra_names().unwrap_or_default() {
            let _ = self.kv.remove(&extra_key(&name));
        }
        let _ = self.kv.remove(KEY_EXTRAS_IDX);

        for key in CANONICAL_KEYS {
            let _ = self.kv.remove(key);
        }

        Ok(())
    }

    /// Reads one string key into an owned `String`, or `None` if absent.
    fn read_str(&self, key: &str) -> anyhow::Result<Option<String>> {
        self.kv
            .get_str(key)
            .with_context(|| format!("failed to read NVS key '{key}'"))
    }

    /// Reads one string key into a capped string type; an absent key reads as
    /// the empty string.
    ///
    /// A value longer than the target's capacity is an error naming `key`,
    /// never the value.
    fn read_capped<T>(&self, key: &str) -> anyhow::Result<T>
    where
        T: for<'a> TryFrom<&'a str>,
    {
        let value = self.read_str(key)?.unwrap_or_default();
        capped(key, &value)
    }

    /// Like [`read_capped`](Self::read_capped), but an absent key reads as
    /// `None`.
    fn read_capped_opt<T>(&self, key: &str) -> anyhow::Result<Option<T>>
    where
        T: for<'a> TryFrom<&'a str>,
    {
        self.read_str(key)?
            .map(|value| capped(key, &value))
            .transpose()
    }

    /// Writes one string key.
    fn set_str(&mut self, key: &str, value: &str) -> anyhow::Result<()> {
        self.kv
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

/// Converts a stored `value` into its capped string type (a
/// `heapless::String<N>` inferred from the caller).
///
/// A value longer than the cap is an error naming `key`, never the value, so
/// neither a secret nor its length reaches a log line through this path.
fn capped<T>(key: &str, value: &str) -> anyhow::Result<T>
where
    T: for<'a> TryFrom<&'a str>,
{
    T::try_from(value)
        .map_err(|_| anyhow::anyhow!("stored value for NVS key '{key}' exceeds its field cap"))
}

/// Builds the prefixed NVS key for an extra field name.
fn extra_key(name: &str) -> String {
    format!("x_{name}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::test_psk;
    use std::collections::BTreeMap;

    /// A stored value: NVS keeps `u8` and string entries apart.
    #[derive(Clone, PartialEq, Eq)]
    enum Value {
        U8(u8),
        Str(String),
    }

    /// In-memory [`ProvisioningKv`]; a typed read of a key stored with the
    /// other type reads as absent, like an NVS typed lookup.
    #[derive(Default)]
    struct MemKv {
        map: BTreeMap<String, Value>,
        /// Every key written through the trait, in call order (for the
        /// write-order test).
        writes: Vec<String>,
    }

    impl MemKv {
        fn put_str(&mut self, key: &str, value: &str) {
            self.map
                .insert(key.to_owned(), Value::Str(value.to_owned()));
        }

        fn put_u8(&mut self, key: &str, value: u8) {
            self.map.insert(key.to_owned(), Value::U8(value));
        }
    }

    impl ProvisioningKv for MemKv {
        type Error = core::convert::Infallible;

        fn get_u8(&self, key: &str) -> Result<Option<u8>, Self::Error> {
            Ok(match self.map.get(key) {
                Some(Value::U8(v)) => Some(*v),
                _ => None,
            })
        }

        fn set_u8(&mut self, key: &str, value: u8) -> Result<(), Self::Error> {
            self.writes.push(key.to_owned());
            self.put_u8(key, value);
            Ok(())
        }

        fn get_str(&self, key: &str) -> Result<Option<String>, Self::Error> {
            Ok(match self.map.get(key) {
                Some(Value::Str(v)) => Some(v.clone()),
                _ => None,
            })
        }

        fn set_str(&mut self, key: &str, value: &str) -> Result<(), Self::Error> {
            self.writes.push(key.to_owned());
            self.put_str(key, value);
            Ok(())
        }

        fn remove(&mut self, key: &str) -> Result<(), Self::Error> {
            self.map.remove(key);
            Ok(())
        }
    }

    /// A 32-hex-digit AppKey-shaped value generated at runtime.
    fn test_app_key() -> String {
        format!("{0}{0}", test_psk())
    }

    /// A record exactly as 0.6.x firmware wrote it for the `wifi_mqtt`
    /// profile, key by key (ADR 014 §4).
    fn record_0_6_wifi_mqtt() -> MemKv {
        let mut kv = MemKv::default();
        kv.put_str("wifi_ssid", "FieldNet");
        kv.put_str("wifi_pass", test_psk());
        kv.put_str("ota_url", "https://ota.example.com/fw.bin");
        kv.put_str("dev_name", "clock-07");
        kv.put_str("mqtt_host", "broker.local");
        kv.put_str("mqtt_port", "1883");
        kv.put_str("mqtt_user", "clock");
        kv.put_str("mqtt_pass", test_psk());
        kv.put_str("mqtt_client", "clock-07-client");
        kv.put_str("x_site", "lab");
        kv.put_str("x_room", "attic");
        kv.put_str("extras_idx", "site,room");
        kv.put_str("profile", "wifi_mqtt");
        kv.put_u8("schema_ver", 2);
        kv
    }

    /// A LoRaWAN record with `schema_ver` but no `profile` key (v1 layout).
    fn record_without_profile() -> MemKv {
        let mut kv = MemKv::default();
        kv.put_str("wifi_ssid", "FarmNet");
        kv.put_str("wifi_pass", test_psk());
        kv.put_str("ota_url", "https://ota.example.com/hive.bin");
        kv.put_str("dev_name", "hive-01");
        kv.put_str("lora_dev_eui", "70B3D57ED0000001");
        kv.put_str("lora_join_eui", "70B3D57ED0000002");
        kv.put_str("lora_app_key", &test_app_key());
        kv.put_u8("schema_ver", 1);
        kv
    }

    fn extra(key: &str, value: &str) -> ExtraField {
        ExtraField {
            key: key.try_into().unwrap(),
            value: value.try_into().unwrap(),
        }
    }

    fn mqtt_config(extras: &[(&str, &str)]) -> ProvisioningConfig {
        let mqtt = MqttFields::from_storage_parts(
            "broker.local".try_into().unwrap(),
            8883,
            None,
            Some(test_psk().try_into().unwrap()),
            None,
        );
        ProvisioningConfig::from_storage_parts(
            "RoundNet".try_into().unwrap(),
            test_psk().try_into().unwrap(),
            "".try_into().unwrap(),
            "round-01".try_into().unwrap(),
            None,
            Some(mqtt),
            extras.iter().map(|(k, v)| extra(k, v)).collect(),
        )
    }

    #[test]
    fn record_written_by_0_6_x_loads_into_provisioning_config() {
        let store = KvProvisioningStore::new(record_0_6_wifi_mqtt());
        assert!(store.is_provisioned().unwrap());

        let cfg = store.load().unwrap().expect("0.6.x record is provisioned");
        assert_eq!(cfg.profile(), SchemaProfile::WifiMqttDevice);
        assert_eq!(cfg.wifi_ssid(), "FieldNet");
        assert_eq!(cfg.wifi_password(), test_psk());
        assert_eq!(cfg.ota_url(), Some("https://ota.example.com/fw.bin"));
        assert_eq!(cfg.device_name(), "clock-07");
        assert!(cfg.lora().is_none());

        let mqtt = cfg.mqtt().expect("mqtt group");
        assert_eq!(mqtt.host(), "broker.local");
        assert_eq!(mqtt.port(), 1883);
        assert_eq!(mqtt.username(), Some("clock"));
        assert_eq!(mqtt.password(), Some(test_psk()));
        assert_eq!(mqtt.client_id(), Some("clock-07-client"));

        let extras: Vec<(&str, &str)> = cfg
            .extras()
            .iter()
            .map(|e| (e.key.as_str(), e.value.as_str()))
            .collect();
        assert_eq!(extras, [("site", "lab"), ("room", "attic")]);
    }

    #[test]
    fn record_without_profile_loads_as_lorawan() {
        let store = KvProvisioningStore::new(record_without_profile());
        let cfg = store.load().unwrap().expect("provisioned");
        assert_eq!(cfg.profile(), SchemaProfile::LorawanFieldDevice);
        assert_eq!(cfg.wifi_ssid(), "FarmNet");
        assert_eq!(cfg.device_name(), "hive-01");
        assert!(cfg.mqtt().is_none());
        let lora: &LoraFields = cfg.lora().expect("lora group");
        assert_eq!(lora.dev_eui_hex(), "70B3D57ED0000001");
        assert_eq!(lora.join_eui_hex(), "70B3D57ED0000002");
        assert_eq!(lora.app_key_hex(), test_app_key());
        assert!(cfg.extras().is_empty());
    }

    #[test]
    fn missing_schema_ver_or_ssid_reads_as_unprovisioned() {
        for key in ["schema_ver", "wifi_ssid"] {
            let mut kv = record_0_6_wifi_mqtt();
            kv.map.remove(key);
            let store = KvProvisioningStore::new(kv);
            assert!(!store.is_provisioned().unwrap(), "{key} absent");
            assert!(store.load().unwrap().is_none(), "{key} absent");
        }
    }

    #[test]
    fn over_cap_value_errors_naming_the_key_only() {
        let long_ssid = "s".repeat(crate::wifi::SSID_MAX_LEN + 1);
        let mut kv = record_0_6_wifi_mqtt();
        kv.put_str("wifi_ssid", &long_ssid);
        let err = KvProvisioningStore::new(kv).load().unwrap_err();
        let shown = format!("{err:#}");
        assert!(shown.contains("wifi_ssid"), "{shown}");
        assert!(!shown.contains(&long_ssid));
        assert!(!shown.contains("33"));
    }

    #[test]
    fn more_than_the_extras_cap_errors_naming_the_count() {
        let mut kv = record_0_6_wifi_mqtt();
        let names: Vec<String> = (0..=crate::provisioning::EXTRA_FIELDS_MAX)
            .map(|i| format!("e{i}"))
            .collect();
        for name in &names {
            kv.put_str(&format!("x_{name}"), "secret-ish-value");
        }
        kv.put_str("extras_idx", &names.join(","));
        let err = KvProvisioningStore::new(kv).load().unwrap_err();
        let shown = format!("{err:#}");
        assert!(shown.contains('9'), "{shown}");
        assert!(!shown.contains("secret-ish-value"));
    }

    #[test]
    fn save_then_load_round_trips_and_drops_stale_extras() {
        let mut store = KvProvisioningStore::new(MemKv::default());
        let first = mqtt_config(&[("site", "lab"), ("room", "attic")]);
        store.save(&first).unwrap();
        assert_eq!(store.load().unwrap().as_ref(), Some(&first));
        assert!(store.kv().map.contains_key("x_room"));

        let second = mqtt_config(&[("site", "lab")]);
        store.save(&second).unwrap();
        assert_eq!(store.load().unwrap().as_ref(), Some(&second));
        assert!(!store.kv().map.contains_key("x_room"));
        assert!(store.kv().map.contains_key("x_site"));
        assert!(store.kv().map.get("schema_ver") == Some(&Value::U8(2)));
    }

    /// The torn-write guard (ADR 014 §4): every value, then `extras_idx`,
    /// then `profile`, and `schema_ver` strictly last, so a commit that loses
    /// power mid-way never reads as provisioned with partial data.
    #[test]
    fn save_writes_values_then_index_then_profile_then_schema_ver_last() {
        let mut store = KvProvisioningStore::new(MemKv::default());
        store
            .save(&mqtt_config(&[("site", "lab"), ("room", "attic")]))
            .unwrap();

        let writes = &store.kv().writes;
        let pos = |key: &str| {
            writes
                .iter()
                .position(|k| k == key)
                .unwrap_or_else(|| panic!("{key} was never written"))
        };
        assert_eq!(writes.last().map(String::as_str), Some("schema_ver"));
        assert_eq!(pos("profile") + 1, pos("schema_ver"));
        assert!(pos("extras_idx") < pos("profile"));
        assert!(pos("x_site") < pos("extras_idx"));
        assert!(pos("x_room") < pos("extras_idx"));
        for key in ["wifi_ssid", "wifi_pass", "mqtt_host", "mqtt_port"] {
            assert!(pos(key) < pos("extras_idx"), "{key} must precede the index");
        }
    }

    #[test]
    fn erase_all_removes_every_store_key() {
        let mut kv = record_0_6_wifi_mqtt();
        for key in ["lora_dev_eui", "lora_join_eui", "lora_app_key"] {
            kv.put_str(key, "70B3D57ED0000003");
        }
        let mut store = KvProvisioningStore::new(kv);
        store.erase_all().unwrap();
        assert!(!store.is_provisioned().unwrap());
        let left: Vec<&String> = store.kv().map.keys().collect();
        assert!(left.is_empty(), "left behind: {left:?}");
    }
}
