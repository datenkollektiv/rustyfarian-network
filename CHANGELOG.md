# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `juggler::espnow::{ScanMachine, ScanInput, ScanAction, ScanError, SendOutcome}`: a pure, clock-injected state machine for `scan_for_peer`, shared by both tiers; the caller feeds `ScanInput` (`Start`, `Completed`, `Failed`, `Abort`, `Sent`) and performs each `ScanAction`, and `ScanError::Aborted` reports an abort. A failed peer registration restores the previously pinned channel and peer (best effort) before reporting the original `ScanError`.
  See ADR 020 and `docs/features/esp-hal-espnow-v1.md`; ADR 007 is amended.
- Bare-metal ESP-NOW: new `espnow` feature on `rustyfarian-esp-hal-network` (ADR 020, `esp32c3` and `esp32c6`); `espnow::EspHalEspNow` implements `juggler::espnow::EspNowDriver` with `&self` and adds `send_and_wait`, `send_async`, `send_async_timeout`, `recv_async`, `set_channel`, `set_max_tx_power`, `tx_power_quarter_dbm`, `sta_mac`, `peer_exists` and `oversize_dropped`.
  `from_controller(&mut WifiController, EspNowConfig)` keeps the caller's TX power and A-MPDU unless configured; `new_owned(WIFI, EspNowConfig)` starts an unassociated station (no SSID, no `embassy-net`) at 13 dBm with A-MPDU off.
  See `docs/features/esp-hal-espnow-v1.md` for the send, receive and channel semantics.
  Async sends and scan probes run through `EspHalEspNow::run_sender`, a runner task the firmware spawns once (a second runner is rejected, and `NoSenderRunner` without one), so a cancelled or timed-out caller never orphans a send completion; a request whose deadline passed before its transmission started is never sent, an in-flight transmission is waited out and its real result returned, a scan probe uses one absolute deadline, a cancelled `scan_for_peer` restores channel, peer registration and TX power, and concurrent scans return `Busy`.
- `EspHalEspNow::scan_for_peer(&mac, &ScanConfig, &mut Option<u8>)`: async peer scan on the esp-hal tier, driven by `juggler::espnow::ScanMachine` with the ESP-IDF tier's semantics; the caller keeps the pinned channel in the `&mut Option<u8>`, and failures are `EspHalEspNowError::Scan(ScanError)`.
  The TX-power boost to `Max` is restored on every exit, including a cancelled future; a `from_controller` handle cannot boost and scans at the caller's level, but it still moves an associated station across channels and drops its access-point link, so use the scan on the owned radio.
- Bare-metal ESP-NOW examples `hal_c6_espnow_sta_coordinator` (ESP32-C6, associated station, echoes frames) and `hal_c3_espnow_peer` (ESP32-C3, owned radio, scan and 100 ms sends for hardware gate G-E1).
  Build with `just build-example`; `just clippy-hal` lints both, `just check-espnow-hal` runs under `check-hal`, and `just clippy-pure` lints the `juggler` `espnow` feature.
  Build-time credentials and the peer MAC are `option_env!` values and are never logged.
