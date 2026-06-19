//! Pure encode/decode/CRC/arbitration logic for flash credential records.
//!
//! # On-flash layout (little-endian throughout)
//!
//! ```text
//! [magic:u32 LE][layout_ver:u8][seq:u32 LE][record_len:u16 LE][profile_len:u8]
//!   [profile_str: profile_len bytes][TLV*][crc32:u32 LE]
//! ```
//!
//! `record_len` is the total byte length of the record from `magic` through
//! `crc32` inclusive — used at decode time to locate the CRC word
//! deterministically without scanning for sentinel patterns. `profile_len` is
//! the byte length of the profile-discriminator string (≤ 255). The CRC
//! covers `[magic..last_byte_before_crc]` — every byte of the record except
//! the trailing CRC word.
//!
//! # TLV tag table
//!
//! Retirement policy: gaps are permitted; **never reuse a retired tag**.
//! A retired tag's numeric value is permanently reserved so old firmware that
//! encounters the tag in a record it wrote will decode it correctly.

use core::fmt;

use juggler::mqtt::CLIENT_ID_MAX_LEN;
use juggler::provisioning::{
    config::{
        DEVICE_NAME_MAX_LEN, MQTT_HOST_MAX_LEN, MQTT_PASS_MAX_LEN, MQTT_USER_MAX_LEN,
        OTA_URL_MAX_LEN,
    },
    profile::{MqttFields, SchemaProfile},
    ProvisioningConfig,
};

use crate::store::StoreError;

// ── Layout constants ──────────────────────────────────────────────────────────

/// Magic bytes that identify a valid record header: ASCII "RFPR".
pub(crate) const MAGIC: u32 = 0x5246_5052;

/// Bump when the on-flash layout changes in a backward-incompatible way.
pub(crate) const LAYOUT_VER: u8 = 1;

/// Flash erase granularity (bytes). One sector = one half of the store.
pub(crate) const SECTOR_SIZE: usize = 4096;

/// Total store size: two sectors for A/B wear-levelling.
pub(crate) const STORE_SIZE: u32 = 8192;

/// esp-storage `FlashStorage` WRITE_SIZE — writes must be multiples of this.
pub(crate) const WRITE_GRANULARITY: usize = 4;

// ── TLV tag table ─────────────────────────────────────────────────────────────

pub(crate) const TAG_WIFI_SSID: u8 = 0x01;
pub(crate) const TAG_WIFI_PASS: u8 = 0x02;
pub(crate) const TAG_MQTT_HOST: u8 = 0x03;
pub(crate) const TAG_MQTT_PORT: u8 = 0x04;
pub(crate) const TAG_MQTT_USER: u8 = 0x05;
pub(crate) const TAG_MQTT_PASS: u8 = 0x06;
pub(crate) const TAG_MQTT_CLIENT: u8 = 0x07;
pub(crate) const TAG_OTA_URL: u8 = 0x08;
pub(crate) const TAG_DEVICE_NAME: u8 = 0x09;

// ── Header overhead ───────────────────────────────────────────────────────────

/// Bytes consumed by the fixed header before the profile string:
/// magic(4) + layout_ver(1) + seq(4) + record_len(2) + profile_len(1) = 12.
pub(crate) const HEADER_FIXED: usize = 12;

/// Byte offset of the `record_len` field in the fixed header.
pub(crate) const RECORD_LEN_OFFSET: usize = 9;

/// Byte offset of the `profile_len` field in the fixed header.
const PROFILE_LEN_OFFSET: usize = 11;

/// Bytes consumed by the trailing CRC word.
const CRC_LEN: usize = 4;

/// Maximum profile-string length that fits within one sector with at least
/// one TLV byte and the trailing CRC. Used to guard against a torn `len` field
/// that would cause a buffer overrun.
const MAX_PROFILE_STR_LEN: usize = SECTOR_SIZE - HEADER_FIXED - CRC_LEN - 1;

// ── CRC-32 (IEEE 802.3 reflected) ────────────────────────────────────────────

/// IEEE 802.3 reflected CRC-32 lookup table, computed at compile time.
///
/// Polynomial: `0xEDB8_8320` (reflected `0x04C1_1DB7`).
/// Init: `0xFFFF_FFFF`. Final XOR: `0xFFFF_FFFF`.
pub(crate) const fn crc32_table() -> [u32; 256] {
    let poly: u32 = 0xEDB8_8320;
    let mut table = [0u32; 256];
    let mut i = 0usize;
    while i < 256 {
        let mut crc = i as u32;
        let mut j = 0u32;
        while j < 8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ poly;
            } else {
                crc >>= 1;
            }
            j += 1;
        }
        table[i] = crc;
        i += 1;
    }
    table
}

/// Pre-computed CRC-32 table — zero runtime cost.
static CRC32_TABLE: [u32; 256] = crc32_table();

/// Compute the IEEE 802.3 CRC-32 of `data`.
pub(crate) fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        let idx = ((crc ^ u32::from(b)) & 0xFF) as usize;
        crc = (crc >> 8) ^ CRC32_TABLE[idx];
    }
    crc ^ 0xFFFF_FFFF
}

// ── Pad helper ────────────────────────────────────────────────────────────────

/// Round `len` up to the next multiple of [`WRITE_GRANULARITY`].
pub(crate) const fn pad_to_write_granularity(len: usize) -> usize {
    (len + WRITE_GRANULARITY - 1) & !(WRITE_GRANULARITY - 1)
}

// ── Decoded record ────────────────────────────────────────────────────────────

/// A successfully decoded and CRC-verified record read from flash.
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct DecodedRecord {
    pub(crate) seq: u32,
    pub(crate) profile: SchemaProfile,
    pub(crate) config: ProvisioningConfig,
}

/// Manual `Debug` impl: prints `seq` and `profile` but redacts `config` as
/// `"<redacted>"` so secrets never appear in log output.
impl fmt::Debug for DecodedRecord {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DecodedRecord")
            .field("seq", &self.seq)
            .field("profile", &self.profile)
            .field("config", &"<redacted>")
            .finish()
    }
}

// ── Encode ────────────────────────────────────────────────────────────────────

/// Write one TLV entry into `buf` at `*pos`.
///
/// Returns `Err(StoreError::BufferTooSmall)` if the buffer is too small to
/// hold the tag byte, the length byte, and the value bytes. Callers must
/// pass a `value` no longer than 255 bytes; this is enforced at the
/// `ProvisioningConfig` field-cap level today (every shipped field is
/// `<= 128` bytes), and the `debug_assert!` here is a tripwire in case a
/// future field cap is raised past 255 — a `len as u8` truncation would
/// silently corrupt the record.
fn write_tlv(buf: &mut [u8], pos: &mut usize, tag: u8, value: &[u8]) -> Result<(), StoreError> {
    debug_assert!(
        value.len() <= u8::MAX as usize,
        "TLV value length {} exceeds u8 — record format requires <=255",
        value.len()
    );
    let need = 2 + value.len();
    if *pos + need > buf.len() {
        return Err(StoreError::BufferTooSmall {
            need: *pos + need,
            have: buf.len(),
        });
    }
    buf[*pos] = tag;
    *pos += 1;
    buf[*pos] = value.len() as u8;
    *pos += 1;
    buf[*pos..*pos + value.len()].copy_from_slice(value);
    *pos += value.len();
    Ok(())
}

