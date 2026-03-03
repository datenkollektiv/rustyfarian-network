//! LoRaWAN downlink command protocol.
//!
//! All command parsing lives here — OTA commands (Port 10, Phase 5) and
//! telemetry interval commands (Port 11, Phase 7) will both be decoded from
//! this module. No separate `telemetry.rs` is needed; Phase 7 extends this file.
//!
//! This module has no hardware dependencies and can be tested on any host.

/// LoRaWAN port number for OTA downlink commands.
pub const OTA_COMMAND_PORT: u8 = 10;

/// LoRaWAN port number for telemetry configuration commands (Phase 7).
pub const TELEMETRY_CONFIG_PORT: u8 = 11;

/// OTA command received via LoRaWAN downlink on [`OTA_COMMAND_PORT`].
///
/// Wire format (Port 10):
/// ```text
/// Byte 0:    Command ID (0x01–0x05)
/// Bytes 1–3: Version (major, minor, patch) — only for UpdateAvailable / ForceUpdate
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OtaCommand {
    /// Trigger a Wi-Fi OTA check against the compiled-in `OTA_SERVER_URL`.
    CheckUpdate,
    /// The server has a new firmware version available; update if newer than running.
    UpdateAvailable { major: u8, minor: u8, patch: u8 },
    /// Flash the given version unconditionally, skipping the battery guard.
    ForceUpdate { major: u8, minor: u8, patch: u8 },
    /// Roll back to the previous OTA slot.
    Rollback,
    /// Respond with a version uplink on the next TX opportunity.
    ReportVersion,
}

/// Parse an OTA command from a raw LoRaWAN downlink payload.
///
/// Returns `None` for unknown command IDs or truncated payloads.
pub fn parse_ota_command(payload: &[u8]) -> Option<OtaCommand> {
    let cmd_id = *payload.first()?;
    match cmd_id {
        0x01 => Some(OtaCommand::CheckUpdate),
        0x02 => {
            let (major, minor, patch) = parse_version(payload)?;
            Some(OtaCommand::UpdateAvailable {
                major,
                minor,
                patch,
            })
        }
        0x03 => {
            let (major, minor, patch) = parse_version(payload)?;
            Some(OtaCommand::ForceUpdate {
                major,
                minor,
                patch,
            })
        }
        0x04 => Some(OtaCommand::Rollback),
        0x05 => Some(OtaCommand::ReportVersion),
        _ => None,
    }
}

/// Encode a firmware version report uplink payload (3 bytes: major, minor, patch).
pub fn encode_version_report(major: u8, minor: u8, patch: u8) -> [u8; 3] {
    [major, minor, patch]
}

fn parse_version(payload: &[u8]) -> Option<(u8, u8, u8)> {
    if payload.len() < 4 {
        return None;
    }
    Some((payload[1], payload[2], payload[3]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_check_update() {
        assert_eq!(parse_ota_command(&[0x01]), Some(OtaCommand::CheckUpdate));
    }

    #[test]
    fn parse_check_update_ignores_trailing_bytes() {
        // Extra bytes are harmless — future extensions may append them.
        assert_eq!(
            parse_ota_command(&[0x01, 0xFF, 0xFF]),
            Some(OtaCommand::CheckUpdate)
        );
    }

    #[test]
    fn parse_update_available() {
        assert_eq!(
            parse_ota_command(&[0x02, 1, 2, 3]),
            Some(OtaCommand::UpdateAvailable {
                major: 1,
                minor: 2,
                patch: 3
            })
        );
    }

    #[test]
    fn parse_update_available_truncated() {
        // Missing version bytes → None.
        assert_eq!(parse_ota_command(&[0x02, 1, 2]), None);
    }

    #[test]
    fn parse_force_update() {
        assert_eq!(
            parse_ota_command(&[0x03, 0, 9, 0]),
            Some(OtaCommand::ForceUpdate {
                major: 0,
                minor: 9,
                patch: 0
            })
        );
    }

    #[test]
    fn parse_force_update_truncated() {
        assert_eq!(parse_ota_command(&[0x03]), None);
    }

    #[test]
    fn parse_rollback() {
        assert_eq!(parse_ota_command(&[0x04]), Some(OtaCommand::Rollback));
    }

    #[test]
    fn parse_report_version() {
        assert_eq!(parse_ota_command(&[0x05]), Some(OtaCommand::ReportVersion));
    }

    #[test]
    fn parse_unknown_command() {
        assert_eq!(parse_ota_command(&[0x99]), None);
    }

    #[test]
    fn parse_empty_payload() {
        assert_eq!(parse_ota_command(&[]), None);
    }

    #[test]
    fn encode_version_report_roundtrip() {
        let encoded = encode_version_report(1, 8, 3);
        assert_eq!(encoded, [1, 8, 3]);
    }

    #[test]
    fn encode_version_report_zeros() {
        assert_eq!(encode_version_report(0, 0, 0), [0, 0, 0]);
    }

    #[test]
    fn encode_version_report_max() {
        assert_eq!(encode_version_report(255, 255, 255), [255, 255, 255]);
    }
}
