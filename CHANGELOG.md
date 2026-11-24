# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.8.0] - 2026-11-24

### Added

- `rustyfarian-esp-idf-network` DDI (hawkBit) OTA client behind `ota-ddi`, a plain-HTTP lab opt-in (`ota-ddi-insecure-http`) until TLS lands ([ddi-client-v1](docs/features/ddi-client-v1.md)).
- `rustyfarian-esp-idf-network` opt-in Wi-Fi portal fallback for the `WifiMqttDevice` profile ([wifi-portal-fallback-v1](docs/features/wifi-portal-fallback-v1.md)).
- `WiFiManager::ip()` and an inherent async `MqttHandle::publish` on both tiers (ADR 022).

### Changed

- **Breaking:** `rustyfarian-esp-idf-network` builds against ESP-IDF v6.1 (picolibc by default, esp-mqtt as the managed `espressif/mqtt` component), so firmware sets `ESP_IDF_VERSION = "v6.1"`.
- **Breaking:** the ESP-IDF tier matches esp-hal: async `wait_connected()`, `is_connected() -> bool`, `&[u8]` payloads for `try_publish`, the sync publish renamed `enqueue`, and `DisconnectReason` as an enum.
- **Breaking:** the MQTT OTA wire moves behind its own `ota-wire` feature, `OtaError` becomes `#[non_exhaustive]` with the DDI variants, and `Version::parse` accepts only plain `MAJOR.MINOR.PATCH`.

### Deprecated

- The MQTT OTA wire and runtime (`ota-wire`), in favour of the DDI client and to be removed in a later release (ADR 018 amendment 2026-11-24).
- `rustyfarian-esp-hal-network` `WifiLink`, `AsyncWifiHandle::wait_for_ip` and `MqttHandle::publish_with`, in favour of the names the ESP-IDF tier shares.

### Removed

- **Breaking:** `rustyfarian-esp-idf-network` removes the items deprecated in 0.7.0, among them `MqttManager`, `with_default_startup_message()` and `WiFiManager::link_state()`.
- **Breaking:** `juggler` removes `WifiDriver`, the old OTA action machines and the ESP-NOW `DeferredRestore` and `ScanConfig::probe_data`.

### Fixed

- `rustyfarian-esp-idf-network` portal runners report a save or factory reset that finishes during shutdown as committed instead of `PortalExitedWithoutCommit`.
- The workspace requires `heapless 0.9.4`, which fixes nine unsound advisories (RUSTSEC-2026-0574 to 0582).

## [0.7.0] - 2026-11-12

### Added

- Tier parity (ADR 022, ADR 023): Wi-Fi, MQTT, ESP-NOW, OTA, provisioning and LoRa logic of both tiers now runs on shared `juggler` cores, so one application body compiles against either tier crate ([tier-parity-v1](docs/features/archive/tier-parity-v1.md)).
- `rustyfarian-esp-hal-network` `mqtt` feature (ADR 019): async MQTT 5 client over `minimq` for `esp32c3`, `esp32c6`, `esp32s3` and `esp32`, with LWT, re-subscribe, auto-reconnect and non-blocking `try_publish*` ([esp-hal-mqtt-v1](docs/features/archive/esp-hal-mqtt-v1.md)).
- `rustyfarian-esp-hal-network` `espnow` feature (ADR 020, `esp32c3` and `esp32c6`): `EspHalEspNow` implements `EspNowDriver` and `EspNowLink` ([esp-hal-espnow-v1](docs/features/archive/esp-hal-espnow-v1.md)).

### Changed

- **Breaking:** station TX power defaults to `juggler::wifi::STATION_TX_POWER_DEFAULT` (`TxPowerLevel::Low`, 8.5 dBm) on both tiers, so firmware that wants the old ESP-IDF level passes `with_tx_power(TxPowerLevel::Medium)`.
- **Breaking:** `rustyfarian-esp-idf-network` `MqttBuilder::build(&wifi)` and `build_and_wait(&wifi, &mut led)` take the Wi-Fi link, and the client is created once the station has an IP.
- **Breaking:** `rustyfarian-esp-hal-network` `ProvisioningBuilder::start(spawner, timg0, sw_int, wifi, store, rng)` brings the SoftAP up itself, so the caller's `ApConfig` and `init_softap_async` calls are deleted.

### Deprecated

- `rustyfarian-esp-idf-network` `mqtt::MqttManager`: use `MqttBuilder` and `MqttHandle`.
- `rustyfarian-esp-idf-network` `MqttBuilder::with_default_startup_message()`: use `with_startup_message(topic, payload, qos, retain)`.
- `juggler::wifi::WifiDriver`: no tier implements it, so use the ADR 022 link traits.

### Removed

- `rustyfarian-esp-idf-network` `MqttHandle::is_connected()` and `build_and_wait()` no longer have a "helper-spawn failure" mode, since no connect thread is spawned.

### Fixed

