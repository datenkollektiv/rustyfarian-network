# Runbook: OTA Hardware Test (Library Runtime)

Run this after any change to the OTA download path, wire contract, record store or runtime, and before every release.
It pushes real firmware updates over MQTT to a device and records one result per scenario.

**Reference device:** an ESP32-C3 with 4 MB flash running the `idf_c3_ota_runtime` example.

**Consumers:** run the same scenario matrix on your own firmware.
Keep only the app-specific parts in your own runbook: your health predicate, your topics, and your build and flash recipes.
Link the generic parts here: <https://github.com/datenkollektiv/rustyfarian-network/blob/main/docs/runbooks/ota-hardware-test.md>.

## Scope

- **Physical scenarios (P1–P21)** run end to end on the device and cover download hardening (A), the wire contract (B), the record store (C) and the runtime (D).
- **Host scenarios (H1–H5)** need injected faults that cannot be produced by hand (a crash after one specific NVS write, a persistently failing NVS, corrupt records).
  They are covered by `just test-ota`; record the library commit you ran them on.
- **Not covered:** HTTPS (plain `http://` only, ADR 011), fleet backends, signature verification, partition tables with more than two OTA slots.

## Prerequisites

- An ESP32-C3 board with 4 MB flash and a USB serial connection.
- An MQTT broker reachable from the board, and `mosquitto_pub` / `mosquitto_sub` on the test machine.
- Python 3 for `just ota-serve`.
- The workspace dotenv file with `WIFI_SSID`, `WIFI_PASS`, `MQTT_HOST`, optionally `MQTT_PORT`, `MQTT_CLIENT_ID` and `OTA_TOPIC_PREFIX` (default `rustyfarian/example/ota`).
- Never put `FIRMWARE_VERSION`, `OTA_DEMO_UNHEALTHY` or `OTA_FAILURE_DEADLINE_SECS` into the dotenv file: they are set per command on the shell below, and a value left behind silently changes later images.

## Shell setup

Use one shell for building and publishing, and define these once per test campaign.
Increase `M` by one for every new campaign on the same board: the refused version and other OTA records survive a reflash, so a new campaign needs versions above everything offered before.

```sh
export M=1
export OTA_HOST=192.168.1.10
export OTA_PORT=8000
export T=rustyfarian/example/ota
pub() { mosquitto_pub -h "$MQTT_HOST" -t "$T/command" "$@"; }
img() { FIRMWARE_VERSION="$1" just ota-image idf_c3_ota_runtime; }
cmd() { pub -f "target/ota/command-v$1.json"; }
```

`OTA_HOST` is the LAN address of the test machine (never `localhost`); `OTA_PORT` must be a free port.
`just ota-image` writes both into the manifest and command URLs at build time, so set them before building any image.

In a second terminal, keep a status subscription open for the whole campaign:

```sh
mosquitto_sub -v -h "$MQTT_HOST" -t "rustyfarian/example/ota/status"
```

In a third terminal, keep the serial monitor open: `just monitor`.

## Flashing vs. recovery

- `just flash` is for the baseline only (P1).
  It writes the two-slot `partitions.ota.csv` (2 × 1.875 MiB), sets 4 MB flash and erases only `otadata`, so the next boot starts from `ota_0`.
- NVS survives a reflash: attempt records, an undelivered report and the refused version are kept.
  A reflash therefore does not clear `attempt_unresolved` or `report_pending`, and a reflash after an update switched the boot slot reads as a rollback on the next boot (limitation documented in `crates/juggler/src/ota/reconcile.rs`).
- Every interrupted-update and recovery scenario uses RST or a power cycle, never a reflash, so the boot evidence stays intact.

## Reading the evidence

Serial lines, once per boot, in this order:

- `[ota] boot slot noted: …`
- `[ota] slots: running=<label> (<state>), next boot=<label>, update=<label> (<state>)` — labels `ota_0`, `ota_1`, `factory`; states `Valid`, `PendingVerify`, `Invalid`, `Unknown`.
- `[ota] boot reconciliation: <disposition>, slot released: <bool>, report pending: <bool>`
- `[ota] health policy finished: <verdict>` — `NothingToVerify` when the running slot was not pending, `Applied` when it has just been validated.
- Every 30 s: `OTA: admission open <bool>, stack marks …, dropped rejections <n>`.

