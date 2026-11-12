# Runbook: Release 0.7.0 Hardware Test

This is the one hardware run before the release 0.7.0 tag, and each feature doc points here.
The bench is the source of truth: the header of `bench/<board>/<check>.toml` holds the procedure, `bench/<board>/results.jsonl` the verdict of every run, and `bench/<board>/logs/` the serial log.
This file lists the checks, the manual checks the bench has no file for, the results, and the flows each check covers.
The closure evidence for the ladder retrofit is in [ladder-closure-v1](../features/ladder-closure-v1.md).
Use only the `just` recipes named here or in the check headers.

## Scope

- Every check runs from the release branch, not from `main`, which lacks the ESP-NOW and retrofit changes this release gates.
- A check that a feature doc records as passed is not repeated, unless the doc still leaves that item open.
- Items that a feature doc still lists as not implemented cannot pass until their code lands.
- The ESP-NOW checks pick their scenario with `ESPNOW_SCENARIO` at build time; `just run` rebuilds, so set the variable on both `just build-example` and `just run` when running by hand.
- The smoke checks block the board on the router instead of switching the AP off; on the mesh a board roams to another AP of the same SSID.
- No router channel change is needed in 0.7.0: the ESP-NOW checks run on the mesh's own channel 11.
- Multi-board ESP-NOW fleet tests and router-channel sweeps run in a downstream project, and consumer firmware checks (Watchtower v2, rgb-clock) in the consumer's own repository, both since 2026-11-11.
- LoRa (Heltec V3: OTAA join, SX1262 bring-up, the bug 007 provider variant) is out of scope for 0.7.0 and moves to the roadmap; see [ttn-otaa-validation](ttn-otaa-validation.md).
- After the run, record each result in the origin feature doc's session log, and in ADR 020 for the ESP-NOW checks.

## Prerequisites

- Everything in the [bench README](../../bench/README.md): `.env`, a reachable broker, a router that blocks a client by MAC, `just doctor` passes.
- Pin `--port` (or `ESPFLASH_PORT` for `just run`) while more than one board is attached.
- Before the run, `just build-all-examples` builds every example of both tiers; pass a prefix such as `hal_c3` to narrow it, and `KEEP_GOING=1` to build past a failure.

## Chip matrix

A cell the code does not support is N/A, not a failure.
AVR is out of scope because it has no radio; a `juggler` compile check on `avr-none` is at most a roadmap idea.
Each supported cell gets a smoke (connect, reconnect after a router block, MQTT round trip where the cell has MQTT); OTA, provisioning, ESP-NOW and suspend/resume run on one primary board per tier: the ESP32-C3 Super Mini, plus the CrowPanel ESP32-S3 for the shared-app pair `idf_esp32s3_mqtt_lwt` and `hal_esp32s3_mqtt_lwt`.

| Chip                                        | ESP-IDF tier                                                                     | esp-hal tier                                                                                         |
|:--------------------------------------------|:---------------------------------------------------------------------------------|:-----------------------------------------------------------------------------------------------------|
| ESP32-C3 (Super Mini)                       | Wi-Fi, MQTT, ESP-NOW, OTA, provisioning                                          | Wi-Fi, MQTT, ESP-NOW, OTA, provisioning                                                              |
| ESP32-C6                                    | Wi-Fi and MQTT smoke                                                             | Wi-Fi, MQTT, ESP-NOW, provisioning                                                                   |
| ESP32-S3 (CrowPanel, Heltec V3)             | Wi-Fi, MQTT; LoRa out of scope                                                   | Wi-Fi, MQTT, OTA, provisioning; LoRa out of scope; ESP-NOW N/A                                       |
| ESP32 classic (WROOM DevKit or Feather V2)  | MQTT smoke                                                                       | Wi-Fi and MQTT smoke; provisioning and ESP-NOW N/A; OTA and LoRa (note 1)                            |

1. OTA is supported with no example; there is no SX1262, so LoRa is N/A.

<details>
<summary>Cell to check map</summary>

| Cell                     | Checks                                                                                                                     | Gap                                                        |
|:-------------------------|:---------------------------------------------------------------------------------------------------------------------------|:-----------------------------------------------------------|
| ESP32-C3, ESP-IDF        | `c3/mqtt`, `c3/espnow-power-cycle`, `c3/espnow-*` (scout half)                                                             | No plain connect check; `idf_c3_connect` is the candidate  |
|                          | `c3/reentry`, `c3/ota-runtime`, C3 portal prefill (manual)                                                                 |                                                            |
| ESP32-C3, esp-hal        | `c3/connect`, `c3/mqtt-lwt`, `c3/soak`, `c3/espnow-*` (peer half)                                                          |                                                            |
|                          | `c3/ota`, `c3/ota-rollback`, C3 hal provisioning extras (manual)                                                           |                                                            |
| ESP32-C6, ESP-IDF        | `c6/connect`, `c6/mqtt`                                                                                                    | The coexistence soak needs consumer firmware               |
|                          | coordinator of `c3/espnow-power-cycle` and `c3/espnow-peer-session`                                                        |                                                            |
|                          | ESP-IDF v5.5 coexistence soak N/A                                                                                          |                                                            |
| ESP32-C6, esp-hal        | `c6/connect`, `c6/mqtt-lwt`, `c6/mqtt-scenarios`, `c6/soak`                                                                |                                                            |
|                          | `c6/dns-*`, `c6/provision`, coordinator of the other ESP-NOW checks                                                        |                                                            |
| CrowPanel S3, ESP-IDF    | `s3/mqtt-lwt`, `s3/mqtt-scenarios`, `s3/suspend`, `s3/soak`                                                                | No plain connect check; R1 in `s3/mqtt-scenarios` covers reconnect |
| CrowPanel S3, esp-hal    | `s3/connect`, `s3/mqtt-lwt`, `s3/mqtt-scenarios`, `s3/suspend`, `s3/soak`                                                  |                                                            |
|                          | `s3/dns-*`, `s3/provision`, `s3/ota`, `s3/ota-rollback`                                                                    |                                                            |
| Heltec V3 ESP32-S3, LoRa | none in 0.7.0                                                                                                              | No check until the roadmap item lands                      |
| ESP32 classic, ESP-IDF   | `esp32/mqtt`                                                                                                               | No Wi-Fi-only check                                        |
| ESP32 classic, esp-hal   | `esp32/connect`, `esp32/mqtt-lwt`, `esp32/soak`                                                                            | No OTA or provisioning example                             |

