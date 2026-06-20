# ADR 016: Crate Consolidation for Publishing

## Status

Accepted — 2026-06-18.
Supersedes the *granularity* guidance of ADR 005 (per-domain crates within a tier); ADR 005's HAL-split core decision stands unchanged.

## Context

The workspace currently hosts 16 crates organised in three HAL tiers — 6 pure crates, 6 ESP-IDF crates, and 4 `esp-hal` crates — one per peripheral domain (Wi-Fi, MQTT, LoRa, ESP-NOW, OTA, Provisioning).
The per-domain granularity was chosen to maximise crates.io search discoverability: a user searching for "esp-idf-provisioning" finds exactly that crate without noise.

The workspace is now ready to publish to crates.io.
The per-domain granularity, while discoverability-optimal, imposes concrete downsides at publication time:

- **Dependency management overhead:** the `Cargo.toml` dependency graph within the workspace spans 16 crates; many examples and tests depend on multiple domains (e.g. Wi-Fi + OTA, Wi-Fi + MQTT), creating a dense mesh of interdependencies and `workspace.dependencies` bindings.
- **Cargo.lock hygiene:** 16 published crates with independent version histories risk uncoordinated semver bumps and the downstream confusion of having to select which subset of versions is compatible (a hard problem that the crate-aggregation tier partially solves).
- **Release ceremony complexity:** publishing 16 crates to crates.io in a correct dependency order is error-prone and slows adoption (e.g. updating a pure crate forces a patch bump through all six HAL bindings if done separately; grouping them lets a single release publish all tiers at once).
- **Publication surface bloat:** 16 crate entries cluttering the `rustyfarian-` namespace on crates.io, many of which are thin wrappers; the discoverability gain is reversed when there are too many closely-related names.

The decision in ADR 005 to split across HAL tiers — keeping ESP-IDF, `esp-hal`, and pure-logic as separate crates — is sound and will remain.
That ADR correctly argued that mutually-exclusive backends (std vs no_std, ESP-IDF vs `esp-hal`, different target triples) cannot be feature-toggled in a single crate without violating Cargo's feature-additivity requirement.

This ADR proposes a narrower consolidation: merging the six per-domain crates *within each tier* into one crate per tier.

## Decision

### 1. Consolidate to three publishable crates — one per HAL tier

Collapse the 16 crates into three:

| New crate                      | Replaces                                                                                             | Mode                  | Domain features                                                                |
|:-------------------------------|:-----------------------------------------------------------------------------------------------------|:----------------------|:-------------------------------------------------------------------------------|
| `juggler`                      | `rustyfarian-network-pure`, `wifi-pure`, `lora-pure`, `espnow-pure`, `ota-pure`, `provisioning-pure` | `no_std` + `std` opt  | `wifi`, `mqtt`, `lora`, `espnow`, `ota`, `provisioning`, `mock`, `std`         |
| `rustyfarian-esp-idf-network`  | `rustyfarian-esp-idf-{wifi,mqtt,lora,espnow,ota,provisioning}`                                       | `std`                 | same domains; heavy deps gated optional                                        |
| `rustyfarian-esp-hal-network`  | `rustyfarian-esp-hal-{wifi,lora,ota,provisioning}`                                                   | `no_std`              | domains × chip features (`esp32`, `esp32c3`, `esp32c6`, `esp32s3`) + `embassy` |

Each crate is published to crates.io under the `datenkollektiv` (organizational) account and uses feature-gating to allow consumers to select only the domains they use.

**Naming:** the shared/pure crate is named `juggler` — a fair-themed name, exactly like the sister project `rustyfarian-ws2812`'s shared crate `pennant`.
`juggler` was chosen because the crate juggles many concurrent wireless protocols (Wi-Fi, MQTT, LoRa, ESP-NOW) simultaneously — many items kept in flight at once.
The two HAL crates take the project-domain postfix `-network` to form `rustyfarian-esp-idf-network` and `rustyfarian-esp-hal-network`, exactly mirroring `rustyfarian-ws2812`'s `rustyfarian-esp-{idf,hal}-ws2812`.
This naming scopes the namespace, future-proofs for sibling projects (e.g., `rustyfarian-esp-idf-power`), and is more consistent with ADR 005's "crate name is a semantic contract" principle than bare names would be (each name now clearly identifies the HAL tier AND the project domain).