- `rustyfarian-esp-idf-network` `WiFiManager` never reconnected after a link loss ([wifi-reconnect-and-suspend-v1](docs/features/archive/wifi-reconnect-and-suspend-v1.md)).
- esp-hal station joined the first AP found instead of the strongest, and both tiers now honour `ApSelection`.
- `juggler::mqtt::PendingAcks` grew with every fire-and-forget QoS 1 publish, and it is now capped at `EARLY_ACK_CAPACITY` (8).

## [0.6.0] - 2026-10-09

### Added

- OTA consumer runtime and record store (`juggler::ota::{runtime, persist}`, IDF tier `ota`): `OtaRuntime`, `OtaSubmitter` and `OtaHandle` run command intake, an update/rollback/repair worker and a `rolled_back` reporter, and the library never restarts on its own ([ota-consumer-runtime-v1](docs/features/archive/ota-consumer-runtime-v1.md)).
- OTA decision core in `juggler::ota`: `decide_offer`, `reconcile` and `OtaError::code()` ([ota-decision-core-v1](docs/features/archive/ota-decision-core-v1.md)).
- OTA wire contract behind the `juggler` `ota-wire` feature, with `OtaCommand`, `Manifest`, `OtaStatus`, `FailReason` and `RollbackReason`.

### Changed

- **Breaking:** `juggler::provisioning::resolve_wait` takes only the `ProvisioningState`, the `committed` argument is gone, and `juggler::ota::OtaError` now implements `Copy`.
- **Breaking:** OTA error codes changed, so an IDF per-read timeout is now `download_timeout` and a HAL socket read error is now `server_unreachable`.
- MQTT callback contract (ADR 017): `on_connect` runs on a per-connect helper thread where `client.enqueue()` and `client.subscribe()` are safe, and `MqttHandle` methods called from any callback return `PublishAckError::WrongThread`.

### Fixed

- MQTT clients (`MqttBuilder`, `MqttManager`) never reconnected after a mid-session disconnect unless `with_reconnect_timeout()` was set, and the default now retries every 10 s (since 0.4.0).
- MQTT deadlock when callbacks called the client while esp-mqtt held its `api_lock`, which left `is_connected()` false and fired the LWT.
- Provisioning `wait_committed` stack overflow from a config clone is fixed, secrets are scrubbed via `zeroize`, and both portals answer stale `/save` or `/factory-reset` with `409` without writing flash.

## [0.5.0] - 2026-09-26

### Added

- `MqttHandle::publish_acked` on `rustyfarian-esp-idf-network` is a QoS 1 publish that blocks until PUBACK or timeout, returning a typed `PublishAckError` ([mqtt-publish-acked-v1](docs/features/archive/mqtt-publish-acked-v1.md)).
- `PortalConfig.ssid_override` (Experimental) sets a verbatim SoftAP SSID that bypasses the `{prefix}-{MAC}` derivation ([ssid-override-v1](docs/features/archive/ssid-override-v1.md)).
- `PortalConfig.defaults` (Experimental) carries `.env`-seeded, non-secret pre-fill for the portal form on a fresh device ([portal-prefill-defaults-v1](docs/features/archive/portal-prefill-defaults-v1.md)).

### Changed

- **Breaking:** the bare-metal stack moves to `esp-hal 1.2.2`, `esp-rtos 0.4.0` and `esp-radio =1.0.0-beta.1` (pre-release), and `with_peripherals` / `with_ap_peripherals` take `FROM_CPU_INTR0` instead of `SW_INTERRUPT` ([esp-hal-stack-upgrade-september-2026-v1](docs/features/archive/esp-hal-stack-upgrade-september-2026-v1.md)).
- **Breaking:** the IDF stack requires `esp-idf-svc 0.53`, `esp-idf-hal 0.47` and `pennant 0.7`, so a driver implementing `pennant 0.6` no longer satisfies `StatusLed`.
- **Breaking:** on both tiers, existing `PortalConfig { .. }` literals must add `ssid_override: None` and `defaults: PortalDefaults::default()` to keep today's behaviour.

### Fixed

- ESP-IDF OTA no longer reports a truncated download as success and fails with `DownloadFailed { status: 0 }`, matching the esp-hal tier.
- The SoftAP captive portal advertises a DNS server via DHCP Option 6, so the OS sign-in sheet actually appears.
- A rejected or failed `POST /save` re-renders with the stored or default non-secret values instead of blanks, and secrets are still never pre-filled.

## [0.4.0] - 2026-06-20

### Changed

- **Breaking:** 16 workspace crates are merged into `juggler`, `rustyfarian-esp-idf-network` and `rustyfarian-esp-hal-network` ([crate-consolidation-3-crates-v1](docs/features/archive/crate-consolidation-3-crates-v1.md)), so every import path changes and consumers must switch dependency names and imports manually.

### Fixed

