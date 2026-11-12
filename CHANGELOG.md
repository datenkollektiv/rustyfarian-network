# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `WiFiManager::ip() -> Option<Ipv4Addr>` on both tiers: the station's address while ready (tier parity Step 10, ADR 022).
- `rustyfarian-esp-idf-network` `WiFiManager::wait_connected_timeout(timeout_ms) -> Result<Ipv4Addr>`: the blocking wait under its new name.
- `juggler::wifi::MockLink::set_ip`: the address the mock reports while `Connected`.

### Changed

- **Breaking:** `juggler::wifi::NetworkLink` requires `fn ip(&self) -> Option<core::net::Ipv4Addr>`, so generic code can read the station address; both tiers forward to `WiFiManager::ip`.
- **Breaking:** `rustyfarian-esp-idf-network` `WiFiManager::wait_connected()` is now async and returns `()`, the same call as on esp-hal; blocking callers move to `wait_connected_timeout(timeout_ms)`.
- `rustyfarian-esp-hal-network` the link supervisor `WifiLink` is now `WiFiManager`, the IDF name; `WiFiManager::init_async` and `init_softap_async` are unchanged, and firmware declares `static LINK: WiFiManager = WiFiManager::new();`.
- `rustyfarian-esp-hal-network` `AP_IP` is spelled `core::net::Ipv4Addr` (the same type as before).

### Deprecated

- `rustyfarian-esp-hal-network` `wifi::WifiLink`: a type alias of `WiFiManager`.
- `rustyfarian-esp-hal-network` `AsyncWifiHandle::wait_for_ip`: use `WiFiManager::wait_connected` and `WiFiManager::ip`.

## [0.7.0] - 2026-11-12

### Added