**Default features:** `default = []` — consumers explicitly declare the domains they depend on; this avoids pulling in heavyweight dependencies (like `sx126x` for LoRa, or `embassy-net` for bare-metal Wi-Fi) unless the feature is requested.

### 2. The pure tier is published first; HAL tiers depend on it as a published crate

- `juggler 0.4.0` is published to crates.io with version `0.4.0` (the breaking consolidation layered on top of the released `0.3.0` from 2026-06-16).
- `rustyfarian-esp-idf-network 0.4.0` adds a workspace dependency on `juggler = "0.4"` (semver-compatible).
- `rustyfarian-esp-hal-network 0.4.0` adds a workspace dependency on `juggler = "0.4"`.

This ensures the two HAL crates always pull the same version of shared types and validation logic, avoiding the pre-publication constraint that internal paths must use `path = "crates/..."`.

After publication, downstream consumers (outside the workspace) depend on `rustyfarian-esp-idf-network` or `rustyfarian-esp-hal-network` directly; they do not directly depend on `juggler` (though it remains a transitive dependency and is publicly re-exported by the HAL crates for consumers who need the shared types).

### 3. Domain features are additive and independent within each tier

Feature selection is orthogonal: enabling `lora` never forces `mqtt`; enabling `wifi` never disables `espnow`.

Example Cargo.toml declarations post-consolidation:

```toml
# ESP-IDF consumer: Wi-Fi + MQTT
rustyfarian-esp-idf-network = { version = "0.4", features = ["wifi", "mqtt"] }

# Bare-metal consumer: LoRa only
rustyfarian-esp-hal-network = { version = "0.4", features = ["lora", "esp32s3"] }

# Bare-metal consumer: Wi-Fi + OTA + provisioning, targeting ESP32-C6
rustyfarian-esp-hal-network = { version = "0.4", features = ["wifi", "ota", "provisioning", "esp32c6", "embassy"] }
```

### 4. Pure-tier exports the consolidated pure crates; HAL crates re-export the domains they support

The consolidated `juggler` crate uses module-level re-exports to surface each domain's public types and traits:

```rust
pub mod wifi { pub use wifi_pure::*; }
pub mod mqtt { pub use rustyfarian_network_pure::mqtt::*; }
pub mod lora { pub use lora_pure::*; }
pub mod espnow { pub use espnow_pure::*; }
pub mod ota { pub use ota_pure::*; }
pub mod provisioning { pub use provisioning_pure::*; }
```

Each HAL crate (`rustyfarian-esp-idf` and `rustyfarian-esp-hal`) similarly re-exports the concrete implementations for the domains it provides:

```rust
#[cfg(feature = "wifi")]
pub mod wifi { /* re-export from rustyfarian_esp_idf_wifi or rustyfarian_esp_hal_wifi */ }
#[cfg(feature = "lora")]
pub use lora_pure;  // or full impl
// etc.
```

This allows consumers to write:

```rust
use rustyfarian_esp_idf::wifi::WiFiManager;
use rustyfarian_esp_idf::mqtt::MqttBuilder;
```

without needing to know the internal crate boundaries.

### 5. Crate boundary contract

Each consolidated crate commits to a strict boundary contract and prohibited-dependency rules to prevent cross-tier leakage and maintain the HAL split:

