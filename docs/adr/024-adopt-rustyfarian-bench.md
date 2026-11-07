# ADR 024: Adopt rustyfarian-bench for On-Device Checks

## Status

Accepted (2026-11-07).
Follows `rustyfarian-peripherals`, `rustyfarian-ws2812` and `rustyfarian-knob` (ADR 016 there), which run their checks through the same runner.

## Context

Both tier crates are validated on hardware through runbooks: `just run <example>` and reading the log by eye, then copying a verdict into a results table.
The [release 0.7.0 hardware runbook](../runbooks/release-0.7.0-hardware-test.md) has 37 gates across four chips, and several of them are the same Wi-Fi and MQTT smoke on a different board.
The rustyfarian ecosystem now has `rustyfarian-bench`, a guided runner (Python + uv, sibling checkout): a consumer describes boards and checks as TOML under `bench/`, and the runner builds, finds the board, flashes, resets, captures serial and judges each step against regexes or an operator confirmation.
The examples already print stable lines (`Wi-Fi connected — IP:`, `[wifi] link up #N`, `[app] connect #N`, `[mqtt] heartbeat N sent`), and the S3 pair `hal_esp32s3_mqtt_lwt` / `idf_esp32s3_mqtt_lwt` shares one application body, so one scenario can judge both tiers.

## Decision

1. Single-board on-device checks are written as `rustyfarian-bench` scenarios under `bench/<board>/`, for the boards of the 0.7.0 chip matrix: `c3`, `c6`, `s3` (CrowPanel) and `esp32`.
2. `bench/bench.toml` calls the existing `just build-example NAME` and `just flash NAME` with `ESPFLASH_PORT` set to the detected port; no bench-only build or flash recipes are added.
3. `just bench`, `just bench-list` and `just bench-detect` match the sibling repos: they run the runner release pinned in the `justfile` through `uvx` (v0.3.0 since 2026-11-07), and `RUSTYFARIAN_BENCH` points them at a local checkout instead.
4. Run logs and `results.jsonl` stay git-ignored, because results record board MACs and this repository is public.
5. The first pass covers Wi-Fi connect and reconnect, MQTT smoke with a router block on every supported cell, and the C3 BOOT re-entry hook.
   ESP-NOW, OTA, the provisioning portal, soaks and LoRa stay runbook gates until they get their own step design.
6. The runbook stays the authority for release gates; a bench result is evidence copied into its results table.

## Consequences

- The smoke cells become rerunnable and comparable across tiers and chips, with an automatic verdict where serial output can prove it and an explicit operator answer where it cannot (broker topics, LED colours).
- Bench runs depend on `uv` and on GitHub access for the pinned runner (or a local checkout); builds, `just verify` and CI are unaffected.
- Build-time variables per scenario (`ESPNOW_SCENARIO`, `COORDINATOR_MAC`) do not reach the runner's build step, which is one reason the ESP-NOW gates stay in the runbook.
- Scenario regexes couple to example and library log lines; changing a log text means updating its scenario.
