# Feature: STA-only `wifi` feature (cfg-gate SoftAP) v1

Gate the SoftAP surface of the `wifi` module behind `#[cfg(esp_idf_esp_wifi_softap_support)]` so the `wifi` feature of `rustyfarian-esp-idf-network` compiles on ESP-IDF builds with `CONFIG_ESP_WIFI_SOFTAP_SUPPORT=n`.
Sourced from a downstream feature request (rustyfarian-beekeeper); see `review-queue/feature-request-wifi-softap-cfg-gate.md` for the original report.

## Problem

`SoftApManager::ap_ip` and `pin_ap_netif_ip` call `EspWifi::ap_netif()` unconditionally, but that method is `#[cfg(esp_idf_esp_wifi_softap_support)]` in `esp-idf-svc` (0.52.1, `src/wifi.rs:1664`).
Any consumer that sets `CONFIG_ESP_WIFI_SOFTAP_SUPPORT=n` and enables `features = ["wifi"]` therefore fails with `E0599: no method named ap_netif`.
The coupling was introduced by the v0.4.0 crate consolidation, which folded the former STA-only `rustyfarian-esp-idf-wifi` crate and `SoftApManager` into one `wifi` module.
Beekeeper's applied workaround (`CONFIG_ESP_WIFI_SOFTAP_SUPPORT=y`) reverses a deliberate flash/RAM optimisation and should become unnecessary.

## Scope and end state

When done, an STA-only consumer with `CONFIG_ESP_WIFI_SOFTAP_SUPPORT=n` builds `features = ["wifi"]` cleanly:

- `WiFiManager`, `IdfWifiConfig`, and all pure re-exports from `juggler::wifi` (including `ApConfig` and `validate_ap_config`, which are host-testable pure types) stay always-available.
- `SoftApManager` (the whole type and impl: `start`, `ap_ip`, `ap_mac`, `station_count`, `stop`) and `pin_ap_netif_ip` are compiled only when the IDF supports SoftAP.
- Enabling `provisioning` on a SoftAP-disabled sdkconfig produces one clear `compile_error!` instead of a wall of E0599s.
- A `just` recipe (wired into CI) checks the `wifi` feature against a SoftAP-disabled sdkconfig so this class of regression is caught.

No API or behaviour change for SoftAP-enabled builds (the default).

## Sketch

```rust
#[cfg(esp_idf_esp_wifi_softap_support)]
pub struct SoftApManager { /* unchanged */ }

#[cfg(esp_idf_esp_wifi_softap_support)]
fn pin_ap_netif_ip(wifi: &EspWifi<'_>) -> anyhow::Result<()> { /* unchanged */ }

#[cfg(all(feature = "provisioning", not(esp_idf_esp_wifi_softap_support)))]
compile_error!(
    "the `provisioning` feature requires SoftAP support; set CONFIG_ESP_WIFI_SOFTAP_SUPPORT=y in sdkconfig.defaults"
);
```

*Illustrative sketch; the exact set of gated items is fixed during implementation by building against a SoftAP-disabled sdkconfig.*

## Decisions

|                                                                                                            Decision | Reason                                                                                                                                                                                                                                                                 | Rejected Alternative                                                                                                                              |
|--------------------------------------------------------------------------------------------------------------------:|:-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|:--------------------------------------------------------------------------------------------------------------------------------------------------|
|                          **Gate with the existing IDF cfg `esp_idf_esp_wifi_softap_support`, no new Cargo feature** | The sdkconfig is the single source of truth for whether the IDF has SoftAP; mirroring how `esp-idf-svc` itself gates `ap_netif` means the crate surface can never disagree with the IDF it links against. Option 1 (preferred) of the downstream request.              | A `wifi-softap` Cargo sub-feature: heavier, introduces a second axis that can contradict the sdkconfig (feature on, IDF support off still fails). |
|                      **Gate `SoftApManager` and `pin_ap_netif_ip` wholesale, not just the `ap_netif()` call sites** | A SoftAP manager on an IDF without SoftAP is not meaningfully degradable; per-method gating would leave a type that constructs but cannot report its IP. Wholesale gating gives one clear "item not found" at the use site instead of runtime surprises.               | Per-call-site gating with runtime fallbacks (e.g. `ap_ip()` returning `Err`) — hides a configuration error until the field.                       |
|                              **Keep pure re-exports (`ApConfig`, `validate_ap_config`) and `softap_mac()` ungated** | The `juggler::wifi` types are pure, host-testable validation logic with no IDF dependency. `softap_mac()` reads the efuse factory MAC via `esp_read_mac` and works without initialising Wi-Fi, independent of SoftAP support.                                          | Gating everything with "SoftAP" in the name — breaks host tests and espnow's MAC-derived naming for no compile-surface gain.                      |
|                                                           **`compile_error!` for `provisioning` + SoftAP-disabled** | `provisioning` inherently requires SoftAP (`ProvisioningBuilder::start` drives `SoftApManager`); a targeted `compile_error!` names the fix in one line, where cfg-gating the portal would surface as scattered unresolved-import noise.                                | Cfg-gating the whole provisioning portal path — silently empties a feature the consumer explicitly enabled.                                       |
|                                                   **Add a SoftAP-disabled check build as a `just` recipe + CI job** | The downstream request explicitly asks for regression coverage; per the working directives, build/verification commands live in `just` recipes. `ESP_IDF_SDKCONFIG_DEFAULTS` can point the check at an alternate defaults file without touching the workspace default. | Relying on downstream consumers to report breakage (this is how the current regression shipped).                                                  |