**`juggler`:**
- `no_std` by default; an optional, non-default `std` feature gates MQTT helper utilities that require `std::thread`, `std::sync`, and `anyhow` (`spawn_subscriber_thread`, `SubscribeClient` trait, `QoS` enum, `format_broker_url`).
- The `std` feature is defined as `std = ["mqtt", "dep:anyhow"]` and is host-tested via `test-subscriber-thread` and a `BlockingMockClient`.
- Contains shared types, traits, protocol logic, state machines, and validation only; no runtime-specific implementations.
- MUST NOT depend on `esp-idf-svc`, `esp-idf-hal`, `esp-hal`, `esp-radio`, or any HAL-specific crate.
- All logic must be host-testable (compilable on `aarch64-apple-darwin`, `x86_64-unknown-linux-gnu`, etc.).
- The `std` feature refines (not breaks) this contract: it pulls only the standard library and `anyhow`, both platform-independent.

**`rustyfarian-esp-idf-network`:**
- `std`, ESP-IDF only.
- Depends on `juggler` (selected features) plus `esp-idf-svc`, `esp-idf-hal`, and related ecosystem crates.
- MUST NOT depend on `rustyfarian-esp-hal-network`, `esp-hal`, `esp-radio`, or any bare-metal crate.
- All target code must compile for `riscv32imac-esp-espidf` or Xtensa ESP-IDF targets only.

**`rustyfarian-esp-hal-network`:**
- `no_std` bare-metal.
- Depends on `juggler` (selected features) plus `esp-hal` (with `unstable` feature as needed), `esp-radio`, and bare-metal ecosystem crates.
- MUST NOT depend on `rustyfarian-esp-idf-network`, `esp-idf-svc`, `esp-idf-hal`, or any ESP-IDF-specific crate.
- All target code must compile for bare-metal RISC-V (`riscv32imac-esp-espidf`, `riscv32imafc-esp-espidf`) or Xtensa bare-metal targets.

**Dependency direction:**
- One-way: both `rustyfarian-esp-idf-network` and `rustyfarian-esp-hal-network` depend on `juggler`, never the reverse.
- No cycles within the workspace.
- The two HAL crates never reference each other (not even indirectly via a shared internal crate).

**Publishable surface rule:**
- Each crate exposes only intentional `pub use` re-exports and `pub mod` domain namespaces.
- Implementation details and internal helpers remain `pub(crate)` or private.
- Consolidation must not cause public-API sprawl: a downstream importer should be able to understand which symbols are stable and which are internal.

### 6. Rejected: consolidating across HAL tiers into a single large crate

A tempting alternative is to merge all 16 into one mega-crate with conditional dependencies for each HAL and domain.
This is explicitly rejected because it would violate ADR 005's core decision: **mutually-exclusive backends cannot be represented via feature flags.**

A user depending on `rustyfarian = { features = ["wifi", "esp-idf"] }` and another depending on `rustyfarian = { features = ["wifi", "esp-hal"] }` would have both the `esp-idf-hal` and `esp-hal` backends in scope, creating a build error (different target triples, std vs no_std, incompatible runtime models).

This ADR refines the granularity guidance from ADR 005 **within a tier only**, preserving the HAL split.

### 7. Rejected: facade crates for backward compatibility

An alternative approach evaluated and deliberately NOT adopted: thin re-export crates (e.g., a `rustyfarian-esp-idf-wifi` that only re-exports `rustyfarian_esp_idf::wifi::*`) as temporary shims to smooth downstream adoption.

**Reason for rejection:**
- At the first crates.io publication (0.4.0), the workspace is fresh to the registry, so only internal-facing projects (first-party firmware repos using git deps) need migration.
- Adding ~16 thin compatibility crates directly opposes the consolidation goal and adds indefinite maintenance burden.
- A coordinated breaking change with a clear migration guide plus a single semver bump (0.3.0 → 0.4.0) is the pre-1.0 cost of making an architectural decision.

**Note for the maintainer:** this is a recommendation, not a lock; the final decision rests with the maintainer.

### 8. Semver impact: BREAKING change, version 0.4.0

This consolidation is a **breaking change** that affects every downstream consumer.
The version bump is `0.3.0` (released 2026-06-16) → `0.4.0`; a minor-version bump pre-1.0 signalling breaking changes to import paths and feature declarations.

