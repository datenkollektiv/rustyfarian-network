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
//! Every record rule — key names (ADR 014 §4), read and write order, the
//! `schema_ver` torn-write guard and the v1 → v2 profile migration — lives in
//! [`juggler::provisioning::KvProvisioningStore`], where it is host-tested;
//! see `juggler::provisioning::kv_store` for the layout. This module supplies
//! only the NVS backend (`EspNvsProvisioningKv`).

use anyhow::Context as _;

use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};
use esp_idf_svc::sys::EspError;

use juggler::provisioning::kv_store::NVS_NAMESPACE;
use juggler::provisioning::{KvProvisioningStore, ProvisioningConfig, ProvisioningKv};

/// Stack buffer size for a single `get_str` read.
///
/// Every stored value is below this bound (the longest, the OTA URL, is capped
/// at 128 bytes by `juggler::provisioning`).
const READ_BUF_LEN: usize = 256;

/// [`ProvisioningKv`] over the `rf_prov` NVS namespace.
///
/// String reads go through a [`READ_BUF_LEN`]-byte stack buffer, above every
/// value the store writes; writes and removals use the `EspNvs` calls
/// directly.
struct EspNvsProvisioningKv(EspNvs<NvsDefault>);

impl ProvisioningKv for EspNvsProvisioningKv {
    type Error = EspError;

    fn get_u8(&self, key: &str) -> Result<Option<u8>, EspError> {
        self.0.get_u8(key)
    }

    fn set_u8(&mut self, key: &str, value: u8) -> Result<(), EspError> {
        self.0.set_u8(key, value)
    }

    fn get_str(&self, key: &str) -> Result<Option<String>, EspError> {
        let mut buf = [0u8; READ_BUF_LEN];
        Ok(self.0.get_str(key, &mut buf)?.map(str::to_string))
    }

    fn set_str(&mut self, key: &str, value: &str) -> Result<(), EspError> {
        self.0.set_str(key, value)
    }

    fn remove(&mut self, key: &str) -> Result<(), EspError> {
        self.0.remove(key).map(drop)
    }
}

/// Experimental: API may change before 1.0.
///
/// NVS-backed store for committed provisioning configuration.
///
/// Open it once per boot with [`ProvisioningStore::open`]; it owns a read/write
/// handle to the `rf_prov` namespace for its lifetime. Every method delegates
/// to [`KvProvisioningStore`] over the NVS backend.
pub struct ProvisioningStore {
    inner: KvProvisioningStore<EspNvsProvisioningKv>,
}

impl ProvisioningStore {
    /// Experimental: API may change before 1.0.
    ///
    /// Opens (creating if absent) the `rf_prov` NVS namespace read/write.
    pub fn open(partition: EspDefaultNvsPartition) -> anyhow::Result<Self> {
        let nvs = EspNvs::new(partition, NVS_NAMESPACE, true)
            .context("failed to open NVS namespace 'rf_prov'")?;
        Ok(Self {
            inner: KvProvisioningStore::new(EspNvsProvisioningKv(nvs)),
        })
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Reports whether a complete provisioning record is present: both the
    /// `schema_ver` guard (written last on a commit) and the Wi-Fi SSID.
    /// See [`KvProvisioningStore::is_provisioned`].
    pub fn is_provisioned(&self) -> anyhow::Result<bool> {
        self.inner.is_provisioned()
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Loads the stored configuration, or `None` if the device is not
    /// provisioned.
    ///
    /// An absent `profile` key reads as [`SchemaProfile::LorawanFieldDevice`](juggler::provisioning::SchemaProfile::LorawanFieldDevice)
    /// (the v1 → v2 migration). The NVS key layout (ADR 014 §4) is unchanged
    /// from 0.6.x, so a record written by 0.6.x firmware loads unchanged; only
    /// the returned in-memory type changed (0.6.x returned the removed
    /// `StoredConfig`).
    /// See [`KvProvisioningStore::load`].
    ///
    /// # Errors
    ///
    /// Returns `Err` on an NVS read error, an unrecognised `profile`, an
    /// invalid `mqtt_port`, a stored value longer than its field cap (the
    /// error names the key, never the value), or more than
    /// [`EXTRA_FIELDS_MAX`](juggler::provisioning::EXTRA_FIELDS_MAX) stored
    /// extras.
    pub fn load(&self) -> anyhow::Result<Option<ProvisioningConfig>> {
        self.inner.load()
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Writes a validated configuration to NVS in one logical commit, the
    /// `schema_ver` guard last. See [`KvProvisioningStore::save`].
    pub fn save(&mut self, config: &ProvisioningConfig) -> anyhow::Result<()> {
        self.inner.save(config)
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Removes every provisioning key, returning the namespace to an
    /// unprovisioned state, the `schema_ver` guard first.
    /// See [`KvProvisioningStore::erase_all`].
    pub fn erase_all(&mut self) -> anyhow::Result<()> {
        self.inner.erase_all()
    }
}
