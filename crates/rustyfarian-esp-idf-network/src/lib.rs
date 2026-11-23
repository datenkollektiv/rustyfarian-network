//! ESP-IDF (std) network drivers for ESP32.
//!
//! This crate consolidates six ESP-IDF networking domains into one, each
//! gated behind a Cargo feature:
//!
//! | Feature | Domain |
//! |:--------|:-------|
//! | `wifi` | Wi-Fi STA connection manager + SoftAP lifecycle manager |
//! | `mqtt` | MQTT builder + handle, auto-reconnecting background event loop |
//! | `lora` | SX1262 radio driver + LoRaWAN Class A adapter for lorawan-device 0.12 |
//! | `espnow` | ESP-NOW peer-to-peer driver with channel scanning |
//! | `ota` | Streaming firmware download + SHA-256 verify + partition swap, NVS record store |
//! | `ota-wire` | MQTT OTA wire contract; with `mqtt`, the MQTT OTA consumer runtime |
//! | `ota-ddi` | credential-bound DDI transport and polling runtime (ADR 027) |
//! | `provisioning` | SoftAP captive-portal provisioning (requires `wifi`) |
//!
//! Enable only the features you need; `default = []` so no domain code is
//! compiled unless explicitly opted in.
//!
//! ## Cargo features
//!
//! ### Domain features (opt-in)
//!
//! - **`wifi`** — Wi-Fi connection manager and SoftAP lifecycle handler via `esp-idf-svc`.
//!   Provides [`wifi::WiFiManager`] for STA mode and [`wifi::SoftApManager`] for AP mode.
//!
//! - **`mqtt`** — Auto-reconnecting MQTT client with builder API and background event loop.
//!   Provides [`mqtt::MqttBuilder`] and [`mqtt::MqttHandle`]. **This feature compiles
//!   without `wifi`** — the MQTT client is transport-agnostic at the type level.
//!   (Note: bundled examples additionally depend on `wifi` because they must bring up
//!   Wi-Fi before connecting to MQTT; see [`[[example]]` `required-features`](https://docs.rs/cargo/1.0.0/cargo/reference/manifest.html#example)
//!   entries in `Cargo.toml`). You may depend on `features = ["mqtt"]` alone if you
//!   provide your own transport layer.
//!
//! - **`lora`** — SX1262 radio driver and LoRaWAN Class A adapter. Implements
//!   [`lora::sx1262_driver::EspIdfLoraRadio`] for use with `lorawan-device 0.12`.
//!   Provides full initialization, RX/TX, and LoRaWAN protocol bindings.
//!   The firmware must enable exactly one `critical-section` provider — `esp-idf-hal`'s
//!   `critical-section` feature or `rustyfarian-esp-idf-power`'s `critical-section-impl`;
//!   this feature selects none.
//!
//! - **`espnow`** — ESP-NOW peer driver with automatic channel scanning. Provides
//!   [`espnow::EspIdfEspNow`] for point-to-point frame exchange.
//!
//! - **`ota`** — OTA firmware update via streaming HTTP download with SHA-256
//!   verification. Provides [`ota::OtaSession`] for fetch-and-apply operations,
//!   the NVS record store ([`ota::EspNvsKv`]) and the update decisions.
//!
//! - **`ota-wire`** — the MQTT OTA wire contract (`ota::OtaCommand`, `ota::Manifest`,
//!   `ota::OtaStatus`), implies `ota`. Together with `mqtt` it adds the MQTT OTA consumer
//!   runtime (`ota::runtime`, `OtaSubmitter`). A DDI build (`ota-ddi`) leaves it out: the pair is a
//!   compile error unless the bench-only `ota-ddi-bench-mqtt-wire` is enabled (fleet DDI profile 0.4.0 §1).
//!
//! - **`ota-ddi`** — the credential-bound DDI transport and the DDI OTA runtime (ADR 027, experimental), implies `ota`.
//!   Provides `ota::ddi::DdiTransport` (one blocking method per DDI endpoint), `ota::ddi::DdiRuntime` and `ota::ddi::DdiHandle` (the `ota-worker` thread and the health hook) and `ota::ddi::DdiNudge` (the MQTT nudge hook; the `mqtt` feature is not required).
//!   Until TLS lands it accepts only an `http://` base URL, and only with `ota-ddi-insecure-http`.
//!
//! - **`ota-ddi-insecure-http`** — lab opt-in for an `http://` DDI base URL, implies `ota-ddi`; release images leave it out.
//!
//! - **`ota-ddi-bench-mqtt-wire`** — bench-only opt-in that builds `ota-wire` next to `ota-ddi` (implies both); a release image never enables it, and an image that does takes MQTT OTA commands only from a topic that only the server principal may write (ADR 018 amendment 2026-11-22).
//!
//! - **`provisioning`** — SoftAP captive-portal provisioning for Wi-Fi and
//!   LoRaWAN credentials. Requires `wifi` feature (SoftAP setup). Provides
//!   [`provisioning::ProvisioningBuilder`] and [`provisioning::ProvisioningSession`].
//!
//! ## Re-export patterns
//!
//! Each domain module in this crate re-exports the corresponding types from
//! [`juggler`] (the platform-agnostic core) for consumer convenience:
//!
//! - `wifi::*` exports [`wifi::WiFiConfig`], [`wifi::LinkMachine`], etc. from `juggler::wifi`
//! - `mqtt::*` exports [`mqtt::MqttConnectionState`], validation functions, etc. from `juggler::mqtt`
//!   and the config types shared with the bare-metal tier — [`mqtt::QoS`],
//!   [`mqtt::MqttConfig`], [`mqtt::LwtConfig`], [`mqtt::TryPublishError`]; raw
//!   `EspMqttClient` calls convert QoS with [`mqtt::to_esp_qos`]
//! - `lora::*` exports [`lora::LoraRadio`], [`lora::LorawanPhy`], [`lora::LorawanTimeline`], [`lora::Region`], etc. from `juggler::lora`
//! - `espnow::*` exports [`espnow::EspNowDriver`], [`espnow::ScanConfig`], etc. from `juggler::espnow`
//! - `ota::*` exports [`ota::OtaError`], [`ota::StreamingVerifier`], etc. from `juggler::ota`
//! - `provisioning::*` exports [`provisioning::ProvisioningState`], [`provisioning::SchemaProfile`], etc. from `juggler::provisioning`

