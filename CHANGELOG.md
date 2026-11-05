# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- ADR 023 adopted: shared tier logic follows a three-rung ladder, and the ladder retrofit moves the Wi-Fi, MQTT, ESP-NOW, OTA, provisioning and LoRa logic of both tiers onto shared `juggler` cores.
  The bench re-runs that the retrofit needs are mapped to gates in `docs/runbooks/release-0.7.0-hardware-test.md`.
- `juggler::wifi::LinkHandover::{start, idle_wait, take_ready_change}` and the enum `juggler::wifi::IdleWait` (`Link`, `Park`, `Restart`), re-exported from `juggler::wifi`.
  `juggler::wifi::ConnectMode::effective_timeout_secs` returns the connect timeout that both tiers share.
- `Clone` on `juggler::wifi::LinkMachine`, `juggler::wifi::LinkHandover` and `juggler::backoff::ExponentialBackoff`.
- `juggler::mqtt::{ConnectWorkPlan, ConnectStep}`: the ordered connect work (subscriptions, startup message and `on_connect`) that both tiers run.
  `ChunkAssembler::dropped_by_last_feed()` reports how many messages the last feed dropped.
- `juggler::ota::runtime::HealthConfig::{time_bound, TIME_BOUND_SLACK}`: the health policy's time bound, which the IDF health loop no longer computes itself.
  The IDF health loop now stops with `SlotUnreadable` on an `Ignored` action instead of replaying the last action.
- `juggler::espnow::TxCore::admit_direct`, `ScanTeardown`, `CancelOutcome`, `ScanMachine::{max_polls, abort_teardown, poll_bounded}`, and the `ScanGate` cancel path (`cancel_running`, `on_probe_complete`, `has_deferred`).
  `ScanGate` takes a third type parameter, defaulting to `ScanTeardown`, so existing uses still compile.
- `juggler::ota::check_content_length` (re-exported flat on both tiers), `juggler::ota::download::{DownloadMachine, DownloadInput, DownloadAction}`, `juggler::ota::runtime::{repair_status, BeginFailure, begin_failure_outcome, RuntimeFlags}`, `BootOutcome::records_suspect()`, and `juggler::ota::persist::{BusyRetry, read_with_busy_retry}`.
  `rustyfarian-esp-idf-network` `ota::BusyRetry` stays at its path as a re-export, and `rustyfarian-esp-hal-network` `ota` re-exports `BusyRetry` and `read_with_busy_retry` under `ota-wire`.
- `juggler::provisioning` gains the shared captive-portal cores, re-exported at `juggler::provisioning`: `portal` (`plan_portal`, `PortalPlan`, `Prefill`, `compose_mqtt_uri` and the nonce helpers), `portal_flow::{PortalMachine, PortalInput, PortalAction}`, `dns` (the DNS codec), `boot::{classify_wifi_mqtt, portal_outcome}` and `templates`.
- `juggler::provisioning::flash_store::{ProvisioningStore, StoreError}` behind the new `flash-store` feature; `rustyfarian-esp-hal-network` re-exports them at the same path.
- `juggler::lora::{LorawanPhy, map_rf_config, LorawanTicker, TickEvent, Outcome, IrqOutcome, classify_irq, ldro_required, RadioErrorKind}`, the PHY constants (for example `LORAWAN_PUBLIC_SYNC_WORD`, `RX_PAYLOAD_MAX` and the `IRQ_*` bits), `Region::{default_rf_hz, image_calibration_hz}`, and the `juggler::lora::sx1262` bring-up (`init`, `InitError`, `InitStep`, `BRING_UP_ORDER`).
  The ESP-IDF and esp-hal `lora` modules re-export the same shared surface.
- `juggler::espnow::EspNowLink` (async `send_acked`, `receive`, `scan_for_peer` and `pinned_channel` on top of `EspNowDriver`), `ClassifySend` with `SendOutcome::of`, `EspNowEvent::try_new` (returns the oversize length instead of truncating), `OversizeLog` (drop counter and rate-limited log decision, first drop then every 32nd) and `espnow::mock::MockEspNowLink` (scripted send and scan results, receive queue); `ScanMachine` now logs an expired burst at `debug` when the scan finishes, and the `espnow` feature alone is enough for the shared host-test fixtures (tier-parity ESP-NOW Step 1, juggler only; neither tier implements the trait yet).
- `juggler::espnow::TxCore<R, N>` (`no_std`, no alloc) with `TxId`, `SendKind`, `Refusal`, `ScanRefusal`, `Completion`, `Abandon` and `Next`: the sans-IO ESP-NOW send queue both tiers will run (tier-parity ESP-NOW Step 2, juggler only; neither tier uses it yet).
  It owns request ids with a stale-completion guard, FIFO order with one frame in flight, deadlines that bound queueing and start (a started frame is waited out), cancellation through `abandon`, per-slot results and wakers, queue pressure (`Full`) and the scan gate (`begin_scan`/`end_scan`: `Busy`, `NotOwned`, app frames refused with `Scanning`, probes allowed, queued app requests completed as `Preempted`, `can_start_scan_io` waits for a frame in flight).
  `shutdown` ends every request (`Completion::Shutdown`) and refuses new ones, so a worker can stop without a send callback; an app send to the broadcast MAC is refused (`Refusal::Broadcast`).
  Host-tested, with mutation checks on the expiry-and-mark step of `next`, the id guards, the scan gate, the preemption, the broadcast refusal and the shutdown.
- `juggler::espnow::PeerLink` (`no_std`, with `PeerLinkConfig`, `PeerLinkEpoch`, `PeerLinkEvent`, `PeerLinkAction` and `PeerLinkState`): a clock-injected supervisor that decides when a node must (re)scan for its coordinator.
  It issues the boot scan, retries a failed scan with a doubling backoff (2 s up to 60 s), rescans after 20 consecutive unacknowledged sends, never rescans for sends that did not leave the node, and returns to the last channel when the coordinator is heard again; stale scan and send results are discarded by epoch.
  Host-tested, with mutation checks on the threshold and both stale guards (tier-parity ESP-NOW peer link).
- `juggler::espnow::ScanFault`, `ScanMachine::{first_fault, log_fault}` and `ScanError::NotFound::first_fault`: the scan machine records the first channel switch or probe send (first or confirmation probe) that failed, and `NotFound` reports it, for example `peer not found after probing 0 of 13 configured channels (first failure: probe send on channel 1)`.
  A scan whose every channel failed before a probe went out therefore no longer reads as plain silence.
  `ScanMachine` and `ScanError` are unreleased, so the new field is not a breaking change.
  Both tiers log through the shared `ScanMachine::log_fault`: the first failure at `warn` with the driver error (`ESP-NOW scan: failed to set channel N: ...` / `ESP-NOW scan: probe on channel N failed: ...`) and later ones in the same scan at `debug`; on the ESP-IDF tier this replaces the per-failure `Failed to set channel` warning and the silent first probe failure.
