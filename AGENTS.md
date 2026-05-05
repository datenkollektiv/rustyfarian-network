# AGENTS.md

> Use this file as the fast-path operating guide for AI coding agents.
> Prefer repository truth over assumptions — check the files referenced below.

## Project Overview

`rustyfarian-network` is a Rust workspace providing Wi-Fi, MQTT, LoRa, and ESP-NOW networking libraries for ESP32 firmware.
Two implementation tiers coexist: an ESP-IDF tier (`rustyfarian-esp-idf-*`, std-based) and a bare-metal `esp-hal` tier (`rustyfarian-esp-hal-*`, no_std).
Both tiers share platform-independent `*-pure` crates that compile and unit-test on any host without the ESP toolchain.

## Architecture

The workspace separates pure logic (host-testable) from hardware-specific implementations.
ADRs in `docs/adr/` document each architectural split.

| Pure (no_std, host-testable) | ESP-IDF (std)                | esp-hal (no_std bare-metal) |
|:-----------------------------|:-----------------------------|:----------------------------|
| `wifi-pure`                  | `rustyfarian-esp-idf-wifi`   | `rustyfarian-esp-hal-wifi`  |
| `lora-pure`                  | `rustyfarian-esp-idf-lora`   | `rustyfarian-esp-hal-lora`  |
| `espnow-pure`                | `rustyfarian-esp-idf-espnow` | (not yet)                   |
| `rustyfarian-network-pure`   | `rustyfarian-esp-idf-mqtt`   | (planned)                   |

Pure crates contain validation, types, traits, state machines, and timing math.
Hardware crates implement the traits using ESP-IDF or `esp-hal` and handle the hardware lifecycle.

## Development Workflow

All build, test, lint, and flash operations go through `just` recipes — never invoke `cargo` directly for these.

```sh
just setup-toolchain       # one-time: install ESP toolchain via espup
just setup-cargo-config    # one-time: copy .cargo/config.toml.dist → .cargo/config.toml
just fmt                   # cargo fmt — modifies files
just verify                # fmt-check + deny + check + clippy — non-modifying, must pass clean
just test                  # all platform-independent unit tests (no ESP toolchain needed)
just build-example <name>  # build a hardware example with auto-detected chip + target
just flash <name>          # build + flash to a connected board
just run <name>            # flash + serial monitor
```

Run `just fmt` before `just verify` — the latter's `fmt-check` will reject unformatted code.
`just verify` only compiles the workspace default target (`riscv32imac-esp-espidf`); use `just build-example <name>` to validate Xtensa IDF and bare-metal targets.
Pure crates iterate fast without the ESP toolchain — see `just check-wifi-pure`, `just test-wifi`, etc.

## Key Conventions

- **Pure-first rule:** any logic testable without hardware belongs in a `*-pure` crate; the ESP-IDF / `esp-hal` crates are thin wrappers that delegate logic downward.
- **Example naming:** `idf_<chip>_<purpose>` for ESP-IDF examples (e.g. `idf_c3_connect`, `idf_esp32s3_join`); `hal_<chip>_<purpose>` for bare-metal examples (e.g. `hal_c6_connect`). `scripts/build-example.sh` and `scripts/flash.sh` extract the chip from the prefix and route to the correct target / MCU.
- **One sentence per line in Markdown** for clean diffs.
- **ADRs** in `docs/adr/` use the Michael Nygard format; filename `NNN-short-description.md`; status one of Proposed / Accepted / Deprecated / Superseded. Record every significant architectural or technology decision there.
- **Feature documents** in `docs/features/` track in-progress work across sessions; start from `docs/features/000-template.md`.
- **Project lore** in `docs/project-lore.md` records non-obvious technical discoveries (failures that took >15 min to diagnose, or root causes not obvious from the error message). Read it before debugging anything ESP-IDF / `esp-hal` / LoRaWAN.
- **Cross-repo git deps** must be pinned with `tag` or `rev` — the workspace pulls in `links = "..."` crates that fail to resolve if upstream bumps without coordination.
- **License:** dual MIT / Apache-2.0 (see `LICENSE`).

## Important Files

| File                          | Why read it                                                                          |
|:------------------------------|:-------------------------------------------------------------------------------------|
| `README.md`                   | Crate inventory, usage example, hardware example workflow                            |
| `justfile`                    | Authoritative list of build / test / flash / verify recipes                          |
| `Cargo.toml` (workspace root) | Workspace deps (incl. pinned cross-repo `rustyfarian-ws2812` git deps), lints        |
| `docs/project-lore.md`        | Non-obvious failure patterns and root causes                                         |
| `docs/ROADMAP.md`             | Current priorities, blocked items, post-v0.1.0 backlog                               |
| `docs/adr/`                   | Architecture decisions (HAL tiering, ESP-NOW init, Wi-Fi non-blocking connect, etc.) |
| `VISION.md`                   | North star, long-term goals, non-goals                                               |
