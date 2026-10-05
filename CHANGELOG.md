# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `juggler::ota::decide_offer` / `OfferDecision` / `Admission` — `decide_update` plus a refused-version loop guard, so a redelivered offer for a version the device rolled back from is `Refused` instead of retried, and an admission gate (`Admission::from_records(attempt_exists, report_undelivered)`) so an offer arriving while an attempt record or undelivered report exists is `OfferDecision::Blocked` and must be kept and re-evaluated once admission reopens.
  Requested by rustyfarian-rgb-clock; re-exported from both tier `ota` modules.
- `juggler::ota::reconcile` with `AttemptRecord`, `BootFacts`, `ReconcileAction`, `SlotId`, and `SlotState` — pure boot reconciliation that turns persisted attempt evidence and bootloader facts into the next action (await health check, refuse image, report rollback, complete, clear, defer), with unparseable versions and failed slot reads expressed as `None`.
  `ReconcileAction::CompleteAttempt` recovers an interrupted success cleanup: the attempted image is running and `Valid`, so the consumer clears a differing refused version and then the attempt.
  `ReconcileAction::ClearAttempt` is abandoned-attempt cleanup only and never clears a refusal.
  `AttemptRecord.boot_selected` is durable evidence that the target slot was selected for boot, persisted after `fetch_and_apply` returns `Ok`, so a new image that crashes before persisting `activated` still reads as a rollback.
  `BootFacts.report_persisted` / `ReportRollback.report_already_persisted` deduplicate the durable, retried report per attempt.
  `AttemptRecord.attempt_id` (echoed in `ReportRollback`) is the stable event id for at-least-once report delivery; admission stays blocked while a report is undelivered.
  A host lifecycle simulation (`crates/juggler/tests/ota_lifecycle.rs`) injects crashes and failed writes at every step.
  Requested by rustyfarian-rgb-clock; re-exported from both tier `ota` modules.
- `OtaError::code` — a stable, lowercase snake_case wire code per variant (`checksum_mismatch`, `download_failed`, ...).
  Requested by rustyfarian-rgb-clock; available through both tier re-exports of `OtaError`.
- `juggler::mqtt::spawn_connect_thread` — runs an optional connect-time prelude and then the builder subscriptions on one short-lived thread; `spawn_subscriber_thread` is kept as a thin wrapper.
- `juggler::mqtt::ConnectionEpoch` — generation-checked connected flag so a stale connect helper can never mark a dead connection as connected.
- `juggler::mqtt::CallbackScope` / `in_callback` — thread-local marker for "inside an MQTT callback", host-tested and lock-free.
- [`idf_c3_mqtt_callback_guard`](crates/rustyfarian-esp-idf-network/examples/idf_c3_mqtt_callback_guard.rs) example: a self-contained hardware check for the MQTT callback contract (startup message + `on_connect` publish, and `try_publish` / `publish` from `on_message` failing fast with `WrongThread`).
- [ADR 017](docs/adr/017-mqtt-callback-threading-contract.md) records the MQTT callback threading contract and why releasing the event before `on_message` was rejected.
- `just doctor` reports whether the MQTT broker configured in `.env` (`MQTT_HOST` / `MQTT_PORT`) accepts a TCP connection; optional, never fatal, credentials are not read.
- `juggler::provisioning::SessionState` / `SessionOutcome` (`std` feature) — the provisioning portal's shared commit/wait state, moved from `rustyfarian-esp-idf-network` so its host toolchain can run a regression test for the commit/wait race fixed in bug 002/003; covered by `just test-provisioning`.
  `apply`, `apply_and_notify`, and `commit` return `Result<_, InvalidTransition>` so a caller can refuse the request behind a rejected transition; `wait_committed` is single-consumer (every other waiter gets `None` and a `warn` log).

### Changed

- `on_connect` runs on the per-connect helper thread (after the `with_startup_message()` publish, before the subscriptions).
  Calling `client.enqueue()` / `client.subscribe()` on its `client` argument from `on_connect` is therefore safe.
  `is_connected()` still flips only after it returns, and on resumed sessions `on_message` may overlap it.
- `MqttHandle::publish`, `publish_retained`, `publish_with`, `try_publish*`, `subscribe`, and `publish_acked` reject calls from inside any callback.
  The error carries `PublishAckError::WrongThread` (via `anyhow::Error` / `TryPublishError::Other`).
  Signatures are unchanged and the variant's `Display` text is method-neutral.
- `with_startup_message()` publishes from the helper thread, never from the event-loop thread; still best-effort, failures logged at `warn`.
- Rustdoc, both READMEs, and the `idf_c3_mqtt` / `idf_esp32_mqtt` examples describe the rule.
- `idf_c3_provision` / `idf_c3_provision_mqtt` examples log secrets as `set` / `missing` instead of their length, so no value derived from a Wi-Fi password or LoRaWAN AppKey reaches a log line (CodeQL `rust/cleartext-logging`).
  Use the `client` argument in `on_connect`.
  Never call a `MqttHandle` method from a callback, and never call the client from `on_message` / `on_disconnect`.
- The `idf_c3_mqtt`, `idf_esp32_mqtt`, `idf_c3_mqtt_button_oled`, and `idf_c3_mqtt_led_grid` examples read the broker port from `MQTT_PORT` (default `1883`).
  Their `.env` handling is shared in `examples/common/env.rs`.
- `try_publish`, `try_publish_retained`, `TryPublishError`, and the module docs state that "non-blocking" holds only outside the MQTT callbacks; inside one the call returns `TryPublishError::Other` carrying `PublishAckError::WrongThread`.
- `idf_c3_mqtt_button_oled` treats the SSD1306 as optional: when no display answers on I2C at boot it logs one warning and runs headless, so the button/MQTT path can be tested on a bare ESP32-C3.
- Workspace version and the `juggler` dependency minimum raised to `0.5.1`.
  `rustyfarian-esp-idf-network` imports `spawn_connect_thread`, `ConnectionEpoch`, and `CallbackScope`, which do not exist in `juggler 0.5.0`.
