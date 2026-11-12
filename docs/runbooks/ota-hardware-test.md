# Runbook: OTA Hardware Test (Library Runtime)

Run this after any change to the OTA download path, wire contract, record store or runtime, and before every release.
It pushes real firmware updates over MQTT to a device and records one result per scenario.

**Reference device:** an ESP32-C3 with 4 MB flash running the `idf_c3_ota_runtime` example.

**Consumers:** run the same scenario matrix on your own firmware; keep only your health predicate, topics, and build and flash recipes in your own runbook, and link the generic parts here: <https://github.com/datenkollektiv/rustyfarian-network/blob/main/docs/runbooks/ota-hardware-test.md>.

## Scope

- **Physical scenarios (P1–P21)** run end to end on the device and cover download hardening, the wire contract, the record store and the runtime.
  P2, P7, P13, P14 and P17 are chained in the bench check `c3/ota-runtime` (`bench/c3/ota-runtime.toml`); the others run by hand from this file.
- **Host scenarios (H1–H5)** need injected faults that cannot be produced by hand (a crash after one specific NVS write, a persistently failing NVS, corrupt records); `just test-ota` covers them, so record the library commit you ran them on.
- **Not covered:** HTTPS (plain `http://` only, ADR 011), fleet backends, signature verification, partition tables with more than two OTA slots.

## Setup

- An ESP32-C3 with 4 MB flash on USB, Python 3 for the image server, `mosquitto_pub` and `mosquitto_sub`, and `.env` with `WIFI_SSID`, `WIFI_PASS` and `MQTT_HOST`.
- P17 and P21 need a broker you can stop and start, so run the whole campaign on `just mosquitto up` (LAN-only, anonymous; `down` and `status` stop and show it) and set `MQTT_HOST` and `MQTT_PORT` in `.env` to the two values it prints before flashing the baseline; the device and the `ota-*` recipes then talk to the same broker.
- Never put `FIRMWARE_VERSION`, `OTA_DEMO_UNHEALTHY` or `OTA_FAILURE_DEADLINE_SECS` into `.env`; the recipes set them per build.
- A direnv `.envrc` exporting the same key overrides `.env` (dotenv never overwrites an existing variable); clear such exports and reload.
- `just doctor` checks that the broker is reachable.

| Variable           | Default                   | Notes                                                                                |
|:-------------------|:--------------------------|:-------------------------------------------------------------------------------------|
| `MQTT_HOST`        | required                  | Broker hostname or IP                                                                |
| `MQTT_PORT`        | 1883                      | Broker port                                                                          |
| `MQTT_USER`        | unset                     | Optional username                                                                    |
| `MQTT_PASS`        | unset                     | Optional password                                                                    |
| `OTA_TOPIC_PREFIX` | `rustyfarian/example/ota` | Topic prefix for commands and status                                                 |
| `OTA_HOST`         | auto-detected             | LAN address of the test machine, never `localhost`; detected from the default route  |
| `OTA_PORT`         | 8000                      | HTTP port for image delivery                                                         |

Terminals: 1 `just ota-serve` (image server, foreground), 2 `just ota-sub` (statuses with ISO timestamps), 3 `just monitor` (reattach after resets), 4 every scenario step.
Use plain `just` recipes for all steps, no shell variables and no manual `mosquitto_pub`.
P10, P11, P12 and P18 switch the server mode in terminal 1: Ctrl-C, then start the new mode.

## Flashing vs. recovery

- `just ota-flash <version>` is for the baseline only (P1): it writes the two-slot `partitions.ota.csv` (2 × 1.875 MiB), sets 4 MB flash and erases only `otadata`, so the next boot starts from `ota_0`.
- NVS survives a reflash (attempt records, an undelivered report, the refused version), so a reflash does not clear `attempt_unresolved` or `report_pending`, and a reflash after an update switched the boot slot reads as a rollback on the next boot (limitation documented in `crates/juggler/src/ota/reconcile.rs`).
- Interrupted-update and recovery scenarios use RST or a power cycle, never a reflash, so the boot evidence stays intact.