- Tier parity (ADR 022, ADR 023): Wi-Fi, MQTT, ESP-NOW, OTA, provisioning and LoRa logic of both tiers now runs on shared `juggler` cores, so one application body compiles against either tier crate; both crate READMEs list the remaining tier capability differences.
- `juggler::wifi::{LinkMachine, LinkInput, LinkAction, LinkState, RECONNECT_BACKOFF_BASE_MS, RECONNECT_BACKOFF_MAX_MS}`: the shared station reconnect policy (1 s back-off doubling to 30 s) with readiness (`NetworkUp`/`NetworkDown`, `is_ready`, `connect_count`, `restart`), suspend/resume (`Suspend`, `Resume`, `LinkState::Suspended`, `LinkAction::Disconnect`), `LinkInput::ConnectFailed` and `LinkAction::Ignored`; re-exported by both tiers.
- `juggler::wifi::{LinkHandover, IdleWait, LinkSupervisor, LinkRadio, LinkEvent, LinkEvents, ReadyWait}`: suspend bookkeeping (`start`, `idle_wait`, `take_ready_change`, `failed_attempts`) and the `std` supervisor (`wait_ready`, `subscribe_ready` returning the readiness at registration atomically, `LinkRadio::joined_ap`), plus `ConnectMode::effective_timeout_secs`.
- `juggler::wifi::NetworkLink` (with `wait_suspended`) and `juggler::mqtt::{MqttClient, MqttMessage}` (with `subscribe(topic, qos)`): shared `no_std` traits implemented by both tiers, with `juggler::wifi::mock::MockLink` and `juggler::mqtt::mock::MockMqttClient` (feature `mock`) for host tests.
- `juggler::wifi::ApSelection` (`Strongest`, the default, and `FirstFound`) with `WiFiConfig::with_ap_selection`, re-exported by both tiers.
- Wi-Fi regulatory country (ADR 026): `juggler::wifi::{Country, CountryError, ChannelSet, Regulatory, CountryPort, apply_country}` with default `Country::WORLD` (`01`, channels 1-11, 802.11d on); both tiers add `wifi::set_country`, `wifi::regulatory`, `WiFiConfig::with_country`, `ApConfig::with_country`, `ProvisioningBuilder::with_country`, `EspIdfEspNow::set_country` / `EspHalEspNow::set_country` (owned radio) and re-export the four types from `wifi` and `espnow`.
- `juggler::wifi::{JoinedAp, lines}` and `juggler::mqtt::lines`: the shared log-line `Display` types; both tiers log the joined AP as `[wifi] link up #N — bssid …, ch N, rssi -NN dBm`.
- `Clone` on `juggler::wifi::LinkMachine`, `juggler::wifi::LinkHandover` and `juggler::backoff::ExponentialBackoff`.
- `juggler::wait_list::{WaitList, Wait}` (`std`) and `juggler::waker_slots::{WakerSlots, poll_ready}` (`no_std`): async waits that unregister on drop and never wake each other in a loop.
- `rustyfarian-esp-idf-network` `WiFiManager::{state, is_ready, is_suspended, connect_count, wait_ready, subscribe_ready, suspend, resume}` and `NetworkLink` impl: ready means associated with an IP, and `suspend(&self)` stops the radio before it returns.
- `rustyfarian-esp-hal-network` `wifi::{WifiLink, link_task}`: library-owned station supervisor (`static LINK: WifiLink = WifiLink::new();`, spawn `link_task(&LINK, controller, stack)`) with `is_connected`, `is_ready`, `wait_connected`, `state`, `connect_count`, `suspend`, `resume`, `is_suspended`, `wait_suspended` and `NetworkLink` impl.
- `rustyfarian-esp-hal-network` `WiFiConfigExt::with_wifi_scheduler_running(wifi)`, `ApConfigExt::with_ap_wifi_scheduler_running(wifi)` and `ProvisioningBuilder::start_scheduler_running(spawner, wifi, store, rng)` for firmware that calls `esp_rtos::start` itself (ADR 021).
- `rustyfarian-esp-hal-network` chip support: `esp32s3` for `wifi`, `provisioning` and `ota` (`ota::TARGET_CHIP = "esp32s3"`), and `esp32` for `wifi` and `mqtt`.
- `rustyfarian-esp-hal-network` `mqtt` feature (ADR 019): async MQTT 5 client over `minimq =0.13.5` for `esp32c3`, `esp32c6`, `esp32s3` and `esp32` with LWT, startup message, re-subscribe and auto-reconnect, link watch and transport timeout, non-blocking `try_publish*` (`Ok` means queued), inbound queue, static buffers, errors in `MqttError`, and a broker host given as IPv4 literal or DNS name (no IPv6).
- `rustyfarian-esp-hal-network` `MqttHandle::subscribe(topic, qos)` adds runtime subscriptions (64-byte topic, up to `MAX_RUNTIME_SUBSCRIPTIONS` = 8, `MqttError::RuntimeSubscriptionsFull`), re-subscribed after every reconnect; `MqttHandle` implements `MqttClient`.
- `juggler::mqtt::{QoS, MqttConfig, LwtConfig, TryPublishError, DEFAULT_MQTT_TASK_STACK_SIZE, classify_broker_host, BrokerHost, MAX_HOSTNAME_LEN, admit_subscription, SubscriptionAdmission}`: MQTT configuration shared by both tiers.
- `juggler::mqtt::{SessionMachine, SessionInput, SessionAction, SessionToken, SESSION_RETRY_MS, NetworkGate}`: the session lifecycle both tiers drive (`SessionAction::{Ready, Ignored}`, `ConnectWork { session_present }`, `SessionMachine::driver_idle`).
- `juggler::mqtt::{ConnectWorkPlan, ConnectStep, ConnectWorkMachine, WorkInput, WorkAction, ConnectPort, perform}`: the ordered connect work (subscriptions, startup message, `on_connect`) both tiers run.
- `juggler::mqtt::{Outbox, PendingPublish, TryClient}` (`std`, with `publish_waiting`, `with_client_mut` in place of `client_mut`, `try_with_client_mut`), `chunks::ChunkAssembler` (`dropped_by_last_feed`), `SubscribeWindow`, `AckStatus`, `SUBSCRIBE_WINDOW` and `SubscriptionTracker::{next_pending_from, send_owed}`.
- `rustyfarian-esp-idf-network` `MqttHandle::{state, connect_count, dropped_publish, wait_connected, receive, try_receive, dropped_inbound}` and `MqttClient` impl, with `mqtt::{MqttConnectionState, InboundMessage, PUBLISH_QUEUE_DEPTH, DEFAULT_RECONNECT_TIMEOUT_MS, MAX_INBOUND_PAYLOAD_LEN}`: without `on_message`, messages go to an 8-slot drop-oldest queue, reassembled from chunks up to 16 KiB.
- `rustyfarian-esp-hal-network` `espnow` feature (ADR 020, `esp32c3`, `esp32c6`): `EspHalEspNow` (`new_owned(WIFI, EspNowConfig)`, `from_controller`) implements `EspNowDriver` and `EspNowLink` and adds `send_and_wait`, `run_sender`, `set_channel`, `set_max_tx_power`, `tx_power_quarter_dbm`, `sta_mac`, `peer_exists`, `oversize_dropped`, `held_dropped`, `current_channel` and `espnow::HOLD_CAPACITY`; errors in `EspHalEspNowError` (a scan on a `from_controller` handle is refused with `ControllerNotOwned`; incl. `ScanInProgress`, `Broadcast`, `Shutdown`, `Scan(ScanError)`, `Country(i32)`).
- `rustyfarian-esp-idf-network` `EspIdfEspNow` implements `EspNowLink` (async `send_acked`, `receive`, `scan_for_peer`, `pinned_channel`, `current_channel`) on one `espnow-tx` worker thread, adds `oversize_dropped`, `espnow::SEND_QUEUE_DEADLINE` and `init_with_radio_config` / `init_with_radio_sta_config` / `init_with_radio_scanning_config` taking `EspNowConfig`, and re-exports `EspNowLink`, `ClassifySend`, `ScanError` and `SendOutcome`.
- `shutdown(&self)` on `EspIdfEspNow`, `EspHalEspNow` and `EspNowLink`: idempotent stop; queued and in-flight requests end as `Shutdown`, a running scan as `Aborted`.
- `juggler::espnow::{EspNowLink, ClassifySend, OversizeLog, EspNowConfig, EspNowConfigError}`, `SendOutcome::of`, `EspNowEvent::try_new` and `mock::MockEspNowLink`; `EspNowConfig` moved from esp-hal (paths unchanged, pure `validate` / `validate_owned`) and gains `owned_tx_power_quarter_dbm()` and `owned_country()`.
- `juggler::espnow` scan core: `ScanMachine`, `ScanInput` (incl. `Shutdown`, `or_abort`), `ScanAction` (incl. `Ignored`), `ScanError` (`#[non_exhaustive]`, `Aborted`, `HomeNotAllowed`, `NotFound { first_fault, not_allowed }`), `ScanFault` (`#[non_exhaustive]`), `ScanTeardown`, `CancelOutcome`, `ScanGate` (`cancel_running`, `on_probe_complete`, `has_deferred`), `GateRefusal` (incl. `Unwinding`), `ScanJobSlot`, `JobEpoch`, `Cancelled`, `restore_matches`, `warn_on_restore_mismatch` and `ScanMachine::{max_polls, abort_teardown, poll_bounded, first_fault, log_fault, with_first_seq, seq, with_allowed_channels}`.
- ESP-NOW probe reply (ADR 025): `juggler::espnow::{ProbeFrame, ProbeReply, TAG_PROBE, TAG_PROBE_REPLY, DEFAULT_REPLY_TIMEOUT}` (`encode`, `parse`, `ProbeReply::answering`), `ScanAction::AwaitReply`, `ScanInput::Reply`, `ScanConfig::{reply_timeout, with_reply_timeout, for_peer}` and `ScanMachine::probe_frame`; a scan ends on the coordinator's stated home channel and falls back to ACK confirmation for peers that do not reply.
- `juggler::espnow` send queue: `TxCore` (`TxId`, `SendKind` incl. `Unacked`, `Refusal` incl. `Expired`, `ScanRefusal`, `Completion` incl. `Unknown`, `Completed`, `Abandon`, `Next`, `admit_direct`, `poll_at`, `is_shut_down`, `PROBE_RESERVED_SLOTS`), `InFlightMarker`, `DeadlineSet`, `DeadlineId`, `DeadlineStorage`, `DeadlineFull` and `worker::{EspNowWorker, EspNowRadio, WorkerConfig, SendEvents, TxOutcome, WorkerError, TX_SLOTS}`.
- `juggler::espnow::{PeerLink, PeerLinkConfig, PeerLinkEpoch, PeerLinkEvent, PeerLinkAction, PeerLinkState, PeerSession, Clock, PeerSendError}` and `EmbassyClock` (juggler `embassy` feature): rescan supervisor (boot scan, 2 s to 60 s retry, rescan after 20 unacknowledged sends) re-exported by both tiers' `espnow`.
- `juggler::ota`: `check_content_length` (re-exported by both tiers), `download::{DownloadMachine, DownloadInput, DownloadAction}`, `persist::{BusyRetry, read_with_busy_retry}`, `bounded_read_request`, `capped_read_len`, `ReadRequest`, `read_bounded`, `FOLLOW_UP_WAIT`, `FollowUp`, `classify_follow_up`, `Deadline::budget_millis`, `ChunkAligner` and `BootOutcome::records_suspect()`.
- `juggler::ota::runtime::{poll_decision, after_unhealthy, deadline_plan, SlotReads, SlotRetry, PollStep, DeadlinePlan, ReportPace, after_publish, CompletionBackoff, completion_step, repair_status, BeginFailure, begin_failure_outcome, RuntimeFlags}`: the pure health and reporter decisions.
- `previous_update_rolled_back()` on `rustyfarian-esp-hal-network` `EspHalOtaManager` and `rustyfarian-esp-idf-network` `OtaSession`, and `EspHalOtaManager::selected_slot()`.
- `juggler::provisioning` portal cores: `portal` (`plan_portal`, `PortalPlanInput` and `PortalPlan` with `country`, `Prefill`, `compose_mqtt_uri`, nonce helpers), `portal_flow::{PortalMachine, PortalInput, PortalAction}`, `dns`, `boot::{classify_wifi_mqtt, portal_outcome}`, `templates`, `PortalPhase` (incl. `Abort`), `IDF_HTTPD_SOCKET_TIMEOUT_MS` and `worst_case_connection_ms`.
- `juggler::provisioning::flash_store::{ProvisioningStore, StoreError}` (feature `flash-store`, re-exported by esp-hal) and `juggler::provisioning::{ProvisioningKv, KvProvisioningStore}` (`std`).
- `juggler::provisioning::{derive_device_name, DeviceName, DEVICE_NAME_MAX_LEN}`: `{prefix}-{xxxx}` from the last two MAC bytes, via `DeviceName::resolve(&mac)`.
- Provisioning re-entry hook: `juggler::provisioning::{ReentryRequest, gate_reentry, ReentryGate}` (re-exported by both tiers) and IDF `WifiMqttBoot::load_with_reentry`, `WifiMqttBoot::default_client_id` and `WifiMqttLoadOutcome::{PortalRequested, FactoryResetRequested}`.
- IDF `WifiMqttBoot::{device_name, ota_url, config}`, `juggler::provisioning::LoraFields::from_storage_parts`, `EUI_HEX_LEN` and `APP_KEY_HEX_LEN`.
- `rustyfarian-esp-hal-network` provisioning persists portal extras (`TAG_EXTRA` record tag, `MAX_RECORD_LEN` 1280 B) and adds `wifi::softap_mac()` and `ProvisioningSession::ap_mac()`.
- `juggler::lora::{LorawanPhy, map_rf_config, LorawanTimeline, classify_response, requested_deadline, TickEvent, Outcome, IrqOutcome, classify_irq, ldro_required, RadioErrorKind}` (incl. `LorawanPhy::phy_due`, `RadioErrorKind::tx_in_progress`), PHY constants, `Region::{default_rf_hz, image_calibration_hz}` and `juggler::lora::sx1262::{init, InitError, InitStep, BRING_UP_ORDER, take_irq_status, irq_read_mask}`, re-exported by both tiers' `lora`.
- Examples: shared app bodies in `examples-shared/` (`lwt_app`, `espnow_echo_app`, `espnow_peer_app`); new `idf_esp32s3_mqtt_lwt`, `idf_c6_mqtt`, `idf_c6_espnow_coordinator`, `idf_c3_espnow_peer`, `idf_c3_async_mqtt`, `idf_c3_async_mqtt_native`, `hal_esp32_connect_async`, `hal_esp32s3_connect_async`, `hal_esp32_mqtt_lwt`, `hal_c3_mqtt_lwt`, `hal_c6_mqtt_lwt`, `hal_esp32s3_mqtt_lwt`, `hal_esp32s3_provision_mqtt`, `hal_c3_ota`, `hal_esp32s3_ota`, `hal_c6_espnow_sta_coordinator` and `hal_c3_espnow_peer`.
- Example build-time switches `MQTT_SUSPEND_DEMO`, `MQTT_ON_CONNECT_DEMO`, `ESPNOW_SCENARIO`, `OTA_DEMO_UNHEALTHY`, `PROVISION_AP_PSK` and `PORTAL_SSID_OVERRIDE`, and just recipes `build-all-examples`, `build-xtensa-idf-mqtt` and `flash-ota-baseline`.
- `juggler::espnow::ScanConfig` implements `Copy`.

