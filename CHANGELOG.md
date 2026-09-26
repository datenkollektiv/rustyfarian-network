# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- **Acknowledged (PUBACK-aware) MQTT publish — `MqttHandle::publish_acked`** (`rustyfarian-esp-idf-network`, `mqtt` feature): a QoS 1 publish that blocks until the broker acknowledges the message or a timeout elapses, so a caller can learn — with a bounded wait — whether a specific message was durably received. This unblocks the one ack-gated action in the OTA contract (clear NVS rollback state only after the `rolled_back` status publish is confirmed, so an unreachable broker retries next boot instead of losing the evidence). Returns a typed `PublishAckError` distinguishing broker-timing outcomes (`Timeout`, `Disconnected` — retry-eligible) from a misuse (`WrongThread` — called from an event-loop callback, which would deadlock) and local faults (`Other`). Purely additive; all existing fire-and-forget publish methods are unchanged. The cross-thread PUBACK correlation is a new pure, host-tested primitive in `juggler::mqtt` (`PendingAcks`, `AckWaiter`, `AckOutcome`, `MessageId`), keyed by the message id that `EspMqttClient::enqueue` returns and `EventPayload::Published` echoes. See `docs/features/mqtt-publish-acked-v1.md`.
- **OTA domain re-export parity:** both tier crates (`rustyfarian-esp-idf-network`, `rustyfarian-esp-hal-network`) now re-export the full `juggler::ota` surface — `ImageMetadata`, `Version`, `OtaState`, `StreamingVerifier`, `bytes_to_hex`, `hex_to_bytes` — from their `ota` module, not just `OtaError`, matching the `wifi`/`espnow` domains. Version-gated OTA consumers no longer need a direct `juggler` dependency to parse sidecar metadata. Purely additive; existing `ota::OtaError` imports are unchanged. See `docs/features/ota-domain-reexport-parity-v1.md`.
- **`juggler::ota::decide_update` / `UpdateDecision`** (Experimental): a `core`-only, alloc-free update decision policy — `Apply` when the offered `Version` is strictly newer than the running one, `Skip` when equal, `Reject` when older (no downgrade path). Re-exported from both tier `ota` modules, matching the existing `Version`/`ImageMetadata` re-export parity. Purely additive; motivated by version-gated OTA consumers (e.g. the `rustyfarian-rgb-clock` MQTT-triggered OTA demo) no longer needing to re-derive this policy from `Ordering` per tier.
- **STA-only Wi-Fi builds: `rustyfarian-esp-idf-network` `features = ["wifi"]` now compiles against a SoftAP-disabled ESP-IDF (`CONFIG_ESP_WIFI_SOFTAP_SUPPORT=n`).** Consumers that disable SoftAP to reclaim flash/RAM previously hit `E0599: no method named ap_netif` (a v0.4.0 consolidation regression). The SoftAP surface (`SoftApManager`, `pin_ap_netif_ip`) is now gated `#[cfg(esp_idf_esp_wifi_softap_support)]`, while `WiFiManager`, `IdfWifiConfig`, `softap_mac()`, and the pure `juggler::wifi` re-exports stay always-available. Enabling `provisioning` on a SoftAP-disabled build yields one clear `compile_error!` (the captive portal requires SoftAP) instead of a cascade of errors. `espnow` and the default (SoftAP-enabled) build are unchanged. A `just check-sta-only` recipe and CI job guard against regressions. See `docs/features/wifi-softap-cfg-gate-v1.md` for the full rationale and build-system notes.
- **`PortalConfig.ssid_override: Option<&str>`** (Experimental — API may change before 1.0): both `rustyfarian-esp-idf-network` and `rustyfarian-esp-hal-network` `PortalConfig` structs gain an `ssid_override` field. When `Some`, the value is used verbatim as the complete SoftAP SSID; `ssid_prefix` and the MAC-derived suffix are ignored. When `None` (default), the existing `{prefix}-{XXXX}` derivation is byte-for-byte unchanged. Invalid overrides (empty, whitespace-only, or exceeding the 32 UTF-8 byte SSID limit) cause `start()` to return an error rather than silently truncating or panicking. SSID length is measured in UTF-8 bytes — a non-ASCII name may exceed the 32-byte cap even if it looks short by character count. Note: using an override disables the per-device MAC suffix, so multiple devices given the same override will share an SSID; uniqueness is the caller's responsibility when opting out of the derived path. **Runtime status:** on `rustyfarian-esp-idf-network` the override drives the actual SoftAP SSID end-to-end; on `rustyfarian-esp-hal-network` the field is currently **validated at `start()` only** — the radio SSID stays caller-controlled (via `ApConfig`/`init_softap_async`) until esp-hal owns AP bring-up (planned — see `docs/features/hal-provisioning-ap-ownership-v1.md`).
- **`juggler::provisioning::resolve_softap_ssid`** (Experimental): new pure `no_std` alloc-free function that resolves the SoftAP SSID from an optional verbatim override plus the `{prefix}-{MAC}` derivation. Validates the override by reusing the existing `juggler::wifi::validate_ssid` (no second length validator added) and additionally rejects whitespace-only overrides. Re-exported from `juggler::provisioning` and both HAL crates' provisioning modules. The shared implementation ensures the SSID resolution/validation *logic* cannot diverge across HALs; runtime radio-SSID behaviour reaches parity on esp-hal only once it owns AP bring-up (planned).
- **`.env`-seeded non-secret pre-fill for the provisioning portal** (Experimental — API may change before 1.0): both `PortalConfig` structs gain a `defaults: PortalDefaults<'a>` field, and `juggler::provisioning` exposes the new `PortalDefaults` type (re-exported from both HAL crates' provisioning modules). On a **fresh / factory-reset device** (empty store) the portal form is now seeded from these non-secret defaults, so testing the WLAN + MQTT and LoRaWAN portals no longer means retyping the SSID, hex EUIs, MQTT URI, client ID, etc. every cycle. A stored configuration still takes precedence; a rejected `POST /save` re-renders with the same stored / default values (the rejected submission itself is never round-tripped). **Secrets are never pre-filled** — `PortalDefaults` structurally carries no `wifi_pass`, `mqtt_pass`, or `app_key`, and the portal templates remain free of secret placeholders. The provisioning examples populate `PortalDefaults` from `option_env!` over the existing `.env` keys (`WIFI_SSID`, `MQTT_HOST`, `MQTT_PORT`, `MQTT_USER`, `MQTT_CLIENT_ID`, `OTA_URL`, `LORAWAN_DEV_EUI`, `LORAWAN_APP_EUI`); a new `build.rs` on `rustyfarian-esp-hal-network` plus extended rerun triggers on `rustyfarian-esp-idf-network`'s `build.rs` force an example rebuild when those values change.

### Migration

- **HAL: `with_peripherals` / `with_ap_peripherals` take `FROM_CPU_INTR0` (was `SW_INTERRUPT`), runners are `Runner<'static, Interface>`, inherits `esp-radio 1.0.0-beta.1` API renames — must use `esp-hal 1.2.2`, `esp-rtos 0.4.0`, `esp-radio =1.0.0-beta.1`, Rust ≥1.95.**
- **IDF: consumers must upgrade to `esp-idf-svc 0.53`, `esp-idf-hal 0.47`, `pennant 0.7` — drivers implementing `pennant 0.6` no longer satisfy `StatusLed` trait.**
- **Existing `PortalConfig { .. }` struct literals must add `ssid_override: None` and `defaults: PortalDefaults::default()`** (additive fields, pre-1.0). This is a source-breaking change for any code constructing `PortalConfig` by name — add both fields (`ssid_override: None`, `defaults: PortalDefaults::default()`) to restore today's behaviour exactly. `PortalDefaults` is re-exported from each crate's `provisioning` module.



- **`juggler::provisioning::resolve_wait` + `WaitResolution`** (Experimental): pure, `no_std`, alloc-free function that decides what a provisioning-session waiter should do given two observable signals — whether a config has been committed and the current `ProvisioningState`. `WaitResolution` is `Committed`, `FactoryReset`, or `Pending`. The "factory-reset terminates an indefinite wait" contract (the bug where `wait_outcome` with `timeout: None` would hang after the portal's factory-reset button was pressed) is now locked by six juggler unit tests covering the full signal matrix, including the committed-takes-precedence-over-reset case. The `rustyfarian-esp-idf-network` provisioning condvar loop in `SharedState::wait_outcome` delegates its per-iteration decision to `resolve_wait`; the `std` condvar plumbing and deadline/poison handling are unchanged. `wait_committed` is unchanged (its loop only acts on the committed flag, not on `FactoryResetPending`; wiring it through `resolve_wait` would change its current behavior).

- **`WifiMqttBoot` boot helper** (Experimental — API may change before 1.0): `rustyfarian-esp-idf-network` provisioning + mqtt features now expose `WifiMqttBoot::load` / `WifiMqttBoot::load_with` (modem-free NVS load, returns a ready-to-borrow `WiFiConfig` + `MqttConfig` bundle) and `run_wifi_mqtt_portal` (portal lifecycle with three-way outcome: `JustProvisioned`, `FactoryResetRequested`, `PortalExitedWithoutCommit`). Gated `#[cfg(all(feature = "provisioning", feature = "mqtt"))]`. Re-exported via `rustyfarian_esp_idf_network::provisioning::{WifiMqttBoot, WifiMqttLoadOutcome, PortalOutcome, BootConfig, run_wifi_mqtt_portal}`.
- **`juggler::mqtt::resolve_client_id`**: pure `no_std` helper that selects between an operator-supplied MQTT client ID, a `device_name`-derived ID truncated on a UTF-8 char boundary to the 23-byte MQTT 3.1.1 cap, and a last-resort fallback. Host-tested.
- `ProvisioningSession::wait_outcome` (crate-internal): three-way condvar wait used by `run_wifi_mqtt_portal`; the factory-reset handler now calls `apply_and_notify` so an indefinite `portal_timeout: None` wait correctly wakes on factory-reset.
- `idf_c3_provision_mqtt` example rewritten on the `WifiMqttBoot` + `run_wifi_mqtt_portal` API, eliminating the copy-paste `derive_client_id` / `mqtt_config_from_stored` helpers.

### Changed

- **IDF stack:** `esp-idf-svc 0.52` → `0.53`, `esp-idf-hal 0.46` → `0.47` (ESP-IDF v5.3.3, no source changes needed); hardware-validated on ESP32-C3 2026-09-26.
- **ws2812 wave:** `pennant 0.6` → `0.7`, `rustyfarian-esp-idf-ws2812 0.6` → `0.7`, `rustyfarian-esp-hal-ws2812 0.6` → `0.7`.
- **Bare-metal stack (esp-radio 1.0.0-beta.1):** `esp-hal 1.1` → `1.2.2`, `esp-rtos 0.3` → `0.4.0`, `esp-radio 0.18` → `=1.0.0-beta.1` (pre-release, re-pin to `1.0.0` on release); compile-verified C3/C6/S3, hardware-validated C3/C6/S3 2026-09-26; see `docs/features/archive/esp-hal-stack-upgrade-september-2026-v1.md`.
- **TX power policy:** bare-metal STA now inherits `esp-radio`'s 5 dBm default instead of forcing 8.5 dBm; hardware-validated on C3 Super Mini 2026-09-26 (no `AuthenticationExpired`); pass `TxPowerLevel::Low` explicitly to restore 8.5 dBm.
- **HAL README:** Wi-Fi and provisioning examples rewritten; "Dependency pins and MSRV" section added.
- **`sha2` 0.10 → 0.11** (`juggler`, `ota` feature).
- **deny.toml:** retired `anyhow 1.0.104` (RUSTSEC-2026-0190) and `crossbeam-epoch 0.9.21` (RUSTSEC-2026-0204); `atomic-polyfill` (RUSTSEC-2023-0089) remains ignored.
- **CI:** action versions (`actions/checkout` v4 → v5, `extractions/setup-just` v2 → v4, `esp-rs/xtensa-toolchain` v1.6 → v1.7.0).
- **MQTT event-loop logging is quieter and more readable.** The per-event trace dropped from `info!` to `debug!`, so steady-state operation no longer spams the default INFO log; a `Received` event now logs its topic and byte length instead of dumping the raw payload as a decimal byte array. Connection-lifecycle events (connected / disconnected / subscribe) still log at INFO.
- **Provisioning portal shutdown stops the DNS catch-all first** (before the HTTP server, then the SoftAP) so OS captive-portal probe domains stop resolving to the device as the httpd tears down — reducing the `httpd_txrx: setsockopt: 22` and probe-404 teardown noise observed on hardware. The server is still dropped before the SoftAP, preserving the netif-teardown ordering.
- **Provisioning `WifiMqttDevice` profile — `ota_url` now optional:** `SchemaProfile::WifiMqttDevice` now treats `ota_url` as optional; previously `ValidationError::Missing` / `Empty` was raised and the provisioning would fail. An absent or empty value is now accepted and stored as an empty string; `ProvisioningConfig::ota_url()` returns `""` (meaning "no OTA configured"). Non-empty values continue to be validated (plain `http://` with non-empty host, max 128 bytes; `https://` still rejected per ADR 011). `LorawanFieldDevice` unchanged — field devices require OTA. The Wi-Fi/MQTT portal template labels the field "(optional)". NVS schema unchanged — both stores already round-trip empty strings. Relaxes validation to serve `rustyfarian-rgb-clock` and other MQTT-only deployments that implement no OTA. See [ADR 014](docs/adr/014-wifi-mqtt-provisioning-profile.md) Amendment 2026-09-26.

### Fixed

- **Provisioning portal: the required "Device name" field is pre-filled on a fresh / factory-reset device** (`rustyfarian-esp-idf-network`). With an empty store the `dev_name` input rendered empty, so every first-time submission had to retype it or was rejected. The defaults path — empty store, a record stored under the other profile, and store / mutex errors — now fills it from `PortalConfig::device_name`, as `PortalDefaults` already documented. A stored device name still takes precedence. `rustyfarian-esp-hal-network` already had this fallback in its renderer and now has host tests covering both cases.
- **Provisioning portal: a rejected or failed `POST /save` no longer wipes the form** (both tiers). The 400 (validation), 500 (persist failure) and, on ESP-IDF, 413 (body too large) re-renders now show the stored / default non-secret values plus the device name instead of blanks. The rejected submission is still not echoed back, and `wifi_pass`, `mqtt_pass` and `app_key` are never pre-filled.
- **Provisioning portal: rejected / failed re-renders now explain why the input is gone** (both tiers). Every error block after a failed `POST /save` ends with the shared `juggler::provisioning::templates::RESUBMIT_HINT` (Experimental) — the form shows saved / default values, so re-enter changes and passwords. The LoRaWAN template marks the OTA URL as required (`required` attribute + hint), matching the Wi-Fi/MQTT template's "(optional)" label. On `rustyfarian-esp-hal-network` the field-error block moved from a 512-byte `heapless::String` to an `alloc` `String`: nine long field errors overflowed the fixed buffer and were silently truncated, leaving unclosed HTML.
- **ESP-IDF OTA rejects an oversized image up front** (`rustyfarian-esp-idf-network`). `fetch_and_apply` compared the declared `Content-Length` with nothing, so an image larger than the inactive slot streamed into flash until a write failed mid-partition (`FlashWriteFailed`). The length is now checked against the update partition's size before anything is erased, failing with `OtaError::InsufficientSpace` as on the esp-hal tier. A response without a `Content-Length` (including chunked transfer) or with a zero length is rejected as `DownloadFailed { status: 0 }`.
- **ESP-IDF OTA no longer treats a truncated download as complete** (`rustyfarian-esp-idf-network`). A connection that closed mid-body returned success, and the failure only surfaced as a misleading `ChecksumMismatch`. It now fails with `DownloadFailed { status: 0 }` after exactly `Content-Length` bytes are expected, matching the esp-hal tier (ADR 011 §2). The boot slot was never at risk — the SHA-256 check already aborted before activation.
- **esp-hal OTA rejects a zero-length image** (`rustyfarian-esp-hal-network`), aligning with the ESP-IDF tier. `Content-Length: 0` used to pass the header checks, write nothing, and rely on the SHA-256 check alone — an expected digest of the empty input would have activated an untouched slot. It now fails before any flash access with `DownloadFailed { status: 0 }`.
- **ESP-IDF OTA contacts the server before erasing flash** (`rustyfarian-esp-idf-network`). The OTA session used to start (erasing the entire inactive slot) before connecting, so every attempt against a down server or a `404` cost a full-partition erase. The session now starts only after the response headers pass the checks above, and erases just the sectors the image needs (`EspOta::initiate_update_with_known_size`). A new `just clippy-ota-tests` recipe type-checks the IDF OTA unit tests, which cannot run on the host.
- **SoftAP captive portal now advertises the AP plus a DNS server via DHCP Option 6** so the OS sign-in sheet appears on connecting devices. Previously clients received an IP address and gateway but no DNS server; without a resolver they could not reach the OS-level captive-portal probe domains (`connectivitycheck.gstatic.com`, `captive.apple.com`, etc.) and the portal never popped — even though the DNS catch-all on port 53 was running. `pin_ap_netif_ip` now calls `esp_netif_set_dns_info` (DNS MAIN = AP IP) and `esp_netif_dhcps_option(OP_SET, DOMAIN_NAME_SERVER, 0x02)` between the DHCPS stop and start, mirroring the esp-hal DHCP Option 6 behaviour that already passed on-hardware validation.

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

[Unreleased]: https://github.com/datenkollektiv/rustyfarian-network/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/datenkollektiv/rustyfarian-network/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/datenkollektiv/rustyfarian-network/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/datenkollektiv/rustyfarian-network/releases/tag/v0.1.0
