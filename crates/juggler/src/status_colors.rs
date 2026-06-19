//! Shared LED status colour palette for the rustyfarian boot sequence.
//!
//! All colours are `(R, G, B)` tuples — compatible with
//! [`pennant::PulseEffect::update`] and convertible to `rgb::RGB8`
//! without pulling in the `rgb` crate here.
//!
//! Wi-Fi and MQTT crates use these values so that every consumer gets a
//! coherent visual language out of the box.

/// Warm white — device is alive, firmware starting.
pub const BOOT: (u8, u8, u8) = (40, 30, 10);

/// Blue — Wi-Fi association in progress.
pub const WIFI_CONNECTING: (u8, u8, u8) = (0, 0, 255);

/// Cyan — MQTT broker connection in progress.
///
/// Distinct from Wi-Fi blue so both phases are distinguishable at a glance.
pub const MQTT_CONNECTING: (u8, u8, u8) = (0, 180, 180);

/// Dim green — connection successful (Wi-Fi or MQTT).
pub const CONNECTED: (u8, u8, u8) = (0, 20, 0);

/// Red — connection timeout or failure.
pub const ERROR: (u8, u8, u8) = (255, 0, 0);

/// Dim amber — offline / broker disconnected.
pub const OFFLINE: (u8, u8, u8) = (40, 20, 0);
