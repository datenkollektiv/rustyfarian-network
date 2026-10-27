---
gate: hardware
gate-note: HAL record with extras round-trips on a C3 or C6 board across a reboot
desk-work: available
---

# Feature: Provisioning boot identity on one juggler type v1

Both tiers hand out the stored record as `juggler::provisioning::ProvisioningConfig`, so a consumer reads the device name and OTA URL without a second store read and no `Debug` on the boot path prints a secret.
Answers `review-queue/rustyfarian-network-boot-device-name-v1.md` (spookhouse) and subsumes the roadmap item "StoredConfig Debug redaction".

## Decisions
|                                                                Decision | Reason                                                                  | Rejected Alternative                        |
|------------------------------------------------------------------------:|:------------------------------------------------------------------------|:--------------------------------------------|
|                               `WifiMqttBoot` wraps `ProvisioningConfig` | One redaction, zeroize and accessor path for both tiers                 | Add accessors to the IDF field copy         |
|            IDF `ProvisioningStore::load()` returns `ProvisioningConfig` | Matches the HAL store; `StoredConfig` is a second copy of both secrets  | Keep `StoredConfig` and add `load_config()` |
|                               `StoredConfig` deprecated for one release | Gives rgb-clock and spookhouse a migration window                       | Delete it in the same release               |
| `StoredConfig` keeps a redacting `Debug` and `From<ProvisioningConfig>` | The deprecation window must not leak passwords                          | Leave `#[derive(Debug)]` until removal      |
|                `load_with`'s `client_id_fn` takes `&ProvisioningConfig` | Same type as the HAL tier and the portal commit                         | Keep `&StoredConfig`                        |
|                                          Extras persisted on both tiers | IDF keeps its `x_*` keys; HAL gains them for parity                     | Extras stay IDF-only                        |
|                                       `from_storage_parts` takes extras | It builds `extras` empty today, so IDF would drop them                  | A separate IDF-side extras list             |
|                    HAL extras use a new repeatable `TAG_EXTRA` (`0x0A`) | The decoder skips unknown tags, so old firmware still reads new records | Bump a record format version                |
|                                      `ota_url()` returns `Option<&str>` | `None` for no OTA is clearer than an empty string                       | Keep `&str`; add `ota_url_opt()`            |
|                              `Debug` prints `<redacted>` with no length | Nothing derived from a secret is logged, not even its length            | Keep `<redacted, len=N>`                    |

## Constraints
- The library never erases or restarts, and `WifiMqttBoot::load` never claims the modem.
- `ProvisioningConfig` is about 1.3 KB by value; it must be moved, never cloned, onto the boot task's stack (bug 002).
- The IDF NVS key layout (ADR 014 §4) does not change; only the in-memory type does.
- Records written by 0.6.x firmware load unchanged on both tiers.
- Breaking for `juggler` and the IDF tier, so it ships in 0.7.0 with CHANGELOG migration notes.
- `just verify` stays green without hardware; host tests cover encode, decode and redaction.

## Open Questions
- [ ] Does the HAL record buffer and sector fit 8 extras of 13-byte names plus values, or does the cap shrink on HAL?
- [ ] Should `device_name()` also become `Option<&str>`, given the HAL prefill leaves `dev_name` empty?
- [ ] Do the store-format and type changes need an ADR 014 amendment?
- [ ] Should a shared host test assert the accessor set is identical on both tiers' re-exports?

## State
- [x] Design approved
- [ ] Core implementation
- [ ] Tests passing
- [ ] Documentation updated

## Session Log
- 2026-10-27 — Feature doc created via /feature dialog
