---
id: 003
title: ProvisioningConfig secrets are never scrubbed from memory (no zeroize on drop)
captured-on: 2026-09-27
doc-version: 3
status: closed
kind: defect
---

# Bug 003: `ProvisioningConfig` secrets are never scrubbed from memory (no `zeroize` on drop)

## Symptom
The Wi-Fi password, MQTT password, and LoRaWAN AppKey held in a `ProvisioningConfig` stay readable in RAM after the value is dropped, and the bug 002 panic dump printed them in clear text.

## Suspected Cause
Verified against source.

- `ProvisioningConfig` (`crates/juggler/src/provisioning/config.rs`) is a plain struct of `heapless::String` buffers with no `Drop` impl.
- Dropping it leaves the bytes in place on the stack or in the `Arc<Mutex<StateInner>>` slot until something else overwrites them.
- `juggler` has no `zeroize` dependency, and the workspace `deny.toml` has never evaluated it.
- The `Debug` impl already redacts the three secrets, so log output is covered; only the memory lifetime is not.

## Linked Artefact
- `crates/juggler/src/provisioning/config.rs` — `ProvisioningConfig`, `LoraFields`, `MqttFields`, `ExtraField`.
- `crates/rustyfarian-esp-idf-network/src/provisioning/mod.rs` — `StateInner.committed`, the `Option<ProvisioningConfig>` that outlives the portal session.
- `crates/rustyfarian-esp-hal-network/src/provisioning/record.rs` — the bare-metal store's `ProvisioningConfig` round-trip.
- Follow-up split out of bug 002 (`docs/bugs/002-provisioning-config-stack-clone-2026-09-27.md`, "Suggested Fix Area").

## Reproduction Confidence
high

## Severity
medium

## Environment
- `juggler` 0.5.1 (unreleased, `scouting-2026-09-27`), both the ESP-IDF and esp-hal tiers.
- Chip-independent: any target that keeps a `ProvisioningConfig` in RAM.
- Most visible on ESP-IDF, where a panic backtrace dumps live stack frames to the console.

## Expected Behaviour
- Dropping a `ProvisioningConfig` (or any struct that holds one) overwrites the secret buffers in that value's final storage before the memory is released.
- Out of scope, and not guaranteed by the fix: bytes left behind by moves of the inline `heapless` buffers (including `Option::take`), the raw form body and parser/NVS scratch buffers, and panic dumps of live frames — a later panic dump can still contain credential bytes from those locations. See `# Secret lifetime` on `ProvisioningConfig`.
- The redaction contract stays intact: `Debug` never prints secrets, and no new code path logs them.

## Actual Behaviour
- Secret bytes persist in freed stack and heap memory until reuse.
- After a commit the ESP-IDF session keeps the config in `StateInner.committed` for the session's lifetime, or until `wait_committed` moves it out.
- Bug 002's stack-protection panic printed the submitted Wi-Fi and MQTT passwords from the live frames.

## Reproduction Steps
1. Build `idf_c3_provision`, flash, join the SoftAP, and submit a form with a Wi-Fi password.
2. Let `wait_committed` return and drop the returned `ProvisioningConfig`.
3. Trigger any panic that prints a stack dump, or attach a debugger and read the freed stack region.
4. Search the dump for the submitted password bytes; they are present.

## Suggested Fix Area
- Evaluate `zeroize` (with `derive`) as an optional `juggler` dependency behind the existing `provisioning` feature, via the `dependency-manager` agent and `cargo deny`; it is `no_std`-compatible.
- `heapless::String` does not implement `Zeroize`; scrub the secret buffers by hand in a `Drop` impl (`as_mut_vec` / `clear` plus a volatile overwrite of the backing array), or wrap them in a small newtype that does.
- Scope the scrub to the three secrets (`wifi_password`, `MqttFields.password`, `LoraFields.app_key`) plus `extras` values, since a portal profile may route a secret through an extra field.
- A `Drop` impl makes the struct non-`Copy` but it already is not; check that `Clone` stays derivable and that the esp-hal `record.rs` round-trip still compiles.
- Add a host test that fills a config, drops it in a controlled buffer, and asserts the secret bytes are gone; mirror the `debug_redacts_password_and_app_key` fixture pattern so CodeQL sees no literal credential.
- Scrubbing does not help a panic that dumps live frames; the bug 002 fix (fewer copies) is the mitigation for that case.

## Owner
Florian Waibel.

## Links
- Parent: `docs/bugs/002-provisioning-config-stack-clone-2026-09-27.md`.
- Related lore: `docs/project-lore.md` "SoftAP Provisioning".
- Related ADR: `docs/adr/014-wifi-mqtt-provisioning-profile.md`.
- CodeQL fixture guidance: `CLAUDE.md` "Common Resolution Failures", `rust/hard-coded-cryptographic-value` row.

## Session Log
- 2026-09-27 — Filed as a defect, split out of bug 002 after the stack-clone fix landed; scope and fix area derived from the bug 002 report and the `juggler` source.
- 2026-09-27 — Fixed on `fix/provisioning-config-stack-clone`: `Drop` impls on `ProvisioningConfig`, `LoraFields`, and `MqttFields` scrub the four secret slots via `zeroize` 1.9.0 (`default-features = false`, dependency review and `cargo deny` clean); host tests `drop_scrubs_*` in `config.rs` were written first and failed, then passed; `just verify`, `check-provisioning-hal`, and `build-example idf_c3_provision` green; closed via /bug and archived.
- 2026-09-28 — Late review: Expected Behaviour narrowed to the implemented guarantee (final storage only); moved-from copies, external buffers, and live-frame dumps listed as out of scope, matching the rustdoc and CHANGELOG. `ExtraField` `Debug` now redacts values, and a full-capacity `scrub` test was added.
- 2026-09-28 — Branch review: `ExtraField` had no `Drop`, so a clone taken out of `extras()` kept its value in freed memory; `ExtraField` now scrubs its own value (`ProvisioningConfig::drop` relies on it), host test `drop_scrubs_cloned_extra_field_value`. `scrub` now `debug_assert!`s its capacity resize instead of discarding the result.
