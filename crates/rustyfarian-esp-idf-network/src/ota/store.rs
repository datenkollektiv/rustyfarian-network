//! NVS backend of the persisted OTA records.
//!
//! [`EspNvsKv`] is the only [`OtaKv`] implementation for ESP-IDF; every record rule and write order lives in `juggler::ota::persist::OtaStore`.
//! Namespace `ota`, read-write, opened on the caller's partition clone (this module never takes the partition itself).

use std::ffi::CString;

use esp_idf_svc::handle::RawHandle;
use esp_idf_svc::nvs::{EspDefaultNvsPartition, EspNvs, NvsDefault};
use esp_idf_svc::sys::{esp, nvs_commit, nvs_set_str, EspError};
use juggler::ota::persist::{KvError, KvStr, OtaKv, OtaStore, StoreError, NVS_NAMESPACE};

/// Longest NVS string this module reads: 32 ASCII bytes plus the terminating NUL.
const STR_BUF_LEN: usize = 33;

/// [`OtaKv`] over an NVS namespace.
///
/// Residual risk, accepted: `EspNvs::get_str` converts with `from_utf8_unchecked`.
/// Every value in this namespace is written by this code as ASCII and NVS entries are CRC-protected, so undefined behaviour would need flash corruption that passes the CRC.
/// `set_str` is a single atomic NVS replace (no erase first), so a failed or interrupted write keeps the previous value.
/// The ASCII check after the read stays as a second line of defence.
/// `set_u8` / `set_u32` on a key that exists with another type replace it (no `ESP_ERR_NVS_TYPE_MISMATCH`; ESP-IDF v5.3.3 `nvs_storage.cpp` `Storage::writeItem` lines 397, 474, 514), which the repair of a mistyped `rb` relies on; with `CONFIG_NVS_LEGACY_DUP_KEYS_COMPATIBILITY=y` the old entry would stay behind as an unreachable duplicate that a typed read never sees.
pub struct EspNvsKv(EspNvs<NvsDefault>);

impl core::fmt::Debug for EspNvsKv {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("EspNvsKv").finish_non_exhaustive()
    }
}

fn kv_err(e: EspError) -> KvError {
    KvError::new(e.code())
}

fn type_mismatch() -> KvError {
    KvError::new(KvError::CODE_TYPE_MISMATCH)
}

fn invalid_value() -> KvError {
    KvError::new(KvError::CODE_INVALID_VALUE)
}

impl EspNvsKv {
    /// `Ok(None)` must mean "absent": NVS also answers "not found" for a key of another type, so check.
    fn absent_not_mismatched<T>(&self, key: &str) -> Result<Option<T>, KvError> {
        match self.0.find_key(key).map_err(kv_err)? {
            Some(_) => Err(type_mismatch()),
            None => Ok(None),
        }
    }
}

impl OtaKv for EspNvsKv {
    fn get_u8(&self, key: &str) -> Result<Option<u8>, KvError> {
        match self.0.get_u8(key).map_err(kv_err)? {
            Some(value) => Ok(Some(value)),
            None => self.absent_not_mismatched(key),
        }
    }

    fn get_u32(&self, key: &str) -> Result<Option<u32>, KvError> {
        match self.0.get_u32(key).map_err(kv_err)? {
            Some(value) => Ok(Some(value)),
            None => self.absent_not_mismatched(key),
        }
    }

    fn get_str(&self, key: &str) -> Result<Option<KvStr>, KvError> {
        let mut buf = [0u8; STR_BUF_LEN];
        match self.0.get_str(key, &mut buf).map_err(kv_err)? {
            Some(text) if text.is_ascii() => {
                KvStr::try_from(text).map(Some).map_err(|_| invalid_value())
            }
            Some(_) => Err(invalid_value()),
            None => self.absent_not_mismatched(key),
        }
    }

    fn set_u8(&mut self, key: &str, value: u8) -> Result<(), KvError> {
        self.0.set_u8(key, value).map_err(kv_err)
    }

