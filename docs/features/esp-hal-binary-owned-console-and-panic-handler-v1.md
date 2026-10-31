---
gate: none
desk-work: none
---

# Feature: esp-hal binary-owned console and panic handler v1

`rustyfarian-esp-hal-network` never prints and never panics on purpose, yet `wifi` and `lora` force `esp-println` and `esp-backtrace` (with its `panic-handler`) into every firmware.
The firmware should own the console, the panic handler and the `critical-section` provider alike (bug 007 settled the third); this feature removes the first two from the library.
Follow-up to the 2026-10-31 U2d fix, which only stopped the chip features from selecting a console.

## Decisions
| Decision                                                                                                                                | Reason                                                                                                | Rejected Alternative                                                                              |
|:----------------------------------------------------------------------------------------------------------------------------------------|:------------------------------------------------------------------------------------------------------|:--------------------------------------------------------------------------------------------------|
| Drop `dep:esp-println` and `dep:esp-backtrace` from `wifi` and `lora`, delete both optional deps and all `?/<chip>` forwards            | The library source references neither crate; forcing a panic handler blocks custom ones               | Keep esp-backtrace without `println` — still forces the panic handler, silently                   |
| Examples get both crates from `[target.<triple>.dev-dependencies]` with chip + `auto` + `log-04` and chip + `panic-handler` + `println` | Only the target triple can carry the chip once the lib stops forwarding it; host tests never see them | One `cfg(target_os = "none")` dev-dep — cannot carry a chip, esp-println's build script needs one |
| Cover all four triples: C3 (`riscv32imc`), C6 (`riscv32imac`), S3 and classic ESP32                                                     | User choice 2026-10-31: symmetry for a future `hal_esp32_*` example                                   | Omit ESP32 — nothing exercises it today                                                           |
| Revert the 24 `esp-println/auto` command-line additions in `justfile` and the docs.rs metadata                                          | A crate that is no dependency cannot be passed with `--features dep/feat`                             | Keep them — cargo rejects the unknown dependency name                                             |
| Record the precedent as ADR 021 "binary-owned providers"                                                                                | Console, panic handler and critical-section now follow one rule across the tiers                      | Feature doc and CHANGELOG only                                                                    |
| One CHANGELOG entry: merge with today's U2d entry under Unreleased                                                                      | Consumers see one migration, not two, in the same minor                                               | A second Breaking line                                                                            |

## Constraints
- `wifi` and `lora` builds must no longer compile `esp-println` or `esp-backtrace` at all (`cargo tree -i` on a scratch consumer proves it).
- Lib-only checks (`just check-hal`, `clippy-hal`, docs.rs, `release-dry-run-hal`) must pass with no console on the command line.
- All 11 hal examples must build with `just build-example`; `hal_esp32s3_join` keeps its own `#[panic_handler]` or switches to `esp_backtrace`, not both.
- Host tests (`just test-hal`) must not compile either crate (the triples exclude the host).
- The crate README's "exact pins are public API" list drops `esp-println` and `esp-backtrace`; the consumer snippet gains both.
- `CLAUDE.md` row, `docs/project-lore.md` amendment and the archived U2d request note must describe the final state, not the interim one.
- Gate per `CLAUDE.md`: `just fmt`, `just verify`, `just lint-docs`, every `just build-example hal_*`, a scratch S3 consumer with `features = ["mqtt", "esp32s3"]` and no esp-println dependency.

## Open Questions
- [x] Does a target-triple dev-dependency table satisfy `cargo publish --dry-run` for the crate (dev-deps are stripped on publish, so it should)? — Yes: cargo publishes `[target.<triple>.dev-dependencies]` like any dev-dependency, and the publish verify step builds only the library and binaries, never the examples that need them.

## State
- [x] Design approved
- [x] Core implementation
- [x] Tests passing
- [x] Documentation updated
- [x] Hardware gate: examples print on CrowPanel S3 (`hal_esp32s3_mqtt_lwt`) and C3 Super Mini (`hal_c3_connect_async`), both 2026-10-31 at 2fd189e

## Session Log
- 2026-10-31 — Feature doc created via /feature dialog after the U2d fix (30bb047) landed on `configurable-console`
- 2026-10-31 — Implemented via workflow: lib drops esp-println and esp-backtrace, per-triple dev-deps, ADR 021, gate green; hardware print re-check open
- 2026-10-31 — S3 hardware check passed on the CrowPanel 1.28" (2fd189e): `hal_esp32s3_mqtt_lwt` prints `println!` and `log` lines over USB-Serial-JTAG, LWT works, free heap 25 928 B unchanged; the knob builds next to its own `auto` and `#[panic_handler]`; C3 Super Mini still open
- 2026-10-31 — C3 hardware check passed on an ESP32-C3 Super Mini (2fd189e): `hal_c3_connect_async` prints link-up and IP over USB-Serial-JTAG, free heap 26 240 B; feature complete, ready for roadmap hygiene and archive
