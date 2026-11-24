# Runbook: Release 0.8.0 Hardware Test

> **2026-11-24:** the `bench-ota` recipe and the `hal_c3_ota` and `hal_esp32s3_ota` examples named in the MQTT OTA rows are removed, so run those rows from the `v0.8.0` tag.

This is the hardware run for release 0.8.0; the C3 and C6 rows run after the tag on the remote bench (see Decisions).
0.8.0 is the first release on ESP-IDF v6.1 and the first with the DDI OTA client ([ddi-client-v1](../features/ddi-client-v1.md)).
The bench is the source of truth: the header of `bench/<board>/<check>.toml` holds the procedure, `bench/<board>/results.jsonl` the verdict of every run, and `bench/<board>/logs/` the serial log.
This file lists the checks, the manual checks the bench has no file for, the items that cannot run yet, and the results.
Only `c3/ddi` and `c6/ddi` have run locally; the ESP32-S3 and ESP32 classic rows are waived for 0.8.0, and every other result remains `not run`.
Use only the `just` recipes named here or in the check headers.

## Scope

- Run every check from the release branch, not from `main`.
- A 0.7.0 pass stands where neither the code path nor the ESP-IDF version changed, and the esp-hal smoke and soak checks stand unless listed below.
- The esp-hal checks that run a changed code path are `c3/ota`, `c3/ota-rollback`, `s3/ota`, `s3/ota-rollback`, `s3/suspend`, `c6/provision` and `s3/provision`, plus the manual `C3 hal provisioning extras`.
- `EspHalOtaManager` now aborts the socket on any error, but no bench check runs an error case, so the happy path re-run is the only evidence this run adds.
- `suspend()` and `resume()` on esp-hal now return `Result`, and the provisioning response buffer grows from 6 KiB to 8 KiB with the new `ProvisioningError::InvalidDeviceName`.
- The esp-hal disconnect line now names the reason (`[wifi] disconnected — reason 8 (ASSOC_LEAVE)`), but no check pins the reason text, so the esp-hal smoke and soak checks on ESP32-S3 and ESP32 stand from 0.7.0 while the ones on ESP32-C3 and ESP32-C6 are re-run in this run (see Decisions and the last item of the new-in-0.8.0 list).
- Every ESP-IDF-tier check needs one pass on v6.1, except the rows that the 2026-11-15 evidence covers.
- The v6.1 evidence covers `s3/mqtt-lwt` (ESP-IDF, 5/5), `s3/mqtt-scenarios` (ESP-IDF, 5/5) and `s3/soak` (ESP-IDF, 3/3) for their unattended steps, so those are not repeated.
- The attended router steps of `s3/mqtt-lwt` and `s3/mqtt-scenarios` stay owed on v6.1, as the evidence doc's open items say.
- `s3/suspend` is re-run on both tiers, although the v6.1 evidence records 13/13 on both, because its esp-hal side changed and the ESP-IDF `wait_suspended` became an inherent method in 0.8.0.
- The v6.1 evidence on the C3 is only `idf_c3_mqtt_callback_guard` (no bench check) and the espflash-bundled bootloader booting the v6.1 app, so every C3 ESP-IDF check is owed.
- The v6.1 evidence doc does not record a git revision, so whether it covers the 0.8.0 code is not verified.
- The lab DDI server is hawkBit 1.2.0 on the bench machine (maintainer decision 2026-11-23), started with `just hawkbit up`.
- The device reaches the lab DDI server only through the lab proxy, started with `just ddi-proxy`.
- TLS is not part of this run, and the DDI checks use the plain-HTTP lab opt-in (`ota-ddi-insecure-http`).
- LoRa on the Heltec V3 is waived for 0.8.0, see Decisions.
- After the run, record each result in the origin feature doc's session log: `docs/features/ddi-client-v1.md` for the DDI checks, `docs/features/wifi-portal-fallback-v1.md` for the portal item, and `docs/features/archive/esp-idf-v6-1-upgrade-v1.md` for the v6.1 rows.

## Prerequisites

