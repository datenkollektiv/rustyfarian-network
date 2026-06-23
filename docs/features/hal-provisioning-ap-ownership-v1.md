# Feature: hal provisioning owns SoftAP bring-up (SSID parity) v1

Make the bare-metal (`rustyfarian-esp-hal-network`) provisioning `start()` own SoftAP
bring-up, so `PortalConfig.ssid_override` (and the derived default) actually drive the
radio SSID — reaching parity with the esp-idf path. This completes the hal half of
[`ssid-override-v1.md`](./ssid-override-v1.md), whose hal field is currently
**validation-only** (it checks the string but does not set the radio SSID).

## Why this exists (the gap that forced it)

On esp-idf, `ProvisioningBuilder::start` owns AP bring-up (it calls `SoftApManager::start`
internally with the resolved SSID), so `PortalConfig.ssid_override` is the only way to set
the SSID and it works end-to-end.

On esp-hal today, the **caller** brings the AP up *before* `start()`:
`ApConfig::open(PORTAL_SSID_PREFIX)` → `WiFiManager::init_softap_async(..)` →
`builder.start(spawner, softap, store, rng)`. So:
- `start()` never sets the radio SSID; the implemented `ssid_override` is validated against a
  dummy zero MAC and discarded — a "works but wrong" footgun.
