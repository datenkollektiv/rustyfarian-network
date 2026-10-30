---
gate: hardware
gate-note: HAL record with extras round-trips on a C3 or C6 board across a reboot
desk-work: available
---

# Feature: Provisioning boot identity on one juggler type v1

Both tiers hand out the stored record as `juggler::provisioning::ProvisioningConfig`, so a consumer reads the device name and OTA URL without a second store read and no `Debug` on the boot path prints a secret.
Answers `review-queue/rustyfarian-network-boot-device-name-v1.md` (spookhouse) and subsumes the roadmap item "StoredConfig Debug redaction".

## Decisions
|                                                     Decision | Reason                                                                                                                                           | Rejected Alternative                        |
|-------------------------------------------------------------:|:-------------------------------------------------------------------------------------------------------------------------------------------------|:--------------------------------------------|
|                    `WifiMqttBoot` wraps `ProvisioningConfig` | One redaction, zeroize and accessor path for both tiers                                                                                          | Add accessors to the IDF field copy         |
| IDF `ProvisioningStore::load()` returns `ProvisioningConfig` | Matches the HAL store; `StoredConfig` is a second copy of both secrets                                                                           | Keep `StoredConfig` and add `load_config()` |
|                   `StoredConfig` removed in the same release | 0.7.0 is breaking anyway and both consumers migrate at once (decided 2026-10-27, superseding the one-release deprecation)                        | Deprecate it for one release                |
|     `load_with`'s `client_id_fn` takes `&ProvisioningConfig` | Same type as the HAL tier and the portal commit                                                                                                  | Keep `&StoredConfig`                        |
|                               Extras persisted on both tiers | IDF keeps its `x_*` keys; HAL gains them for parity                                                                                              | Extras stay IDF-only                        |
|                            `from_storage_parts` takes extras | It builds `extras` empty today, so IDF would drop them                                                                                           | A separate IDF-side extras list             |
|         HAL extras use a new repeatable `TAG_EXTRA` (`0x0A`) | The decoder skips unknown tags, so 0.6.x still reads a new record of at most 512 B (its read cap); larger records with extras fall back on 0.6.x | Bump a record format version                |
|                           `ota_url()` returns `Option<&str>` | `None` for no OTA is clearer than an empty string                                                                                                | Keep `&str`; add `ota_url_opt()`            |
|                   `Debug` prints `<redacted>` with no length | Nothing derived from a secret is logged, not even its length                                                                                     | Keep `<redacted, len=N>`                    |

## Constraints
- The library never erases or restarts, and `WifiMqttBoot::load` never claims the modem.
- `ProvisioningConfig` is about 1.3 KB by value; it must be moved, never cloned, onto the boot task's stack (bug 002).
- The IDF NVS key layout (ADR 014 §4) does not change; only the in-memory type does.
- Records written by 0.6.x firmware load unchanged on both tiers.
- Breaking for `juggler` and the IDF tier, so it ships in 0.7.0 with CHANGELOG migration notes.
- `just verify` stays green without hardware; host tests cover encode, decode and redaction.

## Open Questions
- [x] Does the HAL record buffer and sector fit 8 extras of 13-byte names plus values, or does the cap shrink on HAL? — No at 512 B: a worst-case `wifi_mqtt` record is 508 B and 8 extras add 640 B. Decided 2026-10-27: `MAX_RECORD_LEN` 512 → 1280 B (fits 1148 B worst case; +768 B transient stack per sector read; the 4 KiB sector is unaffected). The cap stays 8 on both tiers.
- [x] Should `device_name()` also become `Option<&str>`, given the HAL prefill leaves `dev_name` empty? — No: `parse_form` rejects a missing or empty name and the HAL decoder requires `TAG_DEVICE_NAME`, so a stored record always carries one; the empty HAL prefill is a render fallback, not a config state.
- [x] Do the store-format and type changes need an ADR 014 amendment? — Yes, short dated amendments: ADR 015 for `TAG_EXTRA` and the 1280 B read cap, ADR 014 for the IDF in-memory type (NVS keys unchanged).
- [ ] Should a shared host test assert the accessor set is identical on both tiers' re-exports? — Deferred: the IDF crate cannot run host tests, so parity is asserted by the type-checked IDF test instead. The IDF redaction tests are type-checked only as well, so `StoredConfig`'s `Debug` has no host runtime coverage.

## State
- [x] Design approved
- [x] Core implementation
- [x] Tests passing
- [x] Documentation updated
- [ ] Hardware gate: `docs/runbooks/esp-hal-provisioning-extras-hardware-test.md` on C3 and C6

## Session Log
- 2026-10-27 — Feature doc created via /feature dialog
- 2026-10-27 — Desk work landed: juggler `ota_url()` as `Option<&str>` and extras through `from_storage_parts`, IDF `load()` and `WifiMqttBoot` on `ProvisioningConfig` with `StoredConfig` removed, HAL `TAG_EXTRA` and the 1280 B read cap, CHANGELOG, ADR 014 and 015 amendments, runbook `docs/runbooks/esp-hal-provisioning-extras-hardware-test.md`.
  The hardware gate (HAL record with extras across a reboot on C3 and C6) is open; the runbook submits extras with the new `just portal-save` recipe.
- 2026-10-27 — Final review asked for an IDF boot smoke at rgb-clock's 8000 B main-task stack before the 0.7.0 release, because the record crosses the stack once in `load`; added as scenario H4 in the runbook.
- 2026-10-27 — `StoredConfig` removed outright instead of deprecated for one release (user decision); the IDF store's NVS logic moved to `juggler::provisioning::kv_store` with host tests for the 0.6.x key set and the write order.
- 2026-10-30 — Bench test 5 of hal-provisioning-ap-ownership-v1 showed a HAL record without extras survives a reboot on an ESP32-C3 Mini-1, but only after bug 006 (esp-storage rejects the unaligned `record_len` read) was fixed; before it no record read back at all, so the extras runbook H1 would have failed. The extras variant of H1 and the C6 run stay open.