- One application body for both tiers (ADR 022, tier-parity Step 9): `examples-shared/lwt_app.rs` is compiled into `idf_esp32s3_mqtt_lwt` and `hal_esp32s3_mqtt_lwt` through `#[path]`, so the two S3 LWT examples differ only in setup and their application lines share the `[app]` prefix for the bench comparison.
  Both examples are excluded from their crate's published package, because the shared file lives outside the crate, and the ESP-IDF crate gains a `heapless` dev-dependency for the shared payload buffer.
  The tick loop, receive loop and suspend demo all start at boot; the first-connect line is logged when the session comes up, so a boot with an unreachable broker still reports link state, ticks and the demo.
  Both crate READMEs gain a "Tier capability differences" table.
- `juggler::mqtt::NetworkGate` (`no_std`) and `juggler::mqtt::{Outbox, PendingPublish}` (`std`): the network gate turns link-readiness observations into exactly one `SessionInput::NetworkUp`/`NetworkDown` per transition (never a `NetworkDown` before the link was up), and the outbox holds up to four publishes that arrive before the client exists, dropping the oldest and counting it for the synchronous path (the shared trait method waits for room instead — see Changed); both host-tested with mutation checks (tier-parity Step 8b-2).
- `juggler::wifi::LinkSupervisor::subscribe_ready` and `rustyfarian-esp-idf-network` `WiFiManager::subscribe_ready`: the supervisor tells one sink per readiness transition and returns the readiness at registration, taken under the same lock, so the seed and the subscription are atomic and no transition can be lost between them; a consumer that must gate its work on the link therefore needs neither a poll nor a second IDF event subscription.
- `juggler::mqtt::SessionMachine::driver_idle`: `true` while closing a session whose driver had already dropped its transport (it was in the retry pause), so the tier reports `Disconnected` itself instead of waiting for a driver event that never comes; closing an idle client also skips the disconnect call.
- `rustyfarian-esp-idf-network` `MqttHandle::dropped_publish()`, `mqtt::PUBLISH_QUEUE_DEPTH` and `mqtt::DEFAULT_RECONNECT_TIMEOUT_MS`: the publish-queue counter matches esp-hal, the queue depth is esp-hal's four slots, and the default retry pause is the 10 s both tiers use.
- `rustyfarian-esp-idf-network` `EspIdfEspNow` runs every transmission on one `espnow-tx` worker thread and implements `juggler::espnow::EspNowLink` (tier-parity ESP-NOW Step 4b): new async `send_acked(mac, data, timeout)` (`Duration::MAX` is unbounded, a started frame is waited out, the broadcast address is refused with `Broadcast`, cancel-safe), async `scan_for_peer(&mac, &cfg)` (the driver keeps the pin, dropping the future cancels the scan and the worker restores channel, TX power and peer before the exclusion ends) and `pinned_channel()`, plus a compile-time parity check with the esp-hal tier.
  The worker owns one permanent send callback and the shared `TxCore`; it is started by every constructor, runs the scan on its own thread, and is joined on drop without depending on a send callback (a hung callback never blocks the join).
- `juggler::espnow::ScanJobSlot<J, R>` with `JobEpoch` and `Cancelled` (`no_std`, no alloc): the scan job hand-off of a worker-thread driver.
  It admits one job at a time (a finished result nobody has taken still counts), keys every caller-side operation by an epoch so a stale ticket can neither take nor cancel a newer scan, and hands every discarded job or result back so the caller drops it outside its lock.
  Host-tested, with the interleaving of the Step 4c review (a scan admitted while the previous result is unread) and a mutation check on the epoch guard.
- `juggler::espnow::InFlightMarker` (`no_std`): the frame awaited by a send callback and its verdict (matched by destination MAC, the first verdict wins, a callback while idle is ignored).
- `juggler::espnow::{DeadlineSet, DeadlineId, DeadlineStorage, DeadlineFull}` (`std`): the registry of caller deadlines a worker thread wakes callers at.
  Entries are removed by id, never by value; `register` never allocates and a full set grows from storage allocated outside the lock.
- `juggler::espnow::ScanInput::or_abort(cancel)`: the input a cancelling driver feeds, `Abort` in place of every result but `Failed`.
- `juggler::espnow::SendKind::Unacked`: an application frame that does not report the ACK; gated and preempted by a scan like `App`, but it may be addressed to the broadcast MAC.
  `EspIdfEspNow` plain `send` uses it, so the adapter-side scan gate for broadcast frames is gone.
- `rustyfarian-esp-idf-network` `espnow::SEND_QUEUE_DEADLINE` (1 s): the bound on plain `EspNowDriver::send` waiting for a queue slot and for its frame to start.
- `juggler::espnow::ScanGate<J, R>` with `GateRefusal`, `juggler::espnow::DeferredRestore<P>`, `TxCore::{poll_at, is_shut_down}` and `PROBE_RESERVED_SLOTS` (tier-parity ESP-NOW Step 4c audit): the scan admission, cancellation and finish rules and the completion-by-deadline rule move out of the ESP-IDF worker and esp-hal driver into host-tested juggler cores.
  `ScanGate::admit` claims the radio and stores the job in one call (a refused scan preempts nothing), `cancel` before pickup ends the exclusion itself, and `finish` ends it in the call that publishes the result.
  `TxCore::poll_at(id, now, waker)` expires a queued request by its own stored deadline and waits out a frame in flight; it replaces `TxCore::poll_expired`, which was unreleased.
  `DeferredRestore` holds a cancelled scan's restore until the probe on the air has completed.
  Mutation-checked: the probe reservation, the admission pre-check, the cancel and finish exclusion ends, `poll_at` expiry and the deferral.
- `rustyfarian-esp-hal-network` `esp32` feature now supports `wifi` and `mqtt` on the classic ESP32, with `esp-radio`, `esp-rtos` and `esp-alloc` forwarded to the chip; provisioning and ESP-NOW stay unsupported on it.
- `rustyfarian-esp-hal-network` `ota::EspHalOtaManager::selected_slot(&mut self) -> Result<SlotId, OtaError>` reports the slot the bootloader selected; the host stub does not mirror it.
- Examples `hal_esp32_connect_async`, `hal_esp32_mqtt_lwt`, `hal_c3_mqtt_lwt`, `hal_esp32s3_provision_mqtt`, `hal_c3_ota` and `hal_esp32s3_ota` in `rustyfarian-esp-hal-network`, and `idf_c6_mqtt` in `rustyfarian-esp-idf-network`.
  `hal_c3_ota` and `hal_esp32s3_ota` run the direct HTTP OTA check and need a server on a 192.168.31.x host.