### Changed

- **BREAKING (behaviour):** station TX power defaults to `juggler::wifi::STATION_TX_POWER_DEFAULT` (`TxPowerLevel::Low`, 8.5 dBm) on both tiers via `WiFiConfig::station_tx_power_quarter_dbm()`; pass `with_tx_power(TxPowerLevel::Medium)` for the old ESP-IDF level.
- **BREAKING:** the esp-hal default Wi-Fi country changes from CN (channels 1-13) to `Country::WORLD` (1-11); set `with_country` to use 12 and 13.
- **BREAKING:** `WiFiConfig` gains the fields `country` and `ap_selection`, `ApConfig` gains `country`, and `ScanConfig` gains `reply_timeout`; struct literals must add them (`WiFiConfig::new`, `ScanConfig::for_peer()` unaffected).
- **BREAKING:** new enum variants need a match arm: esp-hal `WifiError::Country(i32)` and `ProvisioningError::SoftApInit { reason }`; `LoraError` (both tiers) and `juggler::lora::MockRadioError` `TxInProgress`; `MockRadioError::{Timeout, InvalidRfConfig}`; esp-hal `LoraError::InvalidRfConfig`.
- **BREAKING:** `juggler::lora::LoraRadio` gains the required methods `cancel_rx` and `irq_pending`.
- **BREAKING:** `juggler::ota::runtime::{ViewStep, PublishStep, DeliveryStep}` and `juggler::ota::HealthAction` gain `Ignored`, `CompletionRetry::on_result` returns `CompletionStep` (`Retry`, `Stop`, `Ignored`) instead of `bool`, `SlotReading::PendingVerify` carries a `SlotId` and `HealthAction::NoteRollback` carries `(RollbackReason, SlotId)`.
- **BREAKING (IDF):** `MqttBuilder::build(&wifi)` and `build_and_wait(&wifi, &mut led)` take the Wi-Fi link; the client is created once the station has an IP, never retries while the link is down, reconnects when the IP returns, and queues publishes made before that (`publish_acked` reports `PublishAckError::Disconnected` while no client exists).
- **BREAKING (IDF):** `MqttBuilder::with_startup_message(topic, payload, qos, retain)` takes arguments like esp-hal; the old no-argument form is `with_default_startup_message()` (deprecated).
- **BREAKING (IDF):** `mqtt` takes `juggler::mqtt::QoS` everywhere, re-exported as `mqtt::QoS`, and `MqttConfig`, `LwtConfig`, `TryPublishError` are the `juggler` types; import `QoS` from `rustyfarian_esp_idf_network::mqtt` and use `mqtt::to_esp_qos(..)` for raw `EspMqttClient::enqueue`.
- **BREAKING (IDF):** `EspNowDriver::Error` for `EspIdfEspNow` is `espnow::EspIdfEspNowError` (`NoAck`, `Timeout`, `Busy`, `ScanInProgress`, `RadioNotOwned`, `PayloadTooLarge { len }`, `Broadcast`, `Shutdown`, `Scan`, `Driver`) instead of `anyhow::Error`; `?` into `anyhow::Result` still works, inherent methods still return `anyhow::Result`.
- **BREAKING (IDF):** `EspIdfEspNow::scan_for_peer` is renamed `scan_for_peer_blocking`; `scan_for_peer` is now the async `EspNowLink` form, and the driver keeps the pinned channel across scans.
- **BREAKING (IDF):** `EspIdfEspNow::send_and_wait`'s `timeout_ms` bounds only queueing and start (a started frame is waited out); it fails with `NoAck`, `ScanInProgress` during a scan and `Broadcast` for the broadcast address.
- **BREAKING (IDF):** plain `EspNowDriver::send` on `EspIdfEspNow` waits behind the frame in flight, returns once its frame left the radio, fails with `Timeout` after `SEND_QUEUE_DEADLINE` (1 s) and with `ScanInProgress` during a scan.
- **BREAKING (IDF):** every `EspIdfEspNow` constructor starts an `espnow-tx` thread (6 KiB stack, joined on drop), and `ScanConfig::probe_timeout` no longer bounds the probe ACK wait on this tier.
- **BREAKING (HAL):** `rustyfarian-esp-hal-network` no longer depends on `esp-println` or `esp-backtrace` (ADR 021); the firmware adds `esp-println =0.18.0` (`<chip>` plus one of `auto`/`jtag-serial`/`uart`/`no-op`, `log-04` for its logger) and `esp-backtrace =0.20.0` (`<chip>`, `panic-handler`, `println`) with `use esp_backtrace as _;`.
- **BREAKING (both tiers):** `PortalConfig::device_name` is `juggler::provisioning::DeviceName`; use `DeviceName::Fixed("name")` for the old behaviour or `DeviceName::MacSuffixed("name")` for `name-xxxx`; names over 24 bytes are truncated and a stored name always wins; leave `PortalDefaults::mqtt_client` blank, else it becomes every device's client ID instead of the one `resolve_client_id` derives from the name.
- **BREAKING (`juggler`):** `ProvisioningConfig::ota_url()` returns `Option<&str>` (use `.is_none()` or `.unwrap_or("")`), `ProvisioningConfig::from_storage_parts` takes a trailing `extras` vector (pass `heapless::Vec::new()`), and `ExtraField`'s `Debug` redacts the value.
- **BREAKING (IDF):** `ProvisioningStore::load()` returns `Option<ProvisioningConfig>`, `WifiMqttBoot::load_with` takes `&ProvisioningConfig`, and `StoredConfig` is removed; read fields through the accessors (`cfg.profile()`, `cfg.mqtt()`, `cfg.lora()`, `cfg.device_name()`, `cfg.ota_url()`, `cfg.extras()`).
- **BREAKING (HAL):** `StoreError::ExtrasNotSupported` is replaced by `StoreError::MalformedField { tag }`; a record with extras can exceed 512 B and reads as unprovisioned on 0.6.x firmware.
- **BREAKING (IDF):** the `lora` feature no longer enables `esp-idf-hal/critical-section`; the firmware enables `esp-idf-hal`'s `critical-section` or `rustyfarian-esp-idf-power`'s `critical-section-impl`.
- **BREAKING (HAL):** `ProvisioningBuilder::start(spawner, timg0, sw_int, wifi, store, rng)` brings the SoftAP up itself; delete the caller's `ApConfig::open(..).with_ap_peripherals(..)` and `init_softap_async` calls.
- **BREAKING (HAL):** the default provisioning AP SSID is `{prefix}-{MAC}` as on ESP-IDF; set `ssid_override: Some("name")` for the bare name.
- **BREAKING (behaviour):** both tiers print the same Wi-Fi and MQTT log lines (`[wifi] link up #N`, `[wifi] network ready — ip …`, `[mqtt] connected (session present: …)`, TX power and country lines, ...), and examples use the `[app]` prefix; scripts that match log text must update.
- **BREAKING (behaviour, IDF):** `WiFiManager::new`, `get_ip` and `wait_connected` wait for association plus IP; a link worker issues every connect with back-off, `suspend` reports not-ready before the radio stops, and dropping it joins the worker.
- **BREAKING (behaviour, IDF):** `MqttHandle::try_publish*` return `TryPublishError::WouldBlock` while `is_connected()` is `false`, as on esp-hal.
- **BREAKING (behaviour):** `validate_ap_config` rejects channels 12 and 13 under the default `Country::WORLD`; both tiers apply the Wi-Fi country on every radio start (ESP-IDF flash written only on change), `EspHalEspNow::new_owned` (`InvalidChannel`) rejects a forbidden channel and the ESP-NOW scan skips such channels.
- **BREAKING (`juggler`):** the `provisioning` feature no longer implies `lora`; users of `juggler::lora` or `LoraFields::to_lora_config` must enable `lora` (and `espnow` now implies `wifi`).
- **BREAKING (behaviour, OTA):** esp-hal `fetch_and_apply` reports a missing Content-Length as `DownloadFailed { status: 0 }` instead of `ServerUnreachable`, and the IDF tier rejects any `Transfer-Encoding`.
- IDF OTA health policy fails closed with `SlotUnreadable` on a slot that stays busy.
- IDF MQTT runs its connect work and `on_connect` on the `mqtt-net` runner thread (no per-connect thread, tasks named `mqtt-events` and `mqtt-net`, 8 KiB stacks); `is_connected()` turns `true` only after subscriptions, startup message and `on_connect`.
- `MqttClient::publish` waits for queue space on both tiers instead of evicting; IDF `MqttHandle::publish_with` still drops the oldest and counts it.
- esp-hal MQTT reports `Disconnected` while waiting for the network, and both tiers wake `wait_connected` only on `SessionAction::Ready`.
- esp-hal `init_async`, `init_softap_async` and `ProvisioningBuilder::start` start the scheduler before validating their configuration.
- esp-hal station reconnect delay is 1 s doubling to 30 s (was 500 ms / 5 s).
- Feature forwarding: IDF `wifi` and `espnow` enable `juggler/std`, IDF `espnow` and esp-hal `embassy` enable `juggler/embassy`, and esp-hal `embassy` enables `embassy-futures`.
- `juggler::espnow::SendOutcome::Local` means the frame was not transmitted and `NoAck` that it was sent but not acknowledged; an esp-hal send timeout now classifies as `Local`.
- ESP-NOW scans on both tiers hold the radio until channel, TX power and peer are restored (also when cancelled), read the TX power back and warn on a mismatch, and refuse a new scan with `Busy` while the previous result is unread.
- esp-hal `send_and_wait` to `BROADCAST_MAC` returns `Broadcast`, and `EspHalEspNow::run_sender` returns `()` after `shutdown` (drop a `-> !` on its task).
- IDF `EspIdfEspNow::init_with_radio` starts with `pinned_channel()` = 1; `init_with_radio_sta` has none until a scan.
- Provisioning portal: esp-hal prefills `dev_name`, answers non-A DNS queries with zero answers, and applies `ssid_override`; IDF DNS rejects QR=1 queries; both tiers return 409 on an invalid submission.
- IDF `LoraRadioAdapter` is an alias for `juggler::lora::LorawanPhy<EspIdfLoraRadio>` (adds `ignored_events`, `radio`, `radio_mut`, `into_radio`), `EspIdfLoraRadio::cancel_rx`/`irq_pending` are `LoraRadio` trait methods, and the `lora-modulation` dependency is dropped.
- Examples: bare-metal station examples use `link_task`, provisioning examples use `DeviceName::MacSuffixed` and restart after a committed submission, `idf_c3_provision_mqtt` watches BOOT for 2 s, and `idf_c3_mqtt_callback_guard` publishes its startup message to `guard/startup`.