</details>

## Checks

One line per check: name, what it proves, command.
Smoke checks without a line here (`*/connect`, `*/mqtt`, `*/mqtt-lwt`, `*/soak`) run as `just bench <board> <check>`.

### ESP32 classic

- `esp32/mqtt` (ESP-IDF), `esp32/connect` and `esp32/mqtt-lwt` (esp-hal): Wi-Fi and MQTT smoke with recovery after a router block, no reboot — `just bench esp32`.
  The T1 to T6 cases stay on `c6/mqtt-scenarios`; record the exact board.

### ESP32-C3 Super Mini

- `c3/espnow-power-cycle` (ESP-IDF): the scout finds the coordinator, sends steadily, and refinds it after a coordinator power cycle — `just run idf_c6_espnow_coordinator` on the C6, then `COORDINATOR_MAC="<mac>" just bench c3 espnow-power-cycle --port <c3 port>`.
  The gate also named `idf_c3_espnow_scout_promisc`; the check does not run it.
  Record the `espnow-tx` stack high-water mark and the minimum free heap if the example prints them.
- `c3/reentry` (ESP-IDF): the three BOOT re-entry outcomes (untouched, portal, factory reset) — `just bench c3 reentry`; provision again afterwards.
- `c3/ota-runtime` (ESP-IDF): OTA runbook rows P2, P7, P13, P14 and P17, rerun because of the flattened health and reporter loops of 2026-11-05 — `just bench c3 ota-runtime --port <c3 port>`; host scenarios H1 to H5 with `just test-ota`.
  The other P rows stand on the 2026-10 campaign of [ota-hardware-test](ota-hardware-test.md).
  The IDF OTA budget clip is host-only (`a_one_second_remainder_clips_a_sixty_second_wait_to_one_second`, `budget_millis_rounds_up_and_never_reaches_zero` in `crates/juggler/src/ota/deadline.rs`); a bench row needs a server that stalls with about 1 s of the deadline left, and the clip does not bound DNS resolution inside connect.
- `c3/espnow-cancel` (both tiers): a dropped scan keeps the pin, channel, TX power and peer, the next scan finds the coordinator on its own channel (an adjacent channel is a FAIL), not-found scans name the channels the country forbids (ADR 026) — `ESPNOW_SCENARIO=cancel COORDINATOR_MAC="<mac>" ESPNOW_PEER_MAC="<mac>" just bench c3 espnow-cancel --port <c3 port>`.
  Not covered: the failed-rescan restore (gap 67); a cancel that ends in a send is not visible in the log, so that part rests on the channel and next-scan checks.
  Switching the coordinator off again after a find logs a FAIL on the not-found line, because the example expects it to stay up.
- `c3/espnow-flood` (both tiers): 8 concurrent acked sends (2 × `TX_SLOTS`) with 400 ms and 1 ms deadlines end without a hang, and the sender keeps running — same variables with `ESPNOW_SCENARIO=flood`, `just bench c3 espnow-flood`.
- `c3/espnow-shutdown` and `c3/espnow-shutdown-queued` (both tiers): queued and in-flight requests return `Shutdown` at once, a running scan ends `Aborted` (or `Shutdown`) with no channel change, a second `shutdown` neither panics nor hangs — `ESPNOW_SCENARIO=shutdown` and `shutdown-queued`, one board reset between them.
  The scout flushes the console after its gate line, otherwise the line stays in the USB Serial/JTAG FIFO.
- `c3/espnow-txpower` (both tiers): the radio TX power reads back unchanged after every scan, and no `TX power not restored` line appears — `ESPNOW_SCENARIO=txpower`, `just bench c3 espnow-txpower`.
- `c3/espnow-soak` (esp-hal): owned-radio peer, 30 minutes at 100 ms with failures below 1 % (G-E1) — `ESPNOW_PEER_MAC="<mac>" just bench c3 espnow-soak --unattended --port <c3 port>`.
  The attended channel-move half (1, 6, 11) is not a 0.7.0 criterion.
- `c3/espnow-peer-session` (both tiers): `PeerSession` relinks after a coordinator power cycle, with or without a rescan — run twice, once against `idf_c6_espnow_coordinator` (counts for the esp-hal peer) and once against `hal_c6_espnow_sta_coordinator` (counts for the ESP-IDF peer), `ESPNOW_PEER_MAC="<mac>" just bench c3 espnow-peer-session --port <c3 port>`.
- `c3/mqtt-lwt` (esp-hal) smoke.
  Not runnable yet: nine subscriptions via `MqttBuilder::subscribe()` all acknowledged, and a filter refused by a broker ACL ending the session before `Ready`; it needs a `hal_c3_mqtt_lwt` scenario and a `just` recipe for the refusing ACL.
- `c3/ota` and `c3/ota-rollback` (esp-hal): first hardware run of the bug 004 erase-once write path and of the hal `mark_valid`, `rollback` and `selected_slot` fix on real otadata — `just bench-ota c3 ota hal_c3_ota 1.0.0 2.0.0 <host>`, `just bench-ota c3 ota-rollback hal_c3_ota 1.0.0 2.0.0 <host>`, then `just bench-ota c3 ota hal_c3_ota 1.0.0 3.0.0 <host>` for the healthy update after the rollback.
  Interrupted activation is host-only (`no_interruption_point_selects_the_new_slot_without_the_new_state`, `entries_match_the_crates_golden_bytes` in `crates/rustyfarian-esp-hal-network/src/ota/activation.rs`): a power cut during the 32-byte otadata write cannot be timed by hand.
  `scripts/flash.sh` erases otadata on every flash, so the 3.0.0 step does not test the otadata the rollback left behind; the rollback and reset steps do.