/// Encode `config` into `buf`, returning the total byte count including CRC.
///
/// The caller should pad the returned length to [`pad_to_write_granularity`]
/// before issuing a flash write.
pub(crate) fn encode_record(
    config: &ProvisioningConfig,
    seq: u32,
    buf: &mut [u8],
) -> Result<usize, StoreError> {
    // v1 record layout does not allocate TLV tags for opaque extras; a silent
    // drop would lose user data on `save → load`. Reject explicitly until a
    // future layout revision allocates extras tags.
    if !config.extras().is_empty() {
        return Err(StoreError::ExtrasNotSupported {
            count: config.extras().len(),
        });
    }

    let profile = config.profile();
    let profile_str = profile.as_str();
    let profile_bytes = profile_str.as_bytes();
    // `profile_len` is stored as `u8`; the two known discriminator strings are
    // 7 and 9 bytes. Tripwire in case a future profile adds a long discriminator.
    debug_assert!(
        profile_bytes.len() <= u8::MAX as usize,
        "profile discriminator '{profile_str}' exceeds u8 length",
    );

    // Minimum buffer check: fixed header + profile_str + at least zero TLVs + CRC.
    let min_size = HEADER_FIXED + profile_bytes.len() + CRC_LEN;
    if buf.len() < min_size {
        return Err(StoreError::BufferTooSmall {
            need: min_size,
            have: buf.len(),
        });
    }

    let mut pos = 0usize;

    // magic (4 bytes LE)
    buf[pos..pos + 4].copy_from_slice(&MAGIC.to_le_bytes());
    pos += 4;

    // layout_ver (1 byte)
    buf[pos] = LAYOUT_VER;
    pos += 1;

    // seq (4 bytes LE)
    buf[pos..pos + 4].copy_from_slice(&seq.to_le_bytes());
    pos += 4;

    // record_len placeholder (2 bytes LE) — patched in after the full record
    // length is known. The reader uses this to locate the CRC word
    // deterministically without scanning for sentinel patterns.
    debug_assert_eq!(pos, RECORD_LEN_OFFSET);
    pos += 2;

    // profile_len (1 byte): length of profile_str
    debug_assert_eq!(pos, PROFILE_LEN_OFFSET);
    buf[pos] = profile_bytes.len() as u8;
    pos += 1;

    // profile_str
    buf[pos..pos + profile_bytes.len()].copy_from_slice(profile_bytes);
    pos += profile_bytes.len();

    // TLV section — Core fields
    write_tlv(buf, &mut pos, TAG_WIFI_SSID, config.wifi_ssid().as_bytes())?;
    write_tlv(
        buf,
        &mut pos,
        TAG_WIFI_PASS,
        config.wifi_password().as_bytes(),
    )?;
    write_tlv(buf, &mut pos, TAG_OTA_URL, config.ota_url().as_bytes())?;
    write_tlv(
        buf,
        &mut pos,
        TAG_DEVICE_NAME,
        config.device_name().as_bytes(),
    )?;

    // Profile-specific TLVs
    match profile {
        SchemaProfile::WifiMqttDevice => {
            if let Some(mqtt) = config.mqtt() {
                write_tlv(buf, &mut pos, TAG_MQTT_HOST, mqtt.host().as_bytes())?;
                // port: 2 bytes LE
                write_tlv(buf, &mut pos, TAG_MQTT_PORT, &mqtt.port().to_le_bytes())?;
                if let Some(user) = mqtt.username() {
                    write_tlv(buf, &mut pos, TAG_MQTT_USER, user.as_bytes())?;
                }
                if let Some(pass) = mqtt.password() {
                    write_tlv(buf, &mut pos, TAG_MQTT_PASS, pass.as_bytes())?;
                }
                if let Some(cid) = mqtt.client_id() {
                    write_tlv(buf, &mut pos, TAG_MQTT_CLIENT, cid.as_bytes())?;
                }
            }
        }
        SchemaProfile::LorawanFieldDevice => {
            // LoRaWAN fields are not yet stored in Phase 1 (no TAG_* for EUIs/AppKey
            // in the locked table). The profile discriminator is stored; the LoRa
            // fields themselves will be added when TAG_* constants are allocated in a
            // future layout revision. For now encode an empty TLV section so decode
            // round-trips the profile correctly.
        }
    }

    // Patch in record_len now that the full payload-before-CRC is known.
    // Total record length = current pos + CRC_LEN.
    let record_len = (pos + CRC_LEN) as u16;
    buf[RECORD_LEN_OFFSET..RECORD_LEN_OFFSET + 2].copy_from_slice(&record_len.to_le_bytes());

    // Compute CRC over everything written so far (magic..last TLV byte).
    let crc = crc32(&buf[..pos]);
    if pos + CRC_LEN > buf.len() {
        return Err(StoreError::BufferTooSmall {
            need: pos + CRC_LEN,
            have: buf.len(),
        });
    }
    buf[pos..pos + 4].copy_from_slice(&crc.to_le_bytes());
    pos += 4;

    Ok(pos)
}

// ── Decode ────────────────────────────────────────────────────────────────────