## Reading the evidence

Serial lines, once per boot, in this order:

- `[ota] boot slot noted: …`
- `[ota] slots: running=<label> (<state>), next boot=<label>, update=<label> (<state>)`; labels `ota_0`, `ota_1`, `factory`; states `Valid`, `PendingVerify`, `Invalid`, `Unknown`.
- `[ota] boot reconciliation: <disposition>, slot released: <bool>, report pending: <bool>`
- `[ota] health policy finished: <verdict>`: `NothingToVerify` when the running slot was not pending, `Applied` when it has just been validated.
- Every 30 s: `OTA: admission open <bool>, stack marks …, dropped rejections <n>`.

The ESP-IDF bootloader prints the partition it loaded: offset `0x20000` is `ota_0`, `0x200000` is `ota_1`.

| Status                                                                           | Published by                    |
|:---------------------------------------------------------------------------------|:--------------------------------|
| `downloading`, `swap_pending`, worker-decided `failed`, `repaired`               | worker thread                   |
| `applied`                                                                        | health policy (app main thread) |
| `rolled_back`, intake `failed` (`busy`, `command_invalid`, `worker_unavailable`) | reporter thread                 |

Rules in every scenario:

- The manifest is always fetched first; `up_to_date`, `downgrade`, `previously_rolled_back` and `target_mismatch` are decided after the fetch, and only the firmware download is skipped.
- A blocked offer (`report_pending`, `attempt_unresolved`, or `pending_verify` for an update) publishes `failed` once, is retained, and later runs with `downloading` and no new `failed`.
- `attempt_id`, `epoch` and `records` are JSON numbers.
- Any status can arrive more than once: statuses are QoS 1, and esp-mqtt resends a publish whose PUBACK is not handled within 1 s (e.g. while the flasher erases the target partition, about 6–7 s after `downloading`); consumers deduplicate `rolled_back` by `epoch` + `attempt_id`, other statuses are idempotent.
- An image is validated automatically: the health predicate (in the example `!unhealthy && mqtt.is_connected()`) must hold for the 30 s minimum dwell; if it never does, the failure deadline (default 120 s from the start of `main`) rolls the image back with up to 3 attempts 5 s apart.

## Version ladder

Each physical scenario starts from the state the previous one left behind.
On a board that already ran a campaign use the next major version (2.x.0, …) everywhere, because the refused version and other OTA records survive a reflash; on a fresh or erased board start at 1.
The bench check `c3/ota-runtime` erases the flash and uses its own subset ladder (1.0.0, 1.1.0, 1.2.0, 1.5.0, 1.6.0, 1.9.0, 1.10.0).

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
| P2  | First update                                            | Physical, `c3/ota-runtime`  | `downloading`, `swap_pending`, `applied`           |
| P3  | Up to date                                              | Physical                    | `failed{up_to_date}`                               |
| P4  | Invalid command                                         | Physical                    | `failed{command_invalid}`                          |
| P5  | Downgrade                                               | Physical                    | `failed{downgrade}`                                |
| P6  | Wrong target, bad checksum, second update               | Physical                    | `target_mismatch`, `checksum_mismatch`, `applied`  |
| P7  | Operator rollback                                       | Physical, `c3/ota-runtime`  | `rolled_back{operator}`                            |
| P8  | Refused re-offer                                        | Physical                    | `failed{previously_rolled_back}`                   |
| P9  | Refusal cleared by a newer version                      | Physical                    | `applied`                                          |
| P10 | Download timeout                                        | Physical                    | `failed{download_timeout}` after ~60 s             |
| P11 | Manifest stall                                          | Physical                    | `failed{manifest_fetch}` after about 10 s          |
| P12 | Command burst during a download                         | Physical                    | `busy` rejections, then `applied`                  |
| P13 | Unhealthy image, 60 s                                   | Physical, `c3/ota-runtime`  | `rolled_back{health_deadline}`                     |
| P14 | Unhealthy image, 120 s default                          | Physical, `c3/ota-runtime`  | `rolled_back{health_deadline}`                     |
| P15 | Startup admission on a validated boot                   | Physical                    | Early command accepted                             |
| P16 | Startup admission while pending verification            | Physical                    | `pending_verify` ×3, retained update runs          |
| P17 | Rollback report across a broker outage                  | Physical, `c3/ota-runtime`  | Report delivered after reconnect                   |
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