The ESP-IDF bootloader also prints which partition it loaded: offset `0x20000` is `ota_0`, `0x200000` is `ota_1`.

Who publishes what:

| Status                                                                           | Published by                    |
|:---------------------------------------------------------------------------------|:--------------------------------|
| `downloading`, `swap_pending`, worker-decided `failed`, `repaired`               | worker thread                   |
| `applied`                                                                        | health policy (app main thread) |
| `rolled_back`, intake `failed` (`busy`, `command_invalid`, `worker_unavailable`) | reporter thread                 |

Rules that hold in every scenario:

- The manifest is always fetched first; `up_to_date`, `downgrade`, `previously_rolled_back` and `target_mismatch` are decided after the fetch, and only the firmware download is skipped.
- A blocked offer (`report_pending`, `attempt_unresolved`, or `pending_verify` for an update) publishes `failed` once, is retained, and later runs with `downloading` and no new `failed`.
- `attempt_id`, `epoch` and `records` are JSON numbers; `rolled_back` may be delivered more than once and is deduplicated by `epoch` and `attempt_id`.
- An image is validated automatically: the health predicate (in the example `!unhealthy && mqtt.is_connected()`) must hold for the 30 s minimum dwell; if it never does, the failure deadline (default 120 s from the start of `main`) rolls the image back with up to 3 attempts 5 s apart.

## Version ladder

Each physical scenario starts from the state the previous one left behind.

| Version            | Built in | Role                                                          |
|:-------------------|:---------|:--------------------------------------------------------------|
| `$M.0.0`           | P1       | Baseline, flashed                                             |
| `$M.1.0`           | P2       | First update                                                  |
| `$M.2.0`           | P6       | Second update, later rolled back by the operator              |
| `$M.3.0`           | P9       | Validates a newer version, clearing the refusal of `$M.2.0`   |
| `$M.4.0`           | P10      | Download-timeout and manifest-stall target, then burst update |
| `$M.5.0`           | P13      | Unhealthy image, 60 s deadline                                |
| `$M.6.0`           | P14      | Unhealthy image, default 120 s deadline                       |
| `$M.7.0`           | P15      | Early command on a validated boot                             |
| `$M.8.0`, `$M.9.0` | P16      | Pending-verification gate and retained update                 |
| `$M.10.0`          | P17      | Offer during an undelivered report                            |
| `$M.11.0`          | P18      | Power loss during download                                    |
| `$M.12.0`          | P19      | Reset during pending verification                             |

## Scenario matrix

| ID  | Scenario                                                | Type                        | Expected result                                    |
|:----|:--------------------------------------------------------|:----------------------------|:---------------------------------------------------|
| P1  | Baseline flash                                          | Physical                    | `NothingToVerify`, admission open                  |
| P2  | First update                                            | Physical                    | `downloading`, `swap_pending`, `applied`           |
| P3  | Up to date                                              | Physical                    | `failed{up_to_date}`                               |
| P4  | Invalid command                                         | Physical                    | `failed{command_invalid}`                          |
| P5  | Downgrade                                               | Physical                    | `failed{downgrade}`                                |
| P6  | Wrong target, bad checksum, second update               | Physical                    | `target_mismatch`, `checksum_mismatch`, `applied`  |
| P7  | Operator rollback                                       | Physical                    | `rolled_back{operator}`                            |
| P8  | Refused re-offer                                        | Physical                    | `failed{previously_rolled_back}`                   |
| P9  | Refusal cleared by a newer version                      | Physical                    | `applied`                                          |
| P10 | Download timeout                                        | Physical                    | `failed{download_timeout}` after ~60 s             |
| P11 | Manifest stall                                          | Physical                    | `failed{manifest_fetch}`                           |
| P12 | Command burst during a download                         | Physical                    | `busy` rejections, then `applied`                  |
| P13 | Unhealthy image, 60 s                                   | Physical                    | `rolled_back{health_deadline}`                     |
| P14 | Unhealthy image, 120 s default                          | Physical                    | `rolled_back{health_deadline}`                     |
| P15 | Startup admission on a validated boot                   | Physical                    | Early command accepted                             |
| P16 | Startup admission while pending verification            | Physical                    | `pending_verify` ×3, retained update runs          |
| P17 | Rollback report across a broker outage                  | Physical                    | Report delivered after reconnect                   |
| P18 | Power loss during download                              | Physical                    | Old image, admission open                          |
| P19 | Reset during pending verification                       | Physical                    | `rolled_back{bootloader}`                          |
| P20 | Repair on a healthy device                              | Physical                    | `failed{repair_not_needed}`                        |
| P21 | Rejections and report retries share the reporter thread | Physical observation        | Nothing lost                                       |
| H1  | Reset between `mark_valid` and the attempt clear        | Host                        | Next boot completes the attempt                    |
| H2  | Power loss around every record write                    | Host + physical best effort | Never a wedge                                      |
| H3  | `ApplyNotRecorded`                                      | Host                        | `applied`, then `attempt_unresolved` until cleanup |
| H4  | Failed-closed boot stays gated                          | Host                        | Admission closed                                   |
| H5  | Corrupt records repaired                                | Host                        | `repaired`                                         |