- `init_softap_async` uses the SSID **verbatim** (`with_ssid(config.ap.ssid)`, `wifi/mod.rs:569`)
  — hal has **no MAC suffix at all** today (its default AP name is the bare `{prefix}`, unlike
  idf's `{prefix}-{MAC}`). The hal example's "last two MAC bytes appended" comment is stale.
- hal has **no `softap_mac()`** — there is no AP-MAC read in the crate.

## Scope and end state

When done:
- The `embassy`-gated hal `ProvisioningBuilder::start` takes the **AP peripherals directly**
  (`TIMG0`, `SW_INTERRUPT`, `WIFI`) instead of a pre-built `SoftApHandle`, and brings the
  SoftAP up itself.
- `start()` resolves the SSID via the shared `juggler::provisioning::resolve_softap_ssid(
  ssid_override, ssid_prefix, mac)` using the **real AP MAC** (new hal `softap_mac()`), builds
  `ApConfig` internally from `PortalConfig` (resolved SSID + `channel` + `ap_password`), calls
  `init_softap_async`, then runs the portal as today.
- `ssid_override: Some("MyClock")` → the hal AP **broadcasts exactly `MyClock`** (parity with
  idf; acceptance criterion #1 of `ssid-override-v1` now holds on both paths).
- `ssid_override: None` → hal AP broadcasts `{prefix}-{MAC}` (new default, matches idf).
- The caller no longer builds `ApConfig` or calls `init_softap_async` for provisioning.
- The interim validation-only hal `ssid_override` code is replaced by real wiring.
- esp-idf path is unchanged.

## Decisions

|                                                                                                      Decision | Reason                                                                                                                                                                    | Rejected Alternative                                                                                                                                                                  |
|--------------------------------------------------------------------------------------------------------------:|:--------------------------------------------------------------------------------------------------------------------------------------------------------------------------|:--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
|             **`start()` takes individual AP peripherals** — `start(spawner, timg0, sw_int, wifi, store, rng)` | Matches the existing `with_ap_peripherals(TIMG0, SW_INTERRUPT, WIFI)` convention; conventional esp-hal style; the caller stops building `ApConfig` entirely.              | A `ApPeripherals { .. }` bundle struct (extra public type for little gain). Or a deferred-SSID `ApConfig` (splits AP config across two structs; keeps an SSID-less `ApConfig` shape). |
| **`start()` owns AP bring-up** (builds `ApConfig` + calls `init_softap_async` internally from `PortalConfig`) | Makes `PortalConfig` the single source of SSID/channel/password truth on hal, mirroring idf where `start()` owns `SoftApManager`. Eliminates the validation-only footgun. | Keep caller-owned bring-up + validation-only `ssid_override` (the footgun this feature exists to remove).                                                                             |
|                                **hal default SSID becomes `{prefix}-{MAC}`** (parity with idf) — **breaking** | One cross-HAL default; `resolve_softap_ssid`'s `None` path already derives it. Without this, the two HALs keep diverging defaults.                                        | Keep hal's verbatim-`{prefix}` default (defeats parity; leaves a surprising cross-HAL inconsistency).                                                                                 |
|                                               **Add a hal `softap_mac()`** to read the AP MAC before bring-up | Needed to bake `{prefix}-{MAC}` into `ApConfig` before the AP starts; mirrors idf's `softap_mac()`.                                                                       | Support only the override on hal with no derived default (hal would have no SSID without an explicit override — worse UX, still divergent).                                           |
|                                                                    **Reuse the shared `resolve_softap_ssid`** | Same pure, host-tested resolver both HALs use — no drift. The `None` path now receives the real hal MAC.                                                                  | A hal-local SSID resolver (drift; duplicates the policy).                                                                                                                             |

## Constraints

- **BREAKING API change.** `start()`'s signature changes (AP peripherals instead of `SoftApHandle`); both hal examples and any downstream consumer update their call sites. Bare-metal + pre-1.0 ("API may change before 1.0") → acceptable with a CHANGELOG migration note and an appropriate version bump.
- **BREAKING behavior change.** hal's default AP SSID changes from `{prefix}` to `{prefix}-{MAC}`; existing hal APs are renamed. Document the rename and the migration (consumers wanting the bare name set `ssid_override: Some("name")`).
- **MAC must be readable before the AP is up** so the SSID can be baked into `ApConfig`. Confirm the esp-radio/esp-hal path (e.g. `esp_wifi_get_mac` / efuse read) and whether it requires the radio/controller to be initialised first — if the MAC is only available after radio init, the bring-up order must be: init radio → read MAC → resolve SSID → start AP (may mean splitting `init_softap_async`). See Open Questions.
- **Stays within the existing gate** `#[cfg(all(feature = "embassy", any(feature = "esp32c3", feature = "esp32c6")))]`.
- **AP channel + password come from `PortalConfig`** (`channel`, `ap_password`) — the internally-built `ApConfig` honours them; the existing AP-password length check in `start()` still applies.
- **Everything else is preserved** — portal HTML, DHCP, DNS catch-all, nonce, factory-reset, `ProvisioningError` surface (plus the `InvalidSsid` variant from `ssid-override-v1`).
- **esp-idf path untouched** (already correct).
- The general-purpose `WiFiManager::init_softap_async` / `ApConfig` / `ApConfigExt` stay public for non-provisioning SoftAP users; only the provisioning `start()` stops requiring a pre-built handle.

## Open Questions

- [ ] Exact esp-radio/esp-hal API to read the AP MAC before bring-up, and whether it needs radio init first. If MAC is only readable post-init, decide how `start()` orders init → MAC → SSID → AP-start (possibly splitting `init_softap_async` into an init step + a start-with-ssid step).
- [ ] Is `softap_mac()` purely platform (esp-radio FFI) with nothing host-testable, or can the derive be exercised via the existing `resolve_softap_ssid` juggler tests + an on-hardware MAC-suffix check? (Likely the latter — the derivation is already host-tested; only the MAC *read* is platform.)
- [ ] Does the breaking "PortalConfig is the SSID source of truth on both HALs" precedent warrant a short ADR?

## State

- [x] Design approved
- [ ] Core implementation
- [ ] Tests passing
- [ ] Documentation updated

## Acceptance criteria

1. hal `start(spawner, timg0, sw_int, wifi, store, rng)` brings up the SoftAP itself; the caller no longer builds `ApConfig` or calls `init_softap_async` for provisioning.
2. `ssid_override: Some("MyClock")` → the hal AP **broadcasts exactly `MyClock`** (verified on the radio, not merely validated) — `ssid-override-v1` acceptance criterion #1 now holds on both HALs.
3. `ssid_override: None` → the hal AP broadcasts `{prefix}-{MAC}` using the real AP MAC from the new `softap_mac()` (new default; matches idf).
4. The interim validation-only hal `ssid_override` code (dummy zero-MAC `resolve_softap_ssid` call) is removed and replaced by the real wiring.
5. Both hal examples (`hal_c3_provision_mqtt`, `hal_c6_provision_mqtt`) updated to the new `start()` signature and build clean (`just build-example`).
6. CHANGELOG documents BOTH breaking changes (the `start()` signature and the default-SSID rename) with explicit migration guidance.
7. `just verify` green; esp-idf provisioning behavior unchanged.
8. On-hardware check: a hal device with `ssid_override: None` shows a `-XXXX` MAC suffix; with `ssid_override: Some(..)` shows the verbatim name.

## Relationship to other features
- Completes the hal half of [`ssid-override-v1.md`](./ssid-override-v1.md) (idf already ships there; hal was validation-only pending this re-architecture).
- Reuses `juggler::provisioning::resolve_softap_ssid` and the `ProvisioningError::InvalidSsid` variant introduced by `ssid-override-v1`.

## Session Log
- 2026-06-23 — Feature doc created via /feature dialog after implementing `ssid-override-v1` surfaced that the hal `ssid_override` was validation-only (hal's caller owns AP bring-up; hal uses the SSID verbatim with no MAC suffix; hal has no `softap_mac`). Decided: re-architect hal `start()` to own bring-up taking individual AP peripherals (`timg0, sw_int, wifi`); hal default SSID becomes `{prefix}-{MAC}` (breaking, for parity); add hal `softap_mac()`; reuse the shared `resolve_softap_ssid`. Open: exact MAC-read API + ordering vs radio init; host-testability of the MAC read; possible ADR.
