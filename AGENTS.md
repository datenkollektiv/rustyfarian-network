# AGENTS.md

> Use this file as the fast-path operating guide for AI coding agents.
> Prefer repository truth over assumptions — check the files referenced below.

## Project Overview

`rustyfarian-network` is a Rust workspace providing Wi-Fi, MQTT, LoRa, ESP-NOW, and OTA networking libraries for ESP32 firmware.
Three publishable crates consolidate the workspace: pure-tier `juggler`, ESP-IDF-tier `rustyfarian-esp-idf-network` (std-based), and bare-metal-tier `rustyfarian-esp-hal-network` (no_std).
Both HAL tiers consume `juggler` — a consolidated, no_std-by-default crate with optional `std` feature — that compiles and unit-tests on any host without the ESP toolchain (per ADR 016).

## Architecture

The workspace separates pure logic (host-testable) from hardware-specific implementations.
ADRs in `docs/adr/` document each architectural split.

| Pure (no_std + optional `std`)                                                                                                                                                        | ESP-IDF (std)                                                                                                                                                            | esp-hal (no_std bare-metal)                                                                                                                                                                                         |
|:--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|:-------------------------------------------------------------------------------------------------------------------------------------------------------------------------|:--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **`juggler`** (Phase 1 ✓ merged) — consolidated platform-independent crate with feature-gated domains: `wifi`, `mqtt`, `lora`, `espnow`, `ota`, `provisioning`, plus `mock` and `std` | **`rustyfarian-esp-idf-network`** (Phases 2–3 ✓ merged) — consolidated ESP-IDF crate with feature-gated domains: `wifi`, `mqtt`, `lora`, `espnow`, `ota`, `provisioning` | **`rustyfarian-esp-hal-network`** (Phases 2–3 ✓ merged) — consolidated bare-metal crate with feature-gated domains: `wifi`, `lora`, `ota`, `provisioning` + chip features: `esp32`, `esp32c3`, `esp32c6`, `esp32s3` |

The pure tier (`juggler`) contains validation, types, traits, state machines, and timing math — all host-testable, no hardware dependencies.
The ESP-IDF and esp-hal tiers implement the traits and hardware lifecycle; they remain separate per ADR 005 (HAL tiering is mutually exclusive).

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
The pure tier (`juggler`) iterates fast without the ESP toolchain — see `just test` (all tests), or target specific domains via `-p juggler --features wifi,mock` etc.
Bare-metal and ESP-IDF builds are isolated into `target/hal` and `target/idf` (host/IDE builds use `target/ide`); this routing is automatic.
On macOS, `just ramdisk attach` optionally backs those directories with a RAM disk for faster builds, and `just doctor` reports the resolved target dirs and RAM disk status.

## Key Conventions

- **Pure-first rule:** any logic testable without hardware belongs in a `*-pure` crate; the ESP-IDF / `esp-hal` crates are thin wrappers that delegate logic downward.
- **Example naming:** `idf_<chip>_<purpose>` for ESP-IDF examples (e.g. `idf_c3_connect`, `idf_esp32s3_join`); `hal_<chip>_<purpose>` for bare-metal examples (e.g. `hal_c6_connect`). `scripts/build-example.sh` and `scripts/flash.sh` extract the chip from the prefix and route to the correct target / MCU.
- **One sentence per line in Markdown** for clean diffs.
- **ADRs** in `docs/adr/` use the Michael Nygard format; filename `NNN-short-description.md`; status one of Proposed / Accepted / Deprecated / Superseded. Record every significant architectural or technology decision there.
- **Feature documents** in `docs/features/` track in-progress work across sessions; start from `docs/features/000-template.md`.
- **Project lore** in `docs/project-lore.md` records non-obvious technical discoveries (failures that took >15 min to diagnose, or root causes not obvious from the error message). Read it before debugging anything ESP-IDF / `esp-hal` / LoRaWAN.
- **Cross-repo git deps** must be pinned with `tag` or `rev` — the workspace pulls in `links = "..."` crates that fail to resolve if upstream bumps without coordination.
- **Dependency hygiene:** `just deny` (licenses + advisories + bans), `just audit` (RUSTSEC advisories), and `just machete` (unused declared dependencies). `just machete` should report nothing; any `[package.metadata.cargo-machete] ignored` entry must carry a justification comment explaining why the dependency is retained (e.g. build-only like `embuild`, or reserved for pending wiring like `lorawan-device`).
- **Never commit real credentials — not even in example doc comments.** This is a public repo. Real LoRaWAN keys (`AppKey`), Wi-Fi PSKs, and tokens must live only in the git-ignored `.env`, never in `.rs` / `.md` source. Setup examples must use obvious placeholders (`0123456789ABCDEF`, `00112233445566778899AABBCCDDEEFF`, all-zeros) — a realistic-looking key in a doc comment reads as real and gets indexed by secret scanners. A DevEUI/JoinEUI is a device identifier (not secret) but should also be a placeholder so docs do not point at a live device. If a real secret is ever committed, treat it as compromised: rotate it at the source (TTN, AP) first; scrubbing the file does not un-publish pushed history.
- **License:** dual MIT / Apache-2.0 (see `LICENSE`).

## Coding Principles

- **State assumptions** before starting. If a task has multiple valid interpretations, present them rather than picking silently.
- **Simplicity first.** Minimum code that solves the problem. No features beyond what was asked. No abstractions for single-use code. No error handling for impossible scenarios.
- **Surgical changes.** Touch only what the task requires. Do not improve adjacent code, comments, or formatting. Every changed line should trace directly to the user's request.
- When your changes create orphans (unused imports, variables, functions), remove them. Do not remove pre-existing dead code unless asked.

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
