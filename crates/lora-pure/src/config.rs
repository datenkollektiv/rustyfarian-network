//! Hardware-agnostic LoRa and LoRaWAN configuration types.

/// LoRaWAN regional band plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Region {
    /// EU 863–870 MHz (most of Europe).
    EU868,
    /// US 902–928 MHz (North America).
    US915,
}

/// Hardware-agnostic LoRaWAN application configuration.
///
/// Contains the OTAA join credentials and application settings.
/// Does not carry any hardware pin assignments — those live in [`HeltecV3Pins`].
#[derive(Clone)]
pub struct LoraConfig {
    /// LoRaWAN regional plan.
    pub region: Region,
    /// Application EUI (JoinEUI in LoRaWAN 1.1), 8 bytes, MSB-first.
    ///
    /// Stored in the same byte order as the hex string shown in TTN Console
    /// (e.g. `70B3D57ED005ABCD` → `[0x70, 0xB3, 0xD5, 0x7E, 0xD0, 0x05, 0xAB, 0xCD]`).
    /// If the underlying LoRaWAN stack requires LSB-first, reverse at the join-request boundary.
    pub app_eui: [u8; 8],
    /// Device EUI, 8 bytes, MSB-first.
    ///
    /// Stored in the same byte order as the hex string shown in TTN Console.
    /// See `app_eui` for the endianness convention.
    pub dev_eui: [u8; 8],
    /// Application Key (root key for OTAA derivation), 16 bytes.
    pub app_key: [u8; 16],
    /// LoRaWAN port number used for OTA downlink commands.
    pub ota_port: u8,
}

impl Default for LoraConfig {
    /// Returns a zero-credential config for testing only.
    ///
    /// Do not use in production — all-zero EUIs and key will be rejected by
    /// any properly configured LoRaWAN network server.
    fn default() -> Self {
        Self {
            region: Region::EU868,
            app_eui: [0u8; 8],
            dev_eui: [0u8; 8],
            app_key: [0u8; 16],
            ota_port: crate::commands::OTA_COMMAND_PORT,
        }
    }
}

impl core::fmt::Debug for LoraConfig {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LoraConfig")
            .field("region", &self.region)
            .field("app_eui", &"<redacted>")
            .field("dev_eui", &"<redacted>")
            .field("app_key", &"<redacted>")
            .field("ota_port", &self.ota_port)
            .finish()
    }
}

impl LoraConfig {
    /// Build a [`LoraConfig`] from compile-time hex strings (as produced by `build.rs`).
    ///
    /// Each string must be exactly 16 hex chars (EUIs) or 32 hex chars (key).
    /// Strings are accepted in MSB-first order, matching the display format used by
    /// TTN Console (e.g. `"70B3D57ED005ABCD"`).
    /// Returns `None` if parsing fails — callers should log and fall back to all-zero keys.
    pub fn from_hex_strings(
        region: Region,
        dev_eui_hex: &str,
        app_eui_hex: &str,
        app_key_hex: &str,
    ) -> Option<Self> {
        let dev_eui = parse_hex8(dev_eui_hex)?;
        let app_eui = parse_hex8(app_eui_hex)?;
        let app_key = parse_hex16(app_key_hex)?;
        Some(Self {
            region,
            app_eui,
            dev_eui,
            app_key,
            ota_port: crate::commands::OTA_COMMAND_PORT,
        })
    }
}

fn parse_hex8(s: &str) -> Option<[u8; 8]> {
    let bytes = s.as_bytes();
    if bytes.len() != 16 {
        return None;
    }
    let mut out = [0u8; 8];
    for (i, byte) in out.iter_mut().enumerate() {
        let hi = hex_digit(bytes[i * 2])?;
        let lo = hex_digit(bytes[i * 2 + 1])?;
        *byte = (hi << 4) | lo;
    }
    Some(out)
}

fn parse_hex16(s: &str) -> Option<[u8; 16]> {
    let bytes = s.as_bytes();
    if bytes.len() != 32 {
        return None;
    }
    let mut out = [0u8; 16];
    for (i, byte) in out.iter_mut().enumerate() {
        let hi = hex_digit(bytes[i * 2])?;
        let lo = hex_digit(bytes[i * 2 + 1])?;
        *byte = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// GPIO pin assignments for the SX1262 on the Heltec WiFi LoRa 32 V3.
///
/// All GPIOs are the physical ESP32-S3 pin numbers.
/// These are board-specific constants; do not change for the Heltec V3.
#[derive(Debug, Clone, Copy)]
pub struct HeltecV3Pins {
    /// SPI NSS / chip-select (GPIO 8).
    pub nss: u8,
    /// SPI clock (GPIO 9).
    pub sck: u8,
    /// SPI MOSI (GPIO 10).
    pub mosi: u8,
    /// SPI MISO (GPIO 11).
    pub miso: u8,
    /// Radio reset (GPIO 12, active low).
    pub rst: u8,
    /// Radio busy flag (GPIO 13, high = busy).
    pub busy: u8,
    /// Radio IRQ / DIO1 (GPIO 14, rising edge = event ready).
    pub dio1: u8,
}

impl HeltecV3Pins {
    /// Returns the factory GPIO assignments for the Heltec WiFi LoRa 32 V3.
    pub const fn default_pins() -> Self {
        Self {
            nss: 8,
            sck: 9,
            mosi: 10,
            miso: 11,
            rst: 12,
            busy: 13,
            dio1: 14,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex8_valid() {
        let result = parse_hex8("0102030405060708");
        assert_eq!(
            result,
            Some([0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08])
        );
    }

    #[test]
    fn parse_hex8_too_short() {
        assert_eq!(parse_hex8("01020304"), None);
    }

    #[test]
    fn parse_hex8_all_zeros() {
        assert_eq!(parse_hex8("0000000000000000"), Some([0u8; 8]));
    }

    #[test]
    fn parse_hex16_valid() {
        let result = parse_hex16("00112233445566778899aabbccddeeff");
        assert_eq!(
            result,
            Some([
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff
            ])
        );
    }

    #[test]
    fn from_hex_strings_valid() {
        let cfg = LoraConfig::from_hex_strings(
            Region::EU868,
            "0000000000000001",
            "0000000000000002",
            "00000000000000000000000000000003",
        );
        assert!(cfg.is_some());
        let cfg = cfg.unwrap();
        assert_eq!(cfg.region, Region::EU868);
        assert_eq!(cfg.ota_port, 10);
        assert_eq!(cfg.dev_eui[7], 0x01);
    }

    #[test]
    fn from_hex_strings_invalid_eui() {
        let cfg = LoraConfig::from_hex_strings(
            Region::EU868,
            "GGGGGGGGGGGGGGGG", // invalid hex
            "0000000000000002",
            "00000000000000000000000000000003",
        );
        assert!(cfg.is_none());
    }
}