- Everything in the [bench README](../../bench/README.md): the workspace dotenv file with `WIFI_SSID`, `WIFI_PASS` and `MQTT_HOST`, a reachable broker, a router that blocks a client by MAC, and `just doctor` passes.
- Pin `--port` (or `ESPFLASH_PORT` for `just run`) while more than one board is attached.
- Before the run, `just build-all-examples` builds every example of both tiers, pass a prefix such as `idf_c3` to narrow it, and `KEEP_GOING=1` to build past a failure.
- `just mosquitto up` starts the local broker that the DDI and MQTT checks use.
- `just hawkbit up` starts the lab DDI server and prints the `DDI_MGMT_*` values to export.
- The lab DDI server keeps its state in memory, so `just hawkbit down` loses the targets and sets, and they are created again after every restart with the recipes below.
- The router must pass TCP 8090 and 1883 from the IoT subnet to the bench machine.
- Each DDI board needs its own `DDI_CONTROLLER_ID` and `DDI_TOKEN`, plus `DDI_BASE_URL=http://<this machine's LAN IPv4>:8090/DEFAULT`, `DDI_MGMT_URL`, `DDI_MGMT_USER`, `DDI_MGMT_PASS` and `MQTT_HOST=<this machine's LAN IPv4>` in the dotenv file.
- In the shell that starts the DDI run, export `FIRMWARE_VERSION` and `ESPFLASH_PORT`, because the shell beats the dotenv file.
- The bundle and publish sequence for each DDI board is in the header of `bench/c3/ddi-idf.toml` and `bench/c6/ddi-idf.toml` (renamed from `ddi.toml` on 2026-11-24), and it uses only `just ddi-tenant-init`, `just ddi-ensure-target`, `just ddi-bundle` and `just ddi-publish`.
- The check's own setup erases the whole flash, resets the device's target with `just ddi-target-reset` and starts the proxy, so a fleet-owned target needs `just ddi-quiesce` and new versions instead, as the header says.

## Chip matrix

Each cell says what this run must show: new, re-run on v6.1, re-run for the esp-hal changes, stands from 0.7.0, or N/A.
A cell the code does not support is N/A, not a failure.

| Chip                                        | ESP-IDF tier                                                                                                                                 | esp-hal tier                                                                                              |
|:--------------------------------------------|:---------------------------------------------------------------------------------------------------------------------------------------------|:----------------------------------------------------------------------------------------------------------|
| ESP32-C3 (Super Mini)                       | New: `c3/ddi`. Re-run on v6.1: `c3/mqtt`, `c3/reentry`, `c3/ota-runtime`, the ESP-NOW scout checks, the C3 portal prefill (manual).          | Stands from 0.7.0 except the smoke and soak checks. Re-run: `c3/connect`, `c3/mqtt-lwt`, `c3/soak`, `c3/ota`, `c3/ota-rollback`, the C3 hal provisioning extras (manual).         |
| ESP32-C6                                    | New: `c6/ddi`, the first run of the rollback-enabled OTA bootloader on C6. Re-run on v6.1: `c6/connect`, `c6/mqtt`.                          | Stands from 0.7.0 except the smoke and soak checks. Re-run: `c6/connect` (esp-hal twin), `c6/mqtt-lwt`, `c6/mqtt-scenarios`, `c6/soak`, `c6/provision`. ESP-NOW stands from 0.7.0.                                      |
| ESP32-S3 (CrowPanel, Heltec V3)             | Re-run on v6.1: the attended steps of `s3/mqtt-lwt` and `s3/mqtt-scenarios`, and `s3/suspend`. OTA and provisioning N/A. LoRa: waived, see Decisions. | Stands from 0.7.0. Re-run: `s3/suspend`, `s3/ota`, `s3/ota-rollback`, `s3/provision`. ESP-NOW N/A.        |
| ESP32 classic (WROOM DevKit or Feather V2)  | Re-run on v6.1: `esp32/mqtt`.                                                                                                                | Stands from 0.7.0 (`esp32/connect`, `esp32/mqtt-lwt`, `esp32/soak`). Provisioning and ESP-NOW N/A.         |

## Checks

One line per check: name, what it proves, command.
A board passes only when both `--unattended` and `--attended` pass, where a check has both.
When a check lists two examples, one command runs the esp-hal example first and then its ESP-IDF twin, so each example gets its own row in the results.
Checks that stand from 0.7.0 are not listed here, see Scope.

