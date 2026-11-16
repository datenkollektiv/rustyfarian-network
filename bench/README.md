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

## Remote bench

`just pi-submit` runs a check on the remote bench instead of a board attached to this machine.

```bash
just pi-submit c3 hal_c3_connect_async --wait
```

- `rustyfarian-bench submit` builds the example, saves its whole-flash image through `just save-image` into a temporary directory and uploads it.
- The image holds the build-time Wi-Fi credentials, and the remote bench keeps uploaded images until they are cleared by hand.
- `just save-image <example> <path>` refuses a path inside the repository that git would track; use `tmp/` or a path outside the repository.
- The bench settings come from the environment first, otherwise from the SOPS secrets file (`scripts/bench-remote-env.sh`):

| Key                        | Required | Meaning                                                    |
|:---------------------------|:---------|:-----------------------------------------------------------|
| `RUSTYFARIAN_BENCH_URL`    | yes      | Bench host, e.g. `https://bench.example.net:8443`          |
| `RUSTYFARIAN_BENCH_TOKEN`  | yes      | Bearer token                                               |
| `RUSTYFARIAN_BENCH_CA_B64` | no       | Base64 of the bench's private CA certificate (PEM)         |

- An exported `RUSTYFARIAN_BENCH_CA` (a PEM file path) wins over `RUSTYFARIAN_BENCH_CA_B64`.
- The secrets file is decrypted only when the URL or the token is missing; with both exported, also export the CA if the bench needs one.

## Secrets setup (SOPS + age)

The remote bench and the router automation read their settings from `shared-secrets/secrets.env`, a SOPS-encrypted dotenv file.
`shared-secrets/`, `secrets/` and `.sops.yaml` are git-ignored, because this repository is public; each contributor sets them up once.

1. Install the tools with `brew install sops age jq` and create an age key with `age-keygen`.
2. Point SOPS at the key: export `SOPS_AGE_KEY_FILE` (for example in `.envrc`).
3. Create `.sops.yaml` in the repository root with your age public key as recipient for `shared-secrets/*.env`:

```yaml
creation_rules:
  - path_regex: (^|/)shared-secrets/.*\.(env|json)$
    key_groups:
      - age:
          - age1yourpublickey
```

4. Run `just secrets-edit`, which creates the file on first use and never writes plaintext to disk.
5. Run `just doctor`; its router section reports the SOPS rules, the age key and whether the secrets file decrypts.

- After a recipient change in `.sops.yaml`, run `just secrets-updatekeys`.
- `just decrypt` and `just encrypt` exist for hand edits, but leave plaintext on disk in between.

## Router automation (optional)

`just router block|unblock|status <mac>` lets router steps run with `--unattended`; without it, run them with `--attended` and block the board by hand.

- `ROUTER_BENCH_MACS` in the secrets file lists the only MACs the recipe acts on, so a typo cannot lock out this machine or the broker.
- The router-specific code is a machine-local, executable backend script, never committed: export `ROUTER_BACKEND` (for example in `.envrc`) with its path.
- `scripts/router.sh` calls it as `<backend> <action> <mac>` inside `sops exec-env`, so the backend reads its `ROUTER_*` values (host, credentials) from the environment.
- `status` must print `<mac> blocked` or `<mac> not blocked`.
- There is no fallback: with `ROUTER_BACKEND` unset, `just router` fails, and `just doctor` reports it.

## Writing a check

- A step that needs a person and has no `confirm` carries `attended = true`, or `--unattended` waits on it until it times out.
- No step waits for Enter: keep `arm = false`, so the board's reaction is judged even when it comes before any keypress.
- `{…}` in any string is a runner placeholder (`board`, `chip`, `example`, `mac`, `port`); write shell variables as `$VAR` and move awk or other brace syntax into a script under `scripts/`.
- A second board of the same chip (e.g. the Heltec V3 beside the CrowPanel) needs its `mac` pinned in `bench.toml`.