```bash
just ota-flash 1.0.0
```

**Expected:** `[ota] slots: running=ota_0 (Unknown)` or `(Valid)` (a freshly erased `otadata` has no image state yet), `[ota] health policy finished: NothingToVerify`, `OTA: admission open true`; no status.

### P2: First update

Bench: `c3/ota-runtime` (`just ota-image 1.1.0`, `just ota-cmd 1.1.0`).
`applied` follows at least 30 s after the reboot; slots: before `running=ota_0`, after the reboot `running=ota_1 (PendingVerify)`, after the next reset `running=ota_1 (Valid)`.

### P3: Up to date

```bash
just ota-cmd 1.1.0
```

**Expected:** `{"status":"failed","reason":"up_to_date"}`; no `downloading`, no reboot.

### P4: Invalid command

```bash
just ota-pub not-json
```

**Expected:** `{"status":"failed","reason":"command_invalid"}`; nothing else changes.

### P5: Downgrade

```bash
just ota-image 1.0.0
just ota-cmd 1.0.0
```

**Expected:** `{"status":"failed","reason":"downgrade"}`; no `downloading`.

### P6: Wrong target, bad checksum, second update

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

After the checksum failure serial shows `[ota] the failed attempt was cleared` and the slots are unchanged.
**Slots after:** `running=ota_0` with 1.2.0 (the baseline was overwritten); `ota_1` still holds 1.1.0.

### P7: Operator rollback

Bench: `c3/ota-runtime` (`just ota-rollback 1.2.0`): the device reboots at once into 1.1.0 (`running=ota_1`) and publishes `rolled_back{operator}` with `attempt_id` and `epoch`.

### P8: Refused re-offer

```bash
just ota-cmd 1.2.0
```

**Expected:** `{"status":"failed","reason":"previously_rolled_back"}`.

### P9: Refusal cleared by a newer version

```bash
just ota-image 1.3.0
just ota-cmd 1.3.0
```

**Expected:** `downloading`, `swap_pending`, `applied` for 1.3.0.
Validating 1.3.0 removes the refusal of 1.2.0; this cannot be observed directly because 1.2.0 is now a `downgrade`.

### P10: Download timeout

```bash
just ota-image 1.4.0
```

