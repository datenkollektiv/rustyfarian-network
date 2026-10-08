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
- Python 3 for the image server.
- The workspace `.env` file with `WIFI_SSID`, `WIFI_PASS`, and `MQTT_HOST`.

## Environment variables

All `just` OTA recipes use the following variables, loaded from `.env` via `dotenv-load`:

| Variable           | Default                   | Notes                                                                                                        |
|:-------------------|:--------------------------|:-------------------------------------------------------------------------------------------------------------|
| `MQTT_HOST`        | required                  | Broker hostname or IP                                                                                        |
| `MQTT_PORT`        | 1883                      | Broker port                                                                                                  |
| `MQTT_USER`        | unset                     | Optional authentication username                                                                             |
| `MQTT_PASS`        | unset                     | Optional authentication password                                                                             |
| `OTA_TOPIC_PREFIX` | `rustyfarian/example/ota` | MQTT topic prefix for commands and status                                                                    |
| `OTA_HOST`         | auto-detected             | LAN address of the test machine (never `localhost`); auto-detected from the default-route interface if unset |
| `OTA_PORT`         | 8000                      | HTTP server port for image delivery                                                                          |

Never put `FIRMWARE_VERSION`, `OTA_DEMO_UNHEALTHY` or `OTA_FAILURE_DEADLINE_SECS` into `.env` — the recipes set them per build.

If a direnv `.envrc` exports the same key as `.env`, it overrides the file (dotenv never overwrites an existing variable).
Clear any conflicting direnv exports and reload.

## Broker

P17 and P21 need a broker you can stop and start, so run the whole campaign on the local test broker.

```bash
just mosquitto up
```

Set `MQTT_HOST` and `MQTT_PORT` in `.env` to the two values it prints, before flashing the baseline.
The device compiles the broker in at build time and the `ota-*` recipes read the same variables, so both always talk to the same broker.
`just mosquitto down` stops it, `just mosquitto status` shows it, and it is LAN-only and anonymous.

## Setup (three terminals)

All `just` recipes use `.env` and the `MQTT_HOST` must be reachable before starting.
Run `just doctor` to verify the broker is reachable.

**Terminal 1:** image server (runs in the background for the whole campaign)

```bash
just ota-serve
```

**Terminal 2:** status subscription with ISO timestamps (runs for the whole campaign)

```bash
just ota-sub
```

**Terminal 3:** serial monitor (reattach as needed after resets)

```bash
just monitor
```

**Terminal 4:** building and publishing (where you run all scenario steps)

Use plain `just` recipes for all scenario steps — no shell variables, no manual `mosquitto_pub` calls.

`just ota-serve` runs in the foreground until stopped.
Scenarios that need another server mode (P10, P11, P12, P18) switch it in terminal 1: stop it with Ctrl-C and start the new mode there; every other command runs in terminal 4.

## Flashing vs. recovery

- `just ota-flash <version>` is for the baseline only (P1).
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
- Any status can be delivered more than once, not only `rolled_back`, because statuses are published at QoS 1 and esp-mqtt resends a publish whose PUBACK is not handled within 1 s (e.g. while the flasher erases the target partition, about 6–7 s after `downloading`).
- Consumers deduplicate `rolled_back` by `epoch` + `attempt_id`; other statuses are idempotent.
- An image is validated automatically: the health predicate (in the example `!unhealthy && mqtt.is_connected()`) must hold for the 30 s minimum dwell; if it never does, the failure deadline (default 120 s from the start of `main`) rolls the image back with up to 3 attempts 5 s apart.

## Version ladder

Each physical scenario starts from the state the previous one left behind.
On a board that already ran a campaign, use the next major version (2.x.0, …) everywhere, because the refused version and other OTA records survive a reflash.
On a fresh or erased board start at 1.