#### C3 portal prefill (manual)

No bench check; ESP-IDF tier, browser on a laptop.

1. `just run idf_c3_provision_mqtt`, open the portal: the form shows the example defaults.
2. Provision the board and let it restart: the log shows `Loaded provisioned config:` then `Device identity:`, with no stack overflow and no reset loop.
3. Reopen the portal with the short BOOT press of `c3/reentry`: the form shows the stored values with an empty password field.

#### C3 hal provisioning extras (manual)

No C3 bench check; `c6/provision` and `s3/provision` are the bench equivalents.

1. `just erase-flash` if the board holds a record, then `just run hal_c3_provision_mqtt`.
2. Join the SoftAP, run `just portal-save 192.168.4.1 2`, replug USB, run `just monitor`.
3. Pass: `Already provisioned` with the device name, profile `WifiMqttDevice` and `extras=2`.

Not checked: the hal form prefill (gap 57), and a stalled client (headers with a `Content-Length` and no body; a new `just portal-save` connection must be accepted within about 5 s), which needs a `just` recipe that opens a stalling client first.

### ESP32-C6

- `c6/provision` (esp-hal): a record with two extras survives a USB power cut — `just bench c6 provision`; the form is not checked (gap 57).
- `c6/mqtt-scenarios`, `c6/mqtt-lwt` and `c6/soak` (esp-hal): T1 to T6 of the [esp-hal MQTT runbook](esp-hal-mqtt-hardware-test.md) — `MQTT_HOST=<this machine> MQTT_PORT=1883 just bench c6 mqtt-scenarios`, once `--unattended` (T1, T2, T4) and once `--attended` (T3, T5b), and `just bench c6 soak` (T6).
  T5a (AP power off) is not runnable on a mesh; T5b's per-device pause covers the same link-loss path.
- `c6/dns-subscribe` and `c6/dns-unresolvable` (esp-hal): hostname broker, runtime subscribe surviving a broker restart, and a failed lookup retried on the back-off — `MQTT_HOST=bench-broker just bench c6 dns-subscribe`, `MQTT_HOST=no-such-broker.invalid just bench c6 dns-unresolvable`.
  Rebuild with the IPv4 afterwards; the DNS server comes from DHCP, so a static-IP setup is not covered.
- `c6/mqtt` (ESP-IDF) smoke.
  Not runnable yet: two threads calling `MqttHandle::subscribe` on one topic with QoS 0 and QoS 1 at once, where the broker must end on the newest QoS; it needs an example and a `just` recipe that reads the granted QoS.

#### ESP-IDF v5.5 coexistence soak

N/A in 0.7.0; it runs with the ESP-IDF v6.1 bump ([feature doc](../features/esp-idf-v6-1-upgrade-v1.md)).