## Physical scenarios

### P1: Baseline flash

1. `FIRMWARE_VERSION=$M.0.0 just flash idf_c3_ota_runtime`, then `just monitor`.

**Expected:** `[ota] slots: running=ota_0 (Unknown)` or `(Valid)` (a freshly erased `otadata` has no image state yet), `[ota] health policy finished: NothingToVerify`, and `OTA: admission open true`.
No status is published.

### P2: First update

1. `img $M.1.0`, check the URLs in `target/ota/manifest-v$M.1.0.json` and `target/ota/command-v$M.1.0.json`.
2. Start the server in its own terminal: `just ota-serve`.
3. `cmd $M.1.0`.

**Expected statuses:**

```json
{"status":"downloading"}
{"status":"swap_pending"}
{"status":"applied","version":"<M>.1.0"}
```

`applied` follows at least 30 s after the reboot.
**Slots:** before `running=ota_0`; after the reboot `running=ota_1 (PendingVerify)`; after the next reset `running=ota_1 (Valid)`.

### P3: Up to date

1. `cmd $M.1.0`.

**Expected:** `{"status":"failed","reason":"up_to_date"}`; no `downloading`, no reboot.

### P4: Invalid command

1. `pub -m 'not-json'`.

**Expected:** `{"status":"failed","reason":"command_invalid"}`; nothing else changes.

### P5: Downgrade

1. `img $M.0.0` (rebuilds the baseline as an offerable image).
2. `cmd $M.0.0`.

**Expected:** `{"status":"failed","reason":"downgrade"}`; no `downloading`.

### P6: Wrong target, bad checksum, second update

1. `img $M.2.0`.
2. Wrong target:
   `sed 's/"esp32c3"/"esp32s3"/' target/ota/manifest-v$M.2.0.json > target/ota/manifest-target.json`, then
   `pub -m "{\"manifest_url\":\"http://$OTA_HOST:$OTA_PORT/manifest-target.json\"}"`.
3. Bad checksum:
   `sed -E 's/"sha256":"[0-9a-f]+"/"sha256":"0000000000000000000000000000000000000000000000000000000000000000"/' target/ota/manifest-v$M.2.0.json > target/ota/manifest-sha.json`, then
   `pub -m "{\"manifest_url\":\"http://$OTA_HOST:$OTA_PORT/manifest-sha.json\"}"`.
4. Real update: `cmd $M.2.0`.

**Expected statuses:**

```json
{"status":"failed","reason":"target_mismatch"}
{"status":"downloading"}
{"status":"failed","reason":"checksum_mismatch"}
{"status":"downloading"}
{"status":"swap_pending"}
{"status":"applied","version":"<M>.2.0"}
```

After the checksum failure the serial line shows `[ota] the failed attempt was cleared` and the slots are unchanged.
**Slots after:** `running=ota_0` with `$M.2.0` (the baseline was overwritten); `ota_1` still holds `$M.1.0`.

### P7: Operator rollback

1. `pub -m "{\"action\":\"rollback\",\"from\":\"$M.2.0\"}"`.

**Expected:** the device reboots at once into `$M.1.0` (`running=ota_1`), then:

```json
{"status":"rolled_back","reason":"operator","attempt_id":<n>,"epoch":<e>}
```

### P8: Refused re-offer

1. `cmd $M.2.0`.

**Expected:** `{"status":"failed","reason":"previously_rolled_back"}`.

### P9: Refusal cleared by a newer version

1. `img $M.3.0`, then `cmd $M.3.0`.