### ESP32 classic

- `esp32/mqtt` (ESP-IDF, re-run on v6.1): Wi-Fi and MQTT smoke with recovery after a router block — `just bench esp32 mqtt --unattended --port <esp32 port>`, then `just bench esp32 mqtt --attended --port <esp32 port>`.

### ESP32-C3 Super Mini

- `c3/ddi` (ESP-IDF, new): the B2 scenario chain on one board: forced update, `up_to_date` and chip gate (1007), cancel mid-download, reboot mid-download, unhealthy image rolled back (1004) and refused again (1007), and wrong sha256 (1002), with each operator row probed on the server by `just ddi-watch` — `just bench c3 ddi --port <c3 port>`.
- `c3/mqtt` (ESP-IDF, re-run on v6.1): Wi-Fi and MQTT smoke with the attended router steps that the v6.1 evidence does not cover — `just bench c3 mqtt --unattended --port <c3 port>`, then `just bench c3 mqtt --attended --port <c3 port>`.
- `c3/reentry` (ESP-IDF, re-run on v6.1): the three BOOT re-entry outcomes (untouched, portal, factory reset), with the example now starting from `WifiMqttBoot::load_with_fallback` — `just bench c3 reentry --port <c3 port>`; provision the board again afterwards.
- `c3/ota-runtime` (ESP-IDF, re-run on v6.1): the OTA runbook rows listed in its header, on the v6.1 build — `just bench c3 ota-runtime --port <c3 port>`; the host scenarios run with `just test-ota`.
- `c3/espnow-power-cycle` (ESP-IDF, re-run on v6.1): the scout finds the coordinator, sends steadily, and refinds it after a coordinator power cycle — `just run idf_c6_espnow_coordinator` on the C6, then `COORDINATOR_MAC="<mac>" just bench c3 espnow-power-cycle --port <c3 port>`.
- `c3/espnow-cancel` (ESP-IDF scout half, re-run on v6.1): a dropped scan keeps the pin, channel, TX power and peer, and the next scan finds the coordinator on its own channel — `ESPNOW_SCENARIO=cancel COORDINATOR_MAC="<mac>" ESPNOW_PEER_MAC="<mac>" just bench c3 espnow-cancel --port <c3 port>`.
- `c3/espnow-flood` (ESP-IDF scout half, re-run on v6.1): 8 concurrent acked sends with 400 ms and 1 ms deadlines end without a hang and the sender keeps running — `ESPNOW_SCENARIO=flood` with the MAC variables of `c3/espnow-cancel`, `just bench c3 espnow-flood --port <c3 port>`.
- `c3/espnow-shutdown` and `c3/espnow-shutdown-queued` (ESP-IDF scout half, re-run on v6.1): queued and in-flight requests return `Shutdown` at once and a second `shutdown` neither panics nor hangs — `ESPNOW_SCENARIO=shutdown` and then `ESPNOW_SCENARIO=shutdown-queued`, with one board reset between them, via `just bench c3 espnow-shutdown --port <c3 port>` and `just bench c3 espnow-shutdown-queued --port <c3 port>`.
- `c3/espnow-txpower` (ESP-IDF scout half, re-run on v6.1): the radio TX power reads back unchanged after every scan, and no `TX power not restored` line appears — `ESPNOW_SCENARIO=txpower` with the MAC variables of `c3/espnow-cancel`, `just bench c3 espnow-txpower --port <c3 port>`.
- `c3/espnow-peer-session` (ESP-IDF peer, re-run on v6.1): `PeerSession` relinks after a coordinator power cycle, and this half runs against `hal_c6_espnow_sta_coordinator`, as the 0.7.0 run did — `just run hal_c6_espnow_sta_coordinator` on the C6, then `ESPNOW_PEER_MAC="<mac>" just bench c3 espnow-peer-session --port <c3 port>`.
- `c3/ota` (esp-hal, re-run for the esp-hal changes): the erase-once write path and the activation on real otadata, the happy path only — `just bench-ota c3 ota hal_c3_ota 1.0.0 2.0.0 <image server host>`, then `just bench-ota c3 ota hal_c3_ota 1.0.0 3.0.0 <image server host>` for the healthy update.
- `c3/ota-rollback` (esp-hal, re-run for the esp-hal changes): the rollback and reset steps on real otadata — `just bench-ota c3 ota-rollback hal_c3_ota 1.0.0 2.0.0 <image server host>`, then the 3.0.0 step of `c3/ota`.
- `c3/connect` (esp-hal, attended, re-run): Wi-Fi connect and reconnect after a router block on the esp-hal tier, the C3 matrix smoke — `just bench c3 connect --unattended --port <c3 port>`, then `just bench c3 connect --attended --port <c3 port>`.
- `c3/mqtt-lwt` (esp-hal, attended, re-run): MQTT smoke with a router block on the esp-hal tier — `just bench c3 mqtt-lwt --unattended --port <c3 port>`, then `just bench c3 mqtt-lwt --attended --port <c3 port>`.
- `c3/soak` (esp-hal, re-run): the 10-minute MQTT soak (T6) on the esp-hal tier, with the `heap free` trend within 1 KiB — `just bench c3 soak --unattended --port <c3 port>`.

