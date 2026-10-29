---
gate: hardware
gate-note: on-device SSID check on a C3 or C6 board (acceptance 8) — default shows a -XXXX suffix, override shows verbatim
desk-work: available
---

# Feature: hal provisioning owns SoftAP bring-up (SSID parity) v1

Makes the bare-metal `rustyfarian-esp-hal-network` provisioning `start()` own SoftAP bring-up, so `PortalConfig.ssid_override` and the derived default drive the radio SSID, as on esp-idf.
Completes the hal half of [`ssid-override-v1.md`](./ssid-override-v1.md), whose hal field is validation-only today.
Today the caller runs `ApConfig::open(PORTAL_SSID_PREFIX)` → `WiFiManager::init_softap_async(..)` → `builder.start(spawner, softap, store, rng)`; hal uses the SSID verbatim with no MAC suffix, has no AP-MAC read, and its example's "last two MAC bytes appended" comment is stale.

## Decisions
| Decision                                                               | Reason                                                                      | Rejected Alternative                                            |
|:-----------------------------------------------------------------------|:----------------------------------------------------------------------------|:----------------------------------------------------------------|
| `start(spawner, timg0, sw_int, wifi, store, rng)` takes AP peripherals | Matches the `with_ap_peripherals` convention; no caller-built `ApConfig`    | `ApPeripherals` bundle struct; deferred-SSID `ApConfig`         |
| `start()` owns AP bring-up from `PortalConfig`                         | `PortalConfig` is the single SSID/channel/password source, as on idf        | Caller-owned bring-up + validation-only override (the footgun)  |
| hal default SSID becomes `{prefix}-{MAC}` (breaking)                   | One cross-HAL default; without it the HALs keep diverging                   | Keep verbatim `{prefix}` default                                |
| Add hal `softap_mac()`                                                 | Bakes the MAC suffix into `ApConfig` before AP start, as on idf             | Override-only, no derived default (no SSID without an override) |
| Reuse shared `juggler::provisioning::resolve_softap_ssid`              | Same pure, host-tested resolver as idf: `(ssid_override, ssid_prefix, mac)` | hal-local resolver (drift)                                      |

## Constraints
- **BREAKING API:** `start()` takes AP peripherals instead of `SoftApHandle`; pre-1.0, so it needs a CHANGELOG migration note and a version bump.
- **BREAKING behaviour:** hal's default AP SSID changes from `{prefix}` to `{prefix}-{MAC}`, so existing hal APs are renamed; consumers wanting the bare name set `ssid_override: Some("name")`.
- Bring-up order is read MAC → `resolve_softap_ssid` → build `ApConfig` → `init_softap_async`; no split of `init_softap_async` is needed.
- `init_softap_async`'s `with_ssid` call takes the SSID verbatim, so the derived name must be baked into `ApConfig` upstream.
- Gate: `#[cfg(all(feature = "embassy", any(feature = "esp32c3", feature = "esp32c6", feature = "esp32s3")))]`; the new `start()` and `softap_mac()` keep all three chips.
- AP `channel` and `ap_password` come from `PortalConfig`; the existing AP-password length check in `start()` stays.
- Portal HTML, DHCP, DNS catch-all, nonce, factory reset and the `ProvisioningError` surface (incl. `InvalidSsid`) are preserved.
- esp-idf path untouched.
- `WiFiManager::init_softap_async`, `ApConfig` and `ApConfigExt` stay public for non-provisioning SoftAP users.
- The session keeps the AP MAC rather than reading and discarding it, because [`per-device-name-prefill-v1.md`](./per-device-name-prefill-v1.md) derives its hal `dev_name` pre-fill from it.

## Open Questions
- [x] Which API reads the AP MAC before bring-up, and does it need radio init? Resolved: `esp_hal::efuse::interface_mac_address(InterfaceMacAddress::AccessPoint)` (esp-hal 1.2.2) is an efuse read needing no radio init; esp-radio 1.0.0-beta.1 `mac_address()` calls it; `MacAddress::as_bytes()` returns `&[u8]`; confirm on target that the MAC shown matches the broadcast AP.
- [x] Is `softap_mac()` host-testable? Resolved: the derivation is host-tested via `resolve_softap_ssid`; only the MAC read is platform code, covered by acceptance 8.
- [x] Does the breaking "PortalConfig is the SSID source of truth on both HALs" precedent warrant a short ADR? Resolved: amend ADR 015 (dated 2026-10-29) rather than a new ADR; the contract is recorded in the Decisions table and ssid-override-v1.

## State
- [x] Design approved
- [x] Core implementation
- [x] Tests passing
- [x] Documentation updated

## Acceptance criteria
1. hal `start(spawner, timg0, sw_int, wifi, store, rng)` brings up the SoftAP itself; the caller no longer builds `ApConfig` or calls `init_softap_async` for provisioning.
2. `ssid_override: Some("MyClock")` makes the hal AP broadcast exactly `MyClock`, verified on the radio and not merely validated; `ssid-override-v1` criterion 1 then holds on both HALs.
3. `ssid_override: None` makes the hal AP broadcast `{prefix}-{MAC}` using the real MAC from `softap_mac()` (new default, matches idf).
4. The validation-only call (`resolve_softap_ssid(..., &[0u8; 6])`, step 0b of `session.rs` `start()`) is removed and replaced by real wiring.
5. Both hal examples (`hal_c3_provision_mqtt`, `hal_c6_provision_mqtt`) use the new `start()` signature and build clean via `just build-example`.
6. CHANGELOG documents both breaking changes (`start()` signature and default-SSID rename) with explicit migration guidance.
7. `just verify` is green and esp-idf provisioning behaviour is unchanged.
8. On hardware (C3 or C6), `ssid_override: None` shows a `-XXXX` MAC suffix and `ssid_override: Some(..)` shows the verbatim name.

## Relationship to other features
- Completes the hal half of [`ssid-override-v1.md`](./ssid-override-v1.md) and reuses its `juggler::provisioning::resolve_softap_ssid` and `ProvisioningError::InvalidSsid`.
- Blocks [`per-device-name-prefill-v1.md`](./per-device-name-prefill-v1.md), whose hal pre-fill derives from the same AP MAC; that feature is out of scope here.

## Session Log
- 2026-06-23 — Feature doc created after `ssid-override-v1` exposed the validation-only hal `ssid_override`; re-architecture chosen.
- 2026-10-29 — Updated: frontmatter added, MAC-read question resolved (efuse), esp32s3 in gate, per-device-name dependency noted
- 2026-10-29 — Implementation landed: `start()` owns AP bring-up from `PortalConfig`, `softap_mac()` added, examples, README and CHANGELOG updated, ADR 015 amended; acceptance 8 (on-device SSID check on C3 or C6) is still open for the user.
