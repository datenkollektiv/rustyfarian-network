# Feature: ESP-NOW Peripheral Command Framework v1

## Decisions

|                                                                                                          Decision | Reason                                                                                                                        | Rejected Alternative                                                                              |
|------------------------------------------------------------------------------------------------------------------:|:------------------------------------------------------------------------------------------------------------------------------|:--------------------------------------------------------------------------------------------------|
|                                     Tag byte + payload envelope (`CommandFrame<'a>` borrowing from payload slice) | Matches existing diceroller pattern; minimal overhead; zero-copy                                                              | Structured header with length/version — overkill for 250-byte ESP-NOW frames                      |
|                                    System tags `0xF0–0xFF`, module tags `0x01–0xEF`, `0x00` reserved (never used) | Clear separation; 16 system slots is plenty; modules get 239 tags; `0x00` distinguishes empty                                 | Flat namespace — collision risk across modules                                                    |
|                        Free functions (`parse_frame`, `parse_system_command`) instead of a `CommandHandler` trait | Diceroller pattern proves free functions are clean enough; trait adds abstraction without benefit                             | `CommandHandler` trait — deferred until test utilities need generic dispatch                      |
|                     `CommandFrame` is payload-only — sender MAC stays at the dispatch site, not inside the struct | Industry consensus (BLE Mesh, ZigBee ZCL, Espressif ESP-NOW framework); transport-independent; testable on host (see ADR 010) | Bundling MAC into `CommandFrame` — couples parsing to ESP-NOW transport, breaks UART/MQTT reuse   |
|                                Phase 1 system commands: `Ping` (`0xF0`), `SelfTest` (`0xF1`), `Identify` (`0xF2`) | Minimum viable set for health probing and module discovery                                                                    | More system commands upfront — defer `HealthQuery`, `Reset`, response frames to Phase 2           |
|                                                                                      Lives in `espnow-pure` crate | Already `no_std`, zero dependencies, defines transport types; natural home for command layer                                  | New crate — unnecessary; the command layer is tightly coupled to the ESP-NOW transport types      |

## System Command Response Payloads

|             Tag | Request | Response                                                          |
|----------------:|:--------|:------------------------------------------------------------------|
|     `0xF0` Ping | empty   | Pong: tag `0xF0`, 1 byte `0x01`                                   |
| `0xF1` SelfTest | empty   | SelfTestResult: tag `0xF1`, 1 byte (`0x00` = pass, `0x01` = fail) |
| `0xF2` Identify | empty   | Identity: tag `0xF2`, module type `u8` + version `u8`             |

## Consumer Migration Pattern

After implementation, the diceroller firmware migrates from inline protocol handling to shared `CommandFrame` parsing.
System commands are handled uniformly; module-specific commands use the same `CommandFrame` but only match tags `0x01–0xEF`.
See `review-queue/espnow-peripheral-command-framework.md` "Consumer usage" section for the full code example.

## Motivation

Every ESP-NOW peripheral module (diceroller, upcoming HP0, future LED panels, sound triggers) reinvents the same tag-based command dispatch.
Without a shared framework, tag values collide across modules, there is no standard way to probe health or identity, and a coordinator cannot manage arbitrary peripherals uniformly.
The `espnow-pure` crate is the natural home: `no_std`, zero dependencies, already defines transport types.

## Constraints

- Must remain `no_std` with zero external dependencies (`espnow-pure` has none today)
- `CommandFrame` borrows from the payload slice — no heap, no allocator
- Must not depend on `rustbox-peripherals-pure` or any other crate outside this workspace
- Additive only — existing `EspNowEvent`/`EspNowDriver` API unchanged
- Warning logging stays in firmware code, not in `espnow-pure` (no `log` dependency)

## Open Questions

- [x] Should `is_system_tag()` be a method on `CommandFrame` or remain a free function? — Resolved: free function. Predicates operate on `u8` directly so callers can classify a tag without constructing a `CommandFrame`. A method alias can be added later if ergonomics warrant it.
- [ ] Should `SystemCommand` carry parsed response payloads (e.g. `Identify` with a module type and version), or should response parsing be deferred to Phase 2?

## State

- [x] Design approved
- [x] Core implementation
- [x] Tests passing
- [x] Documentation updated

## Session Log

- 2026-04-05 — Feature doc created from the review queue proposal; scoped to Phase 1 only (frame envelope + system tags)
- 2026-04-05 — MAC-in-frame decision resolved via industry research; captured in ADR 010
- 2026-04-10 — Implemented: `command` module in `espnow-pure` with `CommandFrame<'a>` zero-copy parser, `parse_frame()`, `is_system_tag()`/`is_module_tag()` predicates, `SystemCommand` enum with `parse_system_command()`, response helpers (`PONG_RESPONSE`, `SELF_TEST_PASS/FAIL`, `identify_response()`). 19 tests covering parsing, tag ranges, roundtrips. `just test-espnow` (50 tests) and `just verify` pass clean.
