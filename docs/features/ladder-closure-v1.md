# Feature: Ladder closure v1

## Decisions
| Decision                                                                                                  | Reason                                                            | Rejected Alternative                                     |
|:----------------------------------------------------------------------------------------------------------|:------------------------------------------------------------------|:---------------------------------------------------------|
| Closure is split into implementation closure and hardware closure, and each is ticked on its own evidence | A host test or build result never proves a bench result           | One combined "closed" checkbox                           |
| A chip and tier cell is covered only by its own gate                                                      | ESP32-C3 results must not stand in for C6, S3 or classic          | Inheriting coverage across chips or tiers                |
| Every affected flow lists its evidence or an explicit gap                                                 | This is the closure rule for this release                         | Calling a flow closed because a neighbouring test passes |
| Out-of-phase input and bounded waits are recorded in ADR 023 decision 6                                   | The rule has to be checkable from the ADR, not only from this doc | Keeping the rule in this feature doc only                |
| The ESP-NOW safety items are tracked as their own table                                                   | ESP-NOW has the most adapter logic and the most unbounded waits   | Folding them into the general flow table                 |

## Constraints
- Hardware results live in `bench/<board>/results.jsonl` and the bench logs; this doc names only the bench check that closes each item.
- The host and build evidence is dated 2026-11-05 and comes from host tests, `just check-*`, `just build-example` and a complete `just build-all-examples` run (38 of 38).
- The code changes of this release are listed under Implementation closure; each is host-tested or compiled, and hardware closure comes only from bench checks.
- Results from the 2026-10-31 and 2026-11-02 bench runs are dated pre-release results and are not 0.7.0 evidence under the closure rule.

## Open Questions
- [ ] Does `juggler::espnow::TxCore` need an explicit `Ignored` action, or does `Completed::Unknown` satisfy ADR 023 decision 5?
- [ ] Should the esp-hal `WifiLink::run` gain a library-side connect timeout, since `connect_async` has none?
- [x] Are `HealthMachine` and `ReporterMachine` overshoots, since ADR 023 lists them at rung 3 but only one tier runs them?
  Answered 2026-11-05: both are flattened to pure functions on the IDF tier and kept as deprecated wrappers.
- [ ] Is `juggler::ota::OtaState::next_state` deprecated, since its order contradicts `DownloadMachine`?
- [x] Does the ESP-IDF v5.5 bump ship in 0.7.0, which decides whether the ESP-IDF v5.5 coexistence soak runs?
  Answered 2026-11-10: the bump moved to the next release, so the soak is N/A for 0.7.0.

## State
- [ ] Design approved
- [x] Core implementation (ladder cores landed up to commit fd225dd on `tier-parity-quest-espnow`)
- [x] Tests passing (host tests and checks on 2026-11-05, and the full build run of 38 examples; see Scope and evidence basis)
- [x] Documentation updated

## Session Log
- 2026-11-05 — Feature doc created (closure audit over six domains, host and build evidence only); fact-check fixes, flattened health, report and LoRa timeline functions, TX-power read-back, hal ESP-NOW shutdown, IDF ESP-NOW worker core and eight new ESP-NOW bench checks.
- 2026-11-06 — External review of 58fc9a8 applied (F1 to F7, see the review section).
- 2026-11-07 — Review follow-ups and their CHANGELOG entries, unhealthy-rollback step added to `c3/ota-rollback`, station AP selection flow added (gap 78).
- 2026-11-08 — Bench: gaps 78 and 44 resolved, gap 4 half resolved, classic ESP32 checks passed.
- 2026-11-09 — Bench: C6 checks, gap 41 resolved, C3 manual provisioning checks.
- 2026-11-11 — Release bench scope applied (fleet tests and consumer-firmware checks moved downstream, OTA rerun limited to rows P2, P7, P13, P14 and P17); `PeerSession` landed (gap 2).
- 2026-11-12 — Gate IDs replaced by bench check names; hardware closure synced with the bench results.

## Scope and evidence basis

This audit covers the shared flows of the `juggler` ladder across the Wi-Fi, MQTT, ESP-NOW, OTA, provisioning and LoRa domains, on both tiers, for the four chips ESP32, ESP32-C3, ESP32-C6 and ESP32-S3.
The rung of each flow comes from ADR 023.
The evidence below was run on 2026-11-05.

- Host tests: `just test-wifi` (411 + 102 + 2), `test-backoff` (7), `test-mqtt` (186), `test` (incl. `one_app_both_tiers`), `test-espnow`, `test-hal` (155, 32 under `espnow::`), `test-ota` (452 + 6 + 3 + 20), `test-ota-hal` (155 + 2), `test-provisioning` (incl. 64 form tests) and `test-lora`.
- Checks: `just check-mqtt` (IDF, riscv only), `check-mqtt-hal-embassy`, `check-espnow`, `check-espnow-hal` (C6 and C3 only), `check-provisioning`, `check-provisioning-hal-embassy` (C3 and C6), `check-lora` and `check-lora-hal` (C6 only).
- Lints: `just clippy-pure`, `clippy-espnow` and `clippy-hal` with no warning; the two IDF `worker.rs` tests are type-checked there and never run.
- Builds: `KEEP_GOING=1 just build-all-examples` built 38 of 38 (every example of both tier crates); `just build-xtensa-idf-mqtt` ends in `Finished` for `idf_esp32_mqtt` and `idf_esp32s3_mqtt_lwt` from cached output.
- Final `just fmt` and `just verify` pass, also after the `restore_matches` helper was added.
- The 26 worker host tests passed five times in a row, and mutation checks on the scan abort and on the caller deadlines in `park` each fail two of them.

## External review findings (2026-11-06)

Every finding of the external review of 58fc9a8 (F1 to F7, two extras, four follow-ups) is fixed in code and listed in the CHANGELOG `[Unreleased]` Fixed and Added sections; only the residual gaps stay here.

- F1 esp-hal OTA activation (two otadata writes): resolved, one verified 32-byte write.
  - Open: the restore path on an encrypted otadata partition has no check, and no hal C6 OTA example exists (gap 40).
- F2 hal MQTT subscriptions (a ninth subscription failed every connect): resolved, a `SubscribeWindow` of eight.
  - Open: a broker refusal reaching the runner as a `poll` error, retained-slot capacity on a kept session, throughput above eight and the ordering against SUBACKs are unchecked.
  - Only the not-yet-runnable `c3/mqtt-lwt` subscription steps would cover them; `c6/mqtt-scenarios` and `s3/mqtt-scenarios` do not.
- F3 IDF concurrent subscribe QoS: resolved by `SubscriptionTracker::send_owed`.
  - Open: the lock order against esp-mqtt is reasoned from code only, and no runnable check covers it (`c6/mqtt` step not runnable; `s3/soak` and `c3/mqtt` do not run it).
- F4 portal stall: resolved, every portal phase is bounded (`provisioning/portal_deadlines.rs`).
  - Open: only the hal helpers are host-tested, not the `run_portal_dyn` wiring or the accept loop after a stall.
  - `c6/provision` and `s3/provision` do not cover the stalled-client case.
- F5 LoRa premature PHY event: resolved by `LorawanPhy::phy_due` (gap 23).
  - Open: real SX1262 timing (WouldBlock, stale RX IRQs latched at TX start) waits for the LoRa checks, which exist for the S3 cell only (gap 22).
- F6 IDF ESP-NOW TX power boosted after a failed read: resolved.
  - Open: the unknown-level case is host-only, since a failed `esp_wifi_get_max_tx_power` cannot be provoked on the bench.
- F7 IDF OTA budget: resolved, waits clipped through `Deadline::budget_millis`.
  - Open: whether `esp_http_client` honours a timeout changed between reads is unchecked on hardware.
  - Open: a chunked response whose transport read yields only chunk framing can still loop inside one read call (pre-existing).
- Extra `lwt_app` resume log: resolved by the `demo_suspended` flag in `examples-shared/lwt_app.rs`.
- Extra hal `WifiLink` suspend docs: resolved in the rustdoc of `is_suspended` and `wait_suspended`.
- Follow-up hal otadata validity (F1): resolved, `valid_entry` follows the bootloader.
- Follow-up IDF OTA read wait (F7): resolved by `bounded_read_request`.
- Follow-up SX1262 IRQ clear (F5): resolved by `take_irq_status`; real timing waits for the LoRa checks (gap 22).
- Follow-up portal stall tests (F4): resolved by three mutation-checked helper tests in hal `provisioning/portal.rs`.

API facts the CHANGELOG does not state:
- `juggler::lora::RadioErrorKind::tx_in_progress` is a required method, so it breaks external implementors.
- `MockLoraRadio::tx_busy_polls` is a new public field.
- The IDF `FirmwareResponse::read` gained a timeout argument, but its module `downloader` is private, so this is not public API.

## Implementation closure

Each box is ticked only where a host test, a compile check or a build result exists.

- [x] Each shared flow that the tables place in a `juggler` core (rung 1 to 3) runs its order there on the tier named in its placement, including `PeerLink`, driven by `PeerSession` since 2026-11-11.
- [ ] Flat logic and adapter-only flows (for example the SoftAP DHCP server and the IDF OTA step order) stay outside `juggler`, and the adapter-only ones are recorded in the adapter decision lists rather than moved.
- [x] The rung-3 cores (`LinkMachine`, `SessionMachine`, `ScanMachine`, `ScanGate`, `TxCore`, `DownloadMachine`, `PortalMachine`, `PeerLink`) each have state-transition tests in `juggler`.
  The deprecated `HealthMachine` and `ReporterMachine` keep their old tests as wrapper-equivalence tests.
  The `std` driver `EspNowWorker` is not a rung-3 machine, and its 26 host tests are driver tests (ADR 023 decision 1).