#### C3 portal prefill (manual, ESP-IDF, re-run on v6.1)

No bench check; ESP-IDF tier, browser on a laptop.

1. `just run idf_c3_provision_mqtt`, open the portal: the form shows the example defaults.
2. Provision the board and let it restart: the log shows `Loaded provisioned config:` then `Device identity:`, with no stack overflow and no reset loop.
3. Reopen the portal with the short BOOT press of `c3/reentry`: the form shows the stored values with an empty password field.

#### C3 hal provisioning extras (manual, esp-hal, re-run)

No C3 bench check; `c6/provision` and `s3/provision` are the bench equivalents.
The re-run is for the 8 KiB response buffer and the provisioning identity rule.

1. `just erase-flash` if the board holds a record, then `just run hal_c3_provision_mqtt`.
2. Join the SoftAP, run `just portal-save`, replug USB, run `just monitor`.
3. Pass: `Already provisioned` with the device name, profile `WifiMqttDevice` and `extras=2`.

Not checked: the hal form prefill (gap 57), and a stalled client, because no `just` recipe opens a stalling client first.

### ESP32-C6

- `c6/connect` (ESP-IDF twin, re-run on v6.1): the Wi-Fi connect smoke on `idf_c6_connect_nonblocking_rgb`, after its esp-hal twin in the same command — `just bench c6 connect --port <c6 port>`.
- `c6/mqtt` (ESP-IDF, re-run on v6.1): Wi-Fi and MQTT smoke with the router steps — `just bench c6 mqtt --unattended --port <c6 port>`, then `just bench c6 mqtt --attended --port <c6 port>`.
- `c6/ddi` (ESP-IDF, new): the twin of `c3/ddi` on the C6-DevKitC-1, with the same scenario chain — `just bench c6 ddi --port <c6 port>`.
  Flash `idf_c6_ddi` once with `just flash idf_c6_ddi` and watch one boot with `just monitor` before the full check, because the rollback-enabled OTA bootloader has not run on C6 hardware yet (header of `bench/c6/ddi.toml`).
- `c6/provision` (esp-hal, re-run for the esp-hal changes): a record with two extras survives a USB power cut, with the 8 KiB response buffer and the identity rule in place — `just bench c6 provision --port <c6 port>`; the form is not checked (gap 57).
- `c6/mqtt-lwt` (esp-hal, attended, re-run): MQTT smoke with a router block on the esp-hal tier — `just bench c6 mqtt-lwt --unattended --port <c6 port>`, then `just bench c6 mqtt-lwt --attended --port <c6 port>`.
- `c6/mqtt-scenarios` (esp-hal, attended, re-run): esp-hal MQTT scenarios T1 to T5b, with the broker restart and the 3-minute router pause — `MQTT_HOST=<this machine> MQTT_PORT=1883 just bench c6 mqtt-scenarios --unattended --port <c6 port>`, then `MQTT_HOST=<this machine> MQTT_PORT=1883 just bench c6 mqtt-scenarios --attended --port <c6 port>`.
- `c6/soak` (esp-hal, re-run): the 10-minute MQTT soak (T6) on the esp-hal tier, with the `heap free` trend within 1 KiB — `just bench c6 soak --unattended --port <c6 port>`.

