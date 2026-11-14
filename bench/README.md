# Bench

Guided, rerunnable hardware checks for both tiers ([ADR 024](../docs/adr/024-adopt-rustyfarian-bench.md)).
A check is named `board/check`, for example `c3/espnow-cancel`; the TOML header of each check is the source of truth for its steps and prerequisites.
The runner is [`rustyfarian-bench`](https://github.com/datenkollektiv/rustyfarian-bench), pinned in the `justfile` and run through `uvx` (or a local checkout via `RUSTYFARIAN_BENCH`).

| Board                              | Directory        | Last full pass |
|:-----------------------------------|:-----------------|:---------------|
| ESP32-C3 SuperMini                 | [c3/](c3/)       | open           |
| ESP32-C6-DevKitC-1                 | [c6/](c6/)       | open           |
| CrowPanel 1.28" HMI (S3)           | [s3/](s3/)       | open           |
| ESP32 WROOM-32 DevKit / Feather V2 | [esp32/](esp32/) | open           |

## Usage

```bash
just bench-list
just bench c3 mqtt-lwt
just bench s3 --unattended
just bench s3 --attended
```

- Needs the workspace dotenv file with `WIFI_SSID`, `WIFI_PASS` and `MQTT_HOST` (an IPv4); `just doctor` checks the setup.
- `--unattended` runs the steps the log can judge, `--attended` only those that need a person; a board passes only when both pass.
- Per-check build variables (`ESPNOW_SCENARIO`, `COORDINATOR_MAC`, `ESPNOW_PEER_MAC`) and the OTA recipe `just bench-ota` are described in the check headers.
- Pin `--port` while another board is attached: detection probes every candidate port with espflash, which resets whatever is on it.
- A check with two examples runs the hal one first, then its idf twin.
- Every Wi-Fi check starts with a signal step that warns when the joined AP is below -75 dBm (a far mesh node).
- The soak's heap criterion is read by eye from the `heap free` lines.
- Logs and `results.jsonl` under `<board>/` are git-ignored (they record board MACs), so the committed record of a single check is its row in the release runbook's results table: verdict, date, git revision, board and log path.
- A board's "Last full pass" gets the date once `--unattended` and `--attended` both passed on it, never ahead of the run; until then it reads `open`.

## Writing a check

- A step that needs a person and has no `confirm` carries `attended = true`, or `--unattended` waits on it until it times out.
- No step waits for Enter: keep `arm = false`, so the board's reaction is judged even when it comes before any keypress.
- `{…}` in any string is a runner placeholder (`board`, `chip`, `example`, `mac`, `port`); write shell variables as `$VAR` and move awk or other brace syntax into a script under `scripts/`.
- A second board of the same chip (e.g. the Heltec V3 beside the CrowPanel) needs its `mac` pinned in `bench.toml`.