- [x] Out-of-phase input is an explicit no-op action or refusal for `LinkMachine`, `SessionMachine`, `ScanMachine`, `DownloadMachine`, `PeerLink` and `LorawanPhy`, plus the deprecated `HealthMachine` and `ReporterMachine` wrappers, and `PortalMachine` answers `Refused` or `Forbidden`.
  `ScanGate` answers a stale epoch with `Cancelled::Stale` and changes nothing, admission refusal with `GateRefusal`, and a stale `release` does nothing (`espnow/scan_job.rs`).
- [ ] `TxCore` has no explicit `Ignored` action, so its out-of-phase input surfaces as `Completed::Unknown` or `Completion::Unknown`.
- [ ] The tier loops act on an out-of-phase no-op the same way: the IDF MQTT runner counts it and logs at 16 in a row, the esp-hal MQTT runner restarts, and the esp-hal Wi-Fi loop idles.
- [x] Finite flows reach a terminal action: `DownloadMachine` (`Done`), `ScanMachine` (`Done`, then `Ignored`), `ScanGate` (its per-scan exclusion, ended by `finish`, `cancel` or `release`), `PortalMachine` (`Committed` and `FactoryResetPending` absorb input), the IDF health loop (it ends at its verdict), `LorawanTimeline::next_event` (terminal outcomes, budget checked first) and connect work (`WorkDone` or `WorkFailed`).
- [x] Finite flows bound their iterations: `poll_bounded` on `DownloadMachine` and `ScanMachine`, `ScanTeardown::MAX_POLLS` = 7, the IDF health loop bounded by its failure deadline and retry counts, and the LoRa budgets in `LorawanTimeline`.
- [ ] Every wait of every persistent service is bounded, because the IDF Wi-Fi worker, the IDF ESP-NOW worker, the esp-hal ESP-NOW `run_sender` idle wait and the hal Wi-Fi connect have unbounded waits; ADR 023 decision 6 lists the exceptions, and the hal Wi-Fi connect is gap 26.
- [x] The machines take time as an input (`now`, `backoff elapsed`, scan and ticker timestamps), and none reads a clock.
- [x] Flat logic is in `juggler` as pure functions: `decide_offer`, `reconcile`, `ChunkAligner`, the MQTT topic and client-ID validators, `classify_irq`, the disconnect-reason mapping, SSID validation, the LoRa config parser, the health decisions (`poll_decision`, `after_unhealthy`, `deadline_plan`), the report pacing (`after_publish`, `CompletionBackoff`, `completion_step`) and the LoRa timeline (`LorawanTimeline::next_event`, `classify_response`, `requested_deadline`).
- [x] ESP-NOW cancellation, late callbacks, queue saturation, deadlines and shutdown are host-tested on the `juggler` side, including the IDF worker loop now in `juggler::espnow::worker` (see the ESP-NOW safety table).
- [ ] ESP-NOW tier adapters are host-tested: the IDF adapter `worker.rs` (now an `IdfRadio` port with error mapping) and the hal `driver.rs` have no executed tests.
- [x] The helpers `juggler::espnow::scan::restore_matches` and `warn_on_restore_mismatch` exist, and `restore_matches` has three host tests.
- [x] `restore_matches` is wired into both tiers' restore paths through `warn_on_restore_mismatch`: the IDF worker in `juggler::espnow::worker`, and the hal driver in `restore_scan_power`.
  The FFI read-back is compiled but not run on hardware.
- [x] Compile checks pass for the chips named in Scope and evidence basis.
- [x] Every example in `build-all-examples` builds: 38 of 38 (2026-11-05).
- [x] The LoRa examples `idf_esp32s3_join`, `idf_esp32s3_lora_uplink` and `hal_esp32s3_join` build.
- [ ] The esp-hal Wi-Fi adapter branches (`link.rs:267`, `276` and `385`) have a host test, because `WifiLink` is cfg-gated out of `just test-hal`.
- [x] Every flow-map row in the runbook names a gate or is marked deferred; whether a named gate runs its flow is a hardware fact, tracked in the hardware closure and the numbered gaps.
- [x] The hal ESP-NOW `shutdown` has the same name and signature as the IDF `shutdown`, and `EspNowLink::shutdown` is implemented by both tiers and the mock.
  Compile-time guards on both tiers and the juggler test `shutdown_is_idempotent_through_the_trait` cover it.
- [ ] The hal `shutdown` path (scan abort, runner return, refused sends) has no host test, because `EspHalEspNow` needs the radio; gate `c3/espnow-shutdown` (esp-hal) covers it.
- [x] The IDF ESP-NOW worker loop is in `juggler::espnow::worker` behind the six-method `EspNowRadio` port (ADR 023 decision 10), with 26 host tests and mutation checks on the scan abort and the caller deadlines.
- [x] The TX-power read-back is implemented on both tiers, and the juggler worker and scan tests cover the mismatch and the unknown-level paths.
- [x] The flattened health, report and LoRa timeline functions have host tests.
- [ ] The IDF health and reporter loops (`OtaHandle::run_health_policy` and `reporter.rs`) have no host test.

## Hardware closure