### CrowPanel ESP32-S3

- `s3/mqtt-lwt` (ESP-IDF twin, attended steps owed on v6.1): the router steps the v6.1 evidence did not cover — `just bench s3 mqtt-lwt --attended --port <s3 port>`, with the esp-hal twin in the same run.
- `s3/mqtt-scenarios` (ESP-IDF, attended steps owed on v6.1): T1, T4 and T5b/R1, a 3-minute router pause with the back-off capped at 30 s, and the startup message on the first connect — `just bench s3 mqtt-scenarios --attended --port <s3 port>`, with the esp-hal twin in the same run.
- `s3/suspend` (esp-hal, re-run for the esp-hal changes): R3, three suspend and resume cycles, and `wait_suspended` within about 500 ms — `MQTT_SUSPEND_DEMO=1 MQTT_HOST=<this machine> MQTT_PORT=1883 just bench s3 suspend --port <s3 port>`, with the ESP-IDF twin in the same run.
- `s3/ota` and `s3/ota-rollback` (esp-hal, re-run for the esp-hal changes): the same sequence as `c3/ota` and `c3/ota-rollback`, with the 4 MB two-slot table — `just bench-ota s3 ota hal_esp32s3_ota 1.0.0 2.0.0 <image server host>` and `just bench-ota s3 ota-rollback hal_esp32s3_ota 1.0.0 2.0.0 <image server host>`, then `just bench-ota s3 ota hal_esp32s3_ota 1.0.0 3.0.0 <image server host>`.
- `s3/provision` (esp-hal, re-run for the esp-hal changes): as `c6/provision`, with the store at the placeholder offset `0x300000` — `just bench s3 provision --port <s3 port>`.

### New in 0.8.0 with no hardware run yet

The four items below each name the example that exercises the code, and what is missing before a check can run.

#### (a) Portal fallback (ESP-IDF, `WifiMqttDevice`)

- `c3/reentry` drives `idf_c3_provision_mqtt`, which now calls `WifiMqttBoot::load_with_fallback` first, so that check covers the re-entry path but no failed join.
- Not runnable yet: needs a fault that makes joins fail with the credential or network-absent classification, because `just router block <mac>` blocks a client by MAC and the wrong-password case has no check.
- Not runnable yet: the feature doc's open acceptance items are a host test of the timeout path and the rgb-clock acceptance on consumer hardware (waived for the tag, see Decisions), and the maintainer's confirmations of the reason classes and of the threshold 0 are still open.

#### (b) DDI identity rule and the `ddi_token` extra (both tiers)

- Not runnable yet: no example runs the provisioning portal with `ota-ddi` enabled, so the ESP-IDF default `IdentityRule::Ddi` is never reached on hardware, because `idf_c3_provision_mqtt` requires only `wifi`, `mqtt` and `provisioning`.
- Not runnable yet: the esp-hal provisioning examples (`hal_c3_provision_mqtt`, `hal_c6_provision_mqtt` and `hal_esp32s3_provision_mqtt`) keep the `Open` default, and none calls `with_identity(IdentityRule::Ddi)`.
- A check needs an example that builds the portal with `ota-ddi` or calls `with_identity`, and a pass criterion for the rule, which this runbook does not yet state.

#### (c) Tier-parity steps 10 and 11 (`WiFiManager::ip()` and async `MqttHandle::publish`)

- Step 10: the shared app `examples-shared/lwt_app.rs` reads `ip()` on both tiers and logs `[app] wifi ip …` on every change, so `s3/mqtt-lwt` and `s3/mqtt-scenarios` exercise it on `hal_esp32s3_mqtt_lwt` and `idf_esp32s3_mqtt_lwt`.
- No check pattern reads the `[app] wifi ip` line, so the address is judged by eye in the serial log of those runs.
- Step 11: no example on either tier calls the async `MqttHandle::publish`; the shared app calls `try_publish`, and the ESP-IDF examples call `enqueue` or the raw client.
- Not runnable yet for step 11: needs an example call on each tier and a log line that a check can read.

#### (d) Disconnect-reason vocabulary (`[wifi] disconnected — reason 8 (ASSOC_LEAVE)` on esp-hal)