    fn set_u32(&mut self, key: &str, value: u32) -> Result<(), KvError> {
        self.0.set_u32(key, value).map_err(kv_err)
    }

    fn set_str(&mut self, key: &str, value: &str) -> Result<(), KvError> {
        if !value.is_ascii() || value.len() >= STR_BUF_LEN {
            return Err(invalid_value());
        }
        let c_key = CString::new(key).map_err(|_| invalid_value())?;
        let c_value = CString::new(value).map_err(|_| invalid_value())?;
        let handle = self.0.handle();
        // SAFETY: `handle` is the open NVS handle owned by `self.0`, which outlives this call; both pointers come from live `CString`s that are NUL-terminated.
        // `EspNvs::set_str` is not used: it erases the key first, so a failed or interrupted set would lose the old value.
        // `nvs_set_str` alone replaces the entry atomically (new entry written, then the old one erased).
        esp!(unsafe { nvs_set_str(handle, c_key.as_ptr(), c_value.as_ptr()) }).map_err(kv_err)?;
        // SAFETY: same handle as above.
        esp!(unsafe { nvs_commit(handle) }).map_err(kv_err)
    }

    fn remove(&mut self, key: &str) -> Result<(), KvError> {
        self.0.remove(key).map(drop).map_err(kv_err)
    }
}

fn random_u32() -> u32 {
    // SAFETY: `esp_random` has no preconditions; it reads the hardware RNG and may be called from any task.
    unsafe { esp_idf_svc::sys::esp_random() }
}

/// Longest NVS namespace name, in bytes (the terminating NUL is not counted).
const NAMESPACE_MAX_LEN: usize = 15;

/// Whether `namespace` is a valid NVS namespace name: 1 to 15 bytes, no NUL.
fn valid_namespace(namespace: &str) -> bool {
    (1..=NAMESPACE_MAX_LEN).contains(&namespace.len()) && !namespace.contains('\0')
}

/// Opens the record store on `partition` in the namespace [`NVS_NAMESPACE`] (`"ota"`).
///
/// Same as [`open_store_in`] with the default namespace.
///
/// # Errors
///
/// Returns [`StoreError::Kv`] when the namespace cannot be opened.
pub fn open_store(partition: EspDefaultNvsPartition) -> Result<OtaStore<EspNvsKv>, StoreError> {
    open_store_in(partition, NVS_NAMESPACE)
}

/// Opens the record store on `partition` in `namespace`.
///
/// Performs no NVS reads or writes beyond opening the namespace.
/// The install epoch is drawn from the hardware RNG the first time an id is handed out; a weak source would only weaken report deduplication after an NVS erase.
///
/// `namespace` must be 1 to 15 bytes (the NVS limit) without a NUL byte.
/// Two stores in different namespaces of one partition are independent.
///
/// The store is not thread-safe; wrap it in one `Mutex` shared by every user.
///
/// # Errors
///
/// Returns [`StoreError::Kv`] with code [`KvError::CODE_INVALID_VALUE`] for an invalid `namespace` (nothing is opened), and [`StoreError::Kv`] when the namespace cannot be opened.
pub fn open_store_in(
    partition: EspDefaultNvsPartition,
    namespace: &str,
) -> Result<OtaStore<EspNvsKv>, StoreError> {
    if !valid_namespace(namespace) {
        return Err(StoreError::Kv(invalid_value()));
    }
    let nvs = EspNvs::new(partition, namespace, true).map_err(|e| StoreError::Kv(kv_err(e)))?;
    Ok(OtaStore::open(EspNvsKv(nvs), random_u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn namespace_length_is_one_to_fifteen_bytes() {
        assert!(!valid_namespace(""));
        assert!(valid_namespace("a"));
        assert!(valid_namespace("ota"));
        assert!(valid_namespace("123456789012345"));
        assert!(!valid_namespace("1234567890123456"));
        assert!(!valid_namespace("o\0ta"));
    }

    #[test]
    fn the_default_namespace_is_valid() {
        assert!(valid_namespace(NVS_NAMESPACE));
    }
}