- `EspHalEspNow::scan_for_peer(&mac, &ScanConfig, &mut Option<u8>)`: async peer scan on the esp-hal tier, driven by the shared `juggler::espnow::ScanMachine` (same semantics as the ESP-IDF tier).
  Probes use `send_async_timeout` with `probe_timeout` as the deadline for queueing and the start of transmission — a probe already on the air is waited out (ACK = `Acked`, `SendFailed` or timeout = `NoAck`, enqueue error = `Error`), confirmation gaps use `embassy_time::Timer::at` against the scan start, a refused channel switch is reported to the machine as `Failed`, and the TX-power boost to `Max` is restored by a drop guard on every exit including a cancelled future (owned radio only; a `from_controller` handle scans at the caller's level).
  The caller keeps the pinned channel through the `&mut Option<u8>` and the failure is `EspHalEspNowError::Scan(ScanError)`; no lock is held across an await.
- Bare-metal ESP-NOW examples: `hal_c6_espnow_sta_coordinator` (ESP32-C6, station associated first, `EspHalEspNow::from_controller`, A-MPDU off, receives, registers senders and echoes frames, logs counters and the oversize-drop count) and `hal_c3_espnow_peer` (ESP32-C3, `new_owned` at 13 dBm = 52 quarter-dBm, `scan_for_peer` with the pinned channel kept across rescans, a frame every 100 ms with ACK and failure-rate logging for hardware gate G-E1).
  Build with `just build-example`; `just clippy-hal` lints both.
  Wi-Fi credentials and the peer MAC are compile-time `option_env!` values and are never logged.
- Bare-metal MQTT: new `mqtt` feature on `rustyfarian-esp-hal-network` (ADR 019) — an async MQTT 5 client over `minimq =0.13.5` and the Wi-Fi `embassy-net` stack, with LWT, a startup message republished on every (re)connect, builder subscriptions re-issued after reconnect, auto-reconnect, a polled network watch, a transport timeout of 1.5 × keep-alive and a stall deadline of socket timeout + keep-alive (a dead link ends the session within about 0.5 s, a silent broker within roughly 2.5 × keep-alive), a non-blocking `try_publish*` family returning `TryPublishError::WouldBlock` (`Ok` means queued, not delivered; the crate README states the delivery rule), an inbound message queue, and all buffers static.
  Chips `esp32c3`, `esp32c6`, `esp32s3`; examples `hal_c6_mqtt_lwt` and `hal_esp32s3_mqtt_lwt`; runbook `docs/runbooks/esp-hal-mqtt-hardware-test.md`; `just clippy-hal` and `just check-hal` cover the feature.
  Shared MQTT configuration in `juggler::mqtt` (no `std` needed): `QoS`, `MqttConfig`, `LwtConfig`, `DEFAULT_MQTT_TASK_STACK_SIZE` and the generic `TryPublishError<E>`, used by both tier crates.
  Bare-metal `MqttConfig::host` must be an IPv4 literal; the esp-hal tier has no DNS yet (ADR 019 §2).
- ESP32-S3 support for `wifi`, `provisioning` and `ota` in `rustyfarian-esp-hal-network`: the `esp32s3` feature now selects the chip on `esp-radio`, `esp-rtos`, `esp-alloc`, `esp-storage` and `esp-backtrace`, and `ota::TARGET_CHIP` is `"esp32s3"`.
  Example `hal_esp32s3_connect_async` (Wi-Fi station join plus DHCP), and S3 Wi-Fi, OTA and provisioning lines in `just clippy-hal`.
- IDF tier: `WifiMqttBoot::{device_name, ota_url, config}`, so a consumer reads the stored device name and OTA URL without a second store read; `idf_c3_provision` and `idf_c3_provision_mqtt` log them, and `just clippy-provisioning-mqtt-tests` runs in `just verify`.
  HAL tier: portal extras are now persisted through a repeatable `TAG_EXTRA` (`0x0A`) record tag, and `MAX_RECORD_LEN` grows from 512 to 1280 B (ADR 015 amendment 2026-10-27).
  `juggler::provisioning::LoraFields::from_storage_parts`, and public `EUI_HEX_LEN` and `APP_KEY_HEX_LEN`.
  `juggler::provisioning::{ProvisioningKv, KvProvisioningStore}` (feature `std`): the provisioning NVS key layout over a key-value trait, with host tests that load a 0.6.x-shaped record; the IDF tier's `ProvisioningStore` now delegates to it over NVS, with no public API change.
  Runbook `docs/runbooks/esp-hal-provisioning-extras-hardware-test.md`; the hardware gate (a HAL record with extras across a reboot on ESP32-C3 and ESP32-C6) is still open.
  Requested by the spookhouse review-queue item `rustyfarian-network-boot-device-name-v1`; see `docs/features/provisioning-boot-identity-v1.md`.
- Provisioning re-entry hook: `juggler::provisioning::ReentryRequest { None, Portal, FactoryReset }` (`#[non_exhaustive]`).
  `juggler::provisioning::{gate_reentry, ReentryGate}`: the host-tested ordering primitive that asks the hook once and opens the store only on `None`.
  All three are re-exported from the `provisioning` module of both tier crates, so a consumer that opens its `ProvisioningStore` itself (the esp-hal tier has no boot bundle; the IDF LoRaWAN profile has none either) gates the read without a direct `juggler` dependency.
  Requested by the rgb-clock review-queue item `rustyfarian-network-esp-hal-reentry-reexport`.
  IDF tier: `WifiMqttBoot::load_with_reentry(nvs, reentry, client_id_fn)` asks the hook once, before the store is read, and never claims the modem; `None` behaves like `load_with`.
  IDF tier: `WifiMqttBoot::default_client_id`, the client-ID policy of `WifiMqttBoot::load`, for `load_with_reentry` callers.
  IDF tier: `WifiMqttLoadOutcome::{PortalRequested, FactoryResetRequested}`; the library never erases or restarts, so the consumer erases via `ProvisioningStore::erase_all`.
  `idf_c3_provision_mqtt` samples BOOT (GPIO9) after boot: a short hold reopens the portal, a hold of 3 s or more erases in example code and continues into the portal; see `docs/features/provisioning-reentry-hook-v1.md`.
- `rustyfarian-esp-hal-network`: `wifi::softap_mac()` reads the SoftAP MAC from efuse without radio init, and `ProvisioningSession::ap_mac()` returns the MAC the session was started with.
  `ProvisioningError::SoftApInit { reason: &'static str }` reports a failed SoftAP bring-up, with the reason `SoftAP configuration rejected` or `radio driver error`.
- `juggler::provisioning::{derive_device_name, DeviceName}`: `derive_device_name(prefix, &mac)` returns `{prefix}-{xxxx}` as a `heapless::String<DEVICE_NAME_MAX_LEN>` (24 bytes), where `xxxx` is the lowercase hex of the last two MAC bytes.
  The trimmed prefix is truncated on a char boundary so the `-xxxx` suffix always fits.
  An empty prefix after trimming yields the bare four hex digits with no leading dash, and the function is infallible, mirroring `derive_softap_ssid`.
  `DeviceName::resolve(&mac)` is the shared resolution used by both tiers.
  Requested by the spookhouse review-queue item `rustyfarian-network-per-device-name-prefill-v1` (bug 009); see `docs/features/per-device-name-prefill-v1.md`.

### Changed

- `EspIdfEspNow::scan_for_peer` now drives the shared `juggler::espnow::ScanMachine` (blocking `send` plus ACK wait per probe, `std::thread::sleep` for confirmation gaps) instead of its own channel loop, so both tiers run one algorithm.
  The public API, error messages (including the `anyhow` context on registration failures), TX-power boost and restore order and the failed-scan fallback to the last pinned channel are unchanged; log lines are unchanged, except that the burst-timeout line is now emitted after teardown.
  The `espnow` module re-exports `ScanError` and `SendOutcome`.
  Not exercised on hardware: an IDF re-check of scan (nunchuk on channels 1, 6 and 11) is covered by the consumer's manual G0/G2 gates.
- `rustyfarian-esp-hal-network` `esp32c3` feature: `esp-println` now uses `auto` (USB-Serial-JTAG when a USB host is attached, else UART0) instead of `uart`, so log output reaches the USB port of Super Mini boards.
- **Breaking (both tiers):** `PortalConfig::device_name` is now `juggler::provisioning::DeviceName` instead of `&str`, re-exported from both tiers' `provisioning` modules.
  Migration: `device_name: "name"` becomes `device_name: DeviceName::Fixed("name")` for the old behaviour, or `device_name: DeviceName::MacSuffixed("name")` to pre-fill `name-xxxx` from the AP MAC.
  The `xxxx` suffix is the lowercase hex of the last two AP-MAC bytes, so devices whose last two AP-MAC bytes differ get different names.
  The suffix equals the suffix of the derived SSID of the same device, case aside; an `ssid_override` removes that relation.
  The pre-fill stays editable in the portal, and a stored name always wins on re-provisioning.
  A `Fixed` name longer than 24 bytes is truncated on a char boundary on both tiers; the ESP-IDF tier previously passed it through, and the esp-hal tier already truncated.
  A non-empty `PortalDefaults::mqtt_client` (for example `MQTT_CLIENT_ID` from a shared `.env`) is copied into the form and becomes the client ID on every device unless cleared.
  Leave it blank to derive the client ID from the device name; both tiers log a warning for this combination.
  Devices already provisioned keep their stored name after upgrading, because a stored name always wins; to change it, edit and save the name once through the portal (re-entry), or factory-reset the device.
  A `Fixed` name gives every device of one firmware the same MQTT client ID via `resolve_client_id`, so use `MacSuffixed` unless the firmware owns uniqueness.
  The four provisioning examples (`idf_c3_provision`, `idf_c3_provision_mqtt`, `hal_c3_provision_mqtt`, `hal_c6_provision_mqtt`) use `MacSuffixed`.
  The three provisioning MQTT examples no longer pre-fill `mqtt_client` from `MQTT_CLIENT_ID`, so a dotenv file shared with the plain MQTT examples cannot override the per-device client ID.
- **Breaking (`juggler`):** `ProvisioningConfig::ota_url()` returns `Option<&str>` (`None` when no OTA URL is stored, replacing the empty string), `ProvisioningConfig::from_storage_parts` takes a trailing `extras: heapless::Vec<ExtraField, EXTRA_FIELDS_MAX>`, and `ExtraField`'s `Debug` redacts the value.
  Migration: replace `cfg.ota_url().is_empty()` with `cfg.ota_url().is_none()`, use `cfg.ota_url().unwrap_or("")` where a string is needed, and pass `heapless::Vec::new()` as the new `extras` argument when there are none.
  `device_name()` still returns `&str`.
- **Breaking (IDF tier):** `ProvisioningStore::load()` returns `Option<juggler::provisioning::ProvisioningConfig>` and the closure of `WifiMqttBoot::load_with` takes `&ProvisioningConfig`.
  The record is heap-allocated once and scrubbed on drop, and `WifiMqttBoot`'s `Debug` prints secrets as `<redacted>` without a length (it was `<redacted, len=N>`) and shows the MQTT username in clear.
  `StoredConfig` is removed (its derived `Debug` printed secrets); read every field through the `ProvisioningConfig` accessors.
  `load()` now errors naming the NVS key when a stored value exceeds its field cap or more than 8 extras are indexed; a portal-written record cannot trigger this.
  Migration: `cfg.profile` becomes `cfg.profile()`, `cfg.mqtt_host` becomes `cfg.mqtt().map(|m| m.host())`, `cfg.mqtt_port` becomes `cfg.mqtt().map(|m| m.port())`, the LoRa fields (`dev_eui_hex`, `join_eui_hex`, `app_key_hex`) come from `cfg.lora()`, `cfg.device_name` becomes `cfg.device_name()`, `cfg.ota_url` becomes `cfg.ota_url().unwrap_or("")`, and extras come from `cfg.extras()`.
  The NVS key layout (ADR 014 §4) is unchanged, and 0.6.x records load unchanged.
- **Breaking (HAL tier):** `StoreError::ExtrasNotSupported` is removed and `StoreError::MalformedField { tag }` is added (empty or duplicate extra key, bad key length, over-cap key or value, a ninth extra).
  A record with extras can exceed 512 B, and 0.6.x firmware caps its read at 512 B, so such a record reads as unprovisioned there (the device falls back to the other sector or reports unprovisioned); records without extras are byte-identical to 0.6.x.
  The documented provisioning task stack guidance was re-derived as an analytic upper bound (about 12.5 KiB transient including the portal's parsed config, about 20.5 KiB peak, recommended minimum 24 KiB), not measured.
- **Breaking (IDF tier):** `rustyfarian_esp_idf_network::mqtt` now takes `juggler::mqtt::QoS` everywhere (`MqttBuilder::subscribe`, `LwtConfig::new`, `publish_with`, `try_publish_with`, `publish_acked`, …) and re-exports it as `mqtt::QoS`; `MqttConfig`, `LwtConfig` and `TryPublishError` are re-exports/aliases of the `juggler` types.
  Migration: import `QoS` from `rustyfarian_esp_idf_network::mqtt` instead of `esp_idf_svc::mqtt::client`, and convert with `mqtt::to_esp_qos(..)` where an `on_connect` closure calls the raw `EspMqttClient::enqueue`.
- **Breaking (IDF tier):** the `lora` feature of `rustyfarian-esp-idf-network` no longer enables `esp-idf-hal/critical-section`, so the firmware selects the `critical-section` provider (bug 007, requested by `rustyfarian-beekeeper`).
  Migration: add `esp-idf-hal = { version = "0.47", features = ["critical-section"] }` to the firmware, or enable `rustyfarian-esp-idf-power`'s `critical-section-impl` instead; without either, linking fails with `undefined reference to '_critical_section_1_0_acquire'`.
  `EspIdfLoraRadio::new` now pulses NRST directly instead of calling `sx126x::reset()`, so no SPI transaction runs inside `critical_section::with` and an interrupt-masking provider is safe; an OTAA join on a Heltec V3 with that provider is still to be bench-tested.
- The `juggler` `provisioning` feature no longer implies `lora`, so a Wi-Fi + MQTT device that enables provisioning no longer pulls `lorawan-device` (and with it `heapless 0.7` / `atomic-polyfill`, `RUSTSEC-2023-0089`).
  `LoraFields::to_lora_config` now requires the `lora` feature; consumers enabling both `provisioning` and `lora` on either tier crate are unaffected.
- **Breaking (HAL tier):** `rustyfarian-esp-hal-network` `ProvisioningBuilder::start` now takes the AP peripherals `(spawner, timg0, sw_int, wifi, store, rng)` instead of a `SoftApHandle`, and brings the SoftAP up itself from `PortalConfig` (SSID, channel and password).
  Migration: delete the caller's `ApConfig::open(..).with_ap_peripherals(..)` and `WiFiManager::init_softap_async(..)` calls, and pass `peripherals.TIMG0`, `peripherals.FROM_CPU_INTR0` and `peripherals.WIFI` to `start`.
  `ProvisioningError` gains the `SoftApInit` variant, so an exhaustive `match` on it needs a new arm.
  `WiFiManager::init_softap_async`, `ApConfig` and `ApConfigExt` stay public for non-provisioning SoftAP users.
  See `docs/features/archive/hal-provisioning-ap-ownership-v1.md`.
- **Breaking (HAL tier):** the default provisioning AP SSID on `rustyfarian-esp-hal-network` changes from the bare `{prefix}` to `{prefix}-{MAC}`, using the last two AP-MAC bytes in uppercase hex, matching ESP-IDF.
  Migration: existing hal APs are renamed; consumers that want the bare name set `ssid_override: Some("name")` on `PortalConfig`.
  The suffix is each stack's own AP MAC, and ESP-IDF (base MAC plus one in the last byte) and esp-hal (locally-administered bit in the first byte) derive it differently, so one board shows `-C4F5` under ESP-IDF and `-C4F4` under esp-hal.
- `rustyfarian-esp-hal-network`: `PortalConfig.ssid_override` is applied end-to-end on esp-hal as well as ESP-IDF, so the AP broadcasts the override verbatim and the derived default applies on both tiers.
  The hal provisioning examples' default `PORTAL_SSID_PREFIX` is now `Rustyfarian`, the spelling the IDF examples use.
  The hal provisioning examples read `PROVISION_AP_PSK` and `PORTAL_SSID_OVERRIDE` at build time (WPA2 portal password and verbatim SSID), as the IDF example already does for the password.
  The esp-hal `SoftAP configured` log line is now a fixed message: CodeQL reports every value read from the AP config, including the channel and the SSID length, as cleartext credential logging because the config carries the password; an open AP still logs its own warning, and the provisioning layer logs the resolved SSID length.
- `rustyfarian-esp-hal-network`: the hal examples `hal_c3_provision_mqtt` and `hal_c6_provision_mqtt` restart via `esp_hal::system::software_reset()` 200 ms after a committed submission, like the IDF provisioning examples, instead of idling until a manual reset; the library itself still never reboots.

### Fixed

- esp-hal provisioning logged the open-AP warning twice per bring-up (once from `ProvisioningBuilder::start`, once from `WiFiManager::init_softap_async`); it is now logged once, from the Wi-Fi layer, and the host test for it moved with the helper.
- esp-hal provisioning docs: the stack guidance in `hal_c3_provision_mqtt`, `hal_c6_provision_mqtt` and the portal module named knobs the tier does not have (`_stack_size`, `CONFIG_ESP_MAIN_TASK_STACK_SIZE`); they now explain that the main stack is the RAM left after `.data`, `.bss` and the heap and point to `ESP_HAL_CONFIG_ENSURE_MAIN_STACK_MINIMUM` as the link-time floor.
  The `0x300000` store offset in the examples and README is now labelled as lying inside the `factory` partition of espflash's default table, with the instruction to add a dedicated `data` partition; stale `esp-rtos 0.3.0` / `esp-radio 0.18` mentions in the examples are refreshed.
  Reported by rgb-clock (`review-queue/rustyfarian-network-esp-hal-docs-defects.md`).
- esp-hal `ProvisioningStore` never read a committed record back: `try_read_sector` read exactly `record_len` bytes, and esp-storage 0.10 rejects a read length that is not a multiple of its 4-byte `READ_SIZE`, so every provisioned boot reopened the portal with `StoreError::Flash` (bug 006, found on an ESP32-C3 Mini-1).
  The read is now rounded up to `READ_SIZE`, a host mock with word-aligned reads covers it, and the hal provisioning examples log a store error instead of silently re-provisioning.
- The esp-hal docs no longer call the ESP32-S3 LoRa-only: every pinned dependency supports it, and the claimed `compile_error!` for S3 with `wifi`/`ota` never existed (S3 with `ota` silently compiled the stub manager).

## [0.6.0] - 2026-10-09

### Added

- OTA consumer runtime and record store (`juggler::ota::{runtime, persist}`, IDF tier `ota`): `OtaRuntime` / `OtaSubmitter` / `OtaHandle` run command intake, an update/rollback/repair worker, a `rolled_back` reporter and the health policy around an app-supplied predicate; the library never restarts on its own.
  `OtaStore<K: OtaKv>` with `EspNvsKv` persists attempts in 16 NVS keys, `reconcile_boot` returns the next action, and `repair_corrupt` clears unreadable records; see `docs/features/ota-consumer-runtime-v1.md`, including "Behaviour changes versus the first consumer's runbook".
- OTA wire contract behind the `juggler` `ota-wire` feature: `OtaCommand` (incl. `Repair`), `Manifest`, `OtaStatus` (incl. `Repaired`), `FailReason`, `RollbackReason` and `TARGET_CHIP`, re-exported from both tier `ota` modules.
- OTA decision core and download deadline: `decide_offer` / `Admission`, `reconcile`, `OtaError::code()`, and `with_deadline` on `OtaSession` (IDF) and `EspHalOtaManager` (HAL), enforced by `ActivationPermit`.
- Tooling and examples: the `idf_c3_ota_runtime` and `idf_c3_mqtt_callback_guard` examples, public `ota::url_for_log`, `juggler::provisioning::SessionState` for host regression tests, and a broker check in `just doctor`.
  The OTA hardware runbook (`docs/runbooks/ota-hardware-test.md`) runs entirely on `just ota-*` recipes and a local test broker (`just mosquitto up|down`); it passed 21/21 on ESP32-C3, and the rgb-clock adoption tests passed.

### Changed

- **Breaking:** `juggler::provisioning::resolve_wait` takes only the `ProvisioningState` (the `committed` argument is gone), and `juggler::ota::OtaError` now implements `Copy` (both reported by `just release-semver-check` against `v0.5.0`).
  OTA error codes changed: an IDF per-read timeout is now `download_timeout`, a HAL socket read error is now `server_unreachable`.
  The new OTA wire enums (`OtaCommand`, `OtaStatus`, `FailReason`, `ConfigError`) are not `#[non_exhaustive]`, so adding a variant later is a breaking change; wire parsing is strict (unknown fields, labels and JSON-array payloads are rejected, URLs are validated at intake).
- MQTT callback contract (ADR 017): `on_connect` runs on a per-connect helper thread where `client.enqueue()` / `client.subscribe()` are safe, and `MqttHandle` methods called from any callback return `PublishAckError::WrongThread`.
- Workspace and `juggler` minimum version `0.6.0`; the IDF `provisioning` feature enables `juggler/std`, `ota` enables `juggler/ota-wire`; examples read `MQTT_PORT` and ship no placeholder credentials.

### Fixed

- MQTT clients (`MqttBuilder`, `MqttManager`) never reconnected after a mid-session disconnect unless `with_reconnect_timeout()` was set: esp-idf-svc reads `reconnect_timeout: None` as "auto-reconnect off"; the default now retries every 10 s (since 0.4.0, see `docs/bugs/archive/005-mqtt-never-auto-reconnects-2026-10-08.md`).
- MQTT deadlock when callbacks called the client while esp-mqtt held its `api_lock` (`is_connected()` stayed false, LWT fired); see `docs/bugs/001-on-connect-enqueue-deadlock-2026-09-27.md`.
- Provisioning: `wait_committed` stack overflow from a config clone (bug 002), secrets left in freed memory now scrubbed via `zeroize` (bug 003), and both portals answer stale `/save` or `/factory-reset` with `409` without writing flash; see `docs/bugs/archive/`.

## [0.5.0] - 2026-09-26

### Added

- `MqttHandle::publish_acked` — QoS 1 publish that blocks until PUBACK or timeout, returning a typed `PublishAckError`; backed by a new host-tested correlation registry in `juggler::mqtt` (`PendingAcks`, `AckWaiter`, `AckOutcome`, `MessageId`). See `docs/features/archive/mqtt-publish-acked-v1.md`.
- Both tier `ota` modules re-export the full `juggler::ota` surface instead of `OtaError` alone, so version-gated consumers need no direct `juggler` dependency.
- `juggler::ota::decide_update` / `UpdateDecision` (Experimental) — `Apply` when the offered version is newer, `Skip` when equal, `Reject` when older; no downgrade path.
- `rustyfarian-esp-idf-network` `features = ["wifi"]` now compiles against a SoftAP-disabled ESP-IDF; adding `provisioning` to such a build fails with one `compile_error!` instead of a cascade. See `docs/features/archive/wifi-softap-cfg-gate-v1.md`.
- `PortalConfig.ssid_override: Option<&str>` (Experimental) — a verbatim SoftAP SSID that bypasses the `{prefix}-{MAC}` derivation, applied end-to-end on ESP-IDF and validated only on esp-hal until it owns AP bring-up.
- `juggler::provisioning::resolve_softap_ssid` (Experimental) — the shared override-or-derive resolver behind that field, reusing `juggler::wifi::validate_ssid`.
- `PortalConfig.defaults: PortalDefaults<'a>` (Experimental) — `.env`-seeded non-secret pre-fill for the portal form on a fresh device; `PortalDefaults` structurally carries no secrets.
- `juggler::provisioning::resolve_wait` + `WaitResolution` (Experimental) — pure decision for a provisioning waiter, locking the contract that a factory reset ends an indefinite wait.
- `WifiMqttBoot` + `run_wifi_mqtt_portal` (Experimental) — modem-free NVS load and a three-way portal outcome; `idf_c3_provision_mqtt` is rewritten on top of them.
- `juggler::mqtt::resolve_client_id` — selects an MQTT client ID within the 23-byte 3.1.1 cap, truncating on a UTF-8 char boundary.

### Changed

- Bare-metal stack: `esp-hal 1.1` → `1.2.2`, `esp-rtos 0.3` → `0.4.0`, `esp-radio 0.18` → `=1.0.0-beta.1` (pre-release; re-pin to `1.0.0` when it ships), hardware-validated C3/C6/S3 2026-09-26.
- IDF stack: `esp-idf-svc 0.52` → `0.53`, `esp-idf-hal 0.46` → `0.47` (ESP-IDF v5.3.3), hardware-validated C3 2026-09-26.
- ws2812 wave: `pennant`, `rustyfarian-esp-idf-ws2812`, and `rustyfarian-esp-hal-ws2812` all 0.6 → 0.7.
- `sha2` 0.10 → 0.11 (`juggler`, `ota` feature).
- Bare-metal STA TX power inherits `esp-radio`'s 5 dBm default instead of forcing 8.5 dBm; pass `TxPowerLevel::Low` to restore the old value.
- The `WifiMqttDevice` provisioning profile treats `ota_url` as optional — see [ADR 014](docs/adr/014-wifi-mqtt-provisioning-profile.md) Amendment 2026-09-26.
- MQTT event-loop tracing dropped from `info!` to `debug!`, and a `Received` event logs topic and byte length rather than the raw payload.
- Portal shutdown stops the DNS catch-all before the HTTP server, cutting captive-portal teardown noise.
- CI gained a `build-examples` job covering the Xtensa IDF and bare-metal targets `just verify` never builds, and action versions were bumped.
- `deny.toml` retired the `anyhow` (RUSTSEC-2026-0190) and `crossbeam-epoch` (RUSTSEC-2026-0204) ignores; `atomic-polyfill` (RUSTSEC-2023-0089) remains.
- HAL README: Wi-Fi and provisioning examples rewritten, "Dependency pins and MSRV" section added.

### Fixed

- ESP-IDF OTA rejects an oversized image with `InsufficientSpace` before erasing anything, and rejects a missing or zero `Content-Length` outright.
- ESP-IDF OTA no longer reports a truncated download as success — it fails `DownloadFailed { status: 0 }`, matching the esp-hal tier (ADR 011 §2).
- ESP-IDF OTA contacts the server before erasing flash, and now erases only the sectors the image needs.
- esp-hal OTA rejects a zero-length image before any flash access.
- The SoftAP captive portal advertises a DNS server via DHCP Option 6, so the OS sign-in sheet actually appears.
- The portal pre-fills the required "Device name" from `PortalConfig::device_name` on a fresh or factory-reset device.
- A rejected or failed `POST /save` re-renders with the stored / default non-secret values instead of blanks, and explains why via the shared `RESUBMIT_HINT`; secrets are still never pre-filled.
- The esp-hal portal field-error block moved from a 512-byte `heapless::String` to an `alloc` `String`, which had been truncating long error lists into unclosed HTML.

### Migration

- **HAL:** `with_peripherals` / `with_ap_peripherals` take `FROM_CPU_INTR0` (was `SW_INTERRUPT`) and runners are `Runner<'static, Interface>`; requires `esp-hal 1.2.2`, `esp-rtos 0.4.0`, `esp-radio =1.0.0-beta.1`, Rust ≥1.95.
- **IDF:** requires `esp-idf-svc 0.53`, `esp-idf-hal 0.47`, `pennant 0.7` — a driver implementing `pennant 0.6` no longer satisfies `StatusLed`.
- **Both tiers:** existing `PortalConfig { .. }` literals must add `ssid_override: None` and `defaults: PortalDefaults::default()` to keep today's behaviour.

## [0.4.0] - 2026-06-20

### Changed

- **Crate consolidation — 16 workspace crates merged into 3 publishable crates ([ADR 016](docs/adr/016-crate-consolidation-for-publishing.md)):** `juggler` (pure/`no_std` shared types), `rustyfarian-esp-idf-network` (ESP-IDF/`std`), and `rustyfarian-esp-hal-network` (bare-metal/`no_std`), each with per-domain features and `default = []`. **Breaking:** every import path changes (e.g. `wifi_pure::X` → `juggler::wifi::X`, `rustyfarian_esp_idf_wifi::X` → `rustyfarian_esp_idf_network::wifi::X`); no in-place upgrade — consumers must switch dependency names and imports manually. Migration table: [`docs/features/archive/crate-consolidation-3-crates-v1.md`](docs/features/archive/crate-consolidation-3-crates-v1.md#migration-guide--old-paths-to-new-paths). See also [ADR 016](docs/adr/016-crate-consolidation-for-publishing.md) and the archived feature doc for the full rationale and per-phase record.

### Fixed

- Provisioning store (`ProvisioningStore::open`) rejected valid stores at non-zero flash offsets, panicking `OffsetOutOfBounds` at boot (surfaced on ESP32-C3 hardware).
- HAL provisioning examples (`hal_c3_provision_mqtt` / `hal_c6_provision_mqtt`) panicked `time_driver NoneError` on the already-provisioned boot path; now halt without awaiting.
- HAL async Wi-Fi examples failed to compile — added the missing `embassy-executor` / `embassy-time` dev-dependencies.

## [0.3.0] - 2026-06-16

### Added

- `rustyfarian-esp-hal-provisioning` v0.1.0 — bare-metal SoftAP captive-portal provisioning for the `WifiMqttDevice` profile, ESP32-C3 + ESP32-C6, embassy/async-only.
  Public API: `PortalConfig`, `ProvisioningBuilder`, `ProvisioningSession`, `ProvisioningOutcome { Committed(ProvisioningConfig), FactoryResetRequested, HostAborted }`, `ProvisioningEvent`, `ProvisioningError`.
  Session termination via `wait_outcome()` (richer outcome) or `wait_committed()` (IDF-parity convenience).
  Substrate implementation: A/B torn-write-safe flash store with `Magic = "RFPR"`, CRC32-IEEE, manual `StoreError` `Debug` for credential-redacted logging, 12-byte-prefix targeted read (peak stack ~500 B).
  Per-session nonce (TRNG-sourced, 8 hex chars) on every mutating POST; constant-time compare.
  Security-contract checklist: all 10 items ✓ locked by named host tests (no reflection of submitted values, no password prefill, lengths-only logging, early credential-buffer drop, Cache-Control: no-store, HTML/JSON escaping, library never reboots, commit-guard CRC ordering, request-body cap).
  Examples: `hal_c3_provision_mqtt` and `hal_c6_provision_mqtt` for end-to-end captive-portal demo.
  Test counts: 127 unit + 2 library invariant tests pass on the host toolchain (per AGENTS.md).
  ADR 015 § 3 hand-rolled substrate (DHCP / DNS catch-all / HTTP/1.1 router) is `pub(crate)` private implementation detail inside this crate.
  `start()` rejects non-`WifiMqttDevice` profiles with `ProvisioningError::ProfileNotSupported`; only `SchemaProfile::WifiMqttDevice` is implemented in v1.
  `render_portal_template` now returns `Err(())` when an HTML-escaped substituted value would overflow the output buffer, rather than returning `Ok(...)` with silently-truncated content.
  `ProvisioningEvent::ClientConnected` and `ClientDisconnected` carry `mac: Option<[u8; 6]>`; v1 emits `None` because the `esp-radio 0.18` `AccessPointStationEventInfo` MAC field name has not been verified, and `ClientDisconnected` is reserved (not emitted) until v2 wires the disassociation subscription.
  The portal HTTP read path now loops on `socket.read` until `header_end + Content-Length` bytes are present (bounded by `DEFAULT_REQUEST_SIZE_CAP`), so TCP segmentation no longer truncates POST bodies and silently fails the nonce / `parse_form` checks on valid submissions.
  `ProvisioningSession::wait_committed()` now returns `Result<ProvisioningConfig, ProvisioningOutcome>` instead of `ProvisioningConfig` — the prior loop-on-non-commit shape would hang forever when the only signalled outcome was `FactoryResetRequested` or `HostAborted` (`Signal::wait` is destructive, so the second loop iteration would never receive a signal). The new shape surfaces the alternative terminal outcome instead of silently blocking.
  `PortalConfig.device_name` now flows into the `{{DEV_NAME}}` template substitution — previously the placeholder was sourced only from `Prefill.dev_name` (loaded from the flash store), which rendered empty on fresh / unprovisioned devices despite the API documenting `device_name` as "surfaced in the portal header". The renderer prefers a non-empty `Prefill.dev_name` (a previously customised name) over the caller's default.

### Changed

- `rustyfarian-esp-idf-provisioning`: portal HTML templates (`portal_wifi_mqtt.html`, `portal_lorawan.html`) moved upstream to `provisioning-pure::templates` as `include_str!` consts. Both tiers now render from a single source of truth. Behaviour unchanged.

### Added

- Wi-Fi + MQTT provisioning profile — the provisioning triad generalises from one schema to a closed set of named `SchemaProfile`s built from reusable field groups (Core / LoRaWAN / MQTT / OTA). Two profiles ship: `LorawanFieldDevice` (Wi-Fi creds + LoRaWAN OTAA keys + OTA URL + device name, today's behaviour) and the new `WifiMqttDevice` (Wi-Fi creds + MQTT broker + OTA URL + device name, no LoRaWAN). `parse_form` is now profile-parameterised; `ProvisioningConfig` carries optional `LoraFields` / `MqttFields` groups; cross-profile fields are rejected via a Form-level `ValidationError::UnexpectedForProfile`. MQTT credentials are first-class — typed validation (`mqtt_uri` shape, `1..=65535` port, optional auth with an asymmetric guard that rejects password-without-username, 23-byte client ID), redacting `Debug`, and no HTML prefill for `mqtt_pass`. Plain `mqtt://` only; MQTT-over-TLS stays out of scope. Per [ADR 014](docs/adr/014-wifi-mqtt-provisioning-profile.md).
- `MqttConfig::with_username_only` — sets a username with no password, for brokers that authorise by username alone. Omits the CONNECT packet's password field rather than transmitting an empty string, which is semantically distinct on the wire from `with_auth(user, "")` and is what username-only ACLs typically expect. Used by `idf_c3_provision_mqtt`'s `mqtt_config_from_stored`.
- `rustyfarian-network-pure` gains a `no_std`-safe surface — `#![cfg_attr(not(feature = "std"), no_std)]` with a default-enabled `std` feature that gates `format_broker_url`, `spawn_subscriber_thread`, `QoS`, and the `SubscribeClient` trait (the latter pair bound by `anyhow`, which is now optional behind `std`). The validators (`validate_client_id`, `CLIENT_ID_MAX_LEN`, topic validators), `backoff.rs`, and `status_colors.rs` compile under `no_std`, so `provisioning-pure` consumes them with `default-features = false`. MQTT consumers keep the default `std` feature and are unaffected.
- NVS provisioning schema v2 — adds a `profile` discriminator key (`lorawan` | `wifi_mqtt`, written before `schema_ver`) and the MQTT keys (`mqtt_host`, `mqtt_port`, `mqtt_user`, `mqtt_pass`, `mqtt_client`). `load` reads `schema_ver == 1` / absent-`profile` records as the `lorawan` profile, so deployed beekeeper devices are not re-provisioned; `save` writes only the active group and removes the inactive one.
- `idf_c3_provision_mqtt` example — host contract for the `WifiMqttDevice` profile: open store → check provisioned → run the builder with `SchemaProfile::WifiMqttDevice` → `wait_committed` → construct the downstream `MqttConfig` → reboot, with a `derive_client_id` helper that truncates the device name to 23 bytes on a char boundary when `mqtt_client` is blank.
- Provisioning triad (`provisioning-pure` + `rustyfarian-esp-idf-provisioning`) — SoftAP captive-portal provisioning, NVS persistence, a wildcard DNS catch-all, and a backend-neutral state machine. `provisioning-pure` is `no_std` and host-testable (form parsing, per-field validation, SSID derivation); `rustyfarian-esp-idf-provisioning` is the ESP-IDF binding (builder/session/store/portal/dns). Secrets are never echoed into HTML and a per-session nonce guards `POST` routes. Per [ADR 013](docs/adr/013-softap-provisioning-acceptance.md); the schema-profile generalisation arrived under [ADR 014](docs/adr/014-wifi-mqtt-provisioning-profile.md).
- SoftAP support in `wifi-pure` (`ApConfig`, `validate_ap_config`, AP constants) and `rustyfarian-esp-idf-wifi` (`SoftApManager` over `Configuration::AccessPoint`, plus a `softap_mac()` efuse helper for SSID derivation before the radio starts).
- `idf_c3_provision` example — full host contract for the captive portal: open store → check provisioned → run the builder → `wait_committed` → reboot.
- `MqttBuilder::with_startup_message()` — opt-in startup notification on every (re)connect. When enabled, the builder publishes `"1"` to `iot/{client_id}/startup` (`QoS::AtLeastOnce`, not retained) via `client.enqueue()` immediately when the broker transitions to `Connected`, before any user-supplied `on_connect` callback runs. The publish and the user callback run under a single internal mutex acquisition, so the startup message is always first in the outgoing queue. Failed publishes are logged at `warn!` and do not abort the connection. Replaces the deprecated `MqttHandle::send_startup_message()` for the common case — the builder handles the (re)connect lifecycle automatically.
- `MqttBuilder::subscribe(topic, qos)` — registers topics for automatic (re)subscription without blocking the event loop. Subscriptions are sent from a short-lived thread spawned after `on_connect` returns, avoiding the SUBACK deadlock introduced in `esp-idf-svc 0.52+`.
- `EspIdfEspNow::init_with_radio_sta` — opt-in fallback that keeps the prior unassociated-STA radio behaviour of `init_with_radio`, using a promiscuous-bracket channel re-pin before every send.  Documented ~0–20 % `ESP_ERR_ESPNOW_CHAN` rate; use only when SoftAP conflicts with BLE coexistence or a user-facing AP. See ADR 012.
- `idf_c3_espnow_scout_promisc` example — companion to `idf_c3_espnow_scout` demonstrating the `init_with_radio_sta` fallback with an explicit connected / scanning state machine so failures recover cleanly.
- `ScanConfig::probe_confirmations` and `ScanConfig::confirmation_gap` — gap-spaced confirmation probes after the first ACK on a channel, defending against false-positive channel detection when the peer is mid-roam.

### Fixed

- `MqttBuilder::on_connect` callback deadlocks when `client.subscribe()` is called inside it on `esp-idf-svc 0.52+`. `EspMqttClient::subscribe()` blocks until the broker sends SUBACK; since the callback runs on the event loop thread, that thread cannot process the SUBACK and hangs. The new `.subscribe()` builder method eliminates the footgun.
  **Migration:** move every `client.subscribe()` call out of `on_connect` and onto the builder via `.subscribe(topic, qos)`. See `crates/rustyfarian-esp-idf-mqtt/examples/idf_c3_mqtt_button_oled.rs` (publisher) and `idf_c3_mqtt_led_grid.rs` (subscriber) for a working pair.
- ESP-NOW unassociated-STA channel drift: the ESP-IDF Wi-Fi driver's autonomous background scanner hops the radio off the channel set by `scan_for_peer` within milliseconds, causing every subsequent `send_and_wait` to land on the wrong channel.  `init_with_radio` now starts a hidden SoftAP on channel 1; beacon scheduling holds the channel deterministically and eliminates the need for per-send workarounds.  See ADR 012.
- ESP-NOW `scan_for_peer` failure cascade: a failed re-scan previously left the radio on the last-probed channel with the peer registration removed, so the next `send_and_wait` aborted before TX.  The `Err` branch now restores both the peer registration and the radio channel from the last successful scan.

### Changed

- All documentation examples updated to use `.subscribe()` on the builder instead of `client.subscribe()` inside `on_connect`. The `on_connect` callback should now be used only for `client.enqueue()` (retained-state publishes).
- `MqttHandle::send_startup_message()` deprecation note now points at `MqttBuilder::with_startup_message()` as the primary migration path; the secondary `publish() / publish_with()` pointer is preserved for custom lifecycle messages.
- **BREAKING (semantics)** — `EspIdfEspNow::init_with_radio` now starts the radio in **SoftAP mode** instead of unassociated STA mode, and `default_interface()` consequently returns `WifiInterface::Ap` instead of `WifiInterface::Sta`.  Downstream code that hard-coded `WifiInterface::Sta` on a driver-owned radio must either call `default_interface()` or migrate to the new `init_with_radio_sta` to preserve the prior behaviour.
- ESP-NOW driver internals: replaced implicit `(_wifi.is_some(), wifi_interface)` branching with an explicit private `RadioMode` enum (`CallerManagedSta` / `OwnedSoftAp` / `OwnedStaPromisc`).  Behaviour-preserving for all three constructors; the unsafe promiscuous-bracket send path now lives in a dedicated `send_with_promisc_repin` helper.

## [0.2.1] - 2026-05-06

### Changed

- Adopt `rustyfarian-ws2812 v0.5.0` retag covering the upstream crate renames `led-effects` → `pennant` and `ws2812-pure` → `bunting`. Workspace dependency `led-effects` becomes `pennant`; `rustyfarian-esp-hal-ws2812` feature flag `led-effects` becomes `pennant`. All `use led_effects::…` imports updated to `use pennant::…`. The two HAL drivers stay on git (not yet on crates.io) so `pennant` is also kept as a git dep — sharing the source guarantees a single compiled copy and unifies `StatusLed` / `PulseEffect` across the HAL boundary.

## [0.2.0] - 2026-05-06

This release introduces an OTA MVP across both stacks, completes the April 2026 `esp-hal` upgrade wave, and switches bare-metal Wi-Fi to an async-only API built on `embassy-net`.

### Added

- OTA MVP — three new experimental crates (`ota-pure`, `rustyfarian-esp-idf-ota`, `rustyfarian-esp-hal-ota`) for end-to-end firmware update
- Bare-metal async Wi-Fi via the new `embassy` Cargo feature on `rustyfarian-esp-hal-wifi`
- ESP-NOW peer discovery, reliable delivery, and the Peripheral Command Framework
- Wi-Fi TX power and power-save configuration in `wifi-pure`
- MQTT non-blocking publishes, `StatusLed` boot feedback, and configurable task stack / reconnect timeout

### Changed

- **BREAKING** — bare-metal Wi-Fi is now async-only; the synchronous `WiFiManager` surface and the `hal_*_connect` examples are gone
- **BREAKING** — `esp-radio 0.18` API renames cascade through `rustyfarian-esp-hal-wifi`
- April 2026 `esp-hal` stack wave: `esp-hal 1.1.0`, `esp-rtos 0.3.0`, `esp-radio 0.18.0`, plus matching embassy pins

### Fixed

- ESP-NOW channel-scan and `send_and_wait` race conditions

## [0.1.0] - 2026-03-16

### Added

- `wifi-pure` crate with `WifiDriver` trait, `WiFiConfig`, `ConnectMode`, `MockWifiDriver`, and SSID/password validation (ADR 006); `rustyfarian-esp-hal-wifi` bare-metal stub
- `lora-pure` crate with `LoraRadio` trait, LoRaWAN types, OTA command parser, and `MockLoraRadio` (ADR 005); `rustyfarian-esp-hal-lora` bare-metal stub
- `rustyfarian-esp-idf-lora`: `LoraRadioAdapter` bridging to `lorawan-device 0.12`; `idf_esp32s3_join` and `hal_esp32s3_join` examples for Heltec WiFi LoRa 32 V3
- `espnow-pure` crate with `EspNowDriver` trait, `EspNowEvent`, `PeerConfig`, `WifiInterface` (STA/AP), and `MockEspNowDriver` (ADR 007); `rustyfarian-esp-idf-espnow` ESP-IDF driver
- `rustyfarian-esp-idf-mqtt`: `MqttBuilder` API with `MqttHandle`, lifecycle callbacks (`on_connect`, `on_disconnect`, `on_message`), `LwtConfig`, `with_auth()`, and `publish_with()` (ADR 002)
- `rustyfarian-network-pure`: MQTT input validation, `MqttConnectionState` state machine, and `ExponentialBackoff` iterator for retry logic
- Dual-HAL script infrastructure: `build-example.sh`, `flash.sh`, `ensure-bootloader.sh`, and `xtensa-toolchain.sh` for `hal_*` bare-metal targets
- Examples: `idf_c3_connect`, `idf_c3_mqtt`, `idf_esp32_mqtt`; hardware reference `docs/heltec-wifi-lora-32-v3.md`
- CI: pure-crate test job for all host tests (`rustyfarian-network-pure`, `wifi-pure`, `lora-pure`, `espnow-pure`)

[Unreleased]: https://github.com/datenkollektiv/rustyfarian-network/compare/v0.5.0...HEAD
[0.5.0]: https://github.com/datenkollektiv/rustyfarian-network/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/datenkollektiv/rustyfarian-network/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/datenkollektiv/rustyfarian-network/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/datenkollektiv/rustyfarian-network/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/datenkollektiv/rustyfarian-network/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/datenkollektiv/rustyfarian-network/releases/tag/v0.1.0
