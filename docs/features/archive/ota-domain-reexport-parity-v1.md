# Feature: OTA Domain Re-export Parity v1

The `ota` domain module in both tier crates (`rustyfarian-esp-idf-network`,
`rustyfarian-esp-hal-network`) re-exports only `OtaError` from `juggler::ota`,
while the `wifi` and `espnow` domain modules re-export their **full** pure
surface. As a result, a consumer of the OTA driver must add a *second, direct*
dependency on `juggler` (with `features = ["ota"]`) just to import
`ImageMetadata` and `Version` — the two types the OTA driver's own quick-start
implies they will need. This proposes closing that gap: bring `ota` to parity
with the other domains. The change is **purely additive (semver-minor)**.

> Authored downstream by the `rustbox-midway` consumer, surfaced while migrating
> it onto the v0.4.0 consolidated crates. See rustbox-midway
> `docs/features/network-consolidation-migration-v1.md`.

## Problem & evidence

The v0.4.0 consolidation (ADR 016 §4, `docs/adr/016-…md:74,87`) established the
design intent that each tier crate **re-exports the domains it supports** via
`pub mod <domain> { pub use juggler::<domain>::* }`, so consumers import from
`rustyfarian_esp_idf_network::wifi::WiFiManager` rather than depending on the
pure crate directly. Two of three domains follow this fully; `ota` does not:

| Domain module (idf tier) | Re-exports from `juggler`                                                                                     | Evidence                                     |
|:-------------------------|:--------------------------------------------------------------------------------------------------------------|:---------------------------------------------|
| `wifi`                   | validators, `ApConfig`, `ConnectMode`, `TxPowerLevel`, `WiFiConfig`, `WifiDriver`, `WifiPowerSave`, constants | `wifi/mod.rs:72`                             |
| `espnow`                 | `EspNowDriver`, `EspNowEvent`, `MacAddress`, `PeerConfig`, `ScanConfig`, `ScanResult`, `WifiInterface`        | `espnow/mod.rs:57-58`                        |
| `ota`                    | **`OtaError` only**                                                                                           | `ota/mod.rs:19` (idf), `ota/mod.rs:12` (hal) |

But `juggler::ota` publicly exports a full surface (`juggler/src/ota/mod.rs:12-16`):
`OtaError`, `ImageMetadata`, `OtaState`, `StreamingVerifier` (+ `bytes_to_hex`,
`hex_to_bytes`), `Version`.

**Consumer impact.** The tier `ota` quick-start doctest
(`rustyfarian-esp-idf-network/src/ota/mod.rs`) builds `OtaSession` and calls
`fetch_and_apply(url, &expected_sha256)`. Producing that `expected_sha256` and
version-gating an update requires `ImageMetadata` / `Version` — reachable only
by adding a redundant direct `juggler` dependency. Both rustbox-midway OTA demos
(`ferriswheel-demo-idf`, `ferriswheel-demo-hal`) hit exactly this: they import
`OtaError`/driver types from the tier crate but must reach into `juggler::ota`
for `ImageMetadata`/`Version`. That the `ota` module *internally* already
`use`s `juggler::ota::StreamingVerifier` (idf `ota/mod.rs:26`) underlines that
the omission is an oversight, not a curated minimal surface.

The "NO blanket re-export" note in the consolidation archive doc
(`docs/features/archive/crate-consolidation-3-crates-v1.md:28`) is about *not*
flattening all of `juggler` to the crate root — it is fully consistent with the
per-domain `pub use` pattern the `wifi`/`espnow` modules already use.

## Proposed change

Widen the OTA re-export in **both** tier crates to the domain's public surface:

```diff
- pub use juggler::ota::OtaError;
+ pub use juggler::ota::{
+     bytes_to_hex, hex_to_bytes, ImageMetadata, OtaError, OtaState, StreamingVerifier, Version,
+ };
```

- `rustyfarian-esp-idf-network/src/ota/mod.rs:19`
- `rustyfarian-esp-hal-network/src/ota/mod.rs:12`

The minimum that unblocks consumers is `ImageMetadata` + `Version`; the
full-surface re-export above matches the `wifi`/`espnow` precedent and is
preferred.