- Just recipes `build-all-examples` (optional prefix, `KEEP_GOING=1` to continue past failures), `build-xtensa-idf-mqtt` (builds `idf_esp32_mqtt` and `idf_esp32s3_mqtt_lwt`) and `flash-ota-baseline` (flashes the baseline of a bare-metal OTA example).

### Changed

- BREAKING: `juggler::wifi::LinkAction` gains `Ignored` (exhaustive enum).
  `LinkMachine::poll` returns it instead of `None` for a `NetworkUp` outside `Connected`, a duplicate `NetworkUp`, and a `NetworkDown` while already down.
  Both tiers re-export the enum unchanged.
- BREAKING: `juggler::wifi::LinkInput` gains `ConnectFailed`, which a refused radio connect now feeds.
  It backs off from `Connecting` and is ignored in every other state.
- BREAKING: `juggler::mqtt::SessionAction` gains `Ignored` and `Ready`.
  `WorkDone` returns `Ready` instead of `None`, and out-of-phase inputs return `Ignored` instead of `None`.
- BREAKING: `juggler::mqtt::SessionAction::ConnectWork` gains the field `session_present: bool`.
- BREAKING: `juggler::espnow::ScanInput` gains `Shutdown`, which finishes a scan at once without a teardown; `ScanInput::or_abort` keeps it.
- BREAKING: `juggler::espnow::ScanAction` gains `Ignored`.
  `ScanMachine::poll` and `ScanTeardown::poll` return it after `Done` instead of repeating the result.
  `ScanMachine::abort_teardown()` before `Start` is not done until it is polled.
- BREAKING: `juggler::espnow::TxCore::complete` returns the enum `Completed` (`Delivered`, `Discarded`, `Unknown`) instead of `Option<Waker>`.
- BREAKING: `juggler::espnow::Completion::Unknown` and `Refusal::Expired` are new, so exhaustive matches need an arm.
  `poll_result` and `poll_at` on a dead id return `Unknown` instead of `Expired`.
- BREAKING: `juggler::espnow::PeerLinkAction` gains `Ignored`, which rejected `PeerLink` events return instead of `None`.
- BREAKING: `juggler::ota::runtime::{ViewStep, PublishStep, DeliveryStep}` each gain `Ignored`.
  `ReporterMachine::on_view`, `on_publish` and `on_delivery` enforce the view, publish, delivery order.
  `CompletionRetry::on_result` returns the new enum `CompletionStep` (`Retry`, `Stop`, `Ignored`) instead of `bool`.
- BREAKING: `juggler::ota::HealthAction` gains `Ignored`; `SlotReading::PendingVerify` carries a `SlotId`; `HealthAction::NoteRollback` carries `(RollbackReason, SlotId)`.
  Both tiers re-export the types unchanged.
- BREAKING: `juggler::lora::LoraRadio` gains the required methods `cancel_rx` and `irq_pending`, so external implementors must add them.
  The generic bridge needs `juggler::lora::RadioErrorKind` on the radio error.
- BREAKING: `juggler::lora::MockRadioError` gains `Timeout` and `InvalidRfConfig`, and `rustyfarian-esp-hal-network` `lora::LoraError` gains `InvalidRfConfig`.
- `rustyfarian-esp-idf-network` `EspIdfLoraRadio::cancel_rx` and `irq_pending` are now `LoraRadio` trait methods that return `Result`.
  `LoraRadioAdapter` is a type alias for `juggler::lora::LorawanPhy<EspIdfLoraRadio<'d, ANT>>` at the same path, adds `ignored_events()`, `radio()`, `radio_mut()` and `into_radio()`, and the `lora-modulation` dependency is dropped.