### Deprecated

- `rustyfarian-esp-idf-network` `MqttBuilder::with_default_startup_message()`: use `with_startup_message(topic, payload, qos, retain)`.
- `rustyfarian-esp-idf-network` `mqtt::MqttManager`: use `MqttBuilder` and `MqttHandle`.
- `rustyfarian-esp-idf-network` `WiFiManager::link_state()`: use `state()`.
- `rustyfarian-esp-idf-network` inherent `EspIdfLoraRadio::{irq_pending, cancel_rx}`: call the `LoraRadio` trait methods.
- `juggler::mqtt::{SessionPhase, connection_wait_iterations}`: use `SessionMachine` and `MqttHandle::state()`.
- `juggler::mqtt::{ConnectionEpoch, spawn_connect_thread, spawn_subscriber_thread}`: unused by both tiers, removal planned for 1.0.
- `juggler::lora::{LorawanDevice, LorawanResponse}`: use `LorawanTimeline`.
- `juggler::ota::runtime::{HealthMachine, HealthAction, HealthEvent, ReporterMachine, ViewStep, CompletionRetry}`: use `poll_decision`, `after_unhealthy`, `deadline_plan`, `after_publish` and `completion_step`.
- `juggler::ota::runtime::begin_failure`: use `begin_failure_outcome`.
- `juggler::wifi::WifiDriver`: no tier implements it; use the ADR 022 link traits.
- `juggler::espnow::DeferredRestore`: `ScanGate` holds the deferred teardown.
- `juggler::espnow::ScanConfig::new(probe_data)`, `ScanConfig::probe_data` and `ScanMachine::probe_data()`: use `ScanConfig::for_peer()` and `ScanMachine::probe_frame(channel)`.