**Breaking changes:**
- **Crate renames:** consuming code must change from `use rustyfarian_esp_idf_wifi::*;` to `use rustyfarian_esp_idf::wifi::*;`.
- **Module-path changes:** every public type's import path changes; e.g., `rustyfarian_esp_idf_mqtt::MqttBuilder` → `rustyfarian_esp_idf::mqtt::MqttBuilder`.
- **Feature flag changes:** examples and downstream crates must explicitly enable domain features (e.g., `features = ["wifi", "mqtt"]`) instead of implicit transitivity.

**No in-place upgrade path:** there is no way to satisfy both the old and new API in a single binary, so facade/compatibility crates do not help.
Downstream consumers re-point all imports per the migration table (defined in the feature doc), and do so in a single coordinated pull request per project.

**Acceptable at pre-1.0:** this cost is acceptable because the workspace is pre-1.0 and has not been published to crates.io; only internal-facing projects (first-party firmware repos) are affected.
The migration table and release notes provide clear guidance.

## Rationale

### On feature additivity within a tier

Within the ESP-IDF tier, all six domains are targets of the same runtime (FreeRTOS, std, POSIX-like), so enabling `lora` is unconditionally additive relative to `wifi` — both can coexist in the same binary without conflict.
The same is true within the `esp-hal` tier: enabling `provisioning` adds no constraints that break `wifi`, because both use the same allocator model, the same executor, the same async foundation.

This is the key difference from the cross-tier case: the two HAL tiers have *exclusive* requirements (different compilers, different link scripts, different runtime models).
Within a tier, the domains have *inclusive* requirements (all use the same target triple, the same runtime, the same fundamental abstractions).

Therefore, feature-gating per domain within a tier is Cargo-compliant, while feature-gating per HAL is not.

### On publishing to crates.io

The shift to crates.io publication makes internal `path = "crates/..."` dependencies problematic for downstream: once the crates are published, the internal structure is not visible to consumers, so path dependencies cannot express "always use my internal version."

Publishing the three tiers as separate crates to crates.io avoids this: the semver contract is explicit and transparent.
The pure tier is published first; the HAL tiers reference it by version, ensuring they resolve the same published dependency on the downstream side.

### On default features = []

Pulling in heavy dependencies (like `embassy-net`, `sx126x`, `lorawan-device`) by default wastes download time and binary space for consumers who do not use those domains.
Setting `default = []` makes the set of dependencies explicit in the consumer's `Cargo.toml`, improving discoverability and reducing surprise bloat.

### On the search discoverability trade-off

Consolidation trades away some crates.io search specificity: a user searching for "esp-idf provisioning" now finds `rustyfarian-esp-idf` (which is good) but also sees all the other domains listed in its feature table (which is noise compared to a dedicated `rustyfarian-esp-idf-provisioning` crate).

This trade is acceptable because:

- **The alternatives are worse:** 16 crates with similar names are *more* confusing to sort through than 3 well-named crates with documented feature tables.
- **Documentation is the real discovery tool:** downstream projects will find the workspace via the GitHub repository (e.g. "rustyfarian esp32 mqtt rust"), not by scrolling crates.io, and the README will list the three crates with links to their feature tables.
- **The crate names are unambiguous:** `juggler` (fair-themed shared crate), `rustyfarian-esp-idf-network` (ESP-IDF + network domain), and `rustyfarian-esp-hal-network` (bare-metal + network domain) are self-documenting; each name clearly identifies the HAL tier AND the project scope, with no ambiguity about which to use.

### On ADR 005 and this ADR's relationship

ADR 005 established that `esp-idf-hal` and `esp-hal` are mutually exclusive and must live in separate crates.
This ADR **does not change that decision.**
It refines the granularity guidance to clarify: mutual exclusivity is the key criterion for crate separation, not domain granularity.

ADR 005 should be updated (separately, after this ADR is accepted) to note that ADR 016 supersedes its GRANULARITY guidance within tiers (i.e., "collapse per-domain crates within a tier, but not across tiers"), while keeping its HAL-split decision intact and unmodified.

## Consequences

### Positive