## Decisions

|                                                                      Decision | Reason                                                                                                       | Rejected Alternative                                                                                                                                    |
|------------------------------------------------------------------------------:|:-------------------------------------------------------------------------------------------------------------|:--------------------------------------------------------------------------------------------------------------------------------------------------------|
| Re-export the full `juggler::ota` public surface from both tier `ota` modules | Restores parity with `wifi`/`espnow`; the whole OTA API imports from one crate                               | Re-export only `ImageMetadata` + `Version` — unblocks consumers but leaves `OtaState`/`StreamingVerifier`/hex helpers inconsistent with sibling domains |
|                      Treat as additive, semver-minor, target the next release | Only adds `pub use` items; no signature/behaviour/removal — existing `…::ota::OtaError` imports keep working | Defer to the `ota-library` stabilisation milestone — leaves every downstream OTA consumer carrying a redundant `juggler` dep until then                 |
|                                            Fix both tier crates symmetrically | The gap is identical in idf + hal; a one-sided fix creates a new asymmetry                                   | Fix only the crate a given consumer happens to need                                                                                                     |

## Constraints

- **Additive only** — no existing `pub use` is removed and no type moves; this cannot break any current consumer.
- **Keep in sync with `juggler::ota`** — the tier re-export list should track `juggler/src/ota/mod.rs:12-16`; if juggler adds/removes an OTA export, update both tier modules.
- **Experimental types stay experimental** — `StreamingVerifier`/`OtaState` carry the module's existing "APIs experimental, stabilisation deferred to `ota-library`" caveat; re-exporting them implies no new stability promise.
- **CHANGELOG** — add an entry under `[Unreleased] → Added` noting the OTA metadata types are now reachable from the tier crates (no `Migration` entry needed — purely additive).

## Future considerations

**Cross-domain re-export deduplication.** All three domains (`wifi`, `espnow`, `ota`) manually duplicate their `pub use juggler::<domain>::{...}` list across the idf and hal tier crates, relying on code review and constraint documentation to catch drift between them.
A shared macro or helper to auto-generate these lists from `juggler` would eliminate the duplication, but it would be a cross-domain refactor.
Tackling it for `ota` alone would make `ota`'s mechanism inconsistent with the precedent set by `wifi` and `espnow` — the opposite of this PR's parity goal — so it is deferred to a future uniform refactor across all three domains if the team chooses to pursue it.
Current mitigations are sufficient: the HAL `#[cfg(test)]` parity guard catches a dropped HAL re-export as a compiler failure; code review and the `## Constraints` "keep in sync" note cover the rest.

## Open Questions

- [x] Full-surface vs minimal (`ImageMetadata` + `Version` only)? **Resolved:** full surface for domain parity with `wifi`/`espnow` — all items from `juggler::ota`'s public export block are re-exported.
- [x] Include the hex helpers (`bytes_to_hex` / `hex_to_bytes`) in the re-export, or are they internal utilities not meant for the tier API? **Resolved:** YES, included. `hex_to_bytes` is directly useful for consumers converting hex digest strings into the `&[u8; 32]` that `OtaSession::fetch_and_apply` expects.
- [x] Should the workspace-external smoke test (the "scratch consumer crate proves no missing re-exports" gate, `crate-consolidation-3-crates-v1.md:264,282`) be extended to import `ImageMetadata`/`Version`/`OtaState` from the *tier* `ota` module, so this class of per-domain re-export gap is caught before publication rather than by a downstream? **Resolved:** the regression guard is a `#[cfg(test)]` unit test `ota_public_surface_is_reexported_from_this_crate` in the HAL `ota` module (executed by `just test-ota-hal` via `cargo test -p rustyfarian-esp-hal-network`), which `use`s all seven re-exported names from `crate::ota` and asserts `ImageMetadata::parse(...).version == Version::new(1,4,0)` — a dropped re-export → `E0432` compile failure. The IDF tier's re-export is compile-checked only by `just check-ota-idf` (ESP-IDF crates cannot be host-tested); the HAL unit test guards the shared `juggler::ota` name set that both tiers re-export identically.

## State