Every hardware item of this release passed (or has an N/A waiver); see the results table of [release-0.7.0-hardware-test](../runbooks/release-0.7.0-hardware-test.md#results).

- [ ] LoRa (deferred out of 0.7.0): IDF OTAA join and uplink on the Heltec V3 and esp-hal SX1262 bring-up; bug 007 still needs the critical-section provider variant.

## Domain flow tables

Each table lists the flow, its tiers, its placement, its contract and its evidence.
Evidence names the host tests by module (the test names are in the code), plus the gaps and any bench check the runbook flow map does not already map to the flow.

### Wi-Fi

<details>
<summary>Wi-Fi flow table (10 flows)</summary>

| Flow | Tiers | Placement | Contract | Evidence |
|:-----|:------|:----------|:---------|:---------|
| Station connect and reconnect with back-off, refused connect as `ConnectFailed`, adopt-at-boot, late-connect adoption | both | `LinkMachine::poll` (rung 3), run by `LinkSupervisor<IdfRadio>` (IDF) and `LinkHandover` in `WifiLink::run` (hal) | Out-of-phase input is `Ignored`; IDF blocks on `recv_timeout` to the back-off deadline; hal waits `Wait{ms}`, polls every 500 ms or 1 s, and has no library connect timeout; back-off 1 s to 30 s | `wifi/link.rs`, `wifi/handover.rs`, `backoff.rs` tests; hal C3 reconnect `c3/mqtt-lwt`; IDF C3 reconnect gap 4; adopt-at-boot gap 75 |
| Suspend and resume: park, readiness falls before the stop, fresh back-off on resume, late association taken down, `is_suspended` | both | `LinkMachine` Suspend and Resume, `LinkHandover`; IDF `LinkSupervisor::suspend` under the core lock; hal `WifiLink::suspend` through a `Signal` | Duplicate inputs are `Ignored`; hal disconnect gives up after 500 ms and `is_suspended` still turns true; IDF stop fence has no timeout; IDF stops the radio, hal only disconnects | `wifi/link.rs`, `wifi/handover.rs`, `wifi/supervisor.rs` tests; resume-before-disconnect and the parked re-check gap 75 |
| Link readiness: `NetworkUp` and `NetworkDown` edges, readiness once per transition, `wait_ready` and `wait_connected` | both | `LinkMachine::is_ready`, `LinkHandover::take_ready_change` (rung 3); `WaitList` (IDF), `WakerSlots` and `poll_ready` (hal) | Edges outside `Connected` are `Ignored`; hal sees the falling edge up to 500 ms late; hal wakes four waiters, the rest re-check every 500 ms | `wifi/link.rs`, `wifi/supervisor.rs`, `waker_slots.rs` tests |
| IDF std runner `LinkSupervisor<R>`: worker thread, stop fence, event-to-input mapping | idf | `LinkSupervisor` (juggler `std`) over the `LinkRadio` port; `IdfRadio` and the `LinkWorker` subscriptions in the adapter | Events are dropped while the fence is up; `STEP_ROUNDS` = 2 per input, and a step that does not settle pauses for 30 s (no test); the blocking `recv` has no timeout and relies on the driver event contract | `wifi/supervisor.rs` tests; the event mapping has no host test; `s3/soak`, `s3/mqtt-scenarios`, `s3/suspend` |
| `NetworkLink` tier-parity surface | both | trait `juggler::wifi::network_link::NetworkLink`, implemented by IDF `WiFiManager` and hal `WifiLink`; compile-time parity asserts in both tiers | Error type differs (`anyhow::Error` against `Infallible`); `wait_connected` is unbounded on both tiers; the inherent APIs diverge (IDF `is_connected` returns `anyhow::Result<bool>`, hal returns `bool`) | `mqtt/client.rs` tests |
| Disconnect-reason vocabulary and credentials hint | both | pure `wifi_disconnect_reason_name`, `reason_suggests_credentials`, `credentials_hint`; hal re-matches esp-radio variants (`hal_disconnect_hint`, `connect_failure_hint`) | Flat logic; the hal copy of the four-reason set is not guarded against drift | `wifi/mod.rs` tests; no hal host test; gap 5 |
| Station and AP config validation, and the `ConnectMode` timeout | both (validation); idf (`ConnectMode`) | pure `validate_ssid`, `validate_password`, `validate_ap_config`, `ConnectMode::effective_timeout_secs` | Stateless; the IDF blocking loops are bounded by `timeout_secs`; hal ignores `ConnectMode` | `wifi/mod.rs` tests; gap 6 |
| TX power and power-save configuration | both | pure `TxPowerLevel::to_quarter_dbm` and `WifiPowerSave`; radio calls in each adapter | hal applies TX power only when it differs from the default, IDF always applies it | `wifi/mod.rs` tests; the Wi-Fi TX power is not read back (gap 11) |
| Station AP selection: which AP of the SSID the station joins | both | pure `ApSelection` on `WiFiConfig` (`Strongest` default, `FirstFound`); each adapter maps it with an exhaustive `map_ap_selection` (IDF `CompleteScan(Signal)` / `FastScan`, hal `AllChannels` / `Fast`) | Neither tier takes the scan from its driver default; esp-radio's default fast scan joined a -89 dBm mesh node next to a -20 dBm AP (bench 2026-11-07) | `wifi/mod.rs` tests; the adapter mappings are not host-tested (driver types); `s3/mqtt-lwt` (gap 78) |
| SoftAP bring-up: AP config, static 192.168.4.1, open-AP warning | both | adapter-only, with pure `validate_ap_config` and `ApConfig` | The AP address is a literal in IDF and `AP_IP` in hal; only hal warns about an open AP | `wifi/mod.rs` tests; also `c6/provision`, `s3/provision`; no IDF SoftAP gate on C6 or S3 (gap 43) |

</details>

### MQTT

<details>
<summary>MQTT flow table (12 flows)</summary>

| Flow | Tiers | Placement | Contract | Evidence |
|:-----|:------|:----------|:---------|:---------|
| Session lifecycle: network-gated connect, retry pause, close on network loss, state and `connect_count` | both | `SessionMachine::poll` (rung 3), run by the IDF `mqtt-net` thread and the hal async `serve` | `Ignored` is explicit; `Shutdown` closes a busy session (IDF only, the hal runner never returns from `run`); IDF counts `Ignored` and logs an error at 16 in a row; hal restarts; IDF waits at most 500 ms per tick; hal samples the link every 500 ms and bounds connect by `connection_timeout` | `mqtt/session.rs` tests; hal `mqtt/mod.rs` tests; no test drives either tier loop |
| Network gate: link readiness to exactly one `NetworkUp` or `NetworkDown` | both | `NetworkGate::feed` (rung 2) | No `Ignored` variant; a repeated observation returns `None`; a false reading before the first true reports nothing | `mqtt/gate.rs` tests; the IDF seed race has no host test |
| Connect work order after CONNACK: subscriptions, startup message, `on_connect`, then `Ready`; stale-token guard | both | `ConnectWorkMachine` (rung 3), run by the IDF `mqtt-net` thread one step per turn and by the hal `serve` inline; `ConnectPort`/`perform` is the IDF mapping | `WorkDone` leads to `Ready`, `WorkFailed` leads to `Close`; a stale token is `Ignored`; stop at the first failed SUBSCRIBE, startup publish or runtime catch-up (a failing `on_connect` is logged, never fatal), `Finish` before `WorkDone`, `Abandon` on a session that ended | `mqtt/session.rs`, `mqtt/connect_work.rs` tests; the order has no bench criterion (gap 70) |
| Publish gate: `try_publish` refuses with `WouldBlock` unless the session is `Ready`; the `MqttClient` trait | both | `SessionMachine::can_publish` and the trait defaults in `juggler::mqtt::MqttClient` | Stateless check; both tiers return `WouldBlock` when the session is not ready | `mqtt/client.rs` tests; the MQTT smoke checks; gap 46 |
| Async publish admission (`publish` waits, never evicts) and the pre-client queue | both | IDF `Outbox<C>` (`std`); hal `publish_q` channel in the adapter | IDF `publish_waiting` never evicts; the IDF sync path drops and counts the oldest; hal drops after three attempts and counts; depth 4 on both tiers | `mqtt/outbox.rs` tests; the hal drop policy has no host test; gap 47 |
| `wait_connected` wake-up on `SessionAction::Ready` | both | `WaitList` (IDF) and `WakerSlots` with `poll_ready` (hal) | `Ready` is emitted once per accepted `WorkDone`; hal re-checks every 500 ms past four slots | `wait_list.rs`, `waker_slots.rs` tests |
| Inbound receive queue, drop-oldest with a dropped counter | both | adapter-only; the same policy is copied in each tier | Depth differs (IDF 8, hal 4); IDF bypasses the queue when `on_message` is set | hal `mqtt/mod.rs` tests only; gap 49 |
| Inbound chunk reassembly (esp-mqtt splits large messages) | idf | pure `ChunkAssembler::feed` (`std`) | `Assembled::{Message, Pending, TooLarge, OutOfOrder}`; cap 16 KiB; a malformed chunk is dropped without a count | `mqtt/chunks.rs` tests; gap 19 |
| Configuration and topic validation | both | pure `validate_client_id`, `validate_publish_topic`, `validate_subscribe_filter`, `validate_broker_port`; IDF also `validate_broker_host`; hal accepts an IPv4 literal or a DNS name through `classify_broker_host` | Stateless; hal resolves a name on every connect (`Stack::dns_query`, bounded by the connection timeout) | `mqtt/mod.rs` tests; juggler `mqtt/host.rs` tests |
| IDF disconnect notification: `on_disconnect` and `PendingAcks::fail_all`, deduplicated by `next_state` | idf | pure `next_state` and std `PendingAcks`, used by the adapter event loop | A second state tracker lives beside `SessionMachine`, and the two can disagree | `mqtt/mod.rs` tests; gap 48 |
| IDF callback-thread guard (ADR 017) | idf | `CallbackScope` and `in_callback` in `juggler::mqtt` | Returns `PublishAckError::WrongThread` instead of deadlocking on `api_lock` | `mqtt/mod.rs` tests; gap 66 |
| IDF legacy connection wait (deprecated `MqttManager`) | idf | pure `connection_wait_iterations` | Finite; outside the `SessionMachine` path | `mqtt/mod.rs` tests; gap 37 |

</details>

### ESP-NOW

<details>
<summary>ESP-NOW flow table (9 flows)</summary>

| Flow | Tiers | Placement | Contract | Evidence |
|:-----|:------|:----------|:---------|:---------|
| Acked and unacked send queue (`send_acked`, `send`, `send_and_wait`, scan probes): admission, deadlines, cancellation, preemption by a scan, completion ownership, shutdown | both | `TxCore<R, N>` (rung 3); IDF `espnow-tx` thread (`juggler::espnow::EspNowWorker`, internal `Shared::run`); hal `EspHalEspNow::run_sender` | No `Ignored` variant; a stale or dead id answers `Completed::Unknown` or `Completion::Unknown`; shutdown refuses new requests; IDF idle wait is event-driven, bounded by the earliest request deadline, and unbounded when idle (ADR 023 decision 6 exception); a frame already on the air is waited out with no bound (ADR 020 fail-closed exception); the hal `run_sender` idle wait has no deadline while the queue is empty (ADR 023 decision 6 exception) | `espnow/tx.rs`, juggler `espnow/worker.rs` tests; the IDF adapter `worker.rs` and hal `driver.rs` have no executed tests; hal sync gap 68 |
| Channel scan `scan_for_peer`: stale-peer removal, temporary peer, TX-power boost, probes and confirmations, burst budget, restore, fallback to the pinned channel | both | `ScanMachine::poll_bounded` (rung 3); IDF `juggler::espnow::EspNowWorker` (internal `Shared::drive_scan`); hal `EspHalEspNow::scan_for_peer` | After `Done` every input is `Ignored`; a mid-scan out-of-phase input fails closed through teardown; `poll_bounded` gives up after `MAX_POLLS`; `WaitUntil` is interruptible on both tiers | `espnow/scan.rs` tests; the after-`Done` exit has no worker test (gap 76); no C6 hal scan gate |
| Scan cancellation and dropped-future teardown: `ScanTeardown` with `MAX_POLLS` 7, `abort_teardown`, `ScanGate::cancel`, `cancel_running` and `on_probe_complete` | both | `ScanTeardown`, `ScanMachine::abort_teardown`, `ScanGate`; IDF `ScanTicket` drop; hal `ScanGuard` drop | A finished teardown answers `Ignored`; hal's teardown loop is bounded by `MAX_POLLS` and logs an error; an IDF drop mid-scan only asks the worker to cancel, and the worker restores before the exclusion ends (the restore is not skipped); the deferred teardown waits on the on-air probe | `espnow/scan.rs`, `espnow/scan_job.rs`, `espnow/worker.rs` tests; no tier drop test |
| Scan exclusion and job hand-off: admission that claims the radio, preempts queued app frames, and epoch-guarded finish, release and take | both | rung 3 `ScanGate` (ADR 023 decision 1), with the pure helper `ScanJobSlot`, plus `TxCore::begin_scan` and `end_scan` | A stale epoch changes nothing; the waits happen in the tier (IDF `Condvar`, hal `idle_waiters`) | `espnow/scan_job.rs`, `espnow/tx.rs` tests; gap 69 |
| `EspNowLink` surface and send classification (`send_acked`, `receive`, `scan_for_peer`, `pinned_channel`; `ClassifySend`, `SendOutcome::of`) | both | pure trait `EspNowLink`, `ClassifySend` and `SendOutcome::of` in `juggler`; implemented by `EspIdfEspNow` and `EspHalEspNow` | Only `NoAck` counts against the peer; everything else is `Local` | `espnow/link_trait.rs` tests (mock only); hal `espnow/mod.rs` tests; parity is compile-time only (gap 52) |
| Peer-link supervision: when to rescan for the coordinator (`PeerLink`) | both | `PeerLink::poll` (rung 3), driven by `juggler::espnow::PeerSession` in the shared peer app on both tiers (since 2026-11-11) | Rejected events answer `Ignored`; retry backoff from 2 s to 60 s; `PeerSession::run` sleeps until the earlier of `WaitUntil` and the next send result or received frame (no fixed tick) | `espnow/link.rs` tests; `espnow/session.rs` tests on `MockEspNowLink`; `examples-shared/espnow_peer_app.rs` runs `PeerSession`, while the IDF scouts still hand-roll relink policy |
| Receive path: oversize drop-and-count, queue overflow, `try_recv` and async `receive` delivering each frame once | both | pure `EspNowEvent::try_new` and `OversizeLog`; IDF `RxQueue` (drops the newest at capacity 32); hal esp-radio queue with `receive_gate` | `try_recv` is bounded by `RX_SKIP_LIMIT` = 16; the receive waits are unbounded and event-driven; the capacity differs between tiers | `espnow/mod.rs` tests; hal `espnow/cell.rs` tests; gap 51 |
| Peer registration: `add_peer` and `remove_peer` from `PeerConfig` (channel 0 means current, interface, encryption) | both | adapter-only, with the value type `PeerConfig` and the trait `EspNowDriver` in `juggler`; hal has pure `plan_peer` and `existing_peer_is_idempotent` | hal refuses `encrypt = true` with `EncryptionUnsupported`; IDF passes it to `esp_now_add_peer`; hal treats `PeerExists` as idempotent only for broadcast on STA | hal and `juggler` `espnow/mod.rs` tests; only through the scan check `c3/espnow-soak` |
| Pure command codec and `PeerTracker` heartbeat | neither | pure `parse_frame`, `parse_system_command`, `identify_response`, `PeerTracker` | No consumer in any tier or example | `espnow/tracker.rs`, `espnow/command.rs` tests; gap 50 |

</details>

### OTA

<details>
<summary>OTA flow table (14 flows)</summary>

| Flow | Tiers | Placement | Contract | Evidence |
|:-----|:------|:----------|:---------|:---------|
| Firmware download `fetch_and_apply`: connect, Content-Length admission, prepare, streamed read and write, SHA-256, activation permit, activate | both | `DownloadMachine` (rung 3), driven by the IDF blocking thread and the hal async `EspHalOtaManager` | `Ignored` leaves the state unchanged; `poll_bounded` gives `DownloadFailed` after `2*len+8` polls; every error after Prepare aborts, then `Done`; the wall-clock bound is the optional `Deadline`; both tiers clip the `Connect` and `Read` waits to the remaining deadline (IDF since the 2026-11-06 review fix), but DNS inside the IDF connect is unbounded | `ota/download.rs` tests; no tier loop has a test; hal error cases gap 77 |
| Content-Length admission: missing, chunked, zero, larger than the partition | both | pure `check_content_length`, applied only inside `DownloadMachine` on `Headers` | Stateless | `ota/download.rs` tests; hal `ota/http.rs` tests; gap 13 |
| Cooperative deadline and activation permit | both | pure `Deadline` and `ActivationPermit` | Only a successful pre-activation check issues a permit | `ota/deadline.rs` tests |
| Erase-once aligned flash writes (`ChunkAligner`, bug 004) | hal | pure `ChunkAligner` in `juggler`, used by hal `manager.rs` | `AlignError` on a gap, a wrong final length or a driver error; the encrypted read-modify-write path is a hardware fact | `ota/write_align.rs` tests; `c3/ota-rollback`, `s3/ota-rollback`; encrypted branch gap 53 |
| Image integrity and version decision: `StreamingVerifier`, `hex_to_bytes`, `ImageMetadata`, `Version`, `decide_update` | both | pure, in `juggler::ota` | Stateless | `ota/verifier.rs`, `ota/decision.rs` tests; hal bad digest gap 54 |
| Read-error classification: timeout against other, `DownloadTimeout` against `ServerUnreachable` | both | pure `classify_read_error` | Stateless | `ota/error.rs` tests; the IDF `downloader.rs` tests are compiled by clippy and never run |
| Slot activation and image state: `activate`, `mark_valid`, `rollback`, `selected_slot` | both | adapter-only in hal, through the pure `plan_activation` and `encode_entry` in `ota/activation.rs` over the `OtadataSectors` port | IDF `rollback` reboots and returns only on failure; hal `rollback` returns `Ok`, and the caller must reset; `selected_slot` exists only on hal | hal `ota/activation.rs` tests; the rollback arming changed in code (2026-11-06), checked by the unhealthy-image step of `c3/ota-rollback` |
| Boot reconciliation and persisted OTA records: `reconcile_boot`, `reconcile`, `OtaStore` crash consistency, boot-time rollback | idf (hal re-exports, no `OtaKv` backend) | generic `OtaStore<K: OtaKv>` over a narrow custom port; IDF `EspNvsKv` | Fail-closed: a read error or corrupt record means no writes and closed admission | `ota/reconcile.rs`, `ota/persist/tests/fail_closed.rs`, `ota/persist/tests/crash_matrix.rs`, `ota/runtime/worker.rs` tests; `c3/ota-runtime` boot scenarios P1, P18, P19 |
| MQTT command intake in `on_message`: parse, closed and busy claim, `try_send`, rejection queue | idf | pure `decide_intake` (`ota/runtime/intake.rs`) | Non-blocking under the esp-mqtt `api_lock` (ADR 017); the adapter only executes the actions | `ota/runtime/intake.rs` tests; `c3/ota-runtime` P4 and P12 |
| Worker command execution: update gates, offer, manifest fetch, `begin_attempt`, download, swap, restart, offer retention, failed-download settle, operator rollback, repair | idf | pure `plan_offer`, `plan_update_gate`, `plan_rollback`, `plan_repair`, `retained_due` and `store_ops` (`juggler::ota::runtime`); the step order is in the adapter | The worker thread blocks on `recv` with no timeout while idle; a retained offer waits `retained_poll`; the adapter restarts after a failed `mark_boot_selected` and keeps a rollback armed on `Ok` | `ota/runtime/worker.rs`, `ota/runtime/store_ops.rs` tests; the adapter step order has no test; `c3/ota-runtime` P2, P3, P5 to P9, P12, P15, P16, P20 |
| Manifest fetch size and total-time limits | idf | pure `ManifestBody` (`ota/runtime/fetch.rs`), driven by `runtime/manifest.rs` | Each read returns `Continue`, `Done`, `TooLarge` or `DeadlineExceeded`; worst case is total plus one per-op timeout | `ota/runtime/fetch.rs`, `ota/wire/manifest.rs` tests; `c3/ota-runtime` P11 |
| Post-update health policy: dwell, predicate, `mark_valid`, deadline rollback or `NoteAndRestart` | idf | pure `poll_decision`, `after_unhealthy`, `deadline_plan` and `SlotReads` (`ota/runtime/health.rs`), driven by the sequential IDF `OtaHandle::run_health_policy`; flattened on 2026-11-05, and the deprecated `HealthMachine` wraps the same functions | `poll_decision` evaluates only after the dwell and without a refusal; `after_unhealthy` waits for the deadline; reads give up only after the deadline and enough reads; there is no time bound and no `Ignored` stop any more | `ota/runtime/health.rs` tests and the old machine tests; `c3/ota-runtime` P2, P13, P14, P16, P19; no hal health policy |
| `rolled_back` report delivery and completion retry | idf | pure `ReportPace`, `after_publish`, `CompletionBackoff` and `completion_step` (`ota/runtime/report.rs`), driven by the sequential IDF reporter thread (`reporter.rs`); flattened on 2026-11-05, and the deprecated `ReporterMachine` and `CompletionRetry` wrap the same functions | Only an acked publish marks a report delivered; a failed publish keeps the follow-up count, and a finished pass resets it; the immediate follow-up passes are capped by `max_other_passes`; the wait between passes is bounded by `report_retry`; the completion backoff doubles to a cap; the enqueue inside `publish_acked` is not bounded by this code | `ota/runtime/report.rs` tests and the old machine tests; `c3/ota-runtime` P7, P17, P21 |
| `OtaState` linear progression | neither | pure `OtaState::next_state` | Dead: no tier calls it, and its order (Downloading, Verifying, Writing) contradicts `DownloadMachine`, which writes while streaming | `ota/state.rs` tests; gap 25 |

</details>

### Provisioning

<details>
<summary>Provisioning flow table (13 flows)</summary>

| Flow | Tiers | Placement | Contract | Evidence |
|:-----|:------|:----------|:---------|:---------|
| Captive-portal mutating requests: `POST /save` (nonce, validate, persist, commit or persist-fail-and-retry) and `POST /factory-reset` | both | `PortalMachine::poll` (rung 3) over `ProvisioningState::apply`; IDF through `SessionState::step` in httpd handlers; hal through `SharedState::poll` in `dispatch_request` | Out-of-phase input is `Refused` with state unchanged; a bad nonce is `Forbidden`; `Committed` and `FactoryResetPending` absorb input; both tiers map `Refused` to 409; each request is one poll, so there is no loop bound | `provisioning/portal_flow.rs`, `provisioning/session.rs` tests; the tier mapping (409, 403, 500) has no test; gap 12 |
| Session waiter: `wait_committed` and `wait_outcome`, commit payload publication and terminal outcome | both | IDF `SessionState` and pure `resolve_wait`; hal `Signal`-based `SharedState` in the adapter | IDF has an optional timeout and `wait_committed` takes the payload once; hal has no timeout, and `HostAborted` is never signalled; `Signal::wait` has one consumer | `provisioning/session.rs`, `provisioning/state.rs` tests; no hal host test; portal timeout gap 55 |
| Portal start-up plan: profile check, SSID (override or MAC-derived), AP password, channel and AP config, device name, client-ID override warning | both | pure `plan_portal` and `PortalPlan`, with `derive_softap_ssid`, `resolve_softap_ssid` and `derive_device_name` | Fixed order: profile, then SSID, then password; the tier supplies the MAC and the supported-profile predicate | `provisioning/portal.rs`, `provisioning/ssid.rs` tests; other SSID and AP variants gap 73 |
| Per-session nonce: generation and constant-time check of the `_nonce` field | both | pure `format_nonce`, `nonce_matches`, `body_nonce_matches`, `form_field_raw` | A mismatch becomes `nonce_ok = false`, and the machine answers `Forbidden` | `provisioning/portal.rs` tests; wrong nonce gap 12 |
| Form parsing and per-field validation (both schema profiles) | both | pure `parse_form`, `SchemaProfile`, `FieldErrors` | Only `WifiMqttDevice` is reachable on hal; the LoRaWAN form is IDF-only | `provisioning/form.rs` tests (64); invalid form gap 56 |
| Form pre-fill and page rendering (no secrets in the pre-fill) | both | pure `Prefill::select`, `compose_mqtt_uri`, `templates`, `html_json_escape`; placeholder substitution and the error block are duplicated per tier | A store-load error counts as no record; error blocks and persist-failure banners are built separately and already differ in labels | `provisioning/portal.rs`, `provisioning/templates.rs`, `provisioning/html_json_escape.rs` tests; IDF render tests compile only; hal prefill gap 57 |
| Credential store: save, load, `is_provisioned`, `erase_all`, torn-write guard and schema migration | both | rung 1 `ProvisioningStore<F: NorFlash>` in `juggler` (hal); IDF NVS store over `KvProvisioningStore` | Two-sector sequence and CRC recovery; the write order is values, index, profile, `schema_ver` last; hal reads are word-aligned | `provisioning/flash_store/mod.rs`, `provisioning/flash_store/record.rs`, `provisioning/kv_store.rs` tests; hal factory reset and `PersistFailed` gaps 58 and 12 |
| Captive DNS catch-all | both | pure codec `decode_query` and `encode_response`; tier loops in `dns.rs` (IDF) and `dns_catchall.rs` (hal) | Malformed packets are skipped; IDF has a 500 ms receive timeout and a joined thread; hal has no bound and no shutdown | `provisioning/dns.rs` tests; no gate uses DNS; OS probe auto-open gap 12 |
| Portal HTTP serving loop: accept, read, dispatch, respond, close | both | adapter-only: IDF httpd (`EspHttpServer`); hal HTTP/1.1 codec and accept loop `run_portal_dyn` | hal bounds every phase since the 2026-11-06 F4 fix (header read 5 s); IDF relies on httpd defaults | hal `provisioning/portal.rs` tests; no IDF stall gate (gap 59); hal idle connection gap 27 |
| Boot re-entry gate: ask the consumer hook once, open the store only for `None` | idf (hal re-exports `gate_reentry`, no boot helper) | pure `gate_reentry` (`provisioning/reentry.rs`), used by IDF `WifiMqttBoot::load_with_reentry` | Pure; the library never erases or restarts | `provisioning/reentry.rs` tests; hal has no boot helper, recorded as an ADR 022 gap |
| Boot classification of the stored record and portal outcome | idf | pure `classify_wifi_mqtt` and `portal_outcome` (`provisioning/boot.rs`) | `portal_outcome` swallows a shutdown error only after a commit; the MQTT config mapping is IDF-only | `provisioning/boot.rs` tests; timeout branch gap 55 |
| SoftAP station join and leave events mapped to `ProvisioningEvent` | both | adapter-only event mapping | IDF emits `ClientConnected` without a MAC and never reports a disconnect; hal emits both with a MAC | hal `provisioning/session.rs` tests; no IDF mapping test; gap 60 |
| SoftAP DHCP server | hal | adapter-only (`provisioning/dhcp.rs`) | Persistent; the run loop is `-> !`, so it has no shutdown; pure `decide_request` and `LeaseTable` | `provisioning/dhcp.rs` tests; a side effect of C3 hal provisioning extras (manual), `c6/provision` and `s3/provision`; multi-client leases gap 61 |

</details>

### LoRa

<details>
<summary>LoRa flow table (10 flows)</summary>

| Flow | Tiers | Placement | Contract | Evidence |
|:-----|:------|:----------|:---------|:---------|
| SX1262 bring-up order: reset, TCXO, `ClearDeviceErrors`, calibrate, sync word, each step followed by a bounded BUSY wait | both by code path (IDF `EspIdfLoraRadio::new`; hal only through the example `hal_esp32s3_join`) | rung 1: `juggler::lora::sx1262::init`, generic over `embedded-hal` traits and `sx126x 0.3`; `BRING_UP_ORDER` | Terminal result is `Ok` or `InitError::{Command, BusyTimeout, CalibrationFailed}`; each BUSY wait is at most `DEFAULT_BUSY_POLL_BOUND` = 500 polls of 1 ms; the hal library driver does not call it | `lora/sx1262.rs` tests; `CalibrationFailed` and the SPI-error branch are untested; gap 22 (bug 007) |
| `LorawanTimeline` event tick: decode IRQ and clock samples into `Phy`, `TimeoutFired` or `BudgetExceeded`; classify responses | idf (examples only); hal re-exports it, with no loop | flat logic (ADR 023 decision 3): pure `LorawanTimeline::next_event`, `classify_response` and `requested_deadline`, flattened from the never-released `LorawanTicker` on 2026-11-05; the caller keeps the pending deadline | `None` means nothing to deliver; `BudgetExceeded` is checked first; `TimeoutRequest` is absolute; `DownlinkReceived` is terminal; the caller sleeps 10 ms per tick; the timeline has no phase input, so a stale IRQ edge can reach `lorawan-device` out of phase (PLAUSIBLE, not reproduced) | `lora/ticker.rs` tests; no host test drives `nb_device::Device` with the ticker; gaps 22 and 24 |
| `LorawanPhy` bridge between `lorawan-device` `PhyRxTx` and the `LoraRadio` port | both by type; functionally IDF | generic bridge `juggler::lora::LorawanPhy` (not a machine) | Out-of-phase `Phy` while idle is `PhyOutcome::Ignored`, counted in `ignored_events()`; `CancelRx` is best effort; `StillTxing` maps to `Txing`, which `lorawan-device` 0.12.2 does not accept during `SendingData`, so `phy_due` withholds such events (gap 23) | `lora/phy.rs` tests; no test for `StillTxing`; hal parity guard; gaps 22 and 23 |
| TX and RX command sequences and the IRQ read-classify-clear cycle (`LoraRadio::prepare_tx`, `transmit`, `prepare_rx`, `receive`, `cancel_rx`, `irq_pending`) | idf (hal is a stub that returns errors) | adapter-only `EspIdfLoraRadio` implementing `LoraRadio`, using pure `classify_irq` and `irq_flags` | TX timeout is a literal 6000 ms; RX window is 3000 ms; a CRC error window is a hard `ReceiveFailed`, not a closed window; out-of-phase IRQ words return `WouldBlock`; `transmit` and `receive` clear only the IRQ flags they read (`transmit` via `take_irq_status`, `receive` inline via `irq_read_mask`); `cancel_rx` clears all | pure inputs only: `lora/mod.rs` tests; the command sequences are not host-testable; gap 22 |
| Deprecated IDF `irq_pending` and `cancel_rx` inherent forwarders | idf | adapter-only (`sx1262_driver.rs`) | Thin forwarders; a failed IRQ read becomes `false` | none; gap 22 |
| Pure LoRa rules: `classify_irq`, `ldro_required`, the LoRaWAN PHY policy constants, `map_rf_config`, RX window defaults | both (re-export) | pure in `juggler::lora` | Stateless | `lora/mod.rs`, `lora/phy.rs`, hal `lora/mod.rs` tests; RX window timing was bench-tuned on 2026-06-17 and not re-checked (gap 22) |
| LoRa configuration: `Region` RF and calibration bands, `LoraConfig::from_hex_strings`, `HeltecV3Pins` | both | pure in `juggler::lora::config` | `None` on bad length or non-hex digits; the examples fail fast | `lora/config.rs` tests; the MSB-to-LSB EUI reversal is duplicated in the examples; US915 gap 62 |
| LoRaWAN downlink OTA command protocol (port 10 parse, version report encode) | neither in use | pure `parse_ota_command` and `encode_version_report` | `None` for unknown or truncated payloads | `lora/commands.rs` tests; no consumer; gap 63 |
| Deprecated `LorawanDevice<R>` stub and `LorawanSessionData` | both (re-export) | pure stub in `juggler::lora::lorawan` | `process()` returns `NoUpdate`; session persistence is not implemented, and the CRC is always 0 | `lora/lorawan.rs` tests; N/A: no runtime flow, deprecated stub |
| Status LED palette (boot, connecting, connected, error, offline) | idf only | pure constants in `juggler::status_colors` | Constants only; the hal tier does not use them | none (constants); used by IDF Wi-Fi and MQTT, not by LoRa; gap 6 |

</details>

## Overshoot and remaining adapter decisions

These items are logic in an adapter or an overshoot, which ADR 023 decision 7 or decision 2 would move or justify.

Overshoot items:
- Resolved 2026-11-05: `HealthMachine`, `ReporterMachine` and `CompletionRetry` are flattened into pure functions on the IDF tier and kept as deprecated wrappers, so ADR 023 decision 1 no longer lists them at rung 3.
- `juggler::ota::OtaState::next_state` is dead code whose order contradicts `DownloadMachine`, so it should be deprecated.
- Resolved 2026-11-11: `juggler::espnow::PeerLink` is driven by `PeerSession` in both C3 peers; the IDF scouts still hand-roll relink policy.

Wi-Fi adapter decisions:
- `WifiLink::run` skips a connect when the controller is already connected, and it swallows handed-over actions while a connect is in flight (`crates/rustyfarian-esp-hal-network/src/wifi/link.rs`, lines 267 to 281).
- `WifiLink::run` restarts the station after one idle second with nothing handed over, which the comment calls unreachable (lines 385 to 392).
- `WifiLink::end_park` runs after a disconnect timeout, so `is_suspended` turns true after a 500 ms give-up (lines 298 to 313).
- The parked loop synthesizes `Connected` by polling `is_connected` every second (lines 372 to 383).
- The IDF `supervisor.rs` not-settled 30 s pause is placed in `juggler`, but only the IDF tier uses it (lines 467 to 476).
- The inherent `is_connected` and `wait_connected` APIs differ between tiers, which ADR 022 disfavours.

MQTT adapter decisions:
- The out-of-step policy differs: the IDF runner counts `Ignored` and logs at 16 (`Runner::act`, `IGNORED_IN_A_ROW_LIMIT` in `crates/rustyfarian-esp-idf-network/src/mqtt/runner.rs`), while the hal runner restarts (the `SessionAction::Ignored` arm of `Runner::run` in `crates/rustyfarian-esp-hal-network/src/mqtt/runner.rs`).
- The hal runner feeds `Disconnected` before it closes the session on a finished `serve`, and the IDF runner reports `Disconnected` itself when a `Close` arrives with no client (lines 323 to 327).
- Resolved 2026-11-11 (implementation only, no hardware evidence): the stop-at-first-failure connect-work loop is one host-tested rung-3 machine, `juggler::mqtt::ConnectWorkMachine`.
  Placement: the IDF `mqtt-net` runner thread performs it one step per turn through `ConnectPort` and `perform`, and the hal `serve` drives it between awaits; the plan type `ConnectWorkPlan` stays in `session.rs`.
  Contract: stop at the first failed SUBSCRIBE, startup publish or runtime catch-up (a failing `on_connect` is logged, never fatal), `Finish` before `WorkDone`, and `Abandon` on a session that ended, with one host-tested home.
  Evidence: the `connect_work` tests `a_full_plan_steps_in_order_then_finishes_then_reports_done`, `an_empty_plan_still_finishes_before_reporting_done`, `a_failed_subscribe_reports_failed_and_runs_nothing_more` and `session_ended_between_steps_abandons_without_a_report`, and the scripted tests `a_full_connect_work_drives_the_port_in_plan_order_and_reaches_ready` and `connect_work_with_interleaved_inputs_settles_within_a_bounded_number_of_polls`, plus `a_scripted_tier_loop_settles_within_a_bounded_number_of_polls` in `session.rs`.
- The hal `publish_queued` retry and drop policy (three attempts, then drop and count) is untested.
- The inbound drop-oldest policy is copied in both adapters with depths 8 and 4.

ESP-NOW adapter decisions:
- The IDF probe loop (`transmit_probe`) moved into `juggler::espnow::EspNowWorker` on 2026-11-05; it still services every frame that `next()` hands out until its own probe returns.
- IDF re-pins the radio to the pinned channel for application frames in one radio mode only (`RadioMode::OwnedStaPromisc`, in `send_frame`).
- The IDF shutdown skips the radio restore on drop, by design (`mod.rs`, lines 762 to 777).
- The shutdown forms differ: IDF `EspIdfEspNow::drop` joins the worker before the `EspNow` singleton drops, while the esp-hal handle has no `Drop`, because every future borrows it, and `run_sender` returns after `shutdown`.
- hal sync sends lease the sender first, so a busy sender fails at once with `Busy`, while IDF routes plain `send` through the worker with a 1 s queue deadline.
- hal `perform_teardown_action` maps `SendProbe`, `WaitUntil`, `Done` and `Ignored` to `ScanInput::Failed` inside a teardown, a defensive mapping that `juggler` does not state.

juggler worker vs hal driver drift:
- The juggler worker (`Shared::drive_scan`, used by IDF) always attempts the TX-power boost and has no already-boosted guard, while hal `driver.rs` skips the boost when one is already in force; the pin update after a scan also differs.
- The juggler worker classifies `transmit_probe` errors by hand, while hal uses `SendOutcome::of`.
- Both tiers now read the level back after the restore and log a mismatch through the shared `warn_on_restore_mismatch`.
- The IDF RX queue drops the newest frame at capacity, while hal relies on esp-radio's fixed queue.

OTA adapter decisions:
- Both tiers clip the connect and read waits to the remaining deadline (IDF since 2026-11-06 through `Deadline::budget_millis`; hal wraps each read in `with_timeout(budget)`).
- The IDF downloader matches `https://` case-sensitively, while hal matches the scheme case-insensitively, and the shared `is_plain_http_url` is `pub(crate)` and unused.
- The `status != 200` mapping is duplicated in both adapters.
- The IDF worker step order is adapter code, it restarts after a failed `mark_boot_selected`, and it keeps a rollback armed on `Ok`.

Provisioning adapter decisions:
- Both tiers parse the form only behind a valid nonce, which is identical but not shared.
- A `/save` body with invalid UTF-8 gets a 400 on hal before the nonce check, while IDF decodes it lossily.
- The event sets differ: `SubmissionAccepted` and `ClientDisconnected` exist only on hal.
- The route tables differ: `/status` is IDF-only, and `GET /factory-reset` is hal-only.
- hal `wait_committed` returns `Err` on a factory reset, while IDF keeps blocking.
- The publish-then-signal order is coded in the hal adapter but inside `SessionState::step` on IDF.
- hal truncates the device name and firmware version to render caps without reporting it.

LoRa adapter decisions:
- Both examples merge the IRQ sources (`DIO1_FLAG.swap(..) || irq_pending()`), an IRQ-source policy repeated in three example loops.
- The ticker timeline is anchored after `join` or `send` returns, while the radio's TxDone timestamp is measured from `tx_start`; the coupling is implicit.
- The uplink continue-or-abort policy lives in the example (`idf_esp32s3_lora_uplink.rs`, the `uplink_done` check at lines 455 to 463).
- The 200 us NRST pulse width is duplicated in the IDF driver and the hal example.

Drift found by this audit (not edited here):
- `crates/rustyfarian-esp-hal-network/src/lib.rs` lines 7 to 9 and 32 say `wifi` and `mqtt` run only on C3, C6 and S3, and that `esp32` means LoRa and OTA only, which contradicts `Cargo.toml` and the `hal_esp32_*` examples.
- ADR 023 decision 8 says the ESP32-C3 TX-power clamp lives in the hal adapter, but the code has no C3-specific clamp.
- `crates/rustyfarian-esp-idf-network/examples/idf_esp32s3_join.rs` says the ESP-IDF edge interrupts re-arm automatically, which contradicts the one-shot behaviour in the loop comment and in CLAUDE.md.

## ESP-NOW safety table

The bench check of each covered item is in the runbook flow map; the hardware column lists only what the map does not.

<details>
<summary>ESP-NOW safety coverage (21 items)</summary>

| Item | Tier | Host tests | Hardware gaps | Status |
|:-----|:-----|:-----------|:--------------|:-------|
| cancellation | juggler | `espnow/scan_job.rs`, `espnow/tx.rs` tests | none | covered |
| cancellation | idf | `espnow/worker.rs` (juggler) tests; the IDF adapter has none executed | none | partial |
| cancellation | hal | `espnow/cell.rs` tests (`driver.rs` has no tests) | none | partial |
| late-callbacks | juggler | `espnow/callback.rs`, `espnow/tx.rs` tests | none | covered |
| late-callbacks | idf | `espnow/worker.rs` (juggler) tests; the IDF adapter has none executed | plain `send` (gap 74); no check provokes a late callback after an abandon (gap 10) | partial |
| late-callbacks | hal | `espnow/mod.rs` tests | steady sends only (gap 10) | partial |
| queue-saturation | juggler | `espnow/tx.rs`, `espnow/scan_job.rs` tests | none | covered |
| queue-saturation | idf | `espnow/worker.rs` (juggler) tests; the IDF adapter has none executed | none | partial |
| queue-saturation | hal | `espnow/mod.rs`, `espnow/cell.rs` tests; the enqueue park-on-Full loop in `driver.rs` is untested | none | partial |
| deadlines | juggler | `espnow/tx.rs`, `espnow/deadlines.rs` tests | none | covered |
| deadlines | idf | `espnow/worker.rs` (juggler) tests; the IDF adapter has none executed | none | partial |
| deadlines | hal | `espnow/mod.rs` tests; `wait_result` and `poll_completion` in `driver.rs` are untested | none | partial |
| scan-restoration | juggler | `espnow/scan.rs` tests | none | covered |
| scan-restoration | idf | `espnow/worker.rs` (juggler) tests; the IDF adapter has none executed | failed-rescan restore (gap 67) | partial |
| scan-restoration | hal | none (`driver.rs` has no tests) | failed-rescan restore (gap 67) | partial |
| shutdown-through-single-worker | juggler | `espnow/tx.rs`, `espnow/scan.rs` tests | none | covered |
| shutdown-through-single-worker | idf | `espnow/worker.rs` (juggler) tests; the IDF adapter has none executed | drop during a scan (gap 9) | partial |
| shutdown-through-single-worker | hal | `espnow/mod.rs` tests; the core's shutdown tests in `espnow/tx.rs` and `espnow/scan.rs` (juggler); the scan-abort and runner-return paths have no host test | none | partial |
| tx-power-restore-readback | juggler | `espnow/scan.rs`, `espnow/worker.rs` tests | none | partial |
| tx-power-restore-readback | idf | `espnow/worker.rs` (juggler) tests; the IDF adapter's read is compiled only | none | partial |
| tx-power-restore-readback | hal | none executed; reads the radio back after the restore and warns through the shared helper; the FFI call is compiled only | none | partial |

</details>

## Matrix: chip, tier and capability

Hardware status per cell is in the runbook chip matrix, cell map and results table; this table keeps the build facts and the reasons for unsupported or N/A cells.
The build status comes only from the evidence in the Scope section.
A Wi-Fi cell counts only the checks of its own cell whose steps switch the AP off and on or pause the router; an MQTT check without such a step is not Wi-Fi evidence.

<details>
<summary>Build and N/A facts (four chips)</summary>

| Chip | Capability | ESP-IDF | esp-hal |
|:-----|:-----------|:--------|:--------|
| ESP32 (WROOM DevKit or Feather V2, 8 MB flash) | wifi, mqtt | build pass (`idf_esp32_mqtt`) | build pass |
| ESP32 | espnow | no example, no gate: the crate is chip-agnostic but not covered (gap 30) | unsupported: `compile_error!` in the hal ESP-NOW module (C3 and C6 only) |
| ESP32 | ota | no example, no gate (gap 39) | no example, no gate (gap 40) |
| ESP32 | provisioning | no example, no gate: needs SoftAP configuration (gap 39) | unsupported: the portal session tasks are chip-gated out, the DHCP, DNS and store modules still compile, and there is no `compile_error!` (gap 31) |
| ESP32-C3 (Super Mini) | wifi | build pass (`idf_c3_connect`, `idf_c3_connect_nonblocking`) | build pass (`hal_c3_connect_async`) |
| ESP32-C3 | mqtt | build pass (`idf_c3_mqtt`, `idf_c3_async_mqtt`, `idf_c3_async_mqtt_native`, `idf_c3_mqtt_callback_guard`, `idf_c3_mqtt_button_oled`, `idf_c3_mqtt_led_grid`, `idf_c3_ota_runtime`, `idf_c3_provision_mqtt`) | check pass (`just check-mqtt-hal-embassy`), build pass (`hal_c3_mqtt_lwt`) |
| ESP32-C3 | espnow | build pass (`idf_c3_espnow_coordinator`, `idf_c3_espnow_peer`, `idf_c3_espnow_scout`, `idf_c3_espnow_scout_promisc`, `idf_c3_espnow_sta_scout`) | check pass (`just check-espnow-hal`), build pass (`hal_c3_espnow_peer`) |
| ESP32-C3 | ota | build pass (`idf_c3_ota_runtime`) | build pass (`hal_c3_ota`) |
| ESP32-C3 | provisioning | build pass (`idf_c3_provision`, `idf_c3_provision_mqtt`) | check pass (`just check-provisioning-hal-embassy`), build pass (`hal_c3_provision_mqtt`) |
| ESP32-C3 | lora | N/A: no SX1262 on the C3 Super Mini, no example | N/A: no SX1262 on the C3 Super Mini, no example |
| ESP32-C6 | wifi | build pass (`idf_c6_connect_nonblocking_rgb`, `idf_c6_mqtt`) | build pass (`hal_c6_connect_async_led`) |
| ESP32-C6 | mqtt | build pass (`idf_c6_mqtt`) | check pass (`just check-mqtt-hal-embassy`), build pass (`hal_c6_mqtt_lwt`) |
| ESP32-C6 | espnow | build pass (`idf_c6_espnow_coordinator`) | check pass (`just check-espnow-hal`), build pass (`hal_c6_espnow_sta_coordinator`) |
| ESP32-C6 | ota | no example, no gate: consumer firmware (gap 39) | no example, no gate (gap 40) |
| ESP32-C6 | provisioning | no example, no gate (gaps 39 and 43) | check pass (`just check-provisioning-hal-embassy`), build pass (`hal_c6_provision_mqtt`) |
| ESP32-C6 | lora | N/A: no SX1262 board, no example | N/A: no SX1262 board, no example |
| ESP32-S3 (CrowPanel, Heltec V3) | wifi | build pass (`idf_esp32s3_mqtt_lwt`); `s3/soak` has no AP step | build pass (`hal_esp32s3_connect_async`, `hal_esp32s3_mqtt_lwt`) |
| ESP32-S3 | mqtt | build pass (`idf_esp32s3_mqtt_lwt`) | build pass (`hal_esp32s3_mqtt_lwt`) |
| ESP32-S3 | espnow | no example, no gate; the runbook matrix omits this cell (gap 30) | unsupported: `compile_error!` in the hal ESP-NOW module (C3 and C6 only) |
| ESP32-S3 | ota | no example, no gate (gap 39) | build pass (`hal_esp32s3_ota`) |
| ESP32-S3 | provisioning | no example, no gate (gaps 39 and 43) | build pass (`hal_esp32s3_provision_mqtt`) |
| ESP32-S3 | lora | build pass (`idf_esp32s3_join`, `idf_esp32s3_lora_uplink`) | build pass (`hal_esp32s3_join`); the hal `LoraRadio` driver is a stub |

</details>

## Remaining gaps and deferrals

Each item names the next step, and no item names an owner.

1. Resolved 2026-11-05: `with_startup_message` on IDF, `wait_suspended` on both tiers, hal runtime subscribe and hal DNS landed; the S3 startup, DNS and `wait_suspended` checks and `c6/dns-subscribe` pass.
2. Resolved 2026-11-11: `PeerSession` (plan Steps 6 and 7) landed, and `c3/espnow-peer-session` verifies the `PeerLink` relink against both C6 coordinators.
3. Resolved 2026-11-11: the Step 8c fix is implemented, and the S3 checks on both tiers and `s3/soak` pass.
4. Half resolved 2026-11-08 (`c3/connect` runs `hal_c3_connect_async` with a router-block reconnect); no IDF C3 connect or reconnect gate exists; next step: add an AP-off connect gate for `idf_c3_connect`.
5. The hal `ConnectFailed` path (wrong password through a `connect_async` error) has no gate; next step: add a wrong-password step on `hal_esp32s3_mqtt_lwt` or a C3 hal example.
6. Deferred for 0.7.0: the IDF LED blocking connect variant has no gate, and the runbook keeps its non-LED boot and reconnect check under `s3/soak`; next step: add the LED variant to an existing C3 gate in the next hardware run.
7. Resolved 2026-11-11: ESP-NOW scan not-found and cancellation pass in `c3/espnow-cancel` on both tiers.
8. Resolved 2026-11-11: the flood trigger and the queued-versus-in-flight deadline case pass in `c3/espnow-flood` on both tiers.
9. The IDF ESP-NOW driver drop mid-scan has no gate; next step: add a C3 example that drops the driver during a scan and checks the join returns.
10. Late-callback provocation has no gate on either tier; next step: abandon a `send_acked` mid-flight and check the next frame's result on C3.
11. The ESP-NOW TX-power read-back is checked by `c3/espnow-txpower`, but the Wi-Fi TX power is not read back at all.
12. The portal gaps are the invalid-nonce 403, the factory-reset POST, the double submit and submit after commit (409), `PersistFailed` (500 and retry), the hal idle-connection timeout and the OS probe auto-open; next step: add the request steps to C3 hal provisioning extras (manual) and a fault injection for `PersistFailed`, or record a waiver.
13. The OTA matrix P1 to P21 has no row for Content-Length, chunked, short body or oversize; next step: add rows to `docs/runbooks/ota-hardware-test.md`, or record these as host-only by design.
14. Resolved 2026-11-12: `c3/ota-rollback` passed with the rollback-enabled bootloader armed.
15. Resolved 2026-11-06: the IDF OTA download clips the `Connect` and `Read` waits to the remaining deadline (`Deadline::budget_millis`), as hal does.
16. Resolved 2026-11-05: `HealthMachine` and `ReporterMachine` are flattened on the IDF tier, and `c3/ota-runtime` reran P2, P7, P13, P14 and P17; next step: remove the deprecated wrappers in the next release.
17. The hal MQTT runner has no terminal or shutdown path, and its out-of-step policy differs from IDF; next step: decide the hal shutdown contract and align the `Ignored` policy.
18. The IDF MQTT handle drop and shutdown has no gate; next step: add a drop step to an existing C3 MQTT run.
19. Inbound chunking has no gate, because no step sends a payload above the receive buffer; next step: send 3000 B and 20 KB payloads with `just mqtt-pub`.
20. The tier loops are not host-tested (IDF Wi-Fi worker, IDF ESP-NOW worker, hal ESP-NOW `driver.rs`, hal `WifiLink::run`, both MQTT runners); next step: the radio-port refactor that the ESP-NOW plan tracks.
21. The `rustyfarian-esp-idf-network` unit tests are compiled by clippy, but no `just` recipe executes them; next step: add a recipe that runs them on the target, or record that they run only on hardware.
22. LoRa is deferred out of 0.7.0; the LoRa join and uplink check uses esp-idf-hal's mutex provider, so it does not close bug 007, and the hal `LoraRadio` driver is a stub, so the bring-up check covers the bring-up only; next step: run the LoRa checks, then a join and uplink variant with the `rustyfarian-esp-idf-power` critical-section provider for bug 007, and decide the hal driver scope.
23. Fixed 2026-11-06: `lorawan-device` panicked on a non-TxDone response during `SendingData`, which the `StillTxing` mapping could deliver (CONFIRMED by source); `LorawanPhy::phy_due` and the `tx_in_progress` refusal prevent it (test `a_premature_phy_event_during_an_uplink_does_not_panic_the_device`); the LoRa join and uplink check still confirms it on hardware.
24. The LoRa ticker can deliver a stale `Phy` event out of phase (PLAUSIBLE); next step: reproduce on the Heltec V3 or add a phase input.
25. `juggler::ota::OtaState::next_state` is dead and misleading; next step: deprecate it.
26. The hal `connect_async` has no library-side timeout; next step: decide whether to add one in `WifiLink::run`.
27. The hal portal phases are bounded in code since the 2026-11-06 F4 fix, but no bench step holds an idle or stalled connection open; next step: the stalled-client step of C3 hal provisioning extras (manual).
28. Small adapter duplications have a `juggler` home: the inbound drop-oldest queue, the connect-work failure loop, the provisioning error block, the OTA URL-shape rule, the EUI byte reversal, the 200 us NRST constant and the `hal_disconnect_hint` set; next step: one follow-up per item, each with tier parity.
29. Build coverage is complete: all 38 examples build, `just clippy-hal` covers the hal S3 Wi-Fi build that `just verify` does not, and `just build-xtensa-idf-mqtt` passes (the earlier LLVM spill failures are superseded); next step: rerun `just clippy-hal` and `build-all-examples` after any hal change.
30. ESP-NOW on IDF for C6, S3 and classic has no example and no gate, and the runbook matrix does not mark it N/A; next step: mark the cells N/A in the runbook or add a gate.
31. ESP32 classic provisioning is not wired on hal, and an unsupported chip gets no `compile_error!`; next step: add a `compile_error!`, or wire the chip.
32. Tier-parity drift: the event sets, route tables and invalid-UTF-8 handling differ between provisioning tiers and are not recorded in ADR 022; next step: record the differences in the ADR or align them.
33. The IDF MQTT disconnect path keeps a second state tracker (`next_state`) beside `SessionMachine`; next step: move the `on_disconnect` trigger into `SessionMachine` output.
34. The Wi-Fi and LoRa flow-map rows without a gate are deferred in the runbook: the IDF LED blocking connect (gap 6), the LoRa rows (gap 22), and `idf_c3_async_mqtt_native`, which has build evidence only.
35. The deprecated, never-released `juggler::espnow::tx::DeferredRestore` is still exported with no tier consumer; next step: remove it before the 0.7.0 tag, which is not a breaking change.
36. The hal `send_and_wait` to the broadcast address is refused, and only `just test-hal` covers the refusal; next step: add a broadcast-refusal step to a C3 hal ESP-NOW run, or name the host check as the only evidence in the release record.
37. The IDF legacy `MqttManager` path has only `just verify` as evidence; next step: retire the deprecated path.
38. The hal MQTT transport stall with the Wi-Fi link up has no gate; next step: add a link-up stall case to the T1 to T6 set, or record it as deferred in the release record.
39. IDF OTA and IDF provisioning have no example or gate on ESP32, C6 and S3, and the runbook chip matrix does not mark those cells N/A; next step: mark them N/A with a reason, or add an example and a gate.
40. hal OTA has no gate on C6 and classic ESP32, although the hal `ota` feature supports both, and the runbook does not mark C6 OTA N/A; next step: add a C6 hal OTA example and gate, and for classic ESP32 add one or mark the cell N/A.
41. Resolved 2026-11-09: `c6/connect` is the C6 Wi-Fi smoke on both tiers (`hal_c6_connect_async_led`, `idf_c6_connect_nonblocking_rgb`), without a reconnect step.
42. Five IDF OTA runtime rows have host evidence only: the `ApplyNotRecorded` completion retry (H3), the `repaired` result (H5; P20 covers only `repair_not_needed`), the `begin_attempt` failure, the busy hardware-slot reads, and the boot with the OTA handle busy through the failure deadline, and no physical scenario produces them; next step: add fault-injection rows to `docs/runbooks/ota-hardware-test.md`, or record the five as host-only by design.
43. The IDF SoftAP path runs only on C3 gates; next step: add an IDF provisioning gate for C6 or S3, or mark those cells N/A in the runbook chip matrix.
44. Resolved 2026-11-08: `c3/mqtt` (`idf_c3_mqtt`) is the IDF C3 MQTT smoke, with a router block as the outage instead of the AP off.
45. The wrong-password `ConnectFailed` path is the optional R2 of `docs/runbooks/wifi-reconnect-hardware-test.md`, not a pass criterion of `s3/mqtt-scenarios` and `s3/suspend`; next step: make R2 required on both tiers, or record it as waived.
46. No gate publishes while the session is down; next step: stop the broker during `s3/soak` and check that `try_publish` returns `WouldBlock`.
47. No gate fills the async publish admission queue (depth 4 on both tiers); next step: publish more than the queue holds while the session is down on `s3/soak`, and check the waiting path and the drop count.
48. The IDF `on_disconnect` path has no gate; next step: stop the broker during `s3/soak` and check the `Disconnected` log line.
49. No gate overflows the inbound receive queue; next step: publish more than eight queued messages to an example that uses the receive queue, with `just mqtt-pub`.
50. The ESP-NOW command codec has no consumer and no gate; next step: add a consumer, or remove the codec from the release.
51. No gate sends an oversize ESP-NOW frame or overflows the receive queue; next step: add one oversize send and one receive flood step on C3, for each tier.
52. The shared echo example for `EspNowLink` is not delivered; next step: deliver it on both tiers, or record the parity as compile-time only.
53. The encrypted OTA write branch has no gate; next step: run one encrypted partition write, or record the branch as host-only.
54. The hal OTA bad-digest path has no gate; next step: serve a wrong SHA-256 and check the rejection on `hal_c3_ota`.
55. The portal timeout branch (`wait_committed` with no submission, and the boot classification) has no gate; next step: leave the IDF portal idle past its timeout in C3 portal prefill (manual) and check the timeout line.
56. Invalid form submissions have no gate; next step: submit an invalid form in C3 portal prefill (manual) and check the field errors.
57. hal form prefill has no gate, because C3 hal provisioning extras (manual) and `c6/provision` use `just portal-save`; next step: add a browser prefill check to C3 hal provisioning extras (manual).
58. The hal factory reset and the `PersistFailed` path have no gate; next step: run a factory reset from the hal portal and check `is_provisioned` afterwards, and inject the write fault as in gap 12.
59. The IDF portal has no stall gate; next step: open an idle connection to the IDF portal in C3 portal prefill (manual) and check that it still answers.
60. The `ClientConnected` lines have no gate; next step: check them in the C3 portal prefill and C3 hal provisioning extras logs and record the IDF difference (no MAC, no disconnect event).
61. Multi-client DHCP leases have no gate; next step: connect two clients to the hal portal in C3 hal provisioning extras (manual) and check that each gets a lease.
62. The US915 LoRa configuration is not hardware-validated; next step: validate it on hardware, or mark it unvalidated in the release notes.
63. The LoRaWAN downlink OTA commands have no consumer and no gate; next step: add a consumer, or remove the commands module from the release.
64. The IDF `WiFiManager::new` blocking and non-blocking connects, with `wait_connected` and `get_ip` after boot and reconnect, have no gate, and `idf_esp32s3_mqtt_lwt` has no AP-off step; next step: add a C3 IDF gate that calls `WiFiManager::new` and runs an AP-off reconnect.
65. Resolved 2026-11-10: `s3/mqtt-scenarios` T4 restarts the broker on both tiers (`idf_esp32s3_mqtt_lwt` passed 5/5, both T4 steps).
66. The callback-guard re-check example `idf_c3_mqtt_callback_guard` is not in the 0.7.0 runbook; next step: add it as a C3 IDF gate.
67. The failed-rescan restore (a rescan whose peer registration fails after an earlier success) has no gate on either tier; next step: force a failed peer registration on a rescan in both examples, or record the case as host-only.
68. The hal sync `send_and_wait` during a scan has no gate, because `hal_c3_espnow_peer` sends only with `send_acked`; next step: add a sync send to `hal_c3_espnow_peer` and run it during a rescan.
69. No gate runs an application send while a scan is in progress: `idf_c3_espnow_scout` and `idf_c3_espnow_scout_promisc` call the blocking scan and `send_and_wait` in turn on one thread, and `hal_c3_espnow_peer` sends and scans in turn from `main`, so the `ScanInProgress` refusal, the scan preemption and the `ScanGate` exclusion never run on the bench; next step: add a concurrent sender (a second thread on IDF, a second task on hal), then run `c3/espnow-power-cycle` and `c3/espnow-soak` with it.
70. The IDF MQTT connect work (subscriptions, startup message and `on_connect`) and the kept-session skip have no pass criterion, since `s3/soak` checks only the tick cadence and the heap; next step: check in the `s3/soak` log that the startup line follows the SUBSCRIBE packets and that a kept-session reconnect skips the SUBSCRIBE.
71. No gate boots the hal MQTT example with the AP already off, because T5a switches it off only after the device connects; next step: boot `hal_esp32s3_mqtt_lwt` with the AP off, check the boot log, then switch the AP on.
72. No OTA matrix row configures the `NoteAndRestart` health-deadline action (P13 and P14 cover only `rolled_back`); next step: add such a row to `docs/runbooks/ota-hardware-test.md`, or record the action as host-only.
73. The provisioning gates run the example default SSID only, so the overridden and MAC-suffixed SSIDs and the open and WPA2 AP variants are not run on either tier; next step: build override and open-AP variants of `idf_c3_provision_mqtt` and `hal_c3_provision_mqtt` and run them in the two manual C3 provisioning checks.
74. Plain `EspNowDriver::send` has no gate on either tier, since no example calls it (ESP-IDF: a `SendKind::Unacked` frame behind the 1 s `SEND_QUEUE_DEADLINE`; hal: leases the sender); next step: add a plain `send` broadcast or unicast step to `idf_c3_espnow_scout` and `hal_c3_espnow_peer`, then run `c3/espnow-power-cycle` and `c3/espnow-soak`.
75. The esp-hal `WifiLink` adopt-at-boot (`wifi/link.rs` line 264), resume-before-disconnect (line 267) and parked re-check (line 378) have no bench step, because no step provokes their races; host evidence is `start_associated_adopts_without_connect`, `suspend_then_resume_before_the_runner_wakes_hands_over_the_connect` and `connect_landing_after_suspend_is_disconnected_again`; next step: add a boot step with the router already associated and a suspend-then-resume step inside R3 on `hal_esp32s3_mqtt_lwt`, or record the three as host-only.
76. After `Done`, a `scan_for_peer` poll answers `Ignored`, and the worker and driver loops return `UnexpectedInput` (`crates/juggler/src/espnow/worker.rs` line 1115, `crates/rustyfarian-esp-hal-network/src/espnow/driver.rs` lines 539 and 979); a valid scan returns at `Done`, so no gate reaches this exit, and the worker branch has no test (`done_is_sticky` covers only the machine); next step: add a worker host test that polls after `Done` and expects `UnexpectedInput`, and record the esp-hal exit as unexecuted on the host.
77. The esp-hal `EspHalOtaManager::fetch_and_apply` error cases (timeout, missing, chunked or zero `Content-Length`, early body end) have no bench step; host coverage is the header cases in `ota/http.rs` and `classify_read_error` in `ota/mod.rs`, while the `with_timeout` calls in `ota/manager.rs` (lines 452 and 542) have no test; next step: drive the manager with a fake socket in a host test, or serve a stalled and an early-ending body on `hal_c3_ota`.
78. Resolved 2026-11-08: with the shared `ApSelection::Strongest` the CrowPanel S3 joins the strongest mesh AP at boot on both tiers; neither tier roams while associated (a hal reconnect after a router block joined a -74 dBm AP, as the driver never retried the AP that deauthed it), and no step is planned for 0.7.0, since the next boot rejoins the strongest AP.

## Dependency on the runbook

`docs/runbooks/release-0.7.0-hardware-test.md` names this document at the top, and each flow-map row that has no gate is marked "deferred — see ladder-closure-v1".