- Provisioning store (`ProvisioningStore::open`) rejected valid stores at non-zero flash offsets, panicking `OffsetOutOfBounds` at boot.
- HAL provisioning examples (`hal_c3_provision_mqtt` / `hal_c6_provision_mqtt`) panicked with `time_driver NoneError` on the already-provisioned boot path, and now halt without awaiting.
- HAL async Wi-Fi examples failed to compile and now have the missing `embassy-executor` and `embassy-time` dev-dependencies.

## [0.3.0] - 2026-06-16

### Added

- `rustyfarian-esp-hal-provisioning` v0.1.0 provides bare-metal SoftAP captive-portal provisioning for the `WifiMqttDevice` profile on ESP32-C3 and ESP32-C6 ([softap-provisioning-v1](docs/features/archive/softap-provisioning-v1.md)).
- Wi-Fi + MQTT provisioning profile (`WifiMqttDevice`) adds Wi-Fi credentials, an MQTT broker, an OTA URL and a device name, without LoRaWAN, per ADR 014 ([wifi-mqtt-provisioning-profile-v1](docs/features/archive/wifi-mqtt-provisioning-profile-v1.md)).
- `MqttBuilder::subscribe(topic, qos)` registers topics for automatic (re)subscription without blocking the event loop.

### Changed

- **Breaking:** `init_with_radio` on `rustyfarian-esp-idf-network` starts the radio in SoftAP mode instead of unassociated STA mode, so `default_interface()` returns `WifiInterface::Ap`; use `init_with_radio_sta` to keep the prior behaviour.
- All documentation examples use `.subscribe()` on the builder, and `on_connect` is for `client.enqueue()` only.

### Fixed

- `MqttBuilder::on_connect` deadlocked when `client.subscribe()` was called inside it on `esp-idf-svc 0.52+`, so subscriptions belong on the builder via `.subscribe(topic, qos)`.
- ESP-NOW unassociated-STA channel drift: `init_with_radio` now starts a hidden SoftAP on channel 1, so beacon scheduling holds the channel.
- ESP-NOW `scan_for_peer` failure cascade: a failed re-scan left the radio on the last-probed channel with the peer removed, and the `Err` branch now restores both from the last successful scan.

## [0.2.1] - 2026-05-06

### Changed

- Adopt the `rustyfarian-ws2812 v0.5.0` retag, renaming `led-effects` to `pennant` and `ws2812-pure` to `bunting` upstream, and the `rustyfarian-esp-hal-ws2812` feature `led-effects` becomes `pennant`.

## [0.2.0] - 2026-05-06

### Added

- OTA MVP — three new experimental crates (`ota-pure`, `rustyfarian-esp-idf-ota`, `rustyfarian-esp-hal-ota`) for end-to-end firmware update ([ota-mvp-v1](docs/features/archive/ota-mvp-v1.md)).
- Bare-metal async Wi-Fi via the new `embassy` Cargo feature on `rustyfarian-esp-hal-wifi` ([embassy-feature-flag-v1](docs/features/archive/embassy-feature-flag-v1.md)).
- ESP-NOW peer discovery, reliable delivery, and the Peripheral Command Framework ([espnow-peripheral-command-framework-v1](docs/features/archive/espnow-peripheral-command-framework-v1.md)).

### Changed

- **Breaking:** bare-metal Wi-Fi is now async-only, so the synchronous `WiFiManager` surface and the `hal_*_connect` examples are gone.
- **Breaking:** `esp-radio 0.18` API renames cascade through `rustyfarian-esp-hal-wifi`.
- The April 2026 `esp-hal` stack wave moves to `esp-hal 1.1.0`, `esp-rtos 0.3.0` and `esp-radio 0.18.0` with matching embassy pins ([esp-hal-stack-upgrade-april-2026-v1](docs/features/archive/esp-hal-stack-upgrade-april-2026-v1.md)).

### Fixed

- ESP-NOW channel-scan and `send_and_wait` race conditions.

## [0.1.0] - 2026-03-16

### Added

- `rustyfarian-esp-idf-mqtt`: `MqttBuilder` API with `MqttHandle` and lifecycle callbacks (`on_connect`, `on_disconnect`, `on_message`), plus `LwtConfig`, `with_auth()` and `publish_with()`.
- `wifi-pure` crate with `WiFiConfig`, `ConnectMode` and SSID/password validation (ADR 006).
- `rustyfarian-esp-idf-lora`: `LoraRadioAdapter` bridging to `lorawan-device 0.12`, with the `idf_esp32s3_join` and `hal_esp32s3_join` examples for Heltec WiFi LoRa 32 V3.

[Unreleased]: https://github.com/datenkollektiv/rustyfarian-network/compare/v0.8.0...HEAD
[0.8.0]: https://github.com/datenkollektiv/rustyfarian-network/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/datenkollektiv/rustyfarian-network/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/datenkollektiv/rustyfarian-network/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/datenkollektiv/rustyfarian-network/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/datenkollektiv/rustyfarian-network/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/datenkollektiv/rustyfarian-network/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/datenkollektiv/rustyfarian-network/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/datenkollektiv/rustyfarian-network/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/datenkollektiv/rustyfarian-network/releases/tag/v0.1.0
