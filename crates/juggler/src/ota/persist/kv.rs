//! The key-level storage trait the record store is generic over.

/// A bounded ASCII string value (NVS keys of this namespace never exceed 32 bytes).
pub type KvStr = heapless::String<32>;

/// A storage failure, carrying the backend's error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KvError {
    /// Backend error code: an `esp_err_t` on ESP-IDF, or one of the `CODE_*` constants, which the backend synthesizes (they are not `esp_err_t` values).
    pub code: i32,
}

impl KvError {
    /// The key exists with another type.
    pub const CODE_TYPE_MISMATCH: i32 = -0x7001;
    /// A value was over-long or not ASCII.
    pub const CODE_INVALID_VALUE: i32 = -0x7002;

    /// Creates an error with `code`.
    pub const fn new(code: i32) -> Self {
        Self { code }
    }
}

/// Typed key-value access to the `ota` namespace.
///
/// Contract:
/// - every `set_*` and `remove` that changes the store is ONE durable commit;
/// - `remove` of an absent key is `Ok` and commits nothing;
/// - `Ok(None)` means exactly "key absent": a type mismatch, an over-long value, a non-ASCII value or a flash error is `Err`;
/// - `set_*` of a key that exists with another type REPLACES it, in the same single commit (the NVS backend relies on `nvs_set_*` doing this); the repair of a mistyped `rb` depends on it;
/// - `set_str` may take two commit points (erase, then set) and a failed set may leave the key absent; a backend should replace atomically where it can (the NVS backend does), since a lost value is not restored.
///
/// Reads take `&self`, writes `&mut self`; the owner serialises access.
pub trait OtaKv {
    /// Reads a `u8`.
    fn get_u8(&self, key: &str) -> Result<Option<u8>, KvError>;
    /// Reads a `u32`.
    fn get_u32(&self, key: &str) -> Result<Option<u32>, KvError>;
    /// Reads a string of at most 32 ASCII bytes.
    fn get_str(&self, key: &str) -> Result<Option<KvStr>, KvError>;
    /// Writes a `u8`.
    fn set_u8(&mut self, key: &str, value: u8) -> Result<(), KvError>;
    /// Writes a `u32`.
    fn set_u32(&mut self, key: &str, value: u32) -> Result<(), KvError>;
    /// Writes a string of at most 32 ASCII bytes.
    fn set_str(&mut self, key: &str, value: &str) -> Result<(), KvError>;
    /// Removes a key; an absent key is `Ok`.
    fn remove(&mut self, key: &str) -> Result<(), KvError>;
}

impl core::fmt::Display for KvError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self.code {
            Self::CODE_TYPE_MISMATCH => f.write_str("OTA store key has an unexpected type"),
            Self::CODE_INVALID_VALUE => f.write_str("OTA store value is over-long or not ASCII"),
            code => write!(f, "OTA store backend error {code}"),
        }
    }
}

impl core::error::Error for KvError {}