- No check reads the reason text, because the `[wifi] disconnected`, `[wifi] connect failed` and `[wifi] (disconnected|connect failed|reconnecting in)` patterns are prefix matches.
- The esp-hal checks whose patterns read these lines are `c3/connect`, `c3/mqtt-lwt`, `c3/soak`, `c6/mqtt-lwt`, `c6/mqtt-scenarios`, `c6/soak`, `esp32/connect`, `esp32/mqtt-lwt`, `esp32/soak`, `s3/connect`, `s3/mqtt-lwt`, `s3/mqtt-scenarios` and `s3/soak`, the `esp32` and `s3` ones stand on the prefix match, and the `c3` and `c6` ones are re-run in this run (2026-11-24).
- The ESP-IDF checks `c3/mqtt`, `c6/mqtt` and `esp32/mqtt` read `[wifi] disconnected` as well, and they are owed on v6.1 anyway.
- A check that pins the reason text would be new work, outside this run.

#### ESP-IDF v6.1 coexistence soak (not runnable yet)

- Not runnable yet: the soak needs a definition, meaning which example or boards, which Wi-Fi STA peer and ESP-NOW peer, how long, and the pass criterion, and 0.7.0 set no numeric threshold.
- `idf_c6_espnow_coordinator` joins a Wi-Fi AP and runs ESP-NOW in the same firmware, so it is the closest candidate, but it has no STA peer of its own.

## Decisions

The maintainer made the decisions below on 2026-11-24, and none stays open for 0.8.0.

- LoRa on the Heltec V3 (bug 007 ask 4, the OTAA join of [ttn-otaa-validation](ttn-otaa-validation.md)) is out of 0.8.0, waived, and carried to the next release (2026-11-24).
- The esp-hal smoke and soak checks are re-run on ESP32-C3 and ESP32-C6 only, both attached to the bench laptop, while the ESP32-S3 and ESP32 classic esp-hal checks stand from 0.7.0 (2026-11-24).
- The ESP-IDF v6.1 coexistence soak is waived for 0.8.0, and its definition becomes its own item for the next release (2026-11-24).
- The 27 gaps carried from 0.7.0, the partly covered rows 45, 57, 58 and 68, and the two not-runnable 0.7.0 items are under a second blanket waiver, recorded per domain in Carried from 0.7.0 (2026-11-24).
- The rgb-clock acceptance of the portal fallback is waived for the tag and moved to after the release (2026-11-24).
- This run uses only ESP32-C3 and ESP32-C6, so every ESP32-S3 and ESP32 classic row (both tiers) is waived for 0.8.0 and marked `waived` in Results (2026-11-24).
- The not-runnable New in 0.8.0 items (a) portal fallback, (b) DDI identity rule and `ddi_token` extra, and (c) tier-parity step 11 are waived for 0.8.0 (2026-11-24).
- 0.8.0 is tagged before the C3 and C6 run so the DDI OTA client ships; the tag rests on the host and device gates and the local `c3/ddi` and `c6/ddi` passes, and every C3 and C6 row still `not run` runs after the tag on the remote bench (2026-11-24).

## Carried from 0.7.0