### Removed

- IDF `MqttHandle::is_connected()` / `build_and_wait()` no longer have a "helper-spawn failure" mode, since no connect thread is spawned.

### Fixed

- IDF `WiFiManager` never reconnected after a link loss.
- IDF Wi-Fi disconnect log blamed the credentials for every reason; `wifi_disconnect_reason_name` covers more codes and `reason_suggests_credentials` / `DisconnectReason` add the hint only for credential failures, on both tiers.
- esp-hal station joined the first AP found instead of the strongest; both tiers now honour `ApSelection`.
- IDF ESP-NOW owned radios default to 13 dBm (`DEFAULT_TX_POWER_QUARTER_DBM`) instead of 20 dBm, which corrupted frames on the ESP32-C3 Super Mini.
- ESP-NOW scan failed with `set channel 12` where the country forbids it.
- ESP-NOW `scan_for_peer` right after a cancelled scan waits for its restore instead of failing with `Busy`.
- ESP-NOW scan could report `NotFound` with the peer present when application sends filled the queue; `TxCore` reserves one slot for the probe (application sends get `TX_SLOTS - 1`).
- esp-hal: cancelling a scan while a probe is on air no longer changes channel, peer table or TX power under the frame.
- IDF ESP-NOW: acknowledgements could be credited to the wrong frame; one permanent send callback now matches each frame.
- IDF ESP-NOW receive truncated (release) or panicked on (debug) frames above 250 bytes; they are dropped and counted in `oversize_dropped`.
- IDF `EspIdfEspNow::init_with_radio_sta` came up as SoftAP after an erased flash.
- IDF ESP-NOW: shutdown during a scan skips the peer-table, channel and TX-power calls, a failed TX-power read no longer boosts the radio, and the receive-queue-full warning is rate limited.
- esp-hal ESP-NOW: a send right after spawning `run_sender` no longer fails with `NoSenderRunner`.
- IDF MQTT: a kept session with an empty connect plan never sent later runtime subscriptions.
- `juggler::mqtt::PendingAcks` grew per fire-and-forget QoS 1 publish; it is capped at `EARLY_ACK_CAPACITY` (8).
- IDF `MqttBuilder::on_message` received only the first chunk of a large message; it now gets the whole message, larger than 16 KiB ones are dropped and counted.
- IDF `MqttHandle::subscribe` twice with different QoS could leave the broker on the older QoS.
- esp-hal MQTT: a ninth subscription failed every connect; subscriptions are paced in a window of eight and a refused SUBACK ends the session.
- esp-hal `MqttHandle::wait_connected` could strand a second waiter.
- `LinkSupervisor::suspend` reported the link down only after the radio stopped, causing a spurious MQTT retry.
- IDF `on_connect`'s `bool` is the session-present flag; docs and examples called it clean-session.
- IDF `MqttBuilder::build` no longer logs the broker URL and client id (CodeQL cleartext logging).
- `MQTT_SUSPEND_DEMO` is now a build-script rerun trigger in both crates.
- esp-hal OTA `fetch_and_apply` never connected its TCP socket; a connect timeout now maps to `DownloadTimeout`, and an `HTTP/1.0` status line is accepted instead of failing as `BadStatusLine`.
- esp-hal OTA erased each target sector about eight times per image; it now erases once and writes aligned through `ChunkAligner` (bug 004).
- esp-hal OTA activation wrote selection and `New` state in two otadata writes; it is now one verified write.
- esp-hal OTA `plan_activation`, `mark_valid`, `rollback` and `selected_slot` now follow the bootloader's selection and sequence-wrap rules.
- IDF OTA download: waits are clipped to the remaining deadline, reads are capped at the 1024-byte buffer, and the last reads no longer degrade to one byte per write.
- esp-hal OTA examples no longer retry a rolled-back image.
- esp-hal `ProvisioningStore` never read a committed record back.
- esp-hal provisioning never fired `ClientDisconnected` and logged the open-AP warning twice.
- esp-hal provisioning portal: a stalled client no longer blocks the accept loop (per-phase timeouts).
- IDF provisioning portal: httpd stack 10 KB to 16 KB, since `POST /save` overflowed it.
- esp-hal docs: provisioning stack guidance, the `0x300000` store offset and the S3 "LoRa-only" claim corrected.
- IDF LoRaWAN: a radio event during a transmission made `lorawan-device` panic; `LorawanPhy` refuses it until `TxDone`.
- IDF LoRa `transmit`/`receive` clear only the IRQ flags they read.
- The `embassy-time` mock-driver dev-dependency of `rustyfarian-esp-hal-network` no longer applies to the `espidf` target.

