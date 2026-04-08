# Feature: Embassy Feature Flag v1

Foundation work to prepare `rustyfarian-esp-hal-wifi` for async integration.
Adds an opt-in `embassy` Cargo feature that pulls in the embassy ecosystem crates without changing any existing behavior.
This is a prerequisite for `wifi-manager-async-v1` and `hal-c3-connect-async-example-v1`.

Source: `docs/embassy-integration-research.md` (2026-03-20), Option B "blocking + async companion" recommendation.

> **Scope note:** This document covers only the dependency / feature wiring.
> The async API (`WiFiManager::init_async`, `AsyncWifiHandle`, `wait_for_ip`)
> ships in the same PR but is owned by `wifi-manager-async-v1.md`.
> Async examples are owned by `hal-c3-connect-async-example-v1.md`.

## Decisions

| Decision                                                                                                                                       | Reason                                                                                                              | Rejected Alternative                                                                     |
|:-----------------------------------------------------------------------------------------------------------------------------------------------|:--------------------------------------------------------------------------------------------------------------------|:-----------------------------------------------------------------------------------------|
| Introduce `embassy` feature flag, off by default                                                                                               | Keeps blocking users unaffected; no new compile time or binary size cost unless opted in                            | Embassy as a mandatory dependency — forces async on users with working blocking code     |
| Feature flag lives on `rustyfarian-esp-hal-wifi` only (not `wifi-pure`)                                                                        | `wifi-pure` is platform-independent pure logic; async runtime belongs in the HAL crate                              | Feature on `wifi-pure` — leaks runtime choice into shared types, breaks host tests       |
| All embassy crates declared `optional = true`                                                                                                  | Standard Cargo pattern for feature-gated deps; enables clean `cfg(feature = "embassy")` blocks                      | `dev-dependencies` only — wouldn't expose types in the public API                        |
| Pin exact versions per research doc: `embassy-executor 0.9`, `embassy-net 0.7`, `embassy-time 0.5`, `static_cell 2.1`, `embedded-io-async 0.6` | Matches versions compatible with `esp-rtos 0.2` / `esp-radio 0.17` already in use                                   | Floating latest — risk of breaking changes during routine `cargo update`                 |
| `embassy` feature activates `esp-rtos/embassy` transitively                                                                                    | `esp-rtos` already supports embassy via this feature; avoids users having to know the integration detail            | Requiring users to enable `esp-rtos/embassy` manually — undocumented, error-prone        |
| No `WiFiManagerAsync` added in this feature                                                                                                    | Scope isolation — this feature is pure foundation, validated by `cargo check --features embassy` only               | Bundling API work — makes the change larger and harder to review                         |
| Add `just check-embassy` recipe to verify feature-on build                                                                                     | CI and local contributors need an easy way to verify the feature gate compiles; `just verify` uses default features | Relying on contributors to remember `cargo check --features embassy` — will be forgotten |

## Constraints

- No behavior change for existing blocking users — `just verify` must remain green with default features
- No new runtime dependencies when `embassy` feature is off — verified via `cargo tree -e normal --no-default-features`
- `deny.toml` must pass for all new crates (licenses, advisories, duplicates)
- No API surface added by **this feature** — purely a dependency/feature wiring change. The async API surface added by `wifi-manager-async-v1` is gated on the same `embassy` feature.
- Must work on ESP32-C3 (riscv32imc) and ESP32-C6 (riscv32imac) bare-metal targets
- Does not touch `rustyfarian-esp-idf-wifi` — embassy work is HAL-only

## New dependencies (all `optional = true`)

| Crate               | Version | Purpose                                        |
|:--------------------|:--------|:-----------------------------------------------|
| `embassy-executor`  | 0.9     | Async task spawner                             |
| `embassy-net`       | 0.7     | Async network stack (wraps smoltcp)            |
| `embassy-time`      | 0.5     | Async timers (`Timer::after`)                  |
| `static_cell`       | 2.1     | Safe `'static` allocation for task state       |
| `embedded-io-async` | 0.6     | Async I/O traits for sockets                   |

`esp-rtos/embassy` feature is enabled transitively; no new crate required for that.

## Open Questions

- [x] Do any of the new crates introduce duplicate versions of `heapless`, `smoltcp`, or `embedded-hal` already pinned in the workspace? — `cargo deny` bans check passed clean; `embassy-net 0.7.1` resolves to the same `smoltcp 0.12.0` as the workspace; minor `embedded-io` 0.6/0.7 and `rand_core` 0.6/0.9 duplicates exist but are within the `multiple-versions = "warn"` threshold and did not trip `deny`
- [x] Does `embassy-net 0.7` pull in a `smoltcp` version compatible with the 0.12.0 entry already allowed in `deny.toml`? — Yes, exact match with workspace pin
- [x] Should the feature be named `embassy` or `async`? — Named `embassy`; it is honest about the runtime choice and leaves room for a runtime-agnostic `async` feature later if needed

## State

- [x] Design approved
- [x] Core implementation (Cargo.toml edits + feature block)
- [x] `cargo check --features embassy` passes for ESP32-C3 and ESP32-C6 (via `just check-wifi-hal-embassy`, both bare-metal targets with `-Zbuild-std=core,alloc`)
- [x] `cargo deny check` passes with the new deps
- [x] `just verify` passes (default features unchanged)
- [x] `just check-embassy` recipe added (named `check-wifi-hal-embassy` to match the existing `check-wifi-hal` convention)
- [x] CHANGELOG entry

## Session Log

- 2026-04-08 — Feature doc created from `docs/embassy-integration-research.md`
- 2026-04-08 — Implemented: workspace deps added, `embassy` feature block added to `rustyfarian-esp-hal-wifi`, `check-wifi-hal-embassy` just recipe added (uses `-Zbuild-std=core,alloc` for RISC-V bare-metal targets), CHANGELOG updated. `just fmt`, `just verify`, and the new recipe all pass clean on ESP32-C6 and ESP32-C3.