1. On a board with Wi-Fi STA and ESP-NOW on one radio (the consumer's ESP32-C6 brain firmware; this repository has no such ESP-IDF C6 example), soak with one STA peer and one ESP-NOW-only peer.
2. Run the OTA and provisioning checks (`c3/ota-runtime`, C3 portal prefill) on the bumped build.
3. Pass: no ESP-NOW send failures of the kind the v5.3.3 coexistence workarounds were added for.
   The result decides which `CONFIG_ESP_WIFI_AMPDU_*` and `CONFIG_ESP_COEX_SW_COEXIST_ENABLE` workarounds stay, with no numeric threshold.

### CrowPanel ESP32-S3

- `s3/mqtt-scenarios` (both tiers, `--attended`): T1, T4 and T5b/R1, a 3-minute router pause with the back-off capped at 30 s, and the startup message on the first connect, after a broker restart and after the pause — `just bench s3 mqtt-scenarios --attended`.
  R2 (wrong password) is optional and not a pass criterion (gap 45).
- `s3/suspend` (both tiers): R3, three suspend/resume cycles, and `wait_suspended` (IDF at once, hal within about 500 ms) — build with `MQTT_SUSPEND_DEMO=1`, `just bench s3 suspend`.
  The `[app]` line templates must match between the tiers.
- `s3/dns-subscribe` and `s3/dns-unresolvable` (esp-hal): hostname broker and failed lookup — as on the C6.
- `s3/provision` (esp-hal): as `c6/provision`, store at the placeholder offset `0x300000`.
- `s3/ota` and `s3/ota-rollback` (esp-hal): as `c3/ota`, with the 4 MB two-slot table — `just bench-ota s3 …`; only the first 4 MB of flash is used.
- `s3/soak` (both tiers): T6, ticks every 5 s, heap within 1 KiB over 10 minutes — `just bench s3 soak --unattended`.
  The 2026-11-03 IDF run drifted 2148 B; record the minimum free heap against the ESP32-C3 spike figures.

## Results

One row per check and tier; logs are in `bench/<board>/logs/`, and every run (passes, fails, aborts) is in `bench/<board>/results.jsonl`.
Rev `+` means uncommitted changes on top (the bench checks, plus the OTA TCP connect fix in `s3/ota` and the `PendingAcks` cap in `s3/soak`).
Runs up to the C6 runs of 2026-11-10 used a local rustyfarian-bench checkout (`RUSTYFARIAN_BENCH`), later runs bench v0.5.0.

<details>
<summary>Results table</summary>

| Check (tier)                                 | Result                                  | Rev                     | Log                                                                                           | Not in the log                                  |
|:---------------------------------------------|:----------------------------------------|:------------------------|:----------------------------------------------------------------------------------------------|:------------------------------------------------|
| `esp32/mqtt` (ESP-IDF)                       | pass 2026-11-08                         | `6cd883e`               | `idf_esp32_mqtt-20261108-232728.log`, `-235002.log`                                           | see note 2                                      |
| `esp32/connect` (esp-hal)                    | pass 2026-11-08                         | `6cd883e`               | `hal_esp32_connect_async-20261108-232317.log`, `-234208.log`                                  |                                                 |
| `esp32/mqtt-lwt` (esp-hal)                   | pass 2026-11-08                         | `6cd883e`               | `hal_esp32_mqtt_lwt-20261108-232611.log`, `-234648.log`                                       |                                                 |
| `esp32/soak` (esp-hal)                       | pass 2026-11-08                         | `6cd883e`               | `hal_esp32_mqtt_lwt-20261108-233003.log`                                                      |                                                 |
| `c3/connect` (esp-hal)                       | pass 2026-11-08                         |                         | `hal_c3_connect_async-20261108-215009.log`, `-231239.log`                                     |                                                 |
| `c3/mqtt` (ESP-IDF)                          | pass 2026-11-12; first pass 2026-11-08  |                         | `idf_c3_mqtt-20261112-004242.log`; first pass `-215341.log`, `-231722.log`                    | Re-run at the shared station TX default (`STATION_TX_POWER_DEFAULT`, 8.5 dBm) |
| `c3/mqtt-lwt` (esp-hal)                      | pass 2026-11-12; first pass 2026-11-08  | `6cd883e` (2026-11-08)  | `hal_c3_mqtt_lwt-20261112-004356.log`, `-004524.log` (see note 3)                             | Re-run as `c3/mqtt`                             |
| `c3/soak` (esp-hal)                          | pass 2026-11-08                         |                         | `hal_c3_mqtt_lwt-20261108-215800.log`                                                         |                                                 |
| `c3/espnow-power-cycle` (ESP-IDF)            | pass 2026-11-11                         |                         | `idf_c3_espnow_scout-20261111-233717.log`                                                     | see note 4                                      |
| C3 portal prefill (manual, ESP-IDF)          | pass 2026-11-09                         | `c4d3879`               | none                                                                                          | see note 5                                      |
| `c3/reentry` (ESP-IDF)                       | pass 2026-11-08                         | `6cd883e`               | `idf_c3_provision_mqtt-20261108-225738.log`                                                   | see note 6                                      |
| `c3/ota-runtime` (ESP-IDF)                   | pass 2026-11-12                         |                         | `idf_c3_ota_runtime-20261112-002202.log`                                                      | see note 7                                      |
| `c3/espnow-cancel` (ESP-IDF)                 | pass 2026-11-11                         |                         | `idf_c3_espnow_scout-20261111-214052.log` (see note 8)                                        | see note 9                                      |
| `c3/espnow-cancel` (esp-hal)                 | pass 2026-11-11                         |                         | `hal_c3_espnow_peer-20261111-214453.log`                                                      | Verdict judged by the `RadioState` alone        |
| `c3/espnow-flood` (ESP-IDF)                  | pass 2026-11-11; first pass 2026-11-10  | `c4d3879+` (2026-11-10) | `idf_c3_espnow_scout-20261111-082727.log`; first pass `-004841.log`                           | see note 10                                     |
| `c3/espnow-flood` (esp-hal)                  | pass 2026-11-11                         | `37488a7+`              | `hal_c3_espnow_peer-20261111-082803.log` (`-010349.log`)                                     |                                                 |
| `c3/espnow-shutdown` (ESP-IDF)               | pass 2026-11-11; first pass 2026-11-10  | `c4d3879+` (2026-11-10) | `idf_c3_espnow_scout-20261111-082954.log` (`-013107.log`, see note 11)                        | see note 12                                     |
| `c3/espnow-shutdown` (esp-hal)               | pass 2026-11-11                         | `37488a7+`              | `hal_c3_espnow_peer-20261111-083025.log` (`-010848.log`, `-013748.log`)                      |                                                 |
| `c3/espnow-shutdown-queued` (ESP-IDF)        | pass 2026-11-11; first pass 2026-11-10  | `c4d3879+` (2026-11-10) | `idf_c3_espnow_scout-20261111-083402.log` (`-011453.log`, see note 13)                        |                                                 |
| `c3/espnow-shutdown-queued` (esp-hal)        | pass 2026-11-11                         | `37488a7+`              | `hal_c3_espnow_peer-20261111-083529.log` (`-011518.log`)                                     |                                                 |
| `c3/espnow-txpower` (ESP-IDF)                | pass 2026-11-11; first pass 2026-11-10  | `c4d3879+` (2026-11-10) | `idf_c3_espnow_scout-20261111-082900.log`; first pass `-004823.log`                           | see note 14                                     |
| `c3/espnow-txpower` (esp-hal)                | pass 2026-11-11                         | `37488a7+`              | `hal_c3_espnow_peer-20261111-082925.log` (`-010441.log`)                                     |                                                 |
| C3 hal provisioning extras (manual, esp-hal) | pass 2026-11-09                         | `c4d3879`               | none                                                                                          | see note 15                                     |
| `c3/espnow-soak` (esp-hal)                   | pass 2026-11-11                         |                         | `hal_c3_espnow_peer-20261111-094435.log`                                                      | see note 16                                     |
| `c3/espnow-peer-session` (esp-hal)           | pass 2026-11-11                         |                         | `hal_c3_espnow_peer-20261111-234512.log`                                                      | see note 17                                     |
| `c3/espnow-peer-session` (ESP-IDF)           | pass 2026-11-12                         |                         | `idf_c3_espnow_peer-20261112-000357.log`                                                      | see note 18                                     |
| `c3/ota`, `c3/ota-rollback` (esp-hal)        | pass 2026-11-12                         |                         | see note 19                                                                                   | see note 20                                     |
| `c6/connect` (esp-hal)                       | pass 2026-11-08                         |                         | `hal_c6_connect_async_led-20261108-235647.log`                                                |                                                 |
| `c6/connect` (ESP-IDF)                       | pass 2026-11-09                         |                         | `idf_c6_connect_nonblocking_rgb-20261109-000017.log`                                          |                                                 |
| `c6/provision` (esp-hal)                     | pass 2026-11-10                         | `c4d3879+`              | `hal_c6_provision_mqtt-20261110-003519.log`                                                   | see note 21                                     |
| `c6/mqtt-lwt` (esp-hal)                      | pass 2026-11-09                         |                         | `hal_c6_mqtt_lwt-20261109-000257.log`                                                         |                                                 |
| `c6/mqtt-scenarios` (esp-hal)                | pass 2026-11-09                         | `c4d3879+`              | `hal_c6_mqtt_lwt-20261109-224552.log` (unattended), `-234042.log` (attended)                  | see note 22                                     |
| `c6/soak` (esp-hal)                          | pass 2026-11-09                         | `c4d3879+`              | `hal_c6_mqtt_lwt-20261109-235329.log` (`-000915.log`)                                         | Ticks are judged from serial; per-tick broker arrival is not logged |
| `c6/dns-subscribe`, `c6/dns-unresolvable` (esp-hal) | pass 2026-11-10                  | `c4d3879+`              | `hal_c6_mqtt_lwt-20261110-002135.log`, `-002202.log`                                          | see note 23                                     |
| `c6/mqtt` (ESP-IDF)                          | pass 2026-11-09                         | `6cd883e`               | `idf_c6_mqtt-20261109-000441.log`                                                             |                                                 |
| ESP-IDF v5.5 coexistence soak                | N/A                                     |                         | none                                                                                          | see note 24                                     |
| `s3/connect` (esp-hal)                       | pass 2026-11-07                         |                         | `hal_esp32s3_connect_async-20261107-190042.log`                                               |                                                 |
| `s3/mqtt-lwt` (esp-hal)                      | pass 2026-11-09                         |                         | `hal_esp32s3_mqtt_lwt-20261109-014053.log`                                                    | see note 25                                     |
| `s3/mqtt-lwt` (ESP-IDF)                      | pass 2026-11-09                         |                         | `idf_esp32s3_mqtt_lwt-20261109-014217.log`                                                    | see note 25                                     |
| `s3/mqtt-scenarios` (esp-hal)                | pass 2026-11-11                         | `37488a7+`              | see note 26                                                                                   | The second router block ran as T5a              |
| `s3/mqtt-scenarios` (ESP-IDF)                | pass 2026-11-11                         | `37488a7+`              | see note 27                                                                                   | `[app]` line templates match the esp-hal run line for line |
| `s3/suspend` (esp-hal)                       | pass 2026-11-11                         | `37488a7+`              | see note 28                                                                                   |                                                 |
| `s3/suspend` (ESP-IDF)                       | pass 2026-11-11                         | `37488a7+`              | `idf_esp32s3_mqtt_lwt-20261111-001505.log`                                                    |                                                 |
| `s3/dns-subscribe`, `s3/dns-unresolvable` (esp-hal) | pass 2026-11-11                  | `37488a7+`              | `hal_esp32s3_mqtt_lwt-20261111-013742.log`, `-013833.log`                                     |                                                 |
| `s3/provision` (esp-hal)                     | pass 2026-11-11                         | `37488a7+`              | `hal_esp32s3_provision_mqtt-20261111-013141.log`                                              | see note 29                                     |
| `s3/ota`, `s3/ota-rollback` (esp-hal)        | pass 2026-11-11                         | `37488a7+`              | see note 30                                                                                   | see note 31                                     |
| `s3/soak` (ESP-IDF)                          | pass 2026-11-11                         | `37488a7+`              | `idf_esp32s3_mqtt_lwt-20261111-023330.log`                                                    | see note 32                                     |
| `s3/soak` (esp-hal)                          | pass 2026-11-11                         | `37488a7+`              | `hal_esp32s3_mqtt_lwt-20261111-022307.log`                                                    |                                                 |

2. Board Adafruit Feather V2 (ESP32-PICO-V3-02), console UART0 through the USB-UART bridge (all `esp32/*` rows).
3. First pass 2026-11-08 logs: `-215224.log`, `-231526.log`.
4. Coordinator `idf_c6_espnow_coordinator`; the earlier runs that night failed with the scout at the driver's 20 dBm.
5. 32 KiB main stack, 16 KB httpd stack; env defaults with an empty password on the first visit, stored values with an empty password after the re-entry; the rgb-clock 8000 B half was removed on 2026-11-11.
6. Includes the 16 KB httpd stack and the 2 s BOOT window; a second consumer passed all 7 `WifiMqttBoot::load_with_reentry` cases on an ESP32-C3-DevKitM-1 on 2026-10-30.
7. New check, the 0.7.0 subset on one board after a full erase; broker and image server on the Mac (192.168.40.146); statuses in `tmp/bench-ota-status.log`.
8. Unattended halves `-084625.log`, `-084710.log`.
9. Coordinator: hal C6 on the shared echo app; the 15 s settle step was added after the 08:36 run, whose first finds wandered while the C6 still scanned for its AP.
10. Coordinator `hal_c6_espnow_sta_coordinator` instead of a second ESP-IDF C3 (all flood, shutdown and txpower rows).
11. First pass 2026-11-10 log: `-004856.log`.
12. The 01:05 rerun looked like a hang: the gate line was stuck in the USB Serial/JTAG FIFO (JTAG halt showed `main` parked); passed with `fsync` after the boards were moved to opposite table edges.
13. First pass 2026-11-10 log: `-004919.log`.
14. Coordinator on channel 11, not 6 (the read-back does not depend on the channel); the 2026-11-10 adjacent-channel find on scan 3 predates the same-channel criterion.
15. `just portal-save 192.168.4.1 2` answered HTTP 200; after a USB power cycle `Already provisioned — … device_name=hal-extras-test, profile=WifiMqttDevice, extras=2`; a browser submission also committed (no `parse_form` overflow on hal); stalled-client step not runnable.
16. Coordinator hal C6, boards at opposite table edges; the 08:53 attempt on the pre-rewrite peer does not count (serial dropped during the bench reshuffle).
17. Against `idf_c6_espnow_coordinator`; the ESP-IDF peer against the same coordinator passed as an extra, `idf_c3_espnow_peer-20261111-235324.log`.
18. Against `hal_c6_espnow_sta_coordinator`; the esp-hal peer passed as an extra, `hal_c3_espnow_peer-20261112-000025.log`.
19. `hal_c3_ota-20261112-000839.log` (2.0.0), `-001058.log` (rollback), `-001228.log` (3.0.0).
20. Image server `just ota-serve` on the Mac (192.168.40.146:8000) instead of a 192.168.31.x host.
21. An earlier phone-form submission that night committed and reloaded too (`extras=0`, the form sends no extras).
22. Board 192.168.31.238 to the broker 192.168.40.146:1883 through a UniFi rule for TCP 1883; T5a run as a device block; in an earlier attempt the T3 will came 43 s after the unplug.
23. `bench-broker` is a UniFi DNS record for the local broker; the IPv4 literal is every other MQTT run.
24. 2026-11-10: the v5.5.5 bump moves to the next release together with `rustyfarian-esp-idf-ws2812` (v0.9.0 still pins v5.3.3), so 0.7.0 ships on v5.3.3.
25. The 2026-11-11 09:45 reruns of both tiers timed out waiting for the first broker session.
26. `hal_esp32s3_mqtt_lwt-20261111-003838.log` (attended), `-20261110-235333.log` (unattended).
27. `idf_esp32s3_mqtt_lwt-20261111-004547.log` (attended), `-20261110-235424.log` (unattended).
28. `hal_esp32s3_mqtt_lwt-20261111-001115.log` (also `-091140.log`, `-20261112-010625.log`).
29. Open AP `bench-s3-portal` (`PORTAL_SSID_OVERRIDE`); the first `curl` timed out before the SoftAP lease.
30. `hal_esp32s3_ota-20261111-021206.log`, `-021327.log`, `-021511.log`; manual run `hal_esp32s3_ota-manual-20261111-*.log`.
31. Server on the Mac after a UniFi rule for TCP 8000; first failed twice (no TCP connect, then `BadStatusLine` on `HTTP/1.0`), both fixed in this run.
32. Flat with the `PendingAcks` cap, against 2136 B of drift on 2026-11-08 and 2148 B on 2026-11-03.
    The drift was `PendingAcks::early`, not the per-connect helper thread stacks, whose reconnect minimum-heap peak stays open.
    The app prints the free heap, not the minimum.

</details>

## Flow to check map

ADR 023 requires the bench for every flow that a rung-3 machine change drives.
Rows marked deferred have no check in this run; each needs a new check or an explicit waiver before the release tag, and [ladder-closure-v1](../features/ladder-closure-v1.md) records why and the next step.
**Waiver (2026-11-12, maintainer):** all deferred rows below and the open boxes in [ladder-closure-v1](../features/ladder-closure-v1.md) are waived for 0.7.0 and carried into the next release, with no further bench run before the tag.

<details>
<summary>Flow to check map (76 flows)</summary>

| Flow                                                                                                                             | Check, or deferred with the gap                                                                                                      |
|:---------------------------------------------------------------------------------------------------------------------------------|:-------------------------------------------------------------------------------------------------------------------------------------|
| IDF station initial connect and reconnect with back-off (note 33)                                                                | `s3/mqtt-scenarios` (R1); R2 optional (gap 45)                                                                                       |
| IDF suspend/resume (R3), `is_suspended()` and readiness-before-stop order                                                        | `s3/suspend`                                                                                                                         |
| IDF readiness subscribers and async `wait_ready` consumers (note 34)                                                             | `s3/soak`, `s3/mqtt-scenarios`                                                                                                       |
| IDF `WiFiManager::new` blocking connect without LED (note 35)                                                                    | deferred (gap 64): no check runs `WiFiManager::new`                                                                                  |
| IDF `WiFiManager::new` blocking connect with the LED variant                                                                     | deferred (gap 6)                                                                                                                     |
| hal `WifiLink` initial connect, reconnect and suspend/resume                                                                     | `s3/mqtt-scenarios`, `s3/suspend`                                                                                                    |
| hal `WifiLink` adopt-at-boot and connect failure (note 36)                                                                       | deferred (gaps 75 and 5), see note 36                                                                                                |
| hal `wait_connected` readiness wake-ups (`take_ready_change`)                                                                    | `s3/mqtt-scenarios`, `s3/suspend`                                                                                                    |
| IDF MQTT connect with subscriptions, startup message, `on_connect` (note 37)                                                     | `s3/mqtt-scenarios` (startup message); rest deferred (gap 70)                                                                        |
| IDF MQTT reconnect after broker restart                                                                                          | `s3/mqtt-scenarios` (T4, both tiers); gap 65 closed                                                                                  |
| IDF MQTT handle drop/shutdown (`Shutdown` to `Close` through the machine)                                                        | deferred (gap 18): no step drops the handle                                                                                          |
| IDF MQTT inbound chunked messages above the receive buffer (note 38)                                                             | deferred (gap 19), see note 38                                                                                                       |
| hal MQTT connect with subscriptions and startup message                                                                          | `s3/mqtt-scenarios`                                                                                                                  |
| `NetworkLink::wait_suspended` on both tiers                                                                                      | `s3/suspend`                                                                                                                         |
| hal MQTT runtime `MqttHandle::subscribe`, re-subscribed after reconnect                                                          | `c6/dns-subscribe`                                                                                                                   |
| hal MQTT hostname broker (`dns_query` on each connect)                                                                           | `s3/dns-*`, `c6/dns-*`                                                                                                               |
| hal MQTT Wi-Fi outage and recovery (note 39)                                                                                     | `s3/mqtt-scenarios`                                                                                                                  |
| hal MQTT broker restart                                                                                                          | `c6/mqtt-scenarios` (T4)                                                                                                             |
| hal MQTT transport stall with the Wi-Fi link down                                                                                | `s3/mqtt-scenarios`                                                                                                                  |
| hal MQTT transport stall with the Wi-Fi link up                                                                                  | deferred (gap 38): no step stalls the socket while the link stays up                                                                 |
| hal MQTT boot before the network is up                                                                                           | deferred (gap 71): the outage always starts after the device connects                                                                |
| `wait_connected` on both tiers (woken on `SessionAction::Ready`)                                                                 | `s3/mqtt-scenarios`                                                                                                                  |
| Connect ordering (ready only after subscriptions)                                                                                | deferred (gap 70): no pass criterion checks the order                                                                                |
| IDF example `idf_c3_async_mqtt_native`                                                                                           | deferred (gap 34), see note 40                                                                                                       |
| ESP-NOW `scan_for_peer` found, on IDF and hal                                                                                    | every ESP-NOW check; channel sweep runs downstream (note 41)                                                                         |
| ESP-NOW `scan_for_peer` not found and cancelled, on IDF and hal                                                                  | `c3/espnow-cancel`                                                                                                                   |
| IDF ESP-NOW driver drop or deinit during a scan                                                                                  | deferred (gap 9)                                                                                                                     |
| TX-power boost and restore around scans (both tiers)                                                                             | `c3/espnow-txpower`                                                                                                                  |
| hal ESP-NOW scan cancelled by dropping the future, including mid-probe                                                           | `c3/espnow-cancel`                                                                                                                   |
| hal ESP-NOW scan with a previously pinned channel, found path                                                                    | `c3/espnow-peer-session` (note 42)                                                                                                   |
| hal ESP-NOW scan cancel and restore order on the pinned channel                                                                  | `c3/espnow-cancel`                                                                                                                   |
| hal ESP-NOW scan concurrent with app sends (`ScanGate`)                                                                          | deferred (gap 69): no check overlaps an app send with a scan                                                                         |
| ESP-NOW `send_and_wait` with no full queue, IDF (steady sends)                                                                   | `c3/espnow-power-cycle`                                                                                                              |
| ESP-NOW `send_acked` with no full queue, hal (steady sends)                                                                      | `c3/espnow-soak`; the hal sync `send_and_wait` is sent by no check (gap 68)                                                          |
| ESP-NOW plain `send` with no full queue, both tiers (note 43)                                                                    | deferred (gap 74): no example calls `send`                                                                                           |
| ESP-NOW `send`, `send_and_wait` under a full queue or with deadlines (note 44)                                                   | `c3/espnow-flood`                                                                                                                    |
| ESP-NOW shutdown with queued and in-flight requests (both tiers)                                                                 | `c3/espnow-shutdown`, `c3/espnow-shutdown-queued`                                                                                    |
| hal `send_and_wait` to broadcast (now refused)                                                                                   | deferred (gap 36): host check with `just test-hal` only                                                                              |
| ESP-NOW sync sends during a scan (both tiers)                                                                                    | deferred (gap 69): the scout sends and scans in turn                                                                                 |
| IDF ESP-NOW scan while app traffic fills the queue                                                                               | deferred (gap 8)                                                                                                                     |
| `juggler::espnow::PeerLink` consumers (note 45)                                                                                  | `c3/espnow-peer-session` (gap 2 resolved in ladder-closure-v1)                                                                       |
| IDF OTA download: happy path, checksum mismatch and download timeout                                                             | `c3/ota-runtime` (P2); see note 46                                                                                                   |
| IDF OTA download: other deadline checkpoints and body cases (note 47)                                                            | deferred (gaps 13 and 15), see note 47                                                                                               |
| hal `EspHalOtaManager::fetch_and_apply` (same cases, bug 004 erase-once path)                                                    | `c3/ota`, `s3/ota` for the happy path; the error cases are deferred (gap 77)                                                         |
| IDF OTA post-update health check (mark valid)                                                                                    | `c3/ota-runtime`; `ApplyNotRecorded` is host only (gap 42)                                                                           |
| IDF OTA health deadline rollback                                                                                                 | `c3/ota-runtime` (P13, P14)                                                                                                          |
| IDF OTA health deadline `NoteAndRestart` action                                                                                  | deferred (gap 72): no matrix row configures it                                                                                       |
| IDF boot with the OTA handle busy through the failure deadline                                                                   | deferred (gap 42): host coverage `just test-ota` only                                                                                |
| IDF OTA boot reconcile and runtime flags at launch                                                                               | `c3/ota-runtime`                                                                                                                     |
| IDF OTA repair command (`repaired`, `repair_not_needed`)                                                                         | P20 of the 2026-10 campaign covers `repair_not_needed` (note 48)                                                                     |
| IDF OTA update command when `begin_attempt` fails                                                                                | deferred (gap 42): no matrix row makes `begin_attempt` fail                                                                          |
| IDF hardware slot reads with busy retry                                                                                          | deferred (gap 42): host coverage `just test-ota` only                                                                                |
| Captive portal save with a valid nonce (both tiers)                                                                              | see note 49                                                                                                                          |
| Captive portal save with an invalid nonce (403, both tiers)                                                                      | deferred (gap 12): no check sends a wrong nonce                                                                                      |
| Captive portal save, commit, reboot into provisioned (both tiers)                                                                | see note 49                                                                                                                          |
| Portal save with flash or NVS write failure (500 and retry)                                                                      | deferred (gap 12): needs a fault injection                                                                                           |
| Portal factory reset by BOOT (`WifiMqttBoot` re-entry)                                                                           | `c3/reentry`                                                                                                                         |
| Portal factory reset POST, double submit, submit after commit (note 50)                                                          | deferred (gap 12), see note 50                                                                                                       |
| Captive portal form prefill on fresh and provisioned devices                                                                     | C3 portal prefill (manual) on IDF; hal has no check (gap 57)                                                                         |
| Captive portal OS probe detection and auto-open (note 51)                                                                        | deferred (gap 12): no check runs a phone OS probe                                                                                    |
| Portal start with the default SSID (both tiers)                                                                                  | C3 portal prefill (manual), C3 hal provisioning extras (manual)                                                                      |
| Portal start with an overridden or MAC-suffixed SSID (note 52)                                                                   | deferred (gap 73), see note 52                                                                                                       |
| hal portal with a phone that opens idle connections (5 s header timeout)                                                         | deferred (gap 27)                                                                                                                    |
| hal provisioning store write and read across reboot and factory reset                                                            | see note 53                                                                                                                          |
| IDF `idf_esp32s3_join` OTAA join on Heltec V3 (EU868)                                                                            | deferred, LoRa after 0.7.0 (note 54)                                                                                                 |
| IDF `idf_esp32s3_lora_uplink` join and uplink (`LorawanTimeline`)                                                                | deferred, LoRa after 0.7.0 (note 55)                                                                                                 |
| hal `hal_esp32s3_join` SX1262 bring-up (shared `init`)                                                                           | deferred, LoRa after 0.7.0 (note 56)                                                                                                 |
| hal LoRa build and the host test-hal parity guard                                                                                | deferred (gap 22): `just check-lora-hal` and `just test-lora-hal`                                                                    |
| ESP-NOW `scan_for_peer` on both tiers: a poll after `Done` (note 57)                                                             | deferred (gap 76), see note 57                                                                                                       |
| esp-hal ESP-NOW dropped-future teardown (`ScanGuard`) (note 58)                                                                  | `c3/espnow-cancel` (gap 7)                                                                                                           |
| IDF ESP-NOW `espnow-tx` worker scan loop (`drive_scan`)                                                                          | `c3/espnow-cancel`; the channel sweep runs downstream                                                                                |
| IDF legacy MQTT through the deprecated `MqttManager`                                                                             | deferred (gap 37), see note 59                                                                                                       |
| IDF LoRaWAN radio polling via the deprecated `irq_pending` and `cancel_rx`                                                       | deferred, LoRa after 0.7.0: the deprecated calls are not isolated (gap 22)                                                           |
| Wi-Fi link readiness on both tiers (IP up and IP down edges)                                                                     | `s3/mqtt-scenarios` (R1), `s3/soak`                                                                                                  |
| IDF OTA `rolled_back` report delivery                                                                                            | `c3/ota-runtime` (P7, P17)                                                                                                           |
| IDF OTA background completion retry (after `ApplyNotRecorded`)                                                                   | deferred (gap 42): host only, H3 with `just test-ota`                                                                                |

33. Wrong password, AP off; refused connect feeds `ConnectFailed`.
    The wrong-password case is the optional R2 of [wifi-reconnect](wifi-reconnect-hardware-test.md).
34. The subscribers are the MQTT gate via `subscribe_ready`.
35. Covers `wait_connected`/`get_ip` after boot and reconnect, and non-blocking connect.
    `idf_esp32s3_mqtt_lwt` reaches the link through the MQTT gate.
36. Flow lists adopt-at-boot, resume-before-disconnect, the parked re-check and connect failure.
    No step provokes the three races, and the wrong-password path has no check.
37. Flow covers fresh and kept sessions.
    No check judges the order or the kept-session skip.
38. Chunked-message reassembly, `dropped_inbound`.
    Send a 3000 B and a 20 KB payload with `just mqtt-pub` against `idf_esp32s3_mqtt_lwt`.
39. Network edges from `NetworkGate`, `Close` on loss.
40. Build evidence only: `just build-example idf_c3_async_mqtt_native`.
41. The channel sweep (1, 6, 11) runs downstream since 2026-11-11.
42. The channel-move half of `c3/espnow-soak` is not run in 0.7.0.
43. IDF `SendKind::Unacked` with the 1 s queue deadline; hal leases the sender.
44. Both tiers.
45. `PeerSession` in `idf_c3_espnow_peer` and `hal_c3_espnow_peer`.
46. P6 and P10 stand on the 2026-10 campaign; host cases with `just test-ota`.
47. Cases: deadline at the other checkpoints, oversize, no Content-Length, chunked, short body.
    The matrix has no rows for these cases (gap 13), and the IDF budgets are not clipped (gap 15).
48. `repaired` is host only (H5, `just test-ota`), gap 42.
49. C3 portal prefill (manual), C3 hal provisioning extras (manual), `c6/provision`, `s3/provision`.
50. 409 on both tiers.
    The BOOT reset is not the portal POST, and no check submits twice.
51. On Android, iOS and macOS; hal AAAA now answers zero records.
52. Open or WPA2 AP, both tiers.
    `c6/provision` and `s3/provision` run the hal override build on an open AP.
53. C3 hal provisioning extras (manual), `c6/provision`, `s3/provision` for the reboot; the factory reset is not covered (gap 58).
54. `just run idf_esp32s3_join`; the bug 007 provider variant is still missing (gap 22).
55. `just run idf_esp32s3_lora_uplink`; the bug 007 provider variant is still missing (gaps 22 and 24).
56. `just run hal_esp32s3_join`, bring-up only; the hal `LoraRadio` driver is a stub (gap 22).
57. The poll ends as an error exit.
    Host only: the worker has no test for it yet, and no check reaches the exit.
58. Teardown deferred until the probe completes.
59. `just verify` only; the downstream rgb-clock build is checked in its own repository.

</details>
