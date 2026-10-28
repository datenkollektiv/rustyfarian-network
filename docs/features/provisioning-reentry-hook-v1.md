---
gate: hardware
gate-note: BOOT held 3 s after boot on a C3 reaches each outcome in idf_c3_provision_mqtt
desk-work: available
---

# Feature: Provisioning re-entry hook v1

An optional hook the consumer answers once per boot, before the store is read, so a held button can reopen the portal or request a factory reset.
Part 6 of [`wifi-portal-fallback-v1.md`](./wifi-portal-fallback-v1.md); second consumer is spookhouse (`review-queue/archive/rustyfarian-network-reentry-hook-reply-v1.md`).

## Decisions
|                                                            Decision | Reason                                                                | Rejected Alternative                                       |
|--------------------------------------------------------------------:|:----------------------------------------------------------------------|:-----------------------------------------------------------|
|        Hook answers `ReentryRequest { None, Portal, FactoryReset }` | Keep-the-record and erase are different consumer intents              | `FactoryReset` as the only answer                          |
|                    The hook returns an outcome; the consumer erases | The library never erases or restarts, so the contract stays defensive | Library erases on a `FactoryReset` answer                  |
|                   `ReentryRequest` lives in `juggler::provisioning` | Pure type; the esp-hal tier reuses it later                           | Define it in the IDF crate                                 |
| New `WifiMqttLoadOutcome::{PortalRequested, FactoryResetRequested}` | One variant per answer, so log and OLED state stay distinct           | One shared `ReentryPortal` variant                         |
|   New `WifiMqttBoot::load_with_reentry(nvs, reentry, client_id_fn)` | `load` and `load_with` stay unchanged; `None` reproduces them         | Put the hook on `BootConfig` (that is the portal's config) |
|               The hook is `FnOnce() -> ReentryRequest`, no `Result` | The hook cannot fail the boot                                         | `anyhow::Result<ReentryRequest>`                           |
|                                                 IDF tier only in v1 | esp-hal has no boot bundle to hook into yet                           | Add a HAL boot entry point now                             |

## Constraints
- The hook runs before `ProvisioningStore::open`, so a `Portal` or `FactoryReset` answer never reads the record.
- `WifiMqttBoot::load*` never claims the modem; the consumer calls `run_wifi_mqtt_portal` afterwards.
- `PortalRequested` keeps the stored record, and the portal prefills from it as today.
- No library code path calls `erase_all`; `scripts/check-library-never-reboots.sh` extends to IDF `provisioning/boot.rs` and `portal.rs`.
- The hook may sample a pin after boot: return at once when the button is up and block only while it is held (the example's pattern).
- Or it may read a stored flag: a runtime gesture stores a flag in the consumer's own NVS namespace and restarts, and the hook reads and clears the flag before it returns (spookhouse's pattern, so a failed erase cannot loop).
- The hook path never marks the running OTA image valid.
- `WifiMqttLoadOutcome` is `#[non_exhaustive]`, so the new variants are additive.
- The ordering is host-tested in `juggler` against the KV store trait, without ESP-IDF.
- The `idf_c3_provision_mqtt` example demonstrates the hook with BOOT (GPIO9) and erases in example code.

## Open Questions
- [x] Spookhouse needs only `FactoryReset` today; `Portal` stays for routing its fallback portal through the hook later.
- [x] One boot continuing into the portal is acceptable and preferred over a second restart.
- [x] `FactoryReset` does not wait for a pending OTA image; the hook path must never mark the running image valid.
- [ ] Does the esp-hal tier get the hook with its own boot bundle, tracked as a separate feature? (spookhouse: no demand)

## State
- [x] Design approved
- [x] Core implementation
- [x] Tests passing
- [x] Documentation updated

## Session Log
- 2026-10-27 — Feature doc created via /feature dialog; option (a) chosen, design approved upstream.
- 2026-10-27 — Updated: core, example and docs implemented via workflow; hardware gate open.
- 2026-10-27 — Updated: review findings fixed, host tests and `just verify` green; only the hardware gate remains.
- 2026-10-28 — Updated: spookhouse reply closes three open questions, stored-flag hook pattern recorded; hardware gate still open.