- **Simpler dependency graph:** downstream consumers declare one or three crates, not a subset of sixteen.
- **Coordinated releases:** all domains within a tier share a version number, reducing downstream version-resolution friction and eliminating "which versions are compatible?" confusion.
- **Reduced publication ceremony:** publishing `rustyfarian-esp-idf 0.4.0` publishes all six domains at once with a single `cargo publish` and a single CHANGELOG entry, instead of staggering six separate publishes and coordinating pre-release ordering.
- **Cleaner crates.io namespace:** `juggler`, `rustyfarian-esp-idf-network`, and `rustyfarian-esp-hal-network` are the three user-facing crates; the six per-domain crates become internal implementation details not visible on crates.io.
- **Feature clarity:** domain selection is explicit in the feature table, making it obvious which optional dependencies come with each choice.

### Negative

- **Breaking rename for downstream:** any external crate already depending on (e.g.) `rustyfarian-esp-idf-wifi`, `wifi-pure`, or `lora-pure` will need to migrate imports and feature declarations to the consolidated crates (`rustyfarian-esp-idf-network` with `wifi` feature, `juggler` with `lora` feature, etc.).
  This is acceptable as a one-time cost at the 0.4.0 breaking release (following the 0.3.0 publication on 2026-06-16), but it should be called out in the release notes and the migration guide.
- **Feature-matrix combinatorics on CI:** the two HAL crates have a domain × chip feature matrix (6 domains × 4 chips for `rustyfarian-esp-hal`, 6 domains for `rustyfarian-esp-idf`).
  CI must validate enough combinations to catch feature-interaction bugs without testing an exponential explosion of permutations.
  The solution is a sparse matrix of "critical path" combinations (e.g., `wifi+esp32c3`, `lora+esp32s3`, `provisioning+esp32c6`) plus one "all features" target per tier.
- **docs.rs build complexity:** `docs.rs` builds the two HAL crates for the `riscv32imac-esp-espidf` default target, which does not have Xtensa support; the `[package.metadata.docs.rs]` section must specify a build matrix or accept that the docs build for a limited set of examples and fall back on feature documentation.
- **API surface complexity:** the consolidation doubles the size of each crate's public API surface (six domains instead of one).
  The mitigation is clear module structure and consistent rustdoc coverage.
  Bad mitigation would be to glob-import everything into the crate root; instead, domain modules are kept separate, and consumers use explicit paths like `rustyfarian_esp_idf::wifi::WiFiManager`.

### Implications

Follow-through items (not decided in this ADR, but implied):

- A per-crate README section for `juggler`, `rustyfarian-esp-idf-network`, and `rustyfarian-esp-hal-network` covering the feature tables, chip support, and usage examples.
- A feature-discovery table in the root `README.md` showing which features are available on which tier and which optional dependencies they pull.
- `cargo publish --dry-run` validation for all three crates before the first real publication.
- An `[unstable]` features section in `.cargo/config.toml` if `rustyfarian-esp-hal` uses the bare-metal `unstable` features from `esp-hal` or `esp-radio` (e.g., GPIO, SPI raw access for radio drivers).
- An update to ADR 005's status to note that ADR 016 supersedes its GRANULARITY guidance (within tiers only; the HAL-split core remains).

## References

- [ADR 005](005-crate-naming-for-dual-hal-drivers.md) — Crate Naming Convention for Dual-HAL Drivers (the HAL-split decision this ADR preserves).
- [Cargo Book — Features](https://doc.rust-lang.org/cargo/reference/features.html) — the additive-features principle this ADR relies on within a tier.
- [Cargo Book — Publishing](https://doc.rust-lang.org/cargo/reference/publishing.html) — crates.io publication workflow and versioning.
- `AGENTS.md` § Architecture — the current three-tier structure and the pure-first rule.
- `docs/release-plan.md` (when created) — publication checklist and pre-release validation steps.
- `docs/features/crate-consolidation-3-crates-v1.md` — implementation plan, migration table, testing strategy, and per-phase status tracking.