**Expected:** `downloading`, `swap_pending`, `applied` for `$M.3.0`.
Validating `$M.3.0` removes the refusal of `$M.2.0`; it cannot be observed directly because `$M.2.0` is now a `downgrade`.

### P10: Download timeout

1. Stop the running `just ota-serve`, then start `just ota-serve 0 102400` (stalls after 100 KiB).
2. `img $M.4.0`, then `cmd $M.4.0`.

**Expected:** `{"status":"downloading"}`, then about 60 s later (the per-read timeout, not the 300 s total deadline) `{"status":"failed","reason":"download_timeout"}`.
No reboot; `[ota] the failed attempt was cleared`; slots unchanged.

### P11: Manifest stall

1. Stop the server, then start `just ota-serve 0 1`.
2. `cmd $M.4.0`.

**Expected:** only `{"status":"failed","reason":"manifest_fetch"}`, after at most 10 s; no `downloading`.

### P12: Command burst during a download

1. Stop the server, then start `just ota-serve 20000` (about 80 s for the image).
2. `cmd $M.4.0`, wait for `downloading`, then send six commands quickly: `for i in 1 2 3 4 5 6; do pub -m '{"action":"repair"}'; done`.

**Expected:** `downloading`, several `{"status":"failed","reason":"busy"}`, then `swap_pending` and `applied` for `$M.4.0`.
Up to four rejections can wait for the reporter; extra ones are counted instead of published.
**Pass:** published `busy` count plus the increase of `dropped rejections` in the serial line equals 6.
Restart a normal `just ota-serve` afterwards.

### P13: Unhealthy image, 60 s deadline

1. `FIRMWARE_VERSION=$M.5.0 OTA_DEMO_UNHEALTHY=1 OTA_FAILURE_DEADLINE_SECS=60 just ota-image idf_c3_ota_runtime`.
2. `cmd $M.5.0` (never flash this image: a flashed image is not pending verification and is never rolled back).

**Expected:** `downloading`, `swap_pending`, reboot, a loud `OTA_DEMO_UNHEALTHY=1` error on serial, no `applied`.
About 60 s after boot the rollback starts, the device reboots into `$M.4.0`, then:

```json
{"status":"rolled_back","reason":"health_deadline","attempt_id":<n>,"epoch":<e>}
```

A re-offer (`cmd $M.5.0`) answers `failed{previously_rolled_back}`.

### P14: Unhealthy image, default deadline

1. `FIRMWARE_VERSION=$M.6.0 OTA_DEMO_UNHEALTHY=1 just ota-image idf_c3_ota_runtime`, then `cmd $M.6.0`.

**Expected:** as P13, with the rollback about 120 s after boot.
Rebuild the next image without the knobs.

### P15: Startup admission on a validated boot

1. `img $M.7.0`.
2. Press RST and send `cmd $M.7.0` as soon as the serial line shows the MQTT connection.

**Expected:** `downloading` straight away, no `pending_verify`, then `swap_pending` and `applied` for `$M.7.0`.
The boot is validated (`slot released: true`), so admission opens when the runtime starts.

### P16: Startup admission while pending verification

1. `img $M.8.0` and `img $M.9.0`.
2. `cmd $M.8.0` and wait for the reboot; the serial line shows `running=… (PendingVerify)`.
3. Within the 30 s dwell, send `cmd $M.9.0`, then `pub -m "{\"action\":\"rollback\",\"from\":\"$M.8.0\"}"`, then `pub -m '{"action":"repair"}'`.

**Expected statuses after the reboot:**

```json
{"status":"failed","reason":"pending_verify"}
{"status":"failed","reason":"pending_verify"}
{"status":"failed","reason":"pending_verify"}
{"status":"applied","version":"<M>.8.0"}
{"status":"downloading"}
{"status":"swap_pending"}
{"status":"applied","version":"<M>.9.0"}
```

The update is retained and runs after `applied`; rollback and repair are answered and dropped.

### P17: Rollback report across a broker outage

1. `img $M.10.0`.
2. `pub -m "{\"action\":\"rollback\",\"from\":\"$M.9.0\"}"` and stop the broker right away (before the device reconnects after its reboot).
3. Serial: `[ota] boot reconciliation: ReportedRollback, slot released: true, report pending: true`.
4. Wait at least 10 s (one report retry), restart the broker, and send `cmd $M.10.0` as soon as the device reconnects.

**Expected statuses:**

