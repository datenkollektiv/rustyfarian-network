# Bench

Guided, rerunnable hardware checks for both tiers ([ADR 024](../docs/adr/024-adopt-rustyfarian-bench.md)).
The runner is [`rustyfarian-bench`](https://github.com/datenkollektiv/rustyfarian-bench) (Python + uv); `just bench` runs the release pinned in the `justfile` (v0.3.0) through `uvx`, or a local checkout when `RUSTYFARIAN_BENCH` points at one.
Bumping the pin is how this repo opts into a new runner release; the scenarios need v0.3.0 or later for `attended`.
The checks cover the single-board cells of the [release 0.7.0 hardware runbook](../docs/runbooks/release-0.7.0-hardware-test.md); the runbook stays the authority for every gate.

| Board                                 | Directory        | State       |
|:--------------------------------------|:-----------------|:------------|
| ESP32-C3 Super Mini                   | [c3/](c3/)       | in progress |
| ESP32-C6-DevKitC-1                    | [c6/](c6/)       | in progress |
| CrowPanel 1.28" HMI (S3)              | [s3/](s3/)       | in progress |
| ESP32 WROOM-32 DevKit / Feather V2    | [esp32/](esp32/) | in progress |

## Prerequisites

- The workspace `.env` with `WIFI_SSID`, `WIFI_PASS` and `MQTT_HOST` (an IPv4, never `localhost`).
- For the MQTT checks, a running broker at `MQTT_HOST` that the board's network can reach; `just doctor` checks it, and `just mosquitto up` starts a local one if you have none.
- Optional: a subscriber for the one broker-side confirm step (`just mqtt-watch hal`, the default, or `just mqtt-watch idf` for the LWT examples; `just mqtt-watch <client_id>` for the IDF heartbeat examples; a second argument stops after that many seconds); every other step is judged from serial output.
- A router that can block a client by MAC: every Wi-Fi and MQTT check ends by blocking the board, then unblocking it.
- The ESP-RS toolchain for the build step (`just doctor` passes).

## Running a check

```sh
just bench c3 mqtt-lwt
just bench hal_esp32s3_mqtt_lwt
just bench c3 connect --no-build
just bench c3
just bench-list
just bench-detect
```

The runner builds first, waits for the board (plug it in when asked), flashes, resets, and guides each step with an automatic verdict.
`just bench c3` runs every C3 check in turn; a scenario with two examples runs the hal one first, then offers its idf twin.
Pin `--port` while another board is attached: detection probes every candidate port with espflash, which resets whatever is on it.

### Unattended and attended runs

```bash
just bench s3 --unattended
just bench s3 --attended
```

- `--unattended` runs every step the log can judge (boot, broker session, ticks, crash watches, the full 10-minute `soak`), so start it and walk away.
- `--attended` runs only the steps that need you: router block and unblock, the broker and LED questions, and the BOOT button steps.
- A board passes for a release only when both runs pass; each result records its `mode`.
- Steps that need a person without a `confirm` carry `attended = true`; mark any new one the same way, or `--unattended` waits on it until it times out.
- Timed waits show a countdown, and an armed `duration` step (the soak) takes a shorter length at its prompt.
- Every Wi-Fi check starts with an optional signal step: it reads the joined AP from the link-up line and warns when its RSSI is below -75 dBm (a far mesh node).
- The soak's heap criterion is read by eye from the `heap free` lines; a green soak does not mean T6's 1 KiB heap rule passed.

## Layout

- `bench.toml` — build/flash commands and the boards (chip, optional MAC).
- `<board>/<check>.toml` — one scenario: the examples it runs and the shared steps.
- `<board>/logs/` — captured serial output per run (git-ignored).
- `<board>/results.jsonl` — one JSON line per example run (git-ignored: it records board MACs).

The commands call `just build-example NAME` and `ESPFLASH_PORT=PORT just flash NAME`, so the IDF bootloader cache and the `.env` credentials work as in a manual run.

## Recording a result

- Copy each pass or fail into the runbook's results table with the git revision and the board.
- Record any non-obvious gotcha in `docs/project-lore.md`.
- Build-time variables (`ESPNOW_SCENARIO`, `COORDINATOR_MAC`) do not reach the runner's build step per scenario, so those gates still run through `just run`.

## Open

- ESP-NOW (C3-I1 to C3-I10, C3-H2/H3, C3-H6 to C3-H9, C6-H3/H4) — two or three boards at once, and scenarios picked at build time.
- OTA (C3-I5/I6, C3-H5, S3-H3) — needs the OTA server and the runbook's scenario matrix.
- Provisioning portal (C3-I3, C3-H1, C6-H1, S3-H2) — a browser submission between two boots.
- Suspend/resume and the remaining long runs (S3-B1/B2, C6-I1, C6-H2 T1 to T5, C6-H5) — broker-driven or consumer firmware; the T6 soak (S3-I1, C6-H2 T6) is the `soak` check.
- LoRa on the Heltec V3 (HV-I1, HV-H1) — a second `esp32s3` board needs `mac` pinned beside the CrowPanel.