The 0.7.0 runbook waived every deferred flow into this release, and the table lists the gap numbers of the deferred rows only.
The flow text and the reasons are in the [0.7.0 flow map](release-0.7.0-hardware-test.md#flow-to-check-map) and [ladder-closure-v1](../features/archive/ladder-closure-v1.md).
Each row needs a new check or an explicit waiver before the tag.

| Domain       | Gaps                        | Decision for 0.8.0 |
|:-------------|:----------------------------|:-------------------|
| Wi-Fi        | 5, 6, 64, 75                | second waiver (maintainer, 2026-11-24), carried to the next release               |
| MQTT         | 18, 19, 34, 37, 38, 70, 71  | second waiver (maintainer, 2026-11-24), carried to the next release               |
| ESP-NOW      | 8, 9, 36, 69, 74, 76        | second waiver (maintainer, 2026-11-24), carried to the next release               |
| OTA          | 13, 15, 42, 72, 77          | second waiver (maintainer, 2026-11-24), carried to the next release               |
| Provisioning | 12, 27, 73                  | second waiver (maintainer, 2026-11-24), carried to the next release               |
| LoRa         | 22, 24                      | second waiver (maintainer, 2026-11-24), carried to the next release               |

Rows that 0.7.0 covered in part keep their gaps: 45, 57, 58 and 68, and the second waiver covers them too.
The two 0.7.0 items that were not runnable yet, the two-thread QoS subscribe on `c6/mqtt` and the nine-subscription check on `c3/mqtt-lwt`, are under the same waiver.

## Results

Only `c3/ddi` and `c6/ddi` have recorded results; the ESP32-S3 and ESP32 classic rows are `waived` (see Decisions), every other row is `not run`, and its Rev and Log cells stay empty until a run is recorded.
The esp-hal twins that run in the same command as an ESP-IDF row have their own row.
The items under New in 0.8.0 that are not runnable yet have no row.

<details>
<summary>Results table</summary>

| Check (tier)                                        | Result   | Rev | Log |
|:----------------------------------------------------|:---------|:----|:----|
| `esp32/mqtt` (ESP-IDF)                              | waived   |     |     |
| `c3/ddi` (ESP-IDF)                                  | PASS 20/20 | `d579e64+dirty` | `bench/c3/logs/idf_c3_ddi-20261123-223247.log` |
| `c3/mqtt` (ESP-IDF)                                 | not run  |     |     |
| `c3/reentry` (ESP-IDF)                              | not run  |     |     |
| `c3/ota-runtime` (ESP-IDF)                          | not run  |     |     |
| `c3/espnow-power-cycle` (ESP-IDF)                   | not run  |     |     |
| `c3/espnow-cancel` (ESP-IDF)                        | not run  |     |     |
| `c3/espnow-flood` (ESP-IDF)                         | not run  |     |     |
| `c3/espnow-shutdown` (ESP-IDF)                      | not run  |     |     |
| `c3/espnow-shutdown-queued` (ESP-IDF)               | not run  |     |     |
| `c3/espnow-txpower` (ESP-IDF)                       | not run  |     |     |
| `c3/espnow-peer-session` (ESP-IDF)                  | not run  |     |     |
| C3 portal prefill (manual, ESP-IDF)                 | not run  |     |     |
| `c3/ota` (esp-hal)                                  | not run  |     |     |
| `c3/ota-rollback` (esp-hal)                         | not run  |     |     |
| C3 hal provisioning extras (manual, esp-hal)        | not run  |     |     |
| `c3/connect` (esp-hal, attended)                    | not run  |     |     |
| `c3/mqtt-lwt` (esp-hal, attended)                   | not run  |     |     |
| `c3/soak` (esp-hal)                                 | not run  |     |     |
| `c6/connect` (esp-hal twin)                         | not run  |     |     |
| `c6/connect` (ESP-IDF)                              | not run  |     |     |
| `c6/mqtt` (ESP-IDF)                                 | not run  |     |     |
| `c6/ddi` (ESP-IDF)                                  | PASS 20/20 | `d579e64+dirty` | `bench/c6/logs/idf_c6_ddi-20261123-221515.log` |
| `c6/provision` (esp-hal)                            | not run  |     |     |
| `c6/mqtt-lwt` (esp-hal, attended)                   | not run  |     |     |
| `c6/mqtt-scenarios` (esp-hal, attended)             | not run  |     |     |
| `c6/soak` (esp-hal)                                 | not run  |     |     |
| `s3/mqtt-lwt` (esp-hal twin, attended)              | waived   |     |     |
| `s3/mqtt-lwt` (ESP-IDF, attended)                   | waived   |     |     |
| `s3/mqtt-scenarios` (esp-hal twin, attended)        | waived   |     |     |
| `s3/mqtt-scenarios` (ESP-IDF, attended)             | waived   |     |     |
| `s3/suspend` (esp-hal)                              | waived   |     |     |
| `s3/suspend` (ESP-IDF)                              | waived   |     |     |
| `s3/ota` (esp-hal)                                  | waived   |     |     |
| `s3/ota-rollback` (esp-hal)                         | waived   |     |     |
| `s3/provision` (esp-hal)                            | waived   |     |     |

</details>