/// Wi-Fi STA connection manager and SoftAP lifecycle manager.
#[cfg(feature = "wifi")]
pub mod wifi;

/// The Wi-Fi country adapter shared by the Wi-Fi and ESP-NOW drivers.
#[cfg(any(feature = "wifi", feature = "espnow"))]
mod regulatory;

/// MQTT builder, handle, and background event loop.
#[cfg(feature = "mqtt")]
pub mod mqtt;

/// SX1262 LoRa radio driver and LoRaWAN Class A adapter.
#[cfg(feature = "lora")]
pub mod lora;

/// ESP-NOW peer-to-peer driver.
#[cfg(feature = "espnow")]
pub mod espnow;

/// OTA firmware update driver.
#[cfg(feature = "ota")]
pub mod ota;

/// SoftAP captive-portal provisioning driver.
#[cfg(all(feature = "provisioning", esp_idf_esp_wifi_softap_support))]
pub mod provisioning;

// The captive-portal `provisioning` feature drives `SoftApManager`, which is
// only compiled when the linked ESP-IDF has SoftAP support. Emit one clear
// error naming the fix instead of a wall of unresolved-import errors.
#[cfg(all(feature = "provisioning", not(esp_idf_esp_wifi_softap_support)))]
compile_error!(
    "the `provisioning` feature requires SoftAP support; enable CONFIG_ESP_WIFI_SOFTAP_SUPPORT in the ESP-IDF sdkconfig used for this build (see docs/features/archive/wifi-softap-cfg-gate-v1.md)"
);

// A release image accepts no install, rollback or repair command over MQTT:
// DDI is the only network path that installs an image (fleet DDI profile
// 0.4.0 §1, ADR 018 amendment 2026-11-22). Only an image made solely for the
// bench may carry the MQTT OTA wire next to the DDI client, and it says so
// with `ota-ddi-bench-mqtt-wire`. `just check-ota-ddi-no-mqtt-runtime` checks
// that this guard fires.
#[cfg(all(
    feature = "ota-ddi",
    feature = "ota-wire",
    not(feature = "ota-ddi-bench-mqtt-wire")
))]
compile_error!(
    "the `ota-ddi` and `ota-wire` features together build an image that accepts OTA commands over MQTT, which the fleet DDI profile forbids for a release image; drop `ota-wire`, or enable `ota-ddi-bench-mqtt-wire` for an image made solely for the bench (see docs/adr/018-ota-consumer-runtime-in-library.md)"
);