## [0.6.0] - 2026-10-09

### Added

- OTA consumer runtime and record store (`juggler::ota::{runtime, persist}`, IDF tier `ota`): `OtaRuntime` / `OtaSubmitter` / `OtaHandle` run command intake, an update/rollback/repair worker, a `rolled_back` reporter and the health policy around an app-supplied predicate; the library never restarts on its own.
  `OtaStore<K: OtaKv>` with `EspNvsKv` persists attempts in 16 NVS keys, `reconcile_boot` returns the next action, and `repair_corrupt` clears unreadable records; see `docs/features/archive/ota-consumer-runtime-v1.md`, including "Behaviour changes versus the first consumer's runbook".
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

- MQTT clients (`MqttBuilder`, `MqttManager`) never reconnected after a mid-session disconnect unless `with_reconnect_timeout()` was set: esp-idf-svc reads `reconnect_timeout: None` as "auto-reconnect off"; the default now retries every 10 s (since 0.4.0).
- MQTT deadlock when callbacks called the client while esp-mqtt held its `api_lock` (`is_connected()` stayed false, LWT fired).
- Provisioning: `wait_committed` stack overflow from a config clone, secrets left in freed memory now scrubbed via `zeroize`, and both portals answer stale `/save` or `/factory-reset` with `409` without writing flash.

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

[Unreleased]: https://github.com/datenkollektiv/rustyfarian-network/compare/v0.7.0...HEAD
[0.7.0]: https://github.com/datenkollektiv/rustyfarian-network/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/datenkollektiv/rustyfarian-network/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/datenkollektiv/rustyfarian-network/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/datenkollektiv/rustyfarian-network/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/datenkollektiv/rustyfarian-network/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/datenkollektiv/rustyfarian-network/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/datenkollektiv/rustyfarian-network/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/datenkollektiv/rustyfarian-network/releases/tag/v0.1.0