| Version      | Built in | Role                                                          |
|:-------------|:---------|:--------------------------------------------------------------|
| 1.0.0        | P1       | Baseline, flashed                                             |
| 1.1.0        | P2       | First update                                                  |
| 1.2.0        | P6       | Second update, later rolled back by the operator              |
| 1.3.0        | P9       | Validates a newer version, clearing the refusal of 1.2.0      |
| 1.4.0        | P10      | Download-timeout and manifest-stall target, then burst update |
| 1.5.0        | P13      | Unhealthy image, 60 s deadline                                |
| 1.6.0        | P14      | Unhealthy image, default 120 s deadline                       |
| 1.7.0        | P15      | Early command on a validated boot                             |
| 1.8.0, 1.9.0 | P16      | Pending-verification gate and retained update                 |
| 1.10.0       | P17      | Offer during an undelivered report                            |
| 1.11.0       | P18      | Power loss during download                                    |
| 1.12.0       | P19      | Reset during pending verification                             |

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
| P11 | Manifest stall                                          | Physical                    | `failed{manifest_fetch}` after about 10 s          |
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

Flash the baseline image.

```bash
just ota-flash 1.0.0
```

Then monitor the serial output with `just monitor`.

**Expected:** `[ota] slots: running=ota_0 (Unknown)` or `(Valid)` (a freshly erased `otadata` has no image state yet), `[ota] health policy finished: NothingToVerify`, and `OTA: admission open true`.
No status is published.

### P2: First update

Build and publish the first update.

```bash
just ota-image 1.1.0
just ota-cmd 1.1.0
```

**Expected statuses:**

```json
{"status":"downloading"}
{"status":"swap_pending"}
{"status":"applied","version":"1.1.0"}
```

`applied` follows at least 30 s after the reboot.
**Slots:** before `running=ota_0`; after the reboot `running=ota_1 (PendingVerify)`; after the next reset `running=ota_1 (Valid)`.

### P3: Up to date

Offer the same version again.

```bash
just ota-cmd 1.1.0
```

**Expected:** `{"status":"failed","reason":"up_to_date"}`; no `downloading`, no reboot.

### P4: Invalid command

Publish raw JSON that is not a valid command.

```bash
just ota-pub not-json
```

**Expected:** `{"status":"failed","reason":"command_invalid"}`; nothing else changes.

### P5: Downgrade

Build the baseline version as an image and offer it.

```bash
just ota-image 1.0.0
just ota-cmd 1.0.0
```

**Expected:** `{"status":"failed","reason":"downgrade"}`; no `downloading`.

### P6: Wrong target, bad checksum, second update

Build the second version, test with wrong target and bad checksum, then publish the real update.

```bash
just ota-image 1.2.0
just ota-cmd-tampered 1.2.0 target
just ota-cmd-tampered 1.2.0 checksum
just ota-cmd 1.2.0
```

**Expected statuses:**

```json
{"status":"failed","reason":"target_mismatch"}
{"status":"downloading"}
{"status":"failed","reason":"checksum_mismatch"}
{"status":"downloading"}
{"status":"swap_pending"}
{"status":"applied","version":"1.2.0"}
```

After the checksum failure the serial line shows `[ota] the failed attempt was cleared` and the slots are unchanged.
**Slots after:** `running=ota_0` with 1.2.0 (the baseline was overwritten); `ota_1` still holds 1.1.0.

### P7: Operator rollback

Roll back from the current version.

```bash
just ota-rollback 1.2.0
```

**Expected:** the device reboots at once into 1.1.0 (`running=ota_1`), then:

```json
{"status":"rolled_back","reason":"operator","attempt_id":<n>,"epoch":<e>}
```

### P8: Refused re-offer

Offer the rolled-back version again.

```bash
just ota-cmd 1.2.0
```

**Expected:** `{"status":"failed","reason":"previously_rolled_back"}`.

### P9: Refusal cleared by a newer version

Build and offer a new version that clears the refusal.

```bash
just ota-image 1.3.0
just ota-cmd 1.3.0
```