```json
{"status":"rolled_back","reason":"operator","attempt_id":<n>,"epoch":<e>}
{"status":"failed","reason":"report_pending"}
{"status":"downloading"}
{"status":"swap_pending"}
{"status":"applied","version":"<M>.10.0"}
```

`report_pending` appears only if the offer arrives before the report is acknowledged; the offer is retained and runs afterwards.

### P18: Power loss during download

1. Restart the server slowly: `just ota-serve 20000`.
2. `img $M.11.0`, `cmd $M.11.0`, and unplug the board halfway through the download (after `downloading`).
3. Power it again without reflashing.

**Expected:** the old image runs (`[ota] slots: running=<old slot> (Valid)`), `[ota] boot reconciliation: Settled(NoRequest), slot released: true, report pending: false` (the interrupted attempt is cleared silently), and no `rolled_back`.
`cmd $M.11.0` with a normal server then completes with `applied`.

### P19: Reset during pending verification

1. `img $M.12.0`, `cmd $M.12.0`.
2. After the reboot, while the serial line shows `PendingVerify` and before `applied`, press RST.

**Expected:** the bootloader rejects the unverified image and boots the previous one, then:

```json
{"status":"rolled_back","reason":"bootloader","attempt_id":<n>,"epoch":<e>}
```

`cmd $M.12.0` then answers `failed{previously_rolled_back}`.

### P20: Repair on a healthy device

1. `pub -m '{"action":"repair"}'`.

**Expected:** `{"status":"failed","reason":"repair_not_needed"}`; no reboot.

### P21: Rejections and report retries on the shared reporter thread

The reporter thread delivers `rolled_back`, publishes rejections, and retries the attempt clean-up after `ApplyNotRecorded`.
Each schedule is host-tested on its own in `crates/juggler/src/ota/runtime/report.rs`; their interleaving lives in the IDF reporter loop and is only observable here.

1. During P17, between the broker restart and the report's arrival, send a burst: `for i in 1 2 3 4 5 6; do pub -m 'not-json'; done`.

**Pass:** `rolled_back` is still delivered, and published `command_invalid` count plus the `dropped rejections` increase equals 6.

## Host scenarios

Record the library commit on which `just test-ota` passed for each.

| ID | What it proves                                                                                                                                                                                                | Tests                                                                                                     |
|:---|:--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|:----------------------------------------------------------------------------------------------------------|
| H1 | A reset between `mark_valid` and the attempt clear is completed by the next boot without a second `applied`                                                                                                   | `crates/juggler/src/ota/persist/tests/crash_matrix.rs`                                                    |
| H2 | A crash after any single NVS write of any sequence ends in a documented state, never a wedge                                                                                                                  | `crash_matrix.rs`, `counter_loss.rs`; physically, repeat P18 and P19 with different cut-off moments       |
| H3 | `ApplyNotRecorded`: `applied` is published, offers answer `attempt_unresolved` and are retained, the reporter retries `complete_attempt` after 1, 2, 4, 8 s and then every 10 s, admission reopens on success | `report.rs` (`CompletionRetry`), `store_ops.rs` (`retry_completion`), `health.rs`                         |
| H4 | A failed-closed boot keeps admission closed even with a released slot                                                                                                                                         | `health.rs` `a_failed_closed_boot_stays_closed_even_with_a_released_slot`, `persist/tests/fail_closed.rs` |
| H5 | Corrupt records are repaired by the `repair` command (`{"status":"repaired","records":<n>}`)                                                                                                                  | `persist/tests/self_heal.rs`, `worker.rs` `a_repair_clears_the_fail_closed_flags_it_caused`               |

## Evidence record

Record every scenario separately; there is no overall "tested" label.

| Field                          | What to record                                                   |
|:-------------------------------|:-----------------------------------------------------------------|
| Scenario                       | ID from the matrix                                               |
| Library commit                 | `git rev-parse --short HEAD` of rustyfarian-network              |
| Consumer commit                | Commit of the firmware under test                                |
| Board and partition layout     | e.g. ESP32-C3 Super Mini 4 MB, `partitions.ota.csv` 2 × 0x1E0000 |
| Timing configuration           | Defaults, or every override                                      |
| Slots before and after         | The `[ota] slots:` lines                                         |
| Expected and observed statuses | The JSON from the scenario and from `mosquitto_sub`              |
| Method                         | Physical power loss, RST, or host fault injection (test name)    |
| Result                         | pass, fail (with cause) or pending                               |

