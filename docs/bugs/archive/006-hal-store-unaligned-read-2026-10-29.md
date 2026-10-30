---
id: 006
title: esp-hal ProvisioningStore never reads a committed record back (unaligned flash read)
captured-on: 2026-10-29
doc-version: 3
status: closed
kind: defect
---

# Bug 006: esp-hal ProvisioningStore never reads a committed record back (unaligned flash read)

## Symptom
After a successful portal save ("credentials committed to flash"), the next boot of `hal_c3_provision_mqtt` reopens the portal, and with the error logged the example prints `Store is_provisioned error: Flash`.

## Suspected Cause
`ProvisioningStore::try_read_sector` reads exactly `record_len` bytes (146 for the saved `wifi_mqtt` record), and esp-storage 0.10 `ReadNorFlash::read` rejects any length that is not a multiple of `READ_SIZE` (4 bytes without the `bytewise-read` feature), so every real record fails with `NotAligned` while blank flash passes because the clamped 1280-byte read is aligned.

## Linked Artefact
`crates/rustyfarian-esp-hal-network/src/provisioning/store.rs` `try_read_sector`; esp-storage `=0.10.0` `src/nor_flash.rs` `read_nor`; found during `docs/features/archive/hal-provisioning-ap-ownership-v1.md` hardware test 5.

## Reproduction Confidence
high

## Severity
high

## Environment
ESP32-C3 Mini-1, `hal_c3_provision_mqtt` at commit c1ac065 plus the uncommitted working tree of 2026-10-29, esp-hal 1.2.2, esp-storage 0.10.0 with features `critical-section`, `embedded-storage`; host tests pass because `MockFlash` declares `READ_SIZE = 1`.

## Expected Behaviour
A record that `decode_record` accepts on the host (verified against a flash dump of the device) is read back on the next boot, and `is_provisioned()` returns `Ok(true)`.

## Actual Behaviour
`is_provisioned()` returns `Err(StoreError::Flash)` on every boot that follows a save, and the example's `unwrap_or(false)` turned that into a silent re-provisioning loop.

## Reproduction Steps
1. `just flash hal_c3_provision_mqtt` on an unprovisioned C3, join the AP, save the form, and wait for `credentials committed to flash`.
2. Reset the board.
3. Observe `Provisioning AP up` instead of `Already provisioned`; with the error logged, observe `Store is_provisioned error: Flash`.
4. `scripts/espflash.sh read-flash 0x300000 0x2000 tmp/dump.bin` shows a complete record that `decode_record` accepts on the host.

## Suggested Fix Area
Round the second read in `try_read_sector` up to a multiple of `F::READ_SIZE` (capped at `MAX_RECORD_LEN`) and decode only the first `record_len` bytes; add a host mock with `READ_SIZE = 4` that rejects unaligned reads so the contract is tested.

## Owner
Claude Code (fix in progress during the hardware session, user commits).

## Links
`docs/features/archive/hal-provisioning-ap-ownership-v1.md`; `docs/runbooks/esp-hal-provisioning-extras-hardware-test.md` (H1, never run before today); `docs/features/provisioning-boot-identity-v1.md` (open hardware gate).

## Session Log
- 2026-10-29 — Defect captured directly from the hardware session (flash dump decodes on host, device reports `StoreError::Flash`)
- 2026-10-29 — Fix applied in `try_read_sector` (read rounded up to `F::READ_SIZE`), regression test `store_reads_back_record_through_word_aligned_flash` fails without the fix, C3 boot now logs `Already provisioned`
- 2026-10-29 — Closed via /bug after the C3 reboot check; lore recorded under "SoftAP Provisioning"