- Examples no longer fall back to literal placeholder credentials (`WIFI_PASS`, `WIFI_PSK`, LoRaWAN EUIs/AppKey).
  Unset values are the type default and the LoRa examples fail fast at startup.
  This also clears the CodeQL hard-coded-credential findings on example code.
- `rustyfarian-esp-idf-network`'s `provisioning` feature now enables `juggler/std`, so its `SharedState` is a thin alias over `juggler::provisioning::SessionState`; the crate's own public API is unchanged.

### Fixed

- `with_startup_message()`, any `client.enqueue()` / `client.subscribe()` inside `on_connect`, and any `MqttHandle` call from a callback deadlocked the ESP-IDF MQTT event loop.
  esp-mqtt delivers events while holding its recursive `api_lock`.
  esp-idf-svc 0.53 parks the mqtt task until our event loop calls `next()` again.
  `is_connected()` therefore stayed `false` and the broker fired the LWT.
  rustyfarian-rgb-clock confirmed the `on_message` variant on an ESP32-C3 with 0.5.0: a `try_publish` from `on_message` hung the client until reset.
- A three-way cycle appeared whenever `on_connect` was registered.
  It involved a publisher holding the client mutex while blocked on `api_lock` during the connect handshake, the parked mqtt task, and the event loop waiting for that mutex.
  The event loop no longer takes the client mutex at all.
  See [docs/bugs/001-on-connect-enqueue-deadlock-2026-09-27.md](docs/bugs/001-on-connect-enqueue-deadlock-2026-09-27.md).
- `ProvisioningSession::wait_committed` and the internal `wait_outcome` cloned the ~1.3 KB `ProvisioningConfig` onto the caller's stack; `run_wifi_mqtt_portal` then discarded the copy.
  A consumer with `CONFIG_ESP_MAIN_TASK_STACK_SIZE=8000` overflowed the main task right after `Provisioning event: Committed`.
  `wait_committed` now moves the config out with `take()`, `run_wifi_mqtt_portal` never copies it, and the portal publishes `ProvisioningState::Committed` and the payload under one lock so a waiter after the payload was taken still resolves via `juggler::provisioning::resolve_wait`, which now keys on the state alone, and no waiter can observe the state before the payload.
  Both functions document the caller's stack budget.
  See [docs/bugs/archive/002-provisioning-config-stack-clone-2026-09-27.md](docs/bugs/archive/002-provisioning-config-stack-clone-2026-09-27.md).
- `ProvisioningConfig` left the Wi-Fi password, the MQTT password, the LoRaWAN AppKey, and every extra-field value readable in freed stack and heap memory after the value was dropped.
  `ProvisioningConfig`, `LoraFields`, and `MqttFields` now implement `Drop` and overwrite the full backing buffer of each secret with zeros before the memory is released.
  The overwrite uses `zeroize` (`default-features = false`, no transitive crates), a new optional `juggler` dependency enabled by the `provisioning` feature.
  Non-secret fields, `Clone`, `from_storage_parts`, and all accessors are unchanged.
  The scrub covers only each value's final storage: moves of the inline `heapless` buffers, the raw form body, parser and NVS scratch buffers, and a panic dump of live stack frames (bug 002) are not covered.
  See [docs/bugs/archive/003-provisioning-config-no-drop-scrub-2026-09-27.md](docs/bugs/archive/003-provisioning-config-no-drop-scrub-2026-09-27.md).
- `ExtraField`'s `Debug` output (and therefore `ProvisioningConfig`'s) printed extra-field values verbatim, so a secret extra such as `api_token` reached logs.
  The value is now shown as `"<redacted>"`; the key stays visible.
- `ExtraField` scrubs its own value on drop, so a clone taken out of `ProvisioningConfig::extras()` no longer leaves a secret extra (e.g. `api_token`) in freed memory.
  Because of the new `Drop` impl its public fields can be borrowed or cloned but no longer moved out.
- The portal's `/save` reported success (the committed page and `ProvisioningEvent::Committed`) even when the session refused the commit, e.g. a valid submission after `/factory-reset` or a double submit; the credentials were written to NVS and then lost to the pending reset.
  `/save` now checks the transition before touching NVS and answers `409` once the session is committed, reset-pending, or already saving; `/factory-reset` answers `409` after a commit or during a save.
  The first terminal event now stands: up to 0.5.0 a valid submission after a reset request still resolved the waiter as committed.
- The bare-metal (`rustyfarian-esp-hal-network`) portal had the same flaw: a valid `/save` after `/factory-reset` (or a second submit after a commit) was still written to flash, signalled `ProvisioningOutcome::Committed`, and emitted `SubmissionAccepted` / `Committed`; `/factory-reset` after a commit still signalled `FactoryResetRequested`.
  Because the outcome `Signal` keeps only the last value, the later request overrode the first and the user was shown a page for something that did not happen.
  Both routes now answer `409` without writing flash, signalling, or emitting an event when the session refuses the transition, and `Committed` is signalled only after `PersistOk` is accepted.

## [0.5.0] - 2026-09-26

### Added

- `MqttHandle::publish_acked` — QoS 1 publish that blocks until PUBACK or timeout, returning a typed `PublishAckError`; backed by a new host-tested correlation registry in `juggler::mqtt` (`PendingAcks`, `AckWaiter`, `AckOutcome`, `MessageId`). See `docs/features/mqtt-publish-acked-v1.md`.
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
