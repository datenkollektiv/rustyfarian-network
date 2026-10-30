---
id: 007
title: lora feature forces esp-idf-hal's critical-section provider, and the driver assumes it masks interrupts
captured-on: 2026-10-30
doc-version: 2
status: open-defect
kind: defect
---

# Bug 007: lora feature forces esp-idf-hal's critical-section provider, and the driver assumes it masks interrupts

## Symptom
A firmware that enables `rustyfarian-esp-idf-network/lora` together with another `critical-section` provider (`rustyfarian-esp-idf-power`'s `critical-section-impl`) fails to link with `multiple definition of '_critical_section_1_0_acquire'` (and `_release`).

## Suspected Cause
The `lora` feature enables `esp-idf-hal/critical-section`, so the library selects the process-wide provider that only the final binary should select.
The driver comment and the lore call that provider "interrupts disabled, FreeRTOS scheduler suspended", but esp-idf-hal 0.47 `src/task.rs` implements it as a recursive FreeRTOS mutex (`xQueueTakeMutexRecursive` / `xQueueGiveMutexRecursive`).
Latent crash (code reading, not bench-verified): `sx126x` 0.3.0 `reset()` runs `spi.transaction(&mut [Operation::DelayNs(200_000)])` inside `critical_section::with`, and esp-idf-hal's SPI transaction calls the blocking `spi_device_acquire_bus`, which is not allowed with interrupts masked.

## Linked Artefact
`crates/rustyfarian-esp-idf-network/Cargo.toml` feature `lora`; `crates/rustyfarian-esp-idf-network/src/lora/sx1262_driver.rs` `FullDuplexDevice::transaction` and `init_sx1262` step 1; `docs/project-lore.md` "Toolchain & Build" sx126x entry.
Filed upstream by `rustyfarian-beekeeper` as `review-queue/rustyfarian-network-lora-critical-section-provider-v1.md` (2026-10-29).

## Reproduction Confidence
high (link failure); low (interrupt-masking crash, code reading only)

## Severity
medium

## Environment
`rustyfarian-esp-idf-network` 0.6.0 and `main` up to 01ff1b9; ESP-IDF tier with `lora`; beekeeper branch `power-0.3-preview` with `rustyfarian-esp-idf-power` main (`31c52fb`) default features.

## Expected Behaviour
The firmware picks exactly one `critical-section` provider, and LoRa bring-up works under either esp-idf-hal's mutex provider or an interrupt-masking provider.

## Actual Behaviour
The `lora` feature pins esp-idf-hal's mutex provider, a second provider fails to link, and with an interrupt-masking provider `sx126x::reset()` would block on the SPI bus lock with interrupts masked.

## Reproduction Steps
1. Depend on `rustyfarian-esp-idf-network` with `features = ["lora"]`.
2. Add `rustyfarian-esp-idf-power` with its default `critical-section-impl` feature.
3. Build the firmware and observe `multiple definition of '_critical_section_1_0_acquire'` at link time.

## Suggested Fix Area
1. Drop `esp-idf-hal/critical-section` from the `lora` feature and enable it for the crate's examples through a dev-dependency.
2. Drive NRST directly in `EspIdfLoraRadio::new` (low, `Ets::delay_us(200)`, high) instead of `sx126x::reset()`, so no SPI transaction runs inside `critical_section::with`.
3. Correct the driver comment, the lore entry and the `CLAUDE.md` row to say the esp-idf-hal provider is a recursive mutex.
4. Hardware gate: OTAA join on a Heltec V3 with `rustyfarian-esp-idf-power`'s `critical-section-impl` as the only provider.

## Owner
Claude Code (asks 1-3), user (ask 4, bench).

## Links
`review-queue/rustyfarian-network-lora-critical-section-provider-v1.md`; `rustyfarian-peripherals` lore "ESP-IDF — critical-section backend ISR safety" (2026-10-28); `docs/project-lore.md` "Toolchain & Build".

## Session Log
- 2026-10-30 — Promoted from the beekeeper review-queue item straight to defect via /bug
- 2026-10-30 — Asks 1-3 fixed: `lora` drops `esp-idf-hal/critical-section` (examples get it via dev-dependency), `pulse_nrst` replaces `sx126x::reset()`, comment, lore, README and rustdoc corrected (plus the gitignored, developer-local `CLAUDE.md` row); `just verify` green, both LoRa examples build
- 2026-10-30 — Open hardware gate (ask 4): OTAA join on a Heltec V3 with `rustyfarian-esp-idf-power`'s `critical-section-impl` as the only provider; also re-confirm the join with esp-idf-hal's provider after the NRST change