**Expected:** `downloading`, `swap_pending`, `applied` for 1.3.0.
Validating 1.3.0 removes the refusal of 1.2.0; it cannot be observed directly because 1.2.0 is now a `downgrade`.

### P10: Download timeout

Build the image.

```bash
just ota-image 1.4.0
```

In terminal 1, stop the running server (Ctrl-C) and start it in stall mode (it stops sending after 100 KiB, so the device's read times out about 60 s later):

```bash
just ota-serve 0 102400
```

Offer the update.

```bash
just ota-cmd 1.4.0
```

Wait for the download timeout (about 60 s).

**Expected:** `{"status":"downloading"}`, then about 60 s later `{"status":"failed","reason":"download_timeout"}`.
No reboot; `[ota] the failed attempt was cleared`; slots unchanged.

### P11: Manifest stall

In terminal 1, stop the running server (Ctrl-C) and start it in manifest-stall mode (it stops sending after the first byte):

```bash
just ota-serve 0 1
```

Offer the update.

```bash
just ota-cmd 1.4.0
```

**Expected:** only `{"status":"failed","reason":"manifest_fetch"}`, after about 10 s; no `downloading`.

### P12: Command burst during a download

Restart the server in slow mode and offer an update, then send a burst of repair commands while the download is in progress.
Send the burst only after the serial log shows `[ota] … OTA write session started` (the partition erase is over).

In terminal 1, stop the running server (Ctrl-C) and start it in slow mode (20 kB/s, about 80 s for the image):

```bash
just ota-serve 20000
```

Offer the update.

```bash
just ota-cmd 1.4.0
```

Wait for `downloading` in the status terminal, then for `[ota] … OTA write session started` in the serial log, and send the burst.

```bash
just ota-burst 6 repair
```

**Expected:** `downloading`, several `{"status":"failed","reason":"busy"}`, then `swap_pending` and `applied` for 1.4.0.
Up to four rejections can wait for the reporter; extra ones are counted instead of published.
**Pass:** published `busy` count plus the increase of `dropped rejections` in the serial line equals 6.

In terminal 1, stop the server (Ctrl-C) and start the normal one again:

```bash
just ota-serve
```

**Note:** a burst during the erase (before `OTA write session started`) can publish one status twice (QoS 1 resend), so the published count is an upper bound.

### P13: Unhealthy image, 60 s deadline

Build an unhealthy image with a 60 s deadline and offer it.
Never flash this image — a flashed image is not pending verification and is never rolled back.

```bash
just ota-image-unhealthy 1.5.0 60
just ota-cmd 1.5.0
```

**Expected:** `downloading`, `swap_pending`, reboot, a loud `OTA_DEMO_UNHEALTHY=1` error on serial, no `applied`.
About 60 s after boot the rollback starts (at 61.5 s observed), the device reboots into 1.4.0, then:

```json
{"status":"rolled_back","reason":"health_deadline","attempt_id":<n>,"epoch":<e>}
```

A re-offer (`just ota-cmd 1.5.0`) answers `failed{previously_rolled_back}`.
Send this re-offer before P14: the refusal record holds only the most recent rolled-back version, so after P14 1.5.0 is offerable again.

### P14: Unhealthy image, default deadline

Build an unhealthy image with the default 120 s deadline and offer it.

```bash
just ota-image-unhealthy 1.6.0
just ota-cmd 1.6.0
```

**Expected:** as P13, with the rollback about 120 s after boot (at 121.5 s observed).

Rebuild the next image without the unhealthy flag.

```bash
just ota-image 1.7.0
```

### P15: Startup admission on a validated boot

Build the next version and press RST on the board.
Send the command as soon as the serial line shows the MQTT connection.

```bash
just ota-image 1.7.0
```

Press RST, then quickly run:

```bash
just ota-cmd 1.7.0
```

**Expected:** `downloading` straight away, no `pending_verify`, then `swap_pending` and `applied` for 1.7.0.
The boot is validated (`slot released: true`), so admission opens when the runtime starts.

Alternatively, you can re-attach the serial monitor (`just monitor`) to reset the chip while monitoring the output.

### P16: Startup admission while pending verification

Build two new versions and offer them in sequence during a 30 s dwell.
Commands sent within milliseconds of each other hit the single busy slot, so send them about 2 s apart for clear `pending_verify` responses.

```bash
just ota-image 1.8.0
just ota-image 1.9.0
```

Offer the first and wait for the reboot.

```bash
just ota-cmd 1.8.0
```

Wait about 5 s for the reboot to complete (serial shows `running=… (PendingVerify)`).
Then send the second offer, a rollback request and a repair request, waiting about 2 s after each.

```bash
just ota-cmd 1.9.0
```

```bash
just ota-rollback 1.8.0
```

```bash
just ota-repair
```

**Expected statuses after the reboot:**

```json
{"status":"failed","reason":"pending_verify"}
{"status":"failed","reason":"pending_verify"}
{"status":"failed","reason":"pending_verify"}
{"status":"applied","version":"1.8.0"}
{"status":"downloading"}
{"status":"swap_pending"}
{"status":"applied","version":"1.9.0"}
```

The update (1.9.0) is retained and runs after 1.8.0 is applied; rollback and repair are answered and dropped.

### P17: Rollback report across a broker outage

Build a new version and arrange a rollback across a broker disconnect.

```bash
just ota-image 1.10.0
```

Publish a rollback and stop the broker right after it; the device reconnects about 4 s after its reboot, so run both together.

```bash
just ota-rollback 1.9.0 && just mosquitto down
```

Serial shows `[ota] boot reconciliation: ReportedRollback, slot released: true, report pending: true`, then `Error transport connect` about every 15 s while the broker is down.
Wait at least 10 s (one report retry), then start the broker again.

```bash
just mosquitto up
```

Send the next offer as soon as the serial log shows `[mqtt] connected`.

```bash
just ota-cmd 1.10.0
```

**Expected statuses:**

```json
{"status":"rolled_back","reason":"operator","attempt_id":<n>,"epoch":<e>}
{"status":"failed","reason":"report_pending"}
{"status":"downloading"}
{"status":"swap_pending"}
{"status":"applied","version":"1.10.0"}
```

`report_pending` appears only if the offer arrives before the report is acknowledged; the offer is retained and runs afterwards.

### P18: Power loss during download

Build a new image and unplug the board halfway through the download.
Restart with the board unplugged from power (no reflash).

```bash
just ota-image 1.11.0
```

In terminal 1, stop the running server (Ctrl-C) and start it in slow mode (20 kB/s):

```bash
just ota-serve 20000
```

Offer the update.

```bash
just ota-cmd 1.11.0
```

Wait for `downloading` to appear, then unplug the board's power.
Plug it back in (do not reflash or reset).

**Expected:** the old image runs (`[ota] slots: running=<old slot> (Valid)`), `[ota] boot reconciliation: Settled(NoRequest), slot released: true, report pending: false` (the interrupted attempt is cleared silently), and no `rolled_back`.

Then complete the update with a normal server.
In terminal 1, stop the server (Ctrl-C) and start the normal one again:

```bash
just ota-serve
```

```bash
just ota-cmd 1.11.0
```

### P19: Reset during pending verification

Build a new image, offer it, and reset the board while it is in pending verification.
The reset can be done with RST, or by quitting and restarting `just monitor`.

```bash
just ota-image 1.12.0
just ota-cmd 1.12.0
```

After the reboot, while the serial line shows `PendingVerify` and before `applied`, press RST (or restart `just monitor`).

**Expected:** the bootloader rejects the unverified image and boots the previous one, then:

```json
{"status":"rolled_back","reason":"bootloader","attempt_id":<n>,"epoch":<e>}
```

Then offer the same version again to confirm it is refused.

```bash
just ota-cmd 1.12.0
```

The response should be `failed{previously_rolled_back}`.

### P20: Repair on a healthy device

Publish a repair action on a device with no corruption.

```bash
just ota-repair
```

**Expected:** `{"status":"failed","reason":"repair_not_needed"}`; no reboot.

### P21: Rejections and report retries on the shared reporter thread

The reporter thread delivers `rolled_back`, publishes rejections, and retries the attempt clean-up after `ApplyNotRecorded`.
Each schedule is host-tested on its own in `crates/juggler/src/ota/runtime/report.rs`; their interleaving lives in the IDF reporter loop and is only observable here.

During P17, send the burst as soon as the serial log shows `[mqtt] connected` after `just mosquitto up`, before `just ota-cmd 1.10.0`.
Commands sent while the device is still offline are lost (QoS 0), so wait for the reconnect.

```bash
just ota-burst 6 invalid
```

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

### 2026-10-07 Campaign Results (ESP32-C3 rev 0.4)

**Board:** ESP32-C3 rev 0.4, 4 MB, MAC `84:fc:e6:00:e3:98`.

**Library state:** HEAD of main on 2026-10-07, plus uncommitted justfile/runbook changes.

**Image size:** 1,651,056 bytes = 84% of the 1.875 MiB OTA slot.

**Performance observations:**

<details>
<summary>Expand for detailed timings</summary>

- Download time at full speed: about 19 s.
- `applied` arrives about 32 s after the reboot.
- Health rollbacks fired at 61.5 s (60 s deadline) and 121.5 s (default 120 s).
- `download_timeout` fired 67 s after `downloading`.

</details>

**PASS scenarios (16):** P1–P11, P13, P14, P15, P19, P20.

**Re-run scenarios (2):**
- P12: PASS (6+0, no resends). The first run measured 6+1 because QoS 1 resent one status during the partition erase; this re-run avoided that by timing the burst after `OTA write session started`.
- P16: PASS (1.13.0/1.14.0 used). The first run sent commands back-to-back within milliseconds, hitting the single busy slot; re-running with 2 s gaps between commands yielded three `pending_verify` responses before the update ran.

**Skipped scenarios (3):**
- P17: not run (broker outage scenario needs SRE).
- P18: not run (power-cut scenario pending).
- P21: not run (depends on P17).

### 2026-10-08 Campaign Results (same board)

**Library state:** HEAD of main on 2026-10-08 plus the uncommitted bug 005 fix (MQTT auto-reconnect).

**Broker:** local `just mosquitto` broker; versions 1.18.0 (flashed), 1.19.0 and 1.20.0.

**PASS scenarios (3):**
- P18: power cut about 30 s into the 1.21.0 download (after `attempt 28 begun` and the partition erase); the board came back on 1.20.0 with `Settled(NoRequest), slot released: true, report pending: false`, no `rolled_back`, and MQTT reconnected at 4.4 s; the re-offer ran to `applied{1.21.0}`.
- P17: `rolled_back{operator}` delivered 9 s after the broker came back; the offer then ran to `applied{1.20.0}`; no `report_pending` because the report was acknowledged first.
- P21: burst right after the reconnect gave six `command_invalid`, zero dropped, and the report was still delivered.

**Found:** bug 005 — without the fix the device never reconnects after a broker outage (first attempt with the cluster broker on 1.15.0 stayed offline for 9 minutes).

**All physical scenarios P1–P21 have now passed on this board** (P1–P16, P19, P20 on 2026-10-07 before the bug 005 fix and the recipe rework).

### 2026-10-08 Campaign 2 Results (same board, end to end)

**Library state:** `529428d` (bug 005 fix and the `ota-*` / `mosquitto` recipes), versions 2.0.0–2.13.0, local `just mosquitto` broker.

**PASS (21 of 21):** P1–P21, run in one campaign by a driver script that only sequences the runbook's recipes.

<details>
<summary>Expand for notes</summary>

- P10: `download_timeout` after 68 s.
- P12: six `busy`, zero dropped, burst sent after `OTA write session started`.
- P13: the first re-offer was sent after P14 and installed 2.5.0 again (correct: the refusal record holds only the latest rollback); re-run with 2.13.0, the immediate re-offer answered `previously_rolled_back`.
- P15: the reset hit that reinstalled 2.5.0 while pending verification, so the bootloader rolled it back (`rolled_back{bootloader}`); the early 2.7.0 command on the following boot was still accepted and applied.
- P17/P21: report delivered after the broker restart; five `command_invalid` plus one dropped rejection.
- P18: unplugged 8 s into a 6.5 kB/s download; old image, `Settled(NoRequest)`, re-offer applied.
- Driver bug found and fixed mid-run: `set -o pipefail` with `grep -q` misreports matches on large logs (SIGPIPE); the driver resumed at P14 without touching the device.

</details>

## Failure modes

| Symptom                                                                  | Cause                                                                                    | Fix                                                                                                            |
|:-------------------------------------------------------------------------|:-----------------------------------------------------------------------------------------|:---------------------------------------------------------------------------------------------------------------|
| `failed{manifest_fetch}` for every update                                | URLs point at `localhost` or a wrong `OTA_HOST`/`OTA_PORT`, or the server is not running | Set both in `.env`, rebuild with `just ota-image <version>`, restart `just ota-serve` with the same `OTA_PORT` |
| `just ota-serve` exits with "Address already in use"                     | Another server holds the port                                                            | Pick a free `OTA_PORT` in `.env`, rebuild the images, restart the server                                       |
| An update runs again after every reconnect                               | The command was published retained                                                       | Clear it with `mosquitto_pub -r -n -t "$OTA_TOPIC_PREFIX/command"`; never publish with `-r`                    |
| Every update answers `up_to_date` or `downgrade`                         | The image was built without `FIRMWARE_VERSION`, or the campaign reuses old versions      | Build with `just ota-image <version>`; increase the major version for a new campaign on the same board         |
| `just ota-image` fails with "larger than the … OTA slot"                 | The image exceeds 1.875 MiB                                                              | Reduce the image or change `partitions.ota.csv`                                                                |
| An unhealthy image is never rolled back                                  | It was flashed instead of pushed, so it was never pending verification                   | Push it with `just ota-cmd <version>`                                                                          |
| Every later image is unhealthy or rolls back early                       | `OTA_DEMO_UNHEALTHY` or `OTA_FAILURE_DEADLINE_SECS` is still exported in the environment | Unset both and rebuild                                                                                         |
| Startup error naming `OTA_DEMO_UNHEALTHY` or `OTA_FAILURE_DEADLINE_SECS` | Value other than `1`, or a deadline not above the 30 s dwell                             | Fix the value and rebuild                                                                                      |
| Offers answer `report_pending`                                           | A `rolled_back` report is waiting for the broker                                         | Restore the broker connection; the report is retried every 10 s                                                |
| Offers answer `attempt_unresolved`                                       | An attempt record is still open (e.g. after `ApplyNotRecorded`)                          | Wait for the background clean-up or reset the board; a reflash does not clear it                               |
| Every boot is failed closed, nothing is admitted                         | Corrupt OTA records in NVS                                                               | `just ota-repair` until it answers `repair_not_needed`                                                         |

## For consumers

- Run P1–P21 on your firmware with your topics, your health predicate and your own flash recipe; replace the `idf_c3_ota_runtime` commands accordingly.
- Use your own version scheme, but keep the ladder strictly increasing per campaign.
- Your predicate decides validation: document what it checks and how to make it fail on purpose (the counterpart of `OTA_DEMO_UNHEALTHY`).
- Record H1–H5 once per library commit; they do not depend on your firmware.