### Results

Copy this table into your results file and update one cell per run.

| Scenario | Type                 | Example | rgb-clock | Watchtower v2 |
|:---------|:---------------------|:--------|:----------|:--------------|
| P1       | physical             | pending | pending   | pending       |
| P2       | physical             | pending | pending   | pending       |
| P3       | physical             | pending | pending   | pending       |
| P4       | physical             | pending | pending   | pending       |
| P5       | physical             | pending | pending   | pending       |
| P6       | physical             | pending | pending   | pending       |
| P7       | physical             | pending | pending   | pending       |
| P8       | physical             | pending | pending   | pending       |
| P9       | physical             | pending | pending   | pending       |
| P10      | physical             | pending | pending   | pending       |
| P11      | physical             | pending | pending   | pending       |
| P12      | physical             | pending | pending   | pending       |
| P13      | physical             | pending | pending   | pending       |
| P14      | physical             | pending | pending   | pending       |
| P15      | physical             | pending | pending   | pending       |
| P16      | physical             | pending | pending   | pending       |
| P17      | physical             | pending | pending   | pending       |
| P18      | physical             | pending | pending   | pending       |
| P19      | physical             | pending | pending   | pending       |
| P20      | physical             | pending | pending   | pending       |
| P21      | physical observation | pending | pending   | pending       |
| H1       | host                 | pending | n/a       | n/a           |
| H2       | host + physical      | pending | pending   | pending       |
| H3       | host                 | pending | n/a       | n/a           |
| H4       | host                 | pending | n/a       | n/a           |
| H5       | host                 | pending | n/a       | n/a           |

## Failure modes

| Symptom                                                                  | Cause                                                                                    | Fix                                                                              |
|:-------------------------------------------------------------------------|:-----------------------------------------------------------------------------------------|:---------------------------------------------------------------------------------|
| `failed{manifest_fetch}` for every update                                | URLs point at `localhost` or a wrong `OTA_HOST`/`OTA_PORT`, or the server is not running | Set both, rebuild with `img`, restart `just ota-serve` with the same `OTA_PORT`  |
| `just ota-serve` exits with "Address already in use"                     | Another server holds the port                                                            | Pick a free `OTA_PORT`, rebuild the images, restart the server                   |
| An update runs again after every reconnect                               | The command was published retained                                                       | Clear it with `mosquitto_pub -r -n -t "$T/command"`; never publish with `-r`     |
| Every update answers `up_to_date` or `downgrade`                         | The image was built without `FIRMWARE_VERSION`, or the campaign reuses old versions      | Build with `img <version>`; increase `M` for a new campaign                      |
| `just ota-image` fails with "larger than the … OTA slot"                 | The image exceeds 1.875 MiB                                                              | Reduce the image or change `partitions.ota.csv`                                  |
| An unhealthy image is never rolled back                                  | It was flashed instead of pushed, so it was never pending verification                   | Push it with `cmd`                                                               |
| Every later image is unhealthy or rolls back early                       | `OTA_DEMO_UNHEALTHY` or `OTA_FAILURE_DEADLINE_SECS` is still exported                    | `unset` both and rebuild                                                         |
| Startup error naming `OTA_DEMO_UNHEALTHY` or `OTA_FAILURE_DEADLINE_SECS` | Value other than `1`, or a deadline not above the 30 s dwell                             | Fix the value and rebuild                                                        |
| Offers answer `report_pending`                                           | A `rolled_back` report is waiting for the broker                                         | Restore the broker connection; the report is retried every 10 s                  |
| Offers answer `attempt_unresolved`                                       | An attempt record is still open (e.g. after `ApplyNotRecorded`)                          | Wait for the background clean-up or reset the board; a reflash does not clear it |
| Every boot is failed closed, nothing is admitted                         | Corrupt OTA records in NVS                                                               | `pub -m '{"action":"repair"}'` until it answers `repair_not_needed`              |

## For consumers

- Run P1–P21 on your firmware with your topics, your health predicate and your own flash recipe; replace the `idf_c3_ota_runtime` commands accordingly.
- Use your own version scheme, but keep the ladder strictly increasing per campaign.
- Your predicate decides validation: document what it checks and how to make it fail on purpose (the counterpart of `OTA_DEMO_UNHEALTHY`).
- Record H1–H5 once per library commit; they do not depend on your firmware.