/// Decode one record from `bytes`, validating magic, version, CRC, and
/// profile string.
///
/// # Security invariant
///
/// Error variants MUST NOT carry any bytes from the input. Only lengths and
/// expected values are included so error output cannot leak credential data.
pub(crate) fn decode_record(bytes: &[u8]) -> Result<DecodedRecord, StoreError> {
    // Minimum header + CRC must fit
    if bytes.len() < HEADER_FIXED + CRC_LEN {
        return Err(StoreError::ShortRecord {
            need: HEADER_FIXED + CRC_LEN,
            have: bytes.len(),
        });
    }

    // Magic check
    let magic = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    if magic != MAGIC {
        return Err(StoreError::BadMagic);
    }

    // Version check
    let ver = bytes[4];
    if ver != LAYOUT_VER {
        return Err(StoreError::BadVersion {
            found: ver,
            expected: LAYOUT_VER,
        });
    }

    // Sequence number
    let seq = u32::from_le_bytes([bytes[5], bytes[6], bytes[7], bytes[8]]);

    // record_len: total record length from magic..crc inclusive.
    let record_len =
        u16::from_le_bytes([bytes[RECORD_LEN_OFFSET], bytes[RECORD_LEN_OFFSET + 1]]) as usize;
    if record_len < HEADER_FIXED + CRC_LEN || record_len > bytes.len() {
        return Err(StoreError::ShortRecord {
            need: record_len,
            have: bytes.len(),
        });
    }

    // Profile-string length — guard against torn length field
    let profile_len = bytes[PROFILE_LEN_OFFSET] as usize;
    if profile_len > MAX_PROFILE_STR_LEN || HEADER_FIXED + profile_len + CRC_LEN > record_len {
        return Err(StoreError::ShortRecord {
            need: HEADER_FIXED + profile_len + CRC_LEN,
            have: record_len,
        });
    }

    // Profile string → SchemaProfile
    let profile_end = HEADER_FIXED + profile_len;
    let profile_bytes = &bytes[HEADER_FIXED..profile_end];
    let profile_str =
        core::str::from_utf8(profile_bytes).map_err(|_| StoreError::UnknownProfile {
            len: profile_len as u8,
        })?;
    let profile = SchemaProfile::from_nvs_str(profile_str).ok_or(StoreError::UnknownProfile {
        len: profile_len as u8,
    })?;

    // CRC: bytes are at offset `record_len - 4 .. record_len`.
    let crc_offset = record_len - CRC_LEN;
    let stored_crc = u32::from_le_bytes([
        bytes[crc_offset],
        bytes[crc_offset + 1],
        bytes[crc_offset + 2],
        bytes[crc_offset + 3],
    ]);
    let computed_crc = crc32(&bytes[..crc_offset]);
    if computed_crc != stored_crc {
        return Err(StoreError::BadCrc);
    }

    // Decode TLVs (between profile_end and crc_offset). Track presence
    // explicitly via `Option` so a CRC-valid but field-incomplete record
    // surfaces as `MissingRequiredField` rather than silently decoding into
    // a config with empty strings or fabricated MQTT defaults.
    let mut wifi_ssid_bytes: Option<&[u8]> = None;
    let mut wifi_pass_bytes: Option<&[u8]> = None;
    let mut ota_url_bytes: Option<&[u8]> = None;
    let mut device_name_bytes: Option<&[u8]> = None;
    let mut mqtt_host_bytes: Option<&[u8]> = None;
    let mut mqtt_port: Option<u16> = None;
    let mut mqtt_user_bytes: Option<&[u8]> = None;
    let mut mqtt_pass_bytes: Option<&[u8]> = None;
    let mut mqtt_client_bytes: Option<&[u8]> = None;

    /// Helper: assign `value` to `*slot`, returning `DuplicateTag` if the
    /// slot is already filled.
    fn set_once<'a>(
        slot: &mut Option<&'a [u8]>,
        tag: u8,
        value: &'a [u8],
    ) -> Result<(), StoreError> {
        if slot.is_some() {
            return Err(StoreError::DuplicateTag { tag });
        }
        *slot = Some(value);
        Ok(())
    }

    let mut tlv_pos = profile_end;
    while tlv_pos + 2 <= crc_offset {
        let tag = bytes[tlv_pos];
        let vlen = bytes[tlv_pos + 1] as usize;
        tlv_pos += 2;
        if tlv_pos + vlen > crc_offset {
            return Err(StoreError::ShortRecord {
                need: tlv_pos + vlen,
                have: crc_offset,
            });
        }
        let value = &bytes[tlv_pos..tlv_pos + vlen];
        tlv_pos += vlen;

        match tag {
            TAG_WIFI_SSID => set_once(&mut wifi_ssid_bytes, tag, value)?,
            TAG_WIFI_PASS => set_once(&mut wifi_pass_bytes, tag, value)?,
            TAG_OTA_URL => set_once(&mut ota_url_bytes, tag, value)?,
            TAG_DEVICE_NAME => set_once(&mut device_name_bytes, tag, value)?,
            TAG_MQTT_HOST => set_once(&mut mqtt_host_bytes, tag, value)?,
            TAG_MQTT_PORT if vlen == 2 => {
                if mqtt_port.is_some() {
                    return Err(StoreError::DuplicateTag { tag });
                }
                mqtt_port = Some(u16::from_le_bytes([value[0], value[1]]));
            }
            TAG_MQTT_USER => set_once(&mut mqtt_user_bytes, tag, value)?,
            TAG_MQTT_PASS => set_once(&mut mqtt_pass_bytes, tag, value)?,
            TAG_MQTT_CLIENT => set_once(&mut mqtt_client_bytes, tag, value)?,
            // Unknown tags: skip silently (forward-compatibility).
            _ => {}
        }
    }

    // Required-field check — every profile must carry the Core + OTA fields.
    let wifi_ssid_bytes =
        wifi_ssid_bytes.ok_or(StoreError::MissingRequiredField { tag: TAG_WIFI_SSID })?;
    let wifi_pass_bytes =
        wifi_pass_bytes.ok_or(StoreError::MissingRequiredField { tag: TAG_WIFI_PASS })?;
    let ota_url_bytes =
        ota_url_bytes.ok_or(StoreError::MissingRequiredField { tag: TAG_OTA_URL })?;
    let device_name_bytes = device_name_bytes.ok_or(StoreError::MissingRequiredField {
        tag: TAG_DEVICE_NAME,
    })?;

    // Profile-specific required fields.
    let (mqtt_host_bytes, mqtt_port_value) = match profile {
        SchemaProfile::WifiMqttDevice => {
            let host =
                mqtt_host_bytes.ok_or(StoreError::MissingRequiredField { tag: TAG_MQTT_HOST })?;
            let port = mqtt_port.ok_or(StoreError::MissingRequiredField { tag: TAG_MQTT_PORT })?;
            (Some(host), Some(port))
        }
        SchemaProfile::LorawanFieldDevice => (None, None),
    };

    let config = build_config(
        profile,
        wifi_ssid_bytes,
        wifi_pass_bytes,
        ota_url_bytes,
        device_name_bytes,
        mqtt_host_bytes,
        mqtt_port_value,
        mqtt_user_bytes,
        mqtt_pass_bytes,
        mqtt_client_bytes,
    )?;

    Ok(DecodedRecord {
        seq,
        profile,
        config,
    })
}

/// Build a [`ProvisioningConfig`] from decoded byte slices.
///
/// Required-field presence has already been verified by the caller; this
/// function additionally validates that every string-typed TLV value is
/// valid UTF-8, surfacing `StoreError::InvalidUtf8 { tag }` on the first
/// failure. This is the last "fail closed on a malformed-but-CRC-valid
/// record" check — the encoder only ever writes validated
/// `heapless::String` content, so invalid UTF-8 in a CRC-valid record
/// implies a buggy or adversarial producer.
///
/// Heapless-capacity truncation is silent and acceptable: the field caps
/// match encode-time caps, so a well-formed record round-trips without
/// loss.
#[allow(clippy::too_many_arguments)]
fn build_config(
    profile: SchemaProfile,
    wifi_ssid_bytes: &[u8],
    wifi_pass_bytes: &[u8],
    ota_url_bytes: &[u8],
    device_name_bytes: &[u8],
    mqtt_host_bytes: Option<&[u8]>,
    mqtt_port: Option<u16>,
    mqtt_user_bytes: Option<&[u8]>,
    mqtt_pass_bytes: Option<&[u8]>,
    mqtt_client_bytes: Option<&[u8]>,
) -> Result<ProvisioningConfig, StoreError> {
    use heapless::String as HString;

    fn bytes_to_hstring<const N: usize>(b: &[u8], tag: u8) -> Result<HString<N>, StoreError> {
        let s = core::str::from_utf8(b).map_err(|_| StoreError::InvalidUtf8 { tag })?;
        let mut h = HString::new();
        let _ = h.push_str(&s[..s.len().min(N)]);
        Ok(h)
    }

    fn bytes_to_opt_hstring<const N: usize>(
        b: Option<&[u8]>,
        tag: u8,
    ) -> Result<Option<HString<N>>, StoreError> {
        b.map(|v| bytes_to_hstring::<N>(v, tag)).transpose()
    }

    let wifi_ssid: HString<{ juggler::wifi::SSID_MAX_LEN }> =
        bytes_to_hstring(wifi_ssid_bytes, TAG_WIFI_SSID)?;
    let wifi_password: HString<{ juggler::wifi::PASSWORD_MAX_LEN }> =
        bytes_to_hstring(wifi_pass_bytes, TAG_WIFI_PASS)?;
    let ota_url: HString<OTA_URL_MAX_LEN> = bytes_to_hstring(ota_url_bytes, TAG_OTA_URL)?;
    let device_name: HString<DEVICE_NAME_MAX_LEN> =
        bytes_to_hstring(device_name_bytes, TAG_DEVICE_NAME)?;

    let mqtt = match profile {
        SchemaProfile::WifiMqttDevice => {
            // Required-field gate in `decode_record` guarantees both are
            // `Some` for this profile; expect with rationale rather than
            // re-default to silently-correct-but-wrong values.
            let host = mqtt_host_bytes.expect("decode_record guarantees host present");
            let port = mqtt_port.expect("decode_record guarantees port present");
            Some(MqttFields::from_storage_parts(
                bytes_to_hstring::<MQTT_HOST_MAX_LEN>(host, TAG_MQTT_HOST)?,
                port,
                bytes_to_opt_hstring::<MQTT_USER_MAX_LEN>(mqtt_user_bytes, TAG_MQTT_USER)?,
                bytes_to_opt_hstring::<MQTT_PASS_MAX_LEN>(mqtt_pass_bytes, TAG_MQTT_PASS)?,
                bytes_to_opt_hstring::<CLIENT_ID_MAX_LEN>(mqtt_client_bytes, TAG_MQTT_CLIENT)?,
            ))
        }
        SchemaProfile::LorawanFieldDevice => None,
    };

    Ok(ProvisioningConfig::from_storage_parts(
        wifi_ssid,
        wifi_password,
        ota_url,
        device_name,
        None,
        mqtt,
    ))
}

// ── Sector arbitration ────────────────────────────────────────────────────────