In terminal 1 start the server in stall mode (it stops after 100 KiB, so the device's read times out about 60 s later):

```bash
just ota-serve 0 102400
```

```bash
just ota-cmd 1.4.0
```

**Expected:** `{"status":"downloading"}`, about 60 s later `{"status":"failed","reason":"download_timeout"}`; no reboot, `[ota] the failed attempt was cleared`, slots unchanged.

### P11: Manifest stall

In terminal 1 start the server in manifest-stall mode (it stops after the first byte):

```bash
just ota-serve 0 1
```

```bash
just ota-cmd 1.4.0
```

**Expected:** only `{"status":"failed","reason":"manifest_fetch"}`, after about 10 s; no `downloading`.

### P12: Command burst during a download

In terminal 1 start the server in slow mode (20 kB/s, about 80 s for the image):

```bash
just ota-serve 20000
```

```bash
just ota-cmd 1.4.0
```

Wait for `downloading`, then for `[ota] … OTA write session started` in the serial log (the partition erase is over), then send the burst:

```bash
just ota-burst 6 repair
```

**Expected:** `downloading`, several `{"status":"failed","reason":"busy"}`, then `swap_pending` and `applied` for 1.4.0.
Up to four rejections can wait for the reporter; extra ones are counted instead of published.
**Pass:** the published `busy` count plus the increase of `dropped rejections` equals 6.
A burst during the erase can publish one status twice (QoS 1 resend), so the published count is an upper bound.
Afterwards restart the normal server in terminal 1 with `just ota-serve`.

### P13: Unhealthy image, 60 s deadline

Bench: `c3/ota-runtime` (`just ota-image-unhealthy 1.5.0 60`, `just ota-cmd 1.5.0`).
Never flash this image: a flashed image is not pending verification and is never rolled back.
The rollback starts about 60 s after boot (61.5 s observed).
Send the re-offer of 1.5.0 before P14, because the refusal record holds only the most recent rolled-back version; after P14 1.5.0 is offerable again.

### P14: Unhealthy image, default deadline

Bench: `c3/ota-runtime` (`just ota-image-unhealthy 1.6.0`, `just ota-cmd 1.6.0`); rollback about 120 s after boot (121.5 s observed).
Build the next image without the unhealthy flag.

### P15: Startup admission on a validated boot

```bash
just ota-image 1.7.0
```

Press RST (or re-attach `just monitor`, which resets the chip), and as soon as serial shows the MQTT connection run:

```bash
just ota-cmd 1.7.0
```

**Expected:** `downloading` straight away, no `pending_verify`, then `swap_pending` and `applied` for 1.7.0; the boot is validated (`slot released: true`), so admission opens when the runtime starts.

### P16: Startup admission while pending verification

Commands sent within milliseconds of each other hit the single busy slot, so send them about 2 s apart.

```bash
just ota-image 1.8.0
just ota-image 1.9.0
just ota-cmd 1.8.0
```

Wait about 5 s for the reboot (serial shows `running=… (PendingVerify)`), then send these three, about 2 s apart:

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

Bench: `c3/ota-runtime` (`just ota-rollback <v> && just mosquitto down`, at least 10 s down, `just mosquitto up`, `just ota-cmd <next>` as soon as serial shows `[mqtt] connected`).
The device reconnects about 4 s after its reboot, so run the rollback and the broker stop together; while the broker is down serial shows `Error transport connect` about every 15 s.
`{"status":"failed","reason":"report_pending"}` appears between `rolled_back` and `downloading` only if the offer arrives before the report is acknowledged; the offer is retained and runs afterwards.

### P18: Power loss during download

```bash
just ota-image 1.11.0
```

In terminal 1 start the server in slow mode (20 kB/s):

```bash
just ota-serve 20000
```

```bash
just ota-cmd 1.11.0
```

After `downloading`, unplug the board's power and plug it back in (no reflash, no reset).

**Expected:** the old image runs (`[ota] slots: running=<old slot> (Valid)`), `[ota] boot reconciliation: Settled(NoRequest), slot released: true, report pending: false` (the interrupted attempt is cleared silently), no `rolled_back`.
Then restart the normal server with `just ota-serve` and complete the update with `just ota-cmd 1.11.0`.

### P19: Reset during pending verification

```bash
just ota-image 1.12.0
just ota-cmd 1.12.0
```

After the reboot, while serial shows `PendingVerify` and before `applied`, press RST (or restart `just monitor`).

**Expected:** the bootloader rejects the unverified image and boots the previous one, then:

```json
{"status":"rolled_back","reason":"bootloader","attempt_id":<n>,"epoch":<e>}
```

`just ota-cmd 1.12.0` then answers `failed{previously_rolled_back}`.

### P20: Repair on a healthy device

```bash
just ota-repair
```

**Expected:** `{"status":"failed","reason":"repair_not_needed"}`; no reboot.

### P21: Rejections and report retries on the shared reporter thread

The reporter thread delivers `rolled_back`, publishes rejections, and retries the attempt clean-up after `ApplyNotRecorded`.
Each schedule is host-tested alone in `crates/juggler/src/ota/runtime/report.rs`; their interleaving lives in the IDF reporter loop and is only observable here.
The bench check does not send the burst, so run P17 by hand: send the burst as soon as serial shows `[mqtt] connected` after `just mosquitto up`, before the next `just ota-cmd`; commands sent while the device is offline are lost (QoS 0).

```bash
just ota-burst 6 invalid
```

**Pass:** `rolled_back` is still delivered, and the published `command_invalid` count plus the `dropped rejections` increase equals 6.

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

Record every scenario separately, one cell per run and firmware (the example, and consumers such as rgb-clock and Watchtower v2, which keep their own results); there is no overall "tested" label.
Per scenario: the library commit (`git rev-parse --short HEAD`), the consumer commit, board and partition layout (e.g. ESP32-C3 Super Mini 4 MB, `partitions.ota.csv` 2 × 0x1E0000), timing configuration (defaults or every override), the `[ota] slots:` lines before and after, expected and observed statuses, the method (physical power loss, RST, or host fault injection with the test name) and the result (pass, fail with cause, or pending).

### Results

- 2026-11-12: P2, P7, P13, P14 and P17 pass in `c3/ota-runtime`, see the [release runbook](release-0.7.0-hardware-test.md).
- 2026-10-08, campaign 2, ESP32-C3 rev 0.4 (4 MB, MAC `84:fc:e6:00:e3:98`), `2a3ac3f` (MQTT auto-reconnect fix and the `ota-*` / `mosquitto` recipes), versions 2.0.0–2.13.0, local broker: P1–P21 pass (21 of 21) in one campaign, sequenced by a driver script that only calls this runbook's recipes.
  P10 `download_timeout` after 68 s; P12 six `busy`, zero dropped (burst after `OTA write session started`); P13's first re-offer was sent after P14 and installed 2.5.0 again (correct, the refusal record holds only the latest rollback), re-run with 2.13.0 answered `previously_rolled_back`; P15's reset hit the reinstalled 2.5.0 while pending verification, so the bootloader rolled it back (`rolled_back{bootloader}`), and the early 2.7.0 command on the next boot was still applied; P17/P21 report delivered after the broker restart, five `command_invalid` plus one dropped rejection; P18 unplugged 8 s into a 6.5 kB/s download, old image, `Settled(NoRequest)`, re-offer applied.
  Driver bug fixed mid-run: `set -o pipefail` with `grep -q` misreports matches on large logs (SIGPIPE); the driver resumed at P14 without touching the device.
- 2026-10-08, campaign 1, same board, main plus the uncommitted MQTT auto-reconnect fix, versions 1.18.0 (flashed), 1.19.0, 1.20.0: P18 (power cut about 30 s into the 1.21.0 download, after `attempt 28 begun` and the erase; came back on 1.20.0 with `Settled(NoRequest), slot released: true, report pending: false`, no `rolled_back`, MQTT back at 4.4 s, re-offer `applied{1.21.0}`), P17 (`rolled_back{operator}` 9 s after the broker came back, then `applied{1.20.0}`, no `report_pending` because the report was acknowledged first) and P21 (six `command_invalid`, zero dropped, report still delivered) pass.
  Found the missing auto-reconnect: without the fix the device never reconnects after a broker outage (with the cluster broker on 1.15.0 it stayed offline for 9 minutes).
- 2026-10-07, same board, main plus uncommitted justfile and runbook changes: P1–P11, P13, P14, P15, P19, P20 pass; P12 re-run pass (6+0; the first run measured 6+1 because QoS 1 resent one status during the erase); P16 re-run pass with 1.13.0/1.14.0 and 2 s gaps (the first run sent back-to-back commands into the single busy slot); P17, P18 and P21 not run.
  Image 1,651,056 bytes (84% of the 1.875 MiB slot), download about 19 s at full speed, `applied` about 32 s after the reboot, health rollbacks at 61.5 s and 121.5 s, `download_timeout` 67 s after `downloading`.

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