## Constraints

- **Additive, non-breaking for SoftAP-enabled builds.** Default sdkconfig builds see an unchanged API; patch or minor version bump only.
- **No new Cargo feature.** The existing IDF cfg is the only gating axis.
- **Rustdoc must state the requirement.** Gated items document that they require `CONFIG_ESP_WIFI_SOFTAP_SUPPORT=y`, so users don't hit confusing cfg-hidden references.
- **The `espnow` AP path must be verified, not assumed.** `EspNowDriver::init_with_radio` starts a SoftAP via `Configuration::AccessPoint` but never calls `ap_netif()`; whether it compiles SoftAP-disabled must be established by the check build before deciding if `espnow` needs gating or its own `compile_error!`.
- **The check build needs its own sdkconfig.** embuild resolves `sdkconfig.defaults` from the workspace root (see project lore), so the SoftAP-disabled variant must be selected via `ESP_IDF_SDKCONFIG_DEFAULTS` pointing at a dedicated defaults file, and the recipe must account for stale `esp-idf-sys` build artifacts between variant switches.
- **Targets.** The check build uses `riscv32imac-esp-espidf` (the `just verify` target).

## Open Questions

- [ ] Does `espnow` (`init_with_radio`'s `Configuration::AccessPoint` path) compile with SoftAP disabled, or does it need the same gate / `compile_error!`?
- [ ] Where do the SoftAP-disabled defaults file live (`sdkconfig.sta-only.defaults` at workspace root vs. a `ci/` subdirectory), given embuild's workspace-root resolution rule?
- [ ] Does cfg-hiding `SoftApManager` (and `pin_ap_netif_ip`) on SoftAP-disabled builds count as a breaking API change? (These items only disappear for an sdkconfig that previously could not compile against this crate at all, so no build that currently succeeds loses API.) This determines whether the bump is patch (0.4.x) or minor (0.5.0).

## State

- [ ] Design approved
- [ ] Core implementation
- [ ] Tests passing
- [ ] Documentation updated

## Acceptance criteria

1. `cargo check` equivalent via `just` for `--features wifi` succeeds against a sdkconfig with `CONFIG_ESP_WIFI_SOFTAP_SUPPORT=n` on `riscv32imac-esp-espidf`.
2. The default (SoftAP-enabled) build of every feature combination is unchanged: `just verify` passes with no public-API diff.
3. `SoftApManager`, its impl, and `pin_ap_netif_ip` are gated `#[cfg(esp_idf_esp_wifi_softap_support)]`; `WiFiManager`, `IdfWifiConfig`, `softap_mac`, and the pure `juggler::wifi` re-exports are not.
4. Enabling `provisioning` with SoftAP disabled yields a single `compile_error!` naming `CONFIG_ESP_WIFI_SOFTAP_SUPPORT=y` as the fix.
5. The `espnow` question is resolved and, if needed, the same treatment applied.
6. Rustdoc on gated items states the sdkconfig requirement.
7. The SoftAP-disabled check runs as a `just` recipe (e.g. `just check-sta-only`) and in CI.
8. Beekeeper (or an equivalent STA-only consumer configuration) can return `CONFIG_ESP_WIFI_SOFTAP_SUPPORT` to `n` and build `features = ["wifi"]`.

## Session Log

- 2026-07-08 — Feature doc created from the beekeeper review-queue request after triage confirmed the regression is unaddressed (`wifi/mod.rs:645` and `:716` still call `ap_netif()` unguarded; no `esp_idf_esp_wifi_softap_support` cfg anywhere in the workspace). Chose the request's preferred option 1 (IDF cfg, no new Cargo feature); added the `provisioning` `compile_error!` and the espnow verification question, both discovered during triage.