- [x] Design approved
- [x] Re-export widened in `rustyfarian-esp-idf-network/src/ota/mod.rs` — lines 40-42 now export the full `juggler::ota` surface; the prior private `use juggler::ota::StreamingVerifier;` (line 26) was removed to avoid `E0252` duplicate import (the public `pub use` now serves both the module's internal use and the public contract).
- [x] Re-export widened in `rustyfarian-esp-hal-network/src/ota/mod.rs` — lines 32-34 now export the full `juggler::ota` surface.
- [x] Regression guard in place: `#[cfg(test)]` unit test `ota_public_surface_is_reexported_from_this_crate` in the HAL `ota` module, which `use`s all seven re-exported names from `crate::ota` and asserts `ImageMetadata::parse(...).version == Version::new(1,4,0)`. Executed by `just test-ota-hal` (`cargo test -p rustyfarian-esp-hal-network`); a dropped re-export immediately yields `E0432` compile failure. The IDF tier's re-export is compile-checked only by `just check-ota-idf` (ESP-IDF crates cannot be host-tested); the HAL unit test guards the shared `juggler::ota` name set both tiers re-export identically.
- [x] CHANGELOG `[Unreleased] → Added` entry — comprehensive note linking to this feature doc and explaining the parity rationale.
- [x] Documentation updated — the IDF tier `ota` module docs (heading "Firmware metadata (re-exported)") carry a short runnable rustdoc example (importing `ImageMetadata`/`Version` from the tier crate; matches the `wifi`/`espnow` doctest convention); the HAL tier module stays prose plus a `#[cfg(test)]` unit-test guard (no_std, so no doctest).

## Session Log

- 2026-07-09 — Feature doc created as a downstream-authored change request from
  `rustbox-midway`. Root cause: during the v0.4.0 consolidation (ADR 016) the
  `ota` domain module's per-domain re-export was completed only for `OtaError`,
  not the rest of `juggler::ota`'s public surface, unlike `wifi`/`espnow`.
  Surfaced while migrating rustbox-midway's two OTA demos, which need a redundant
  direct `juggler` dep solely for `ImageMetadata`/`Version`. The proposed fix is a
  two-line additive re-export widening in each tier crate; classified
  semver-minor.
- 2026-07-09 — Implemented. Both tier `ota` modules widened to the full `juggler::ota` public surface (`bytes_to_hex`, `hex_to_bytes`, `ImageMetadata`, `OtaError`, `OtaState`, `StreamingVerifier`, `Version`). The IDF tier's redundant private `use juggler::ota::StreamingVerifier;` was removed to avoid `E0252` duplicate import — internal references now resolve through the widened `pub use`. Regression guard is a `#[cfg(test)]` unit test in the HAL `ota` module (`ota_public_surface_is_reexported_from_this_crate`) that `use`s all seven re-exported names and asserts `ImageMetadata::parse(...).version == Version::new(1,4,0)` — dropped re-exports yield `E0432` compile failure. Executes on the host via `just test-ota-hal` (cargo test sets `cfg(test)`, pulling the `ota` module without ESP deps). The IDF tier's re-export is compile-checked only by `just check-ota-idf` (ESP-IDF crates cannot host-test); the HAL unit test guards the shared `juggler::ota` name set both tiers re-export identically. Prose documentation added to both module docs explaining the re-exports (no code examples). CHANGELOG entry added under `[Unreleased] → Added`. Validated: 167 host tests pass (up from 166), clippy `-D warnings` clean on `--all-targets`, both tier check targets pass.
- 2026-07-09 — PR #88 review round. Shortened CHANGELOG bullet to a scannable release-note summary linking to this feature doc. Added IDF consumer doctest example to the tier `ota` module docs (matching the `wifi`/`espnow` convention); HAL module retains prose plus `#[cfg(test)]` unit-test guard (no_std, no doctest). Recorded cross-domain re-export de-duplication as a deferred future consideration — declined here to preserve mechanism parity with `wifi`/`espnow` (the same duplication exists across all three domains; uniform refactor if pursued). Current mitigations sufficient: HAL unit test catches dropped re-exports; code review + constraint doc cover the rest.
