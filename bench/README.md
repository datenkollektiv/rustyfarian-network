# Bench

Guided, rerunnable hardware checks for both tiers ([ADR 024](../docs/adr/024-adopt-rustyfarian-bench.md)).
The runner is [`rustyfarian-bench`](https://github.com/datenkollektiv/rustyfarian-bench) (Python + uv); `just bench` runs the release pinned in the `justfile` (v0.5.0) through `uvx`, or a local checkout when `RUSTYFARIAN_BENCH` points at one.
Bumping the pin is how this repo opts into a new runner release; the scenarios need v0.5.0 or later for `probe`, `run`, `setup`/`teardown`, `serial_reject` and the `{mac}` placeholder in prompts, and an older runner rejects those keys and loads no check at all.
`c6/mqtt-scenarios` drives the local broker from `just mosquitto`, so `MQTT_HOST` must be this machine's LAN IPv4 and the board's network must reach it on TCP 1883; set it in the shell for the run, which wins over the dotenv file.
A probe reads `just mqtt-status` or `just mqtt-watch hal`, whose lines carry `retain=1` for the replay at subscribe and `retain=0` for a live message.
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
- Broker messages are judged by `probe` steps that run `just mqtt-status {example}` and `just mqtt-watch {example}` (`scripts/mqtt-topic.sh` maps an example name to its topic prefix), so the run needs no second terminal; every other step is judged from serial output.
  The only question left is the C6 `connect` LED colour.
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
- Timed waits show a countdown; the soak always runs its full length, and Ctrl-C ends it early.
- No step waits for Enter (`arm = false` everywhere): an action step shows its instruction and watches from the end of the previous step, so the board's reaction is judged even when it comes before any keypress; keep new steps unarmed.
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
- Suspend/resume and the remaining long runs (S3-B1/B2, C6-I1, C6-H5) — broker-driven or consumer firmware; the T6 soak (S3-I1, C6-H2 T6) is the `soak` check, and C6-H2 T1 to T5 is `c6/mqtt-scenarios`.
- LoRa on the Heltec V3 (HV-I1, HV-H1) — a second `esp32s3` board needs `mac` pinned beside the CrowPanel.