- Behaviour: IDF `WiFiManager::new` (blocking, with or without LED), `get_ip` and `wait_connected` now wait for supervisor readiness (association plus the station's DHCP IP event) instead of polling `is_connected()` and `wait_netif_up()`.
  The signatures are unchanged.
- Behaviour: the IDF `LinkSupervisor` and the esp-hal `WifiLink` feed `LinkMachine` from the IP up and IP down edges.
  Duplicate or out-of-phase edges are logged or dropped as `Ignored`, and `is_ready()` is unchanged.
  `LinkSupervisor::suspend` reports readiness before the radio stops.
- Behaviour: the IDF MQTT shutdown runs `Shutdown` through `SessionMachine` to `Close` before the client stops.
  The esp-hal MQTT runner closes the socket on network loss, and both tiers run connect work from `ConnectWorkPlan`.
- Behaviour: IDF `scan_for_peer` and the `espnow-tx` scan loop end a scan that is polled after `Done` as an error exit.
  A probe that has not started within `ScanConfig::probe_timeout` is a local failure.
- Behaviour: esp-hal `send_and_wait` to `BROADCAST_MAC` returns `EspHalEspNowError::Broadcast`, matching ESP-IDF.
  A sync send checks the sender lease before the payload size, so an oversize frame while the sender is busy reports `Busy`.
  A cancelled scan restores TX power, removes the temporary peer, restores the channel, and then registers the fallback peer.
- Behaviour: esp-hal `fetch_and_apply` reports a missing Content-Length as `DownloadFailed { status: 0 }` instead of `ServerUnreachable`, reads the partition table only after the server answered, and stops the write path on every error after Prepare.
  The IDF tier also rejects a response with any `Transfer-Encoding` header, as esp-hal does.
- Behaviour: the IDF health policy fails closed with `SlotUnreadable` on a slot that stays busy past the failure deadline, on two `Ignored` events in a row, or past its time bound.
- Behaviour: a fresh device prefills `dev_name` with `device_name` on esp-hal.
  esp-hal DNS answers AAAA, and every type except A and ANY, with NOERROR and zero answers; IDF DNS rejects QR=1 queries and copies RD.
  A refused invalid submission now gets a 409 on both tiers, and both tiers check the profile before the AP settings.
- ESP-IDF `Session` and esp-hal `Shared` wake `wait_connected` only on `SessionAction::Ready`.
- `juggler::espnow::ScanMachine`: `ScanInput::Abort` inside the teardown (`RestoreTxPower` through `RegisterFallbackPeer`) now continues the teardown as if the step had completed, instead of finishing at once.
  An abort during the temporary-peer fallback therefore still emits `RegisterFallbackPeer`, and a scan that had found the peer keeps its new pin.
  Before the teardown `Abort` unwinds as before; the esp-hal scan never feeds `Abort`, so it is unaffected.
- `rustyfarian-esp-idf-network` plain `EspNowDriver::send` now waits at most `SEND_QUEUE_DEADLINE` (1 s) for a queue slot and for its frame to start and then fails with `Timeout`; before it could wait for ever when every slot was held.
  A started frame is still waited out.
  The blocking forms (`send`, `send_and_wait`, `scan_for_peer_blocking`) are documented as not to be run on a thread that also polls `EspNowLink` futures of the same driver.
- `rustyfarian-esp-idf-network` `EspIdfEspNow::scan_for_peer` refuses a new scan with `Busy` while the previous scan's result has not been read, and a ticket can only read or cancel its own scan.
  The `espnow-tx` thread naming restores the caller's pthread configuration exactly instead of resetting it, and a shutdown stops the scan teardown from touching the channel and TX power.
- BREAKING: `rustyfarian-esp-idf-network` `EspNowDriver::Error` for `EspIdfEspNow` is now `espnow::EspIdfEspNowError` (variants `NoAck`, `Timeout`, `Busy`, `ScanInProgress`, `RadioNotOwned`, `PayloadTooLarge { len }`, `Broadcast`, `Shutdown`, `Scan(ScanError)`, `Driver(anyhow::Error)`) instead of `anyhow::Error`; it implements `std::error::Error` and `juggler::espnow::ClassifySend` (only `NoAck` is `SendOutcome::NoAck`).
  Migration: `?` into `anyhow::Result` and `.context(..)` keep working; code that names `anyhow::Error` as the driver error (for example generic code bound to `EspNowDriver<Error = anyhow::Error>`) must use `EspIdfEspNowError`.
  The inherent `scan_for_peer`, `send_and_wait` and the constructors still return `anyhow::Result`.
  The `espnow` feature now also enables `juggler/std`, and `EspIdfEspNow::receive` (async, safe for concurrent callers, each frame to exactly one) shares one queue with `try_recv` (tier-parity ESP-NOW Step 4a, B1).
- BREAKING: `rustyfarian-esp-idf-network` `EspIdfEspNow::scan_for_peer` is renamed `scan_for_peer_blocking` (same `anyhow::Result<ScanResult>`); `scan_for_peer` is now the async, typed (`EspIdfEspNowError`) form of the shared `EspNowLink` trait.
  Migration: call `scan_for_peer_blocking` from threaded code, or `.await` the new `scan_for_peer`.
  The driver now keeps the pin: after a successful scan `pinned_channel()` returns the channel, a failed or cancelled scan leaves it alone, and the next scan restores the radio to it (tier-parity ESP-NOW Step 4b).
- BREAKING: `rustyfarian-esp-idf-network` `EspIdfEspNow::send_and_wait` changes behaviour (B2, B4): `timeout_ms` bounds waiting for the queue and the start of the transmission only, a started frame is waited out however long its callback takes, an unacknowledged frame fails with `EspIdfEspNowError::NoAck` (inside the `anyhow::Error`), a send during a scan fails at once with `ScanInProgress`, and a send to the broadcast address fails with `Broadcast`.
  A timeout now means the frame never started (`Timeout`) and classifies as a local failure; before it covered the ACK wait too.
- BREAKING: `rustyfarian-esp-idf-network` plain `EspNowDriver::send` on `EspIdfEspNow` is serialised behind the frame in flight (B3): it waits for it (normally a few tens of milliseconds, without limit if the send callback never fires), returns once its own frame has left the radio instead of when it was queued, and fails with `ScanInProgress` during a scan.
  In unassociated STA mode (`init_with_radio_sta`) every worker transmission, plain `send` included, now re-pins the radio to the pinned channel first, as `send_and_wait` did.
- BREAKING: every `EspIdfEspNow` constructor (`init`, `init_with_capacity`, `init_with_radio`, `init_with_radio_sta`, `init_with_radio_scanning`) now starts an `espnow-tx` thread with a fixed 6 KiB stack, joined when the driver is dropped (B6).
  `ScanConfig::probe_timeout` no longer bounds the ACK wait of a scan probe on this tier: a probe that has started is waited out, so the radio never changes channel under a frame on the air.
- `juggler::espnow::SendOutcome::Error` is renamed `Local` and means the frame was not transmitted (queue full, no sender runner, invalid peer, driver error, or a timeout before transmission); `NoAck` is a frame that was transmitted but not acknowledged.
  The type is unreleased, so the rename is not breaking.
  esp-hal `scan_for_peer` now reports a send timeout as `Local`, because on esp-hal a timeout always means the frame never started: a probe that never went out no longer counts as no ACK or as a probed channel, and is recorded as a `ScanFault::ProbeSend`.
- esp-hal `EspHalEspNow` runs on the shared `juggler::espnow::TxCore` and implements `juggler::espnow::EspNowLink` (tier-parity ESP-NOW Step 3); the esp-hal ESP-NOW API is unreleased, so these renames and refusals are not breaking.
  `send_async_timeout` is now `send_acked(mac, data, timeout)` (`Duration::MAX` means unbounded), `send_async` is removed (use `send_acked` with `Duration::MAX`), and `recv_async` is now the infallible `receive()`, which is safe to call from several tasks (each frame goes to exactly one) instead of returning `Busy`.
  The driver now keeps the pinned channel: `scan_for_peer(&mac, &config)` loses its `pinned` argument, `pinned_channel()` reports it (the configured channel at start, then the last successful scan's), and a failed or cancelled scan leaves it unchanged.
  A scan on a `from_controller` handle is refused with `ControllerNotOwned`, and a concurrent scan with `Busy`.
  New `EspHalEspNowError::ScanInProgress` (application sends during a scan, async and sync, and queued ones the scan preempts), `Broadcast` (`send_acked` to the broadcast address, which is never acknowledged) and `Shutdown`; `probe_outcome` became `impl ClassifySend` (only `SendFailed` is `NoAck`).
  The scan holds the radio against application sends until channel, TX power and pin are restored, also when its future is dropped, and waits out a frame already on the air before its first radio action.
  Oversize receive uses `EspNowEvent::try_new` and `OversizeLog`; the hal copies are gone.
- `juggler::mqtt::MqttClient::publish` now waits for admission queue space on both tiers and never evicts a queued message to make room.
  The contract is implemented once, in the new `juggler::mqtt::Outbox::publish_waiting`, which the ESP-IDF trait impl delegates to; esp-hal's existing publish-channel send already implemented it, so its code is unchanged.
  The ESP-IDF pre-client queue therefore no longer drops its oldest entry when a `publish` arrives with all slots taken, while the legacy synchronous `MqttHandle::publish_with` keeps dropping (and counting) it — its behaviour is unchanged.
  A message admitted and then refused by the transport is still dropped and counted in `dropped_publish()`.
  `juggler::mqtt::Outbox` also gains `with_client_mut` and `try_with_client_mut` (with `TryClient`) in place of `client_mut`, so the outbox is reachable without an outer mutex.

- `rustyfarian-esp-idf-network` MQTT is gated on the Wi-Fi link (bug 010, tier-parity Step 8b-2): the esp-mqtt client is created only once the station is associated with an IP — so a normal boot logs no `Error transport connect` — no attempt and no retry happens while the link is down or suspended, and the session reconnects as soon as the IP returns instead of up to `reconnect_timeout` later.
  The shared `SessionMachine` drives it: esp-mqtt's own auto-reconnect is off, every attempt runs from a new `mqtt-net` runner thread and is paced by `MqttConfig::reconnect_timeout_ms`, the event-loop thread only translates esp-mqtt events into session inputs, and the connect helper reports its result to the runner, which performs the close (ADR 017 unchanged).
  **Breaking:** `MqttBuilder::build` and `build_and_wait` take the link — `build(&wifi)` and `build_and_wait(&wifi, &mut led)`; the gate is mandatory, approved by the maintainer for 0.x.
  A publish made before the station has an IP is queued instead of failing (`publish_acked` reports `PublishAckError::Disconnected` while no client exists).
  `WiFiManager::suspend` no longer produces MQTT retry errors, and `idf_c3_mqtt` drops its pre-build `get_ip` wait.

- `rustyfarian-esp-idf-network` `MqttHandle::{wait_connected, receive, try_receive, dropped_inbound}` and `mqtt::InboundMessage`: without an `on_message` callback, received messages go to an 8-slot queue that drops the oldest, as on esp-hal, and a message esp-mqtt delivers in chunks is reassembled (up to `MAX_INBOUND_PAYLOAD_LEN`, 16 KiB) before it is queued; `juggler::mqtt::chunks::ChunkAssembler` (`std`) does the joining; `MqttHandle` implements `juggler::mqtt::MqttClient` (tier-parity Step 8b-1).
- `MqttBuilder::on_message` now receives each message whole instead of only its first chunk (bug 015): esp-mqtt splits a message above its receive buffer into several `Received` events, and the callback used to see the first one as if it were the body.
  Both the callback and the receive queue now go through the same `ChunkAssembler`, so a message above `MAX_INBOUND_PAYLOAD_LEN` (16 KiB) is dropped and counted in `dropped_inbound()` rather than delivered truncated.
  The deprecated `MqttManager` keeps the old behaviour.
- `juggler::wait_list::{WaitList, Wait}` (`std`): async waits on a condition another thread changes; a dropped wait unregisters itself and a repolled one replaces its waker.
- `rustyfarian-esp-idf-network` `WiFiManager::{state, is_ready, is_suspended, connect_count, wait_ready}`: the link worker now also follows the station's IP events, so `is_ready()` and the async `wait_ready()` mean associated with an IP, as on esp-hal (IP events of other interfaces are ignored); `is_suspended()` turns `true` only once the radio is stopped; `WiFiManager` implements `juggler::wifi::NetworkLink` (tier-parity Step 8a).
- `juggler::waker_slots::WakerSlots<N>` (`no_std`): a fixed set of wakers that refuses a registration when full instead of waking everyone, so more waiters than slots never wake each other in a loop; hal's `WifiLink::wait_connected` uses it and lets extra waiters re-check every `NETWORK_POLL_MS`.
- `juggler::mqtt::SessionMachine` (`no_std`, sans-IO) with `SessionInput`, `SessionAction`, `SessionToken` and `SESSION_RETRY_MS`: the MQTT session lifecycle both tiers will drive — no attempt without the network, subscriptions and startup message before the session reports ready, stale connect work ignored, a 5 s retry pause while the network is up (tier-parity Step 6).
- `juggler::wifi::LinkMachine` tracks readiness and associations: `LinkInput::{NetworkUp, NetworkDown}`, `is_ready()` (associated with an IP), `connect_count()` and `restart()`; `LinkHandover` and `LinkSupervisor` expose both, and `LinkSupervisor::wait_ready` returns a `ReadyWait` future that its worker wakes, that unregisters itself when dropped and that replaces its waker when repolled (tier-parity Step 5).
- `juggler::wifi::NetworkLink` and `juggler::mqtt::{MqttClient, MqttMessage}`: the shared `no_std` traits firmware can be generic over, so the same application code runs on both tiers (ADR 022, tier-parity Step 4); the tiers implement them in later steps.
  `juggler::wifi::mock::MockLink` and `juggler::mqtt::mock::MockMqttClient` (feature `mock`) let firmware host-test that code.
- Tier-parity spike examples (ADR 022, Step 2): `idf_c3_async_mqtt` runs an async app over the existing `WiFiManager` and `MqttHandle` threads, `idf_c3_async_mqtt_native` runs the same app on `esp-idf-svc`'s `AsyncWifi` and `EspAsyncMqttClient`; both use `block_on` with `embassy-time` on esp-idf-svc's time driver.
- `juggler::wifi::{LinkMachine, LinkInput, LinkAction, LinkState, RECONNECT_BACKOFF_BASE_MS, RECONNECT_BACKOFF_MAX_MS}`: the pure station reconnect policy both tiers run (1 s back-off doubling to 30 s, reset on connect, stale-input tolerant), host-tested and re-exported from both tiers' `wifi` modules.
  Requested by `rustyfarian-knob` (U2f); see `docs/features/wifi-reconnect-and-suspend-v1.md` and bug 009.
- `rustyfarian-esp-hal-network::wifi::{WifiLink, link_task}`: a library-owned station supervisor; the firmware declares `static LINK: WifiLink = WifiLink::new();` and spawns `link_task(&LINK, controller)` instead of writing a reconnect loop.
  `is_connected()`, `state()` and `connect_count()` are readable from any task.
- Radio suspend/resume on both tiers for deep-sleep firmware (`rustyfarian-knob` U2f, PR 2): `LinkMachine` gains `LinkInput::{Suspend, Resume}`, `LinkState::Suspended` and `LinkAction::Disconnect`, pauses reconnects while suspended, answers a connect that lands after a suspend with another disconnect, and resumes with a fresh back-off.
  The tiers differ in what suspend does to the radio: ESP-IDF stops it, esp-hal only disconnects the station (esp-radio has no stop; the reset of deep sleep powers it down).
  ESP-IDF `WiFiManager::{suspend, resume, link_state}` take `&self` and run `esp_wifi_disconnect` + `esp_wifi_stop` (resume: `esp_wifi_start` + `esp_wifi_connect`) inline, so the radio is stopped when `suspend()` returns; a resume right after a suspend issues one connect, never two.
  esp-hal `WifiLink::{suspend, resume, is_suspended}` are non-blocking signals that `run` honours at every await; `is_suspended()` turns `true` once the disconnect finished or gave up after `SUSPEND_DISCONNECT_TIMEOUT_MS` (500 ms).
  No MQTT suspend call: the client sends no DISCONNECT, so the broker publishes the will; see `docs/wifi-suspend-and-deep-sleep.md` for the deep-sleep recipes and the per-tier MQTT behaviour, and runbook scenario R3.
- `juggler::wifi::LinkHandover` (pure, `no_std`) and `juggler::wifi::{LinkSupervisor, LinkRadio, LinkEvent, LinkEvents}` (`std`): the suspend bookkeeping of the esp-hal `WifiLink` and the thread-based supervisor of the ESP-IDF `WiFiManager`, host-tested with a mock radio against delayed driver events, rapid suspend/resume cycles and concurrent callers.
  The supervisor drops driver events queued before a stop however late they arrive, and only that stop's `Stopped` event re-admits link events (`resume()` never waits, no timeout); it requires the tier to deliver every driver event in order, which ESP-IDF's blocking event post provides. Every transition, including a failed resume's retry, is scheduled under one lock.
  `rustyfarian-esp-idf-network`'s `wifi` feature now enables `juggler/std`.
- `rustyfarian-esp-idf-network` `MqttHandle::connect_count()`: broker sessions since boot, bumped when `is_connected()` turns `true`, like the esp-hal handle.
- `rustyfarian-esp-idf-network` `MqttHandle::state()` returns `MqttConnectionState` like the esp-hal handle (ADR 022, tier parity step 1): `Connecting` per esp-mqtt attempt, `Connected` exactly when `is_connected()` is `true`, `Disconnected` after a lost session or failed attempt; `MqttConnectionState` is now re-exported from the IDF `mqtt` module.
  The state comes from the new lock-free `juggler::mqtt::SessionPhase` (host-tested, `load`/`store` only); `idf_esp32s3_mqtt_lwt` logs `[example] mqtt state X -> Y` like its esp-hal twin.
- `MQTT_SUSPEND_DEMO=1` makes `hal_esp32s3_mqtt_lwt` and `idf_esp32s3_mqtt_lwt` suspend Wi-Fi for 20 s every 60 s; both examples now log link-state changes.
- `idf_esp32s3_mqtt_lwt`: ESP-IDF twin of `hal_esp32s3_mqtt_lwt` (same env keys, LWT `offline` and retained `online` on `<prefix>/status`, a tick every 5 s, default client id `idf-s3-mqtt`, prefix `rustyfarian/idf-mqtt`).
  Runbook `docs/runbooks/wifi-reconnect-hardware-test.md` covers the router-outage protocol for both tiers.
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
- `rustyfarian-esp-hal-network`: `WiFiConfigExt::with_wifi_scheduler_running(wifi)` and `ApConfigExt::with_ap_wifi_scheduler_running(wifi)` build a `HalWifiConfig` or `HalApConfig` from the `WIFI` peripheral alone, for a firmware that already called `esp_rtos::start` in `main`, for example to boot without Wi-Fi when no credentials exist; `init_async` and `init_softap_async` then start nothing.
  Use them instead of `with_peripherals` / `with_ap_peripherals`, which keep handing over `TIMG0` and `FROM_CPU_INTR0`.
  The example `hal_esp32s3_mqtt_lwt` starts the scheduler in `main` and uses `with_wifi_scheduler_running`; validated on a CrowPanel ESP32-S3 on 2026-10-31 (record in `docs/runbooks/esp-hal-mqtt-hardware-test.md`).
  `ProvisioningBuilder::start_scheduler_running(spawner, wifi, store, rng)` is the captive-portal counterpart of `start` and takes only `WIFI`, for a firmware that starts the scheduler in `main` and then decides between the portal and a station join.
  The example `hal_c3_provision_mqtt` starts the scheduler in `main`, uses `start_scheduler_running`, and its already-provisioned branch now awaits an embassy timer instead of spinning.
  Requested by `rustyfarian-knob` (U2e); see ADR 021 and `docs/features/esp-hal-wifi-init-with-running-scheduler-v1.md`.

### Changed

- The ESP-IDF MQTT session is driven by `juggler::mqtt::SessionMachine`: `is_connected()` turns `true` only after the subscriptions, the startup message and `on_connect`, in that order (bug 014); a subscribe or startup-message failure closes the session so esp-mqtt reconnects and repeats it; a kept session whose subscriptions completed is not subscribed again.
- The esp-hal MQTT runner is driven by `juggler::mqtt::SessionMachine` and `MqttHandle` implements `juggler::mqtt::MqttClient` (tier-parity Step 7b).
  While it waits for the network after a loss, `state()` now reports `Disconnected` instead of `Connecting`, as on ESP-IDF.
  The retry pause also ends early when the network drops.
  The first session after boot subscribes even if the broker kept a session.
- `rustyfarian-esp-hal-network::wifi::{link_task, WifiLink::run}` take the embassy-net `Stack` as well: while associated the supervisor checks the IPv4 configuration every `NETWORK_POLL_MS` (500 ms), so the new `WifiLink::{is_ready, wait_connected}` mean associated with an IP, as on ESP-IDF; `connect_count` now comes from the shared link machine, and `WifiLink` implements `juggler::wifi::NetworkLink` (tier-parity Step 7a).
- `hal_esp32s3_mqtt_lwt` reads its build-time configuration through a new `no_std` `examples/common/env.rs` with the same names as the IDF one, checks the broker host and client-id length at start-up like its IDF twin, and logs through `log`; both S3 LWT examples now print the same `would block (mqtt <state>)` and inbound (`rx`, binary-aware) lines.
- `rustyfarian-esp-hal-network`: the `embassy` feature now enables the optional `embassy-futures` dependency (already used by `mqtt`) for the `WifiLink` select.
- All seven bare-metal station examples spawn `link_task` instead of their own `wifi_task`; the reconnect delay after a link loss is now 1 s doubling to 30 s (was 500 ms / 5 s), and the link log lines are `[wifi] link up #<n>`, `[wifi] disconnected — <reason>` and `[wifi] reconnecting in <ms> ms`.
- `rustyfarian-esp-idf-network` `MqttHandle::try_publish`, `try_publish_retained` and `try_publish_with` return `TryPublishError::WouldBlock` while `is_connected()` is `false`, matching the esp-hal tier; the check is best effort, so a publish accepted just before esp-mqtt notices a dead transport can still sit in the outbox and go out after the next connect at QoS 1.
  Callers that relied on queueing before the first connect must wait for `is_connected()` (every example already does); the OTA runtime's status publish already treats `WouldBlock` as retry-then-drop.
- `rustyfarian-esp-idf-network` `WiFiManager`: the link worker issues the first `esp_wifi_connect` in every connect mode, so a transient driver error or a failed first association is retried with back-off; the blocking and LED modes only wait for the link until their timeout (before, a failed first association waited out the whole timeout without a retry).
  Dropping the manager, or an error inside `new`, stops and joins the worker before the driver is destroyed.
- `rustyfarian-esp-hal-network`: `WiFiManager::init_async`, `WiFiManager::init_softap_async` and `ProvisioningBuilder::start` now start the `esp-rtos` scheduler before they validate the profile, SSID, password or AP configuration, so a `WifiError::ConfigureFailed` no longer leaves the firmware without a scheduler (every embassy timer await used to hang); callers of `with_peripherals` and `with_ap_peripherals` see no API change.
- `EspIdfEspNow::scan_for_peer` now drives the shared `juggler::espnow::ScanMachine` (blocking `send` plus ACK wait per probe, `std::thread::sleep` for confirmation gaps) instead of its own channel loop, so both tiers run one algorithm.
  The public API, error messages (including the `anyhow` context on registration failures), TX-power boost and restore order and the failed-scan fallback to the last pinned channel are unchanged; log lines are unchanged, except that the burst-timeout line is now emitted after teardown.
  The `espnow` module re-exports `ScanError` and `SendOutcome`.
  Not exercised on hardware: an IDF re-check of scan (nunchuk on channels 1, 6 and 11) is covered by the consumer's manual G0/G2 gates.
- **Breaking (HAL tier):** `rustyfarian-esp-hal-network` no longer depends on `esp-println` or `esp-backtrace`; `wifi` and `lora` stop pulling them in, and the chip features `esp32c3`, `esp32c6`, `esp32s3` and `esp32` forward nothing to them.
  The firmware adds both crates itself and selects exactly one console (`auto`, `jtag-serial`, `uart`, `no-op`) and the panic handler, like the `critical-section` provider (ADR 021).
  Migration: `esp-println = { version = "=0.18.0", default-features = false, features = ["<chip>", "auto", "log-04"] }` (`log-04` only if the firmware uses `esp_println::logger`), `esp-backtrace = { version = "=0.20.0", default-features = false, features = ["<chip>", "panic-handler", "println"] }` and `use esp_backtrace as _;` in the binary; a firmware with its own `#[panic_handler]` omits `esp-backtrace`.
  `auto` prints over USB-Serial-JTAG when a USB host is attached (Super Mini, CrowPanel) and over UART0 otherwise; a missing console fails esp-println's build script with `Currently enabled: none`, a missing panic handler fails the link with `#[panic_handler] function required`.
  Before this change a firmware that selected `auto` next to the old `esp32s3` feature (which forwarded `uart`) failed with `Currently enabled: uart, auto`.
  The crate's examples take both crates from per-target-triple dev-dependencies (C3, C6, S3, ESP32), and the S3 example docs now say USB-Serial-JTAG (the CrowPanel has no CH340 bridge).
  Requested by `rustyfarian-knob` (U2d); confirmed on hardware on 2026-10-31: a CrowPanel ESP32-S3 (`hal_esp32s3_mqtt_lwt`, LWT intact, free heap unchanged) and an ESP32-C3 Super Mini (`hal_c3_connect_async`) both print over USB-Serial-JTAG.
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

### Deprecated

- `rustyfarian-esp-idf-network` `mqtt::MqttManager`: deprecated since 0.7.0 and kept, not removed; use `MqttBuilder` and `MqttHandle`.
- `juggler::mqtt::SessionPhase` and `juggler::mqtt::connection_wait_iterations`: deprecated since 0.7.0 and kept; use `SessionMachine` and `MqttHandle::state()`.
- `juggler::espnow::DeferredRestore`: deprecated since 0.7.0; `ScanGate` now holds the deferred teardown.
- `juggler::lora::{LorawanDevice, LorawanResponse}`: deprecated and kept; use `LorawanTicker`.
- `rustyfarian-esp-idf-network` `EspIdfLoraRadio::irq_pending()` and `cancel_rx()` inherent forwarders: deprecated since 0.7.0; call the `LoraRadio` trait methods.
- `juggler::ota::runtime::begin_failure`: deprecated; use `begin_failure_outcome`.
- `juggler::mqtt::{ConnectionEpoch, spawn_connect_thread, spawn_subscriber_thread}`: no tier uses them since tier-parity Steps 8b-1 and 8b-2 — `SessionMachine`'s `SessionToken` guards stale sessions and the ESP-IDF tier runs its connect work on its own worker — and they are slated for removal in 1.0; the regression tests and the public-path test still cover them until then.
- `rustyfarian-esp-idf-network` `WiFiManager::link_state()`: use `state()`, the name both tiers share.
- `juggler::wifi::WifiDriver`: no tier implements it; ADR 022's shared link traits replace it. Its docs no longer claim the ESP-IDF `WiFiManager` as an implementor, and `MqttConnectionState`'s docs now hold for both tiers.

### Fixed

- `juggler::espnow::TxCore` keeps one slot for the scan's probe, so application callers can no longer fill the queue and starve the probe of a scan admitted afterwards: ESP-IDF scans could report `NotFound` with the peer present, because a refused probe counted as a local failure and the channel was skipped.
  Application sends now have `TX_SLOTS - 1` (3) slots on both tiers; `TxCore::new` needs at least two slots.
- esp-hal `scan_for_peer`: cancelling the future while a probe is on the air no longer changes the channel, the peer table or the TX power under that frame.
  The restore waits for the frame's completion, the sender runner applies it, and the scan exclusion lasts until then.
- `rustyfarian-esp-idf-network` shutdown during a scan skips the peer-table calls as well as the channel and TX-power calls, and `EspIdfEspNow::init_with_radio` now starts with `pinned_channel()` returning the SoftAP's channel 1 (the trait documents the pin as set by a scan or at start); `init_with_radio_sta` sets no channel and has none until a scan.
- ESP-NOW docs on both tiers state the fail-closed rule (a send callback that never fires blocks that frame, every later transmission and every scan until the driver is dropped), and `ScanConfig::probe_timeout` now says what it bounds: queueing and start of a probe on esp-hal, nothing on ESP-IDF.
- `rustyfarian-esp-idf-network` the receive-queue-full warning is rate limited (the first drop, then every 32nd, with a running total).
- `rustyfarian-esp-idf-network` ESP-NOW receive no longer truncates frames above 250 bytes in release builds or panics on them in debug builds: the receive callback now drops them, counts them (`EspIdfEspNow::oversize_dropped`) and logs the first and then every 32nd drop (tier-parity ESP-NOW Step 4a, B5).
- `rustyfarian-esp-idf-network` ESP-NOW acknowledgements can no longer be taken for the next frame's: `send_and_wait` and the scan registered a send callback per call and dropped it on timeout, so a late callback could be read as a later frame's ACK, and plain `send` had no callback at all.
  The `espnow-tx` worker now owns one permanent callback and keeps one frame in flight at a time, matched by id and destination (tier-parity ESP-NOW Step 4b, inventory finding 1).
- `rustyfarian-esp-idf-network` `EspIdfEspNow::init_with_radio_sta` came up as SoftAP after an erased flash, so every ESP-NOW send and scan probe failed: it never called `set_configuration`, the only esp-idf-svc call that sets the Wi-Fi mode, and the driver kept the mode stored in NVS; it now sets STA mode before starting the radio.
- esp-hal `MqttHandle::wait_connected` could strand a second waiter because the `Signal` that woke it parks one waker; the MQTT wait and `WifiLink::wait_connected` now share `juggler::waker_slots::poll_ready` over a four-slot `WakerSlots`, re-checking every 500 ms beyond the slots, and the `MqttClient` trait documents that any number of tasks may wait.
- `LinkSupervisor::suspend` reported the link as down only after the radio had stopped, so the MQTT gate learned about a deliberate suspend from the transport's corpse: esp-mqtt logged an error and the session scheduled a retry that the gate then had to park (runbook R3).
  Readiness is now reported before the stop, because the state machine is already `Suspended` when `poll(Suspend)` returns; the spurious `[mqtt] reconnecting in` line is gone, and one `[mqtt] error` from esp-mqtt's own teardown remains on ESP-IDF.
- `MQTT_SUSPEND_DEMO` now has a `rerun-if-env-changed` trigger in both crates' `build.rs`: without it, setting the variable kept the value the example was last compiled with, so the suspend demo silently did not run.
- The ESP-IDF Wi-Fi disconnect log blamed the credentials for every reason, including a router deauth and the library's own suspend (bug 011).
  `wifi_disconnect_reason_name` now names the 802.11 codes 1..=24 and the ESP-IDF band 200..=212, and the new `reason_suggests_credentials` / `DisconnectReason` pair appends the hint only for `4WAY_HANDSHAKE_TIMEOUT`, `NO_AP_FOUND`, `AUTH_FAIL` and `HANDSHAKE_TIMEOUT`; the esp-hal `WifiLink` logs the same hint from esp-radio's variants.
  A refused association (`ConnectionError::Failed`) carries the hint as well, since that — not a disconnect event — is how a wrong password surfaces on the esp-hal tier.
- The esp-hal OTA download erased every target flash sector about eight times per image: `FlashRegion::write` is read-modify-erase-write, so each 512-byte chunk erased its sector (bug 004).
  An unencrypted partition is now erased once and streamed through the non-erasing `NorFlash` view, with `juggler::ota::ChunkAligner` (host-tested) holding back a partial word so every write is 4-byte aligned; an encrypted partition keeps the read-modify-write path, which the ROM's whole-sector encryption requires.
  The socket read buffer is word-aligned so the driver does not copy through its own 4 KiB sector buffer, and `esp-bootloader-esp-idf` now enables its `embedded-storage` feature.
- The `bool` passed to the ESP-IDF `on_connect` callback is esp-idf-svc's session-present flag; the docs called it `is_clean_session` with the opposite meaning, and `idf_c3_mqtt`, `idf_esp32_mqtt` and `idf_esp32s3_mqtt_lwt` logged "clean session" for a resumed one. The value passed is unchanged.
- `rustyfarian-esp-idf-network` `MqttBuilder::build` logged the broker URL and client id read from an `MqttConfig` that can carry credentials, which CodeQL reports as `rust/cleartext-logging`; it now logs the fixed message `[mqtt] creating client`, and callers log host and client id from their own values.
- `rustyfarian-esp-idf-network` `WiFiManager` never reconnected after a link loss in either connect mode (bug 009): the `StaDisconnected` subscription only logged, so a router outage left the station down until reboot.
  Both modes now reconnect through `juggler::wifi::LinkMachine` on a worker thread; `is_connected()` stays `false` for the whole outage and esp-mqtt reconnects on its own once the link is back.
- esp-hal provisioning reported a station leaving the SoftAP as `ClientConnected { mac: None }` and never fired `ClientDisconnected`: the esp-radio wait method resolves on both directions since the `1.0.0-beta.1` pin and `wifi_task` ignored the variant (bug 008).
  Both events now carry the station MAC, the log line names the direction, and the mapping is a pure `station_event` function with host tests.
- esp-hal provisioning logged the open-AP warning twice per bring-up (once from `ProvisioningBuilder::start`, once from `WiFiManager::init_softap_async`); it is now logged once, from the Wi-Fi layer, and the host test for it moved with the helper.
- esp-hal provisioning docs: the stack guidance in `hal_c3_provision_mqtt`, `hal_c6_provision_mqtt` and the portal module named knobs the tier does not have (`_stack_size`, `CONFIG_ESP_MAIN_TASK_STACK_SIZE`); they now explain that the main stack is the RAM left after `.data`, `.bss` and the heap and point to `ESP_HAL_CONFIG_ENSURE_MAIN_STACK_MINIMUM` as the link-time floor.
  The `0x300000` store offset in the examples and README is now labelled as lying inside the `factory` partition of espflash's default table, with the instruction to add a dedicated `data` partition; stale `esp-rtos 0.3.0` / `esp-radio 0.18` mentions in the examples are refreshed.
  Reported by rgb-clock (`review-queue/rustyfarian-network-esp-hal-docs-defects.md`).
- esp-hal `ProvisioningStore` never read a committed record back: `try_read_sector` read exactly `record_len` bytes, and esp-storage 0.10 rejects a read length that is not a multiple of its 4-byte `READ_SIZE`, so every provisioned boot reopened the portal with `StoreError::Flash` (bug 006, found on an ESP32-C3 Mini-1).
  The read is now rounded up to `READ_SIZE`, a host mock with word-aligned reads covers it, and the hal provisioning examples log a store error instead of silently re-provisioning.
- The esp-hal docs no longer call the ESP32-S3 LoRa-only: every pinned dependency supports it, and the claimed `compile_error!` for S3 with `wifi`/`ota` never existed (S3 with `ota` silently compiled the stub manager).
- `rustyfarian-esp-hal-network` ESP-NOW: an async send or scan probe issued right after spawning `run_sender` no longer fails with `NoSenderRunner`.
  The runner marks itself active on its first poll, which a task spawned behind it can precede; the sender now waits for it, polling every millisecond on an `embassy_time` timer, for at most 100 ms or its own deadline, and only then returns `NoSenderRunner`.
  The boot scan of a peer that spawned the runner and a sending task together no longer reports `peer not found after probing 0 of 13 configured channels`.

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
- MQTT deadlock when callbacks called the client while esp-mqtt held its `api_lock` (`is_connected()` stayed false, LWT fired); see `docs/bugs/archive/001-on-connect-enqueue-deadlock-2026-09-27.md`.
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