/// Pick the active record from two sector candidates.
///
/// # Arbitration policy
///
/// Higher sequence number wins. If both are `None`, returns `None`.
///
/// # Sequence number saturation
///
/// Sequence numbers are monotonically increasing and are never wrapped:
/// `ProvisioningStore::plan_save` calls `saturating_add(1)` so reaching
/// `u32::MAX` clamps further increments. In the degenerate
/// `seq_a == u32::MAX`, `seq_b == 0` case, the higher numeric value
/// (`u32::MAX`) wins — deterministic but with a known cost: once saturation
/// occurs, every subsequent `save` writes a new record with `seq == u32::MAX`
/// to the standby sector while `pick_active`'s `>=` tie keeps returning the
/// frozen sector, so `load` reports the stale record and the new writes are
/// effectively orphaned. Reaching `u32::MAX` requires ~4 billion saves — for
/// a credential store that is written single-digit times per device lifetime,
/// this is operationally impossible. If a future use case raises the write
/// frequency, the fix is to widen `seq` to `u64` or detect saturation and
/// surface a maintainer signal at `save` time.
pub(crate) fn pick_active(
    a: Option<DecodedRecord>,
    b: Option<DecodedRecord>,
) -> Option<DecodedRecord> {
    match (a, b) {
        (None, None) => None,
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (Some(a), Some(b)) => {
            if a.seq >= b.seq {
                Some(a)
            } else {
                Some(b)
            }
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::format;
    use juggler::provisioning::profile::MqttFields;
    use juggler::provisioning::ProvisioningConfig;

    /// Build a minimal `WifiMqttDevice` config for testing.
    fn make_wifi_mqtt_config(
        ssid: &str,
        pass: &str,
        host: &str,
        port: u16,
        user: Option<&str>,
        mqtt_pass: Option<&str>,
        client_id: Option<&str>,
    ) -> ProvisioningConfig {
        use heapless::String as HS;

        fn hs<const N: usize>(s: &str) -> HS<N> {
            let mut h = HS::new();
            let _ = h.push_str(&s[..s.len().min(N)]);
            h
        }

        fn opt_hs<const N: usize>(s: Option<&str>) -> Option<HS<N>> {
            s.map(hs::<N>)
        }

        let mqtt = MqttFields::from_storage_parts(
            hs::<MQTT_HOST_MAX_LEN>(host),
            port,
            opt_hs::<MQTT_USER_MAX_LEN>(user),
            opt_hs::<MQTT_PASS_MAX_LEN>(mqtt_pass),
            opt_hs::<CLIENT_ID_MAX_LEN>(client_id),
        );

        ProvisioningConfig::from_storage_parts(
            hs::<{ juggler::wifi::SSID_MAX_LEN }>(ssid),
            hs::<{ juggler::wifi::PASSWORD_MAX_LEN }>(pass),
            hs::<OTA_URL_MAX_LEN>("http://ota.example.com/fw.bin"),
            hs::<DEVICE_NAME_MAX_LEN>("test-device"),
            None,
            Some(mqtt),
        )
    }

    // Sentinel used to detect information leaks via error Display/Debug output.
    const LEAK_SENTINEL: &str = "LEAK-SENTINEL-XYZ";

    // ── CRC tests ─────────────────────────────────────────────────────────────

    #[test]
    fn crc32_known_vector() {
        // Industry-standard CRC-32 test vector (ISO 3309 / IEEE 802.3).
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn crc_byte_flipped_returns_bad_crc() {
        let config = make_wifi_mqtt_config(
            "myssid",
            "mypass",
            "broker.example.com",
            1883,
            None,
            None,
            None,
        );
        let mut buf = [0u8; SECTOR_SIZE];
        let len = encode_record(&config, 1, &mut buf).unwrap();
        // Flip a byte inside the 4-byte CRC field (last 4 bytes of record).
        let crc_start = len - 4;
        buf[crc_start] ^= 0xFF;
        assert_eq!(decode_record(&buf[..len]), Err(StoreError::BadCrc));
    }

    #[test]
    fn payload_byte_flipped_returns_bad_crc() {
        let config = make_wifi_mqtt_config(
            "myssid",
            "mypass",
            "broker.example.com",
            1883,
            None,
            None,
            None,
        );
        let mut buf = [0u8; SECTOR_SIZE];
        let len = encode_record(&config, 1, &mut buf).unwrap();
        // Flip a byte well inside the TLV region — past the fixed header AND
        // past the profile string ("wifi_mqtt" = 9 bytes), so the BadCrc
        // check fires rather than UnknownProfile.
        let tlv_byte = HEADER_FIXED + 9 + 5;
        buf[tlv_byte] ^= 0xFF;
        assert_eq!(decode_record(&buf[..len]), Err(StoreError::BadCrc));
    }

    #[test]
    fn decode_rejects_torn_length_field() {
        // Construct a buffer where `record_len` claims a larger value than the
        // slice can support. The bounds check on `record_len` itself catches
        // the tear before any subsequent field is read.
        let mut buf = [0u8; 50];
        buf[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        buf[4] = LAYOUT_VER;
        buf[5..9].copy_from_slice(&1u32.to_le_bytes());
        buf[RECORD_LEN_OFFSET..RECORD_LEN_OFFSET + 2].copy_from_slice(&200u16.to_le_bytes());
        buf[PROFILE_LEN_OFFSET] = 9;
        let result = decode_record(&buf);
        assert!(
            matches!(result, Err(StoreError::ShortRecord { .. })),
            "expected ShortRecord, got {result:?}"
        );
    }

    #[test]
    fn decode_rejects_bad_magic() {
        let config = make_wifi_mqtt_config(
            "myssid",
            "mypass",
            "broker.example.com",
            1883,
            None,
            None,
            None,
        );
        let mut buf = [0u8; SECTOR_SIZE];
        let len = encode_record(&config, 1, &mut buf).unwrap();
        // Overwrite magic with 0xDEAD_BEEF
        buf[0..4].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        assert_eq!(decode_record(&buf[..len]), Err(StoreError::BadMagic));
    }

    #[test]
    fn decode_rejects_bad_version() {
        let config = make_wifi_mqtt_config(
            "myssid",
            "mypass",
            "broker.example.com",
            1883,
            None,
            None,
            None,
        );
        let mut buf = [0u8; SECTOR_SIZE];
        let len = encode_record(&config, 1, &mut buf).unwrap();
        // Overwrite layout_ver with 99
        buf[4] = 99;
        // Recompute CRC so only version is wrong
        let crc_start = len - 4;
        let new_crc = crc32(&buf[..crc_start]);
        buf[crc_start..len].copy_from_slice(&new_crc.to_le_bytes());
        assert_eq!(
            decode_record(&buf[..len]),
            Err(StoreError::BadVersion {
                found: 99,
                expected: LAYOUT_VER
            })
        );
    }

    #[test]
    fn decode_rejects_unknown_profile() {
        // Manually build a record with profile string "unknown" (7 bytes).
        let profile_str = b"unknown";
        let mut buf = [0xFFu8; SECTOR_SIZE];
        // Reserve space for the full header before writing fields:
        // record_len is patched in last.
        let record_len = HEADER_FIXED + profile_str.len() + CRC_LEN;
        buf[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        buf[4] = LAYOUT_VER;
        buf[5..9].copy_from_slice(&1u32.to_le_bytes());
        buf[RECORD_LEN_OFFSET..RECORD_LEN_OFFSET + 2]
            .copy_from_slice(&(record_len as u16).to_le_bytes());
        buf[PROFILE_LEN_OFFSET] = profile_str.len() as u8;
        buf[HEADER_FIXED..HEADER_FIXED + profile_str.len()].copy_from_slice(profile_str);
        // No TLVs
        let crc_offset = HEADER_FIXED + profile_str.len();
        let crc = crc32(&buf[..crc_offset]);
        buf[crc_offset..crc_offset + 4].copy_from_slice(&crc.to_le_bytes());
        assert_eq!(
            decode_record(&buf[..record_len]),
            Err(StoreError::UnknownProfile { len: 7 })
        );
    }

    #[test]
    fn encode_into_buffer_too_small() {
        let config = make_wifi_mqtt_config(
            "myssid",
            "mypass",
            "broker.example.com",
            1883,
            None,
            None,
            None,
        );
        let mut buf = [0u8; 32];
        let result = encode_record(&config, 1, &mut buf);
        assert!(
            matches!(result, Err(StoreError::BufferTooSmall { .. })),
            "expected BufferTooSmall, got {result:?}"
        );
    }

    #[test]
    fn encode_pads_to_write_granularity_multiple() {
        let config = make_wifi_mqtt_config(
            "myssid",
            "mypass",
            "broker.example.com",
            1883,
            None,
            None,
            None,
        );
        let mut buf = [0u8; SECTOR_SIZE];
        let len = encode_record(&config, 1, &mut buf).unwrap();
        let padded = pad_to_write_granularity(len);
        assert_eq!(padded % WRITE_GRANULARITY, 0);
        // The raw len may or may not be aligned, but padded always is.
        assert!(padded >= len);
        assert!(padded < len + WRITE_GRANULARITY);
    }

    #[test]
    fn torn_write_recovery_keeps_higher_seq() {
        let config = make_wifi_mqtt_config(
            "myssid",
            "mypass",
            "broker.example.com",
            1883,
            None,
            None,
            None,
        );
        let mut buf5 = [0u8; SECTOR_SIZE];
        let mut buf6 = [0u8; SECTOR_SIZE];
        let len5 = encode_record(&config, 5, &mut buf5).unwrap();
        let len6 = encode_record(&config, 6, &mut buf6).unwrap();
        let a = decode_record(&buf5[..len5]).ok();
        let b = decode_record(&buf6[..len6]).ok();
        let winner = pick_active(a, b).unwrap();
        assert_eq!(winner.seq, 6);
    }

    #[test]
    fn torn_write_recovery_corrupt_higher_seq_falls_back() {
        let config = make_wifi_mqtt_config(
            "myssid",
            "mypass",
            "broker.example.com",
            1883,
            None,
            None,
            None,
        );
        let mut buf5 = [0u8; SECTOR_SIZE];
        let mut buf6 = [0u8; SECTOR_SIZE];
        let len5 = encode_record(&config, 5, &mut buf5).unwrap();
        let len6 = encode_record(&config, 6, &mut buf6).unwrap();
        // Corrupt seq=6 CRC
        buf6[len6 - 1] ^= 0xFF;
        let a = decode_record(&buf5[..len5]).ok();
        let b = decode_record(&buf6[..len6]).ok();
        assert!(b.is_none(), "corrupted seq=6 should fail to decode");
        let winner = pick_active(a, b).unwrap();
        assert_eq!(winner.seq, 5);
    }

    #[test]
    fn torn_write_recovery_handles_seq_wraparound() {
        // Policy: higher numeric value wins. u32::MAX > 0 numerically.
        // This is deterministic and documents the no-wraparound policy.
        let config = make_wifi_mqtt_config(
            "myssid",
            "mypass",
            "broker.example.com",
            1883,
            None,
            None,
            None,
        );
        let mut buf_max = [0u8; SECTOR_SIZE];
        let mut buf_zero = [0u8; SECTOR_SIZE];
        let len_max = encode_record(&config, u32::MAX, &mut buf_max).unwrap();
        let len_zero = encode_record(&config, 0, &mut buf_zero).unwrap();
        let a = decode_record(&buf_max[..len_max]).ok();
        let b = decode_record(&buf_zero[..len_zero]).ok();
        // u32::MAX wins — higher numeric value.
        let winner = pick_active(a, b).unwrap();
        assert_eq!(winner.seq, u32::MAX);
    }

    #[test]
    fn wifi_mqtt_round_trip_fully_populated() {
        let config = make_wifi_mqtt_config(
            "my-network",
            "s3cr3t-p@ss",
            "mqtt.example.com",
            8883,
            Some("user1"),
            Some("hunter2"),
            Some("device-abc"),
        );
        let mut buf = [0u8; SECTOR_SIZE];
        let len = encode_record(&config, 1, &mut buf).unwrap();
        let decoded = decode_record(&buf[..len]).unwrap();
        assert_eq!(decoded.config.wifi_ssid(), "my-network");
        assert_eq!(decoded.config.wifi_password(), "s3cr3t-p@ss");
        let mqtt = decoded.config.mqtt().unwrap();
        assert_eq!(mqtt.host(), "mqtt.example.com");
        assert_eq!(mqtt.port(), 8883);
        assert_eq!(mqtt.username(), Some("user1"));
        assert_eq!(mqtt.password(), Some("hunter2"));
        assert_eq!(mqtt.client_id(), Some("device-abc"));
    }

    #[test]
    fn wifi_mqtt_round_trip_anonymous() {
        let config = make_wifi_mqtt_config("open-net", "", "broker.local", 1883, None, None, None);
        let mut buf = [0u8; SECTOR_SIZE];
        let len = encode_record(&config, 1, &mut buf).unwrap();
        let decoded = decode_record(&buf[..len]).unwrap();
        let mqtt = decoded.config.mqtt().unwrap();
        assert_eq!(mqtt.username(), None);
        assert_eq!(mqtt.password(), None);
    }

    #[test]
    fn wifi_mqtt_round_trip_user_only() {
        let config =
            make_wifi_mqtt_config("net", "p", "b.example.com", 1883, Some("alice"), None, None);
        let mut buf = [0u8; SECTOR_SIZE];
        let len = encode_record(&config, 1, &mut buf).unwrap();
        let decoded = decode_record(&buf[..len]).unwrap();
        let mqtt = decoded.config.mqtt().unwrap();
        assert_eq!(mqtt.username(), Some("alice"));
        assert_eq!(mqtt.password(), None);
    }

    #[test]
    fn profile_discriminator_round_trip() {
        let config = make_wifi_mqtt_config("net", "p", "b.local", 1883, None, None, None);
        let mut buf = [0u8; SECTOR_SIZE];
        let len = encode_record(&config, 1, &mut buf).unwrap();
        let decoded = decode_record(&buf[..len]).unwrap();
        assert_eq!(decoded.profile, SchemaProfile::WifiMqttDevice);
    }

    #[test]
    fn lorawan_profile_string_decodes_as_lorawan_field_device() {
        // Build a record with profile="lorawan" + the four required Core
        // TLVs (wifi_ssid / wifi_pass / ota_url / device_name). The
        // discriminator string is the source of truth — decode must return
        // `SchemaProfile::LorawanFieldDevice` even when the producer was a
        // hand-built buffer rather than `encode_record`.
        //
        // The four Core TLVs are required by the new missing-field gate; a
        // record without them is rejected with `MissingRequiredField`.
        let mut buf = [0u8; SECTOR_SIZE];
        let lorawan_bytes = b"lorawan";
        let ssid = b"open-sesame";
        let pass = b"";
        let ota_url = b"http://example.com/fw.bin";
        let dev_name = b"hive";

        // Reserve placeholders; record_len patched in after we know total length.
        buf[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        buf[4] = LAYOUT_VER;
        buf[5..9].copy_from_slice(&1u32.to_le_bytes());
        buf[PROFILE_LEN_OFFSET] = lorawan_bytes.len() as u8;
        let mut pos = HEADER_FIXED;
        buf[pos..pos + lorawan_bytes.len()].copy_from_slice(lorawan_bytes);
        pos += lorawan_bytes.len();

        // Emit the four required Core TLVs.
        let mut write = |tag: u8, value: &[u8], pos: &mut usize| {
            buf[*pos] = tag;
            buf[*pos + 1] = value.len() as u8;
            buf[*pos + 2..*pos + 2 + value.len()].copy_from_slice(value);
            *pos += 2 + value.len();
        };
        write(TAG_WIFI_SSID, ssid, &mut pos);
        write(TAG_WIFI_PASS, pass, &mut pos);
        write(TAG_OTA_URL, ota_url, &mut pos);
        write(TAG_DEVICE_NAME, dev_name, &mut pos);

        let record_len = pos + CRC_LEN;
        buf[RECORD_LEN_OFFSET..RECORD_LEN_OFFSET + 2]
            .copy_from_slice(&(record_len as u16).to_le_bytes());
        let crc = crc32(&buf[..pos]);
        buf[pos..pos + 4].copy_from_slice(&crc.to_le_bytes());

        let decoded = decode_record(&buf[..record_len]).unwrap();
        assert_eq!(decoded.profile, SchemaProfile::LorawanFieldDevice);
        assert_eq!(decoded.config.wifi_ssid(), "open-sesame");
        assert!(decoded.config.mqtt().is_none());
    }

    #[test]
    fn tag_table_round_trip_locks_every_field() {
        // Regression guard: every TLV field must survive encode→decode.
        let config = make_wifi_mqtt_config(
            "ssid-lock",
            "pass-lock",
            "host-lock.example.com",
            9999,
            Some("user-lock"),
            Some("pass-lock-mqtt"),
            Some("client-lock"),
        );
        let mut buf = [0u8; SECTOR_SIZE];
        let len = encode_record(&config, 42, &mut buf).unwrap();
        let decoded = decode_record(&buf[..len]).unwrap();
        assert_eq!(decoded.seq, 42);
        assert_eq!(decoded.config.wifi_ssid(), "ssid-lock");
        assert_eq!(decoded.config.wifi_password(), "pass-lock");
        assert_eq!(decoded.config.ota_url(), "http://ota.example.com/fw.bin");
        assert_eq!(decoded.config.device_name(), "test-device");
        let mqtt = decoded.config.mqtt().unwrap();
        assert_eq!(mqtt.host(), "host-lock.example.com");
        assert_eq!(mqtt.port(), 9999);
        assert_eq!(mqtt.username(), Some("user-lock"));
        assert_eq!(mqtt.password(), Some("pass-lock-mqtt"));
        assert_eq!(mqtt.client_id(), Some("client-lock"));
    }

    /// Embed a sentinel value in `buf` so any error variant that accidentally
    /// captured input bytes would render it in its `Debug` output.
    fn fill_with_sentinel(buf: &mut [u8]) {
        let s = LEAK_SENTINEL.as_bytes();
        for i in 0..buf.len() {
            buf[i] = s[i % s.len()];
        }
    }

    /// Locks the [`StoreError`] security contract: every variant must render
    /// `Debug` output that contains no input bytes from the offending record.
    /// Exercises each error-producing decode path with a sentinel-filled
    /// buffer and asserts the sentinel never appears in the rendered error.
    ///
    /// If a future variant is added but not exercised here, the exhaustive
    /// match at the bottom will fail to compile until the new variant is
    /// covered.
    #[test]
    fn store_errors_carry_no_input_bytes() {
        // BadMagic — overwrite the magic prefix, leave the rest sentinel-filled
        let mut buf = [0u8; SECTOR_SIZE];
        fill_with_sentinel(&mut buf);
        buf[0..4].copy_from_slice(&0xDEAD_BEEFu32.to_le_bytes());
        assert_error_no_leak(decode_record(&buf[..64]), "BadMagic");

        // BadVersion — encode a valid record, flip the version byte, recompute CRC
        let cfg = make_wifi_mqtt_config(
            "myssid",
            "mypass",
            "broker.example.com",
            1883,
            None,
            None,
            None,
        );
        let mut buf = [0u8; SECTOR_SIZE];
        let len = encode_record(&cfg, 1, &mut buf).unwrap();
        buf[4] = 99;
        let crc_start = len - 4;
        let new_crc = crc32(&buf[..crc_start]);
        buf[crc_start..len].copy_from_slice(&new_crc.to_le_bytes());
        assert_error_no_leak(decode_record(&buf[..len]), "BadVersion");

        // BadCrc — encode a valid record then corrupt the CRC field
        let mut buf = [0u8; SECTOR_SIZE];
        let len = encode_record(&cfg, 1, &mut buf).unwrap();
        buf[len - 1] ^= 0xFF;
        assert_error_no_leak(decode_record(&buf[..len]), "BadCrc");

        // ShortRecord — a slice too small for the fixed header
        let mut buf = [0u8; HEADER_FIXED + CRC_LEN - 1];
        fill_with_sentinel(&mut buf);
        assert_error_no_leak(decode_record(&buf), "ShortRecord(fixed-header)");

        // ShortRecord (torn record_len) — claim more bytes than the slice has
        let mut buf = [0u8; 50];
        buf[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        buf[4] = LAYOUT_VER;
        buf[5..9].copy_from_slice(&1u32.to_le_bytes());
        buf[RECORD_LEN_OFFSET..RECORD_LEN_OFFSET + 2].copy_from_slice(&200u16.to_le_bytes());
        buf[PROFILE_LEN_OFFSET] = 9;
        assert_error_no_leak(decode_record(&buf), "ShortRecord(torn-length)");

        // UnknownProfile — build a record with profile string "unknown"
        let profile_str = b"unknown";
        let mut buf = [0u8; SECTOR_SIZE];
        fill_with_sentinel(&mut buf);
        let record_len = HEADER_FIXED + profile_str.len() + CRC_LEN;
        buf[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        buf[4] = LAYOUT_VER;
        buf[5..9].copy_from_slice(&1u32.to_le_bytes());
        buf[RECORD_LEN_OFFSET..RECORD_LEN_OFFSET + 2]
            .copy_from_slice(&(record_len as u16).to_le_bytes());
        buf[PROFILE_LEN_OFFSET] = profile_str.len() as u8;
        buf[HEADER_FIXED..HEADER_FIXED + profile_str.len()].copy_from_slice(profile_str);
        let crc_offset = HEADER_FIXED + profile_str.len();
        let crc = crc32(&buf[..crc_offset]);
        buf[crc_offset..crc_offset + 4].copy_from_slice(&crc.to_le_bytes());
        assert_error_no_leak(decode_record(&buf[..record_len]), "UnknownProfile");

        // BufferTooSmall — encode into a too-small buffer
        let mut tiny = [0u8; 32];
        let result = encode_record(&cfg, 1, &mut tiny);
        assert_error_no_leak(result.map(|_| ()), "BufferTooSmall");

        // Exhaustiveness lock: this match forces every StoreError variant to
        // appear in the test above. Adding a new variant fails compilation
        // here until the new variant is added to this match and exercised
        // by one of the cases above.
        //
        // `TooSmall`, `NotAligned`, `OffsetOutOfBounds`, `UnsupportedGeometry`,
        // `ExtrasNotSupported`, and `Flash` are not produced by `decode_record`
        // or by `encode_record` against the sample config — they are exercised
        // by the `store::tests::store_open_*` and `extras_rejected_at_encode`
        // tests instead, which also assert no input bytes leak.
        fn _exhaustiveness_lock(err: StoreError) {
            match err {
                StoreError::TooSmall { .. }
                | StoreError::NotAligned
                | StoreError::OffsetOutOfBounds { .. }
                | StoreError::UnsupportedGeometry { .. }
                | StoreError::BadMagic
                | StoreError::BadVersion { .. }
                | StoreError::BadCrc
                | StoreError::ShortRecord { .. }
                | StoreError::UnknownProfile { .. }
                | StoreError::BufferTooSmall { .. }
                | StoreError::ExtrasNotSupported { .. }
                | StoreError::MissingRequiredField { .. }
                | StoreError::DuplicateTag { .. }
                | StoreError::InvalidUtf8 { .. }
                | StoreError::Flash => {}
            }
        }
    }

    /// Shared assertion: error `Debug` and `Display` (where applicable)
    /// must not contain the sentinel substring.
    fn assert_error_no_leak<T: core::fmt::Debug>(result: Result<T, StoreError>, label: &str) {
        let err = result
            .err()
            .unwrap_or_else(|| panic!("{label}: expected Err"));
        let debug_str = format!("{err:?}");
        assert!(
            !debug_str.contains(LEAK_SENTINEL),
            "{label}: debug output leaks sentinel: {debug_str}"
        );
    }

    /// Build a CRC-valid record that emits only the subset of TLVs given in
    /// `tlvs`. Used by the missing-required-field tests to construct records
    /// that pass the CRC check but are missing fields the gate must catch.
    fn build_record_with_tlvs(
        profile: SchemaProfile,
        seq: u32,
        tlvs: &[(u8, &[u8])],
    ) -> ([u8; SECTOR_SIZE], usize) {
        let mut buf = [0u8; SECTOR_SIZE];
        let profile_str = profile.as_str().as_bytes();
        buf[0..4].copy_from_slice(&MAGIC.to_le_bytes());
        buf[4] = LAYOUT_VER;
        buf[5..9].copy_from_slice(&seq.to_le_bytes());
        buf[PROFILE_LEN_OFFSET] = profile_str.len() as u8;
        let mut pos = HEADER_FIXED;
        buf[pos..pos + profile_str.len()].copy_from_slice(profile_str);
        pos += profile_str.len();
        for (tag, value) in tlvs {
            buf[pos] = *tag;
            buf[pos + 1] = value.len() as u8;
            buf[pos + 2..pos + 2 + value.len()].copy_from_slice(value);
            pos += 2 + value.len();
        }
        let record_len = pos + CRC_LEN;
        buf[RECORD_LEN_OFFSET..RECORD_LEN_OFFSET + 2]
            .copy_from_slice(&(record_len as u16).to_le_bytes());
        let crc = crc32(&buf[..pos]);
        buf[pos..pos + 4].copy_from_slice(&crc.to_le_bytes());
        (buf, record_len)
    }

    #[test]
    fn decode_rejects_missing_wifi_ssid() {
        // Omit only wifi_ssid; everything else present.
        let (buf, len) = build_record_with_tlvs(
            SchemaProfile::WifiMqttDevice,
            1,
            &[
                (TAG_WIFI_PASS, b"pass"),
                (TAG_OTA_URL, b"http://o/fw"),
                (TAG_DEVICE_NAME, b"dev"),
                (TAG_MQTT_HOST, b"b.local"),
                (TAG_MQTT_PORT, &1883u16.to_le_bytes()),
            ],
        );
        assert_eq!(
            decode_record(&buf[..len]),
            Err(StoreError::MissingRequiredField { tag: TAG_WIFI_SSID })
        );
    }

    #[test]
    fn decode_rejects_missing_ota_url() {
        // Omit only ota_url.
        let (buf, len) = build_record_with_tlvs(
            SchemaProfile::WifiMqttDevice,
            1,
            &[
                (TAG_WIFI_SSID, b"ssid"),
                (TAG_WIFI_PASS, b"pass"),
                (TAG_DEVICE_NAME, b"dev"),
                (TAG_MQTT_HOST, b"b.local"),
                (TAG_MQTT_PORT, &1883u16.to_le_bytes()),
            ],
        );
        assert_eq!(
            decode_record(&buf[..len]),
            Err(StoreError::MissingRequiredField { tag: TAG_OTA_URL })
        );
    }

    #[test]
    fn decode_rejects_missing_mqtt_host_for_wifi_mqtt_profile() {
        // Profile demands MQTT host + port; omit host only.
        let (buf, len) = build_record_with_tlvs(
            SchemaProfile::WifiMqttDevice,
            1,
            &[
                (TAG_WIFI_SSID, b"ssid"),
                (TAG_WIFI_PASS, b"pass"),
                (TAG_OTA_URL, b"http://o/fw"),
                (TAG_DEVICE_NAME, b"dev"),
                (TAG_MQTT_PORT, &1883u16.to_le_bytes()),
            ],
        );
        assert_eq!(
            decode_record(&buf[..len]),
            Err(StoreError::MissingRequiredField { tag: TAG_MQTT_HOST })
        );
    }

    #[test]
    fn decode_rejects_missing_mqtt_port_for_wifi_mqtt_profile() {
        // Profile demands MQTT host + port; omit port only — closes the
        // silent `port=1883` fallback the original decode synthesised.
        let (buf, len) = build_record_with_tlvs(
            SchemaProfile::WifiMqttDevice,
            1,
            &[
                (TAG_WIFI_SSID, b"ssid"),
                (TAG_WIFI_PASS, b"pass"),
                (TAG_OTA_URL, b"http://o/fw"),
                (TAG_DEVICE_NAME, b"dev"),
                (TAG_MQTT_HOST, b"b.local"),
            ],
        );
        assert_eq!(
            decode_record(&buf[..len]),
            Err(StoreError::MissingRequiredField { tag: TAG_MQTT_PORT })
        );
    }

    #[test]
    fn decode_rejects_invalid_utf8_in_required_field() {
        // `0xFF, 0xFE, 0xFD` is not valid UTF-8. With every required TLV
        // present (so the missing-field gate does not fire first), the
        // decoder must surface the UTF-8 problem on the offending tag rather
        // than silently coercing it to an empty string via the old
        // `unwrap_or("")` fallback.
        let invalid_utf8: &[u8] = &[0xFF, 0xFE, 0xFD];
        let (buf, len) = build_record_with_tlvs(
            SchemaProfile::WifiMqttDevice,
            1,
            &[
                (TAG_WIFI_SSID, invalid_utf8),
                (TAG_WIFI_PASS, b"pass"),
                (TAG_OTA_URL, b"http://o/fw"),
                (TAG_DEVICE_NAME, b"dev"),
                (TAG_MQTT_HOST, b"b.local"),
                (TAG_MQTT_PORT, &1883u16.to_le_bytes()),
            ],
        );
        assert_eq!(
            decode_record(&buf[..len]),
            Err(StoreError::InvalidUtf8 { tag: TAG_WIFI_SSID })
        );
    }

    #[test]
    fn decode_rejects_invalid_utf8_in_optional_mqtt_user() {
        // The optional MQTT user/pass/client TLVs are also string-typed and
        // must be UTF-8-validated when present.
        let invalid_utf8: &[u8] = &[0xC0, 0xC1]; // both forbidden in UTF-8
        let (buf, len) = build_record_with_tlvs(
            SchemaProfile::WifiMqttDevice,
            1,
            &[
                (TAG_WIFI_SSID, b"ssid"),
                (TAG_WIFI_PASS, b"pass"),
                (TAG_OTA_URL, b"http://o/fw"),
                (TAG_DEVICE_NAME, b"dev"),
                (TAG_MQTT_HOST, b"b.local"),
                (TAG_MQTT_PORT, &1883u16.to_le_bytes()),
                (TAG_MQTT_USER, invalid_utf8),
            ],
        );
        assert_eq!(
            decode_record(&buf[..len]),
            Err(StoreError::InvalidUtf8 { tag: TAG_MQTT_USER })
        );
    }

    #[test]
    fn decode_rejects_duplicate_tag() {
        // Two TAG_WIFI_SSID entries in the same record — defence-in-depth
        // against a buggy or adversarial producer; our encoder never emits
        // duplicates, so a duplicate in a CRC-valid record is by definition
        // malformed.
        let (buf, len) = build_record_with_tlvs(
            SchemaProfile::WifiMqttDevice,
            1,
            &[
                (TAG_WIFI_SSID, b"first"),
                (TAG_WIFI_SSID, b"second"),
                (TAG_WIFI_PASS, b"pass"),
                (TAG_OTA_URL, b"http://o/fw"),
                (TAG_DEVICE_NAME, b"dev"),
                (TAG_MQTT_HOST, b"b.local"),
                (TAG_MQTT_PORT, &1883u16.to_le_bytes()),
            ],
        );
        assert_eq!(
            decode_record(&buf[..len]),
            Err(StoreError::DuplicateTag { tag: TAG_WIFI_SSID })
        );
    }

    #[test]
    fn extras_rejected_at_encode() {
        // Build a config carrying one opaque extra via `parse_form`, then
        // assert the encoder rejects it rather than silently dropping it.
        use juggler::provisioning::parse_form;
        let body = "wifi_ssid=open-sesame&wifi_pass=secret-pass\
                    &mqtt_uri=mqtt://broker.example.com:1883\
                    &ota_url=http://example.com/fw.bin&dev_name=hive\
                    &custom_field=custom-value";
        let cfg = parse_form(body, SchemaProfile::WifiMqttDevice).expect("valid fixture body");
        assert_eq!(cfg.extras().len(), 1, "fixture should produce one extra");

        let mut buf = [0u8; SECTOR_SIZE];
        let result = encode_record(&cfg, 1, &mut buf);
        assert_eq!(
            result,
            Err(StoreError::ExtrasNotSupported { count: 1 }),
            "encoder must refuse non-empty extras until a future layout adds tags"
        );

        // Error variant must not leak the extra's key or value.
        let err = result.err().unwrap();
        let debug_str = format!("{err:?}");
        assert!(!debug_str.contains("custom_field"));
        assert!(!debug_str.contains("custom-value"));
    }

    #[test]
    fn decoded_record_debug_redacts_secrets() {
        let config = make_wifi_mqtt_config(
            "safe-ssid",
            "SECRET-PSK-XYZ",
            "broker.local",
            1883,
            None,
            None,
            None,
        );
        let mut buf = [0u8; SECTOR_SIZE];
        let len = encode_record(&config, 1, &mut buf).unwrap();
        let decoded = decode_record(&buf[..len]).unwrap();
        let debug_str = format!("{decoded:?}");
        assert!(
            !debug_str.contains("SECRET-PSK-XYZ"),
            "debug output leaks Wi-Fi password: {debug_str}"
        );
        assert!(
            debug_str.contains("<redacted>"),
            "debug output should contain '<redacted>': {debug_str}"
        );
    }

    // ── Security-checklist item 8: CRC commit-guard locking tests ────────────

    /// Security-checklist item 8 lock: the CRC word must be the LAST 4 bytes
    /// of every encoded record, covering all preceding bytes from magic through
    /// the last TLV.
    ///
    /// For 50 distinct configurations (varying SSID, password, MQTT host, port,
    /// and optional fields) this test verifies:
    ///
    /// 1. `bytes[record_len - 4 .. record_len]` equals the CRC computed over
    ///    `bytes[..record_len - 4]`.
    /// 2. Flipping any single byte in the payload produces a `BadCrc` decode
    ///    error (the existing `crc_byte_flipped_returns_bad_crc` test covers
    ///    this; this test only checks the position invariant).
    #[test]
    fn crc_is_the_final_word_of_every_encoded_record() {
        // Simple deterministic pseudo-random generator — xorshift32 — so the
        // test is reproducible without a hardware RNG and without pulling in
        // an external rand crate.
        let mut rng_state: u32 = 0xDEAD_CAFE;
        let next_u32 = |s: &mut u32| -> u32 {
            *s ^= *s << 13;
            *s ^= *s >> 17;
            *s ^= *s << 5;
            *s
        };

        let next_short_str =
            |s: &mut u32, prefix: &str, max_extra: usize| -> alloc::string::String {
                let n = (next_u32(s) as usize % max_extra) + 1;
                let mut out = alloc::string::String::from(prefix);
                for _ in 0..n {
                    let c = (b'a' + (next_u32(s) % 26) as u8) as char;
                    out.push(c);
                }
                out
            };

        for i in 0u32..50 {
            let ssid = next_short_str(&mut rng_state, "net", 8);
            let pass = next_short_str(&mut rng_state, "pw", 12);
            let host = next_short_str(&mut rng_state, "broker", 10);
            let port = (next_u32(&mut rng_state) % 60000 + 1024) as u16;
            let user = if i % 2 == 0 {
                Some(next_short_str(&mut rng_state, "user", 6))
            } else {
                let _ = next_short_str(&mut rng_state, "", 6); // advance RNG
                None
            };
            let mqtt_pass = if i % 3 == 0 {
                Some(next_short_str(&mut rng_state, "pass", 8))
            } else {
                let _ = next_short_str(&mut rng_state, "", 8);
                None
            };

            let config = make_wifi_mqtt_config(
                &ssid,
                &pass,
                &host,
                port,
                user.as_deref(),
                mqtt_pass.as_deref(),
                None,
            );

            let mut buf = [0u8; SECTOR_SIZE];
            let record_len = encode_record(&config, i, &mut buf)
                .unwrap_or_else(|e| panic!("encode failed for iteration {i}: {e:?}"));

            // Compute the expected CRC over [0..record_len-4].
            let expected_crc = crc32(&buf[..record_len - 4]);

            // Read the stored CRC from [record_len-4..record_len].
            let stored_crc = u32::from_le_bytes([
                buf[record_len - 4],
                buf[record_len - 3],
                buf[record_len - 2],
                buf[record_len - 1],
            ]);

            assert_eq!(
                stored_crc, expected_crc,
                "iteration {i}: CRC at [record_len-4..record_len] ({stored_crc:#010x}) \
                 must equal CRC over [0..record_len-4] ({expected_crc:#010x})"
            );

            // Also verify the record round-trips correctly.
            let decoded = decode_record(&buf[..record_len])
                .unwrap_or_else(|e| panic!("decode failed for iteration {i}: {e:?}"));
            assert_eq!(decoded.seq, i, "iteration {i}: seq mismatch");
        }
    }

    /// Security-checklist item 8 lock: a record whose CRC word is not fully
    /// written (torn write — truncated one byte before the CRC starts) must
    /// NOT decode successfully.
    ///
    /// A truncation at `record_len - 5` means the CRC bytes are absent; the
    /// decoder must return `Err(ShortRecord { .. })` or `Err(BadCrc)` — never
    /// a successfully-typed `DecodedRecord`.
    #[test]
    fn torn_write_with_valid_header_but_no_crc_decodes_as_none() {
        let config = make_wifi_mqtt_config(
            "myssid",
            "mypass",
            "broker.example.com",
            1883,
            None,
            None,
            None,
        );
        let mut buf = [0u8; SECTOR_SIZE];
        let record_len = encode_record(&config, 1, &mut buf).unwrap();

        // Truncate one byte before the CRC starts — the CRC 4-byte word at
        // [record_len-4..record_len] is entirely missing from the slice.
        // `record_len - 5` is inside the last TLV payload region.
        let torn_len = record_len - 5;
        assert!(
            torn_len >= 1,
            "record_len ({record_len}) must be > 5 for this test to make sense"
        );

        let result = decode_record(&buf[..torn_len]);

        assert!(
            result.is_err(),
            "a torn record (missing CRC) must NOT decode as Ok; got: {result:?}"
        );

        // The error must be ShortRecord (the record_len field in the header
        // claims more bytes than the slice has) or BadCrc (if the decode
        // managed to read past the header but the CRC check fires first).
        // Either is acceptable — both are "fail closed" on a torn write.
        match result {
            Err(
                crate::store::StoreError::ShortRecord { .. } | crate::store::StoreError::BadCrc,
            ) => {
                // Correct: torn write detected.
            }
            Err(other) => {
                panic!(
                    "torn write returned unexpected error {other:?}; \
                     expected ShortRecord or BadCrc"
                );
            }
            Ok(_) => {
                panic!("torn write must not produce Ok(DecodedRecord)");
            }
        }
    }
}
