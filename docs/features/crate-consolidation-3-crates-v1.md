# Feature: Crate Consolidation — 3 Publishable Crates v1

## Decisions

| Decision                                                           | Reason                                                                                                                                                            | Rejected Alternative                                                         |
|:-------------------------------------------------------------------|:------------------------------------------------------------------------------------------------------------------------------------------------------------------|:-----------------------------------------------------------------------------|
| **Consolidate to 3 crates by HAL tier** (pure, esp-idf, esp-hal)   | Domain features are additive within a tier, mutually-exclusive only across tiers; 16 crates is publication overhead.                                              | Keep 16; consolidate to a mega-crate (violates ADR 005).                     |
| **Domain features are opt-in (`default = []`)**                    | Avoid pulling heavy deps (`sx126x`, `lorawan-device`, `embassy-net`) unless explicitly requested.                                                                 | `default = ["wifi", "mqtt", "lora", …]` (bloats binaries; silent surprises). |
| **Publish pure tier first; HAL tiers depend on it**                | External `juggler 0.3` is the canonical source of truth for shared types; HAL crates reference it, not path-deps.                                                 | Keep all 16 as path-deps forever; never depend on published crates.          |
| **Re-export domains via `pub mod wifi { pub use wifi_pure::*; }`** | Stable public API surface across tier consolidation; consumers import from `rustyfarian_esp_idf::wifi::WiFiManager`, not `rustyfarian_esp_idf_wifi::WiFiManager`. | Flatten to crate root (ambiguous / unscalable with 6 domains).               |
| **HAL tiers keep chip features independent of domains**            | ESP-IDF has 0 chip features (one std variant); `esp-hal` has 4 chip features × 6 domains. Feature matrix is sparse.                                               | Merge chip+domain into combined features (combinatorial explosion).          |

## Constraints

- Each tier must stay in a single published crate; merging across tiers violates ADR 005.
- Domain features must be additive within a tier; no feature disables another.
- The pure tier exports all six domain modules even if some consumers use only a subset.
- Examples must declare `required-features` in `Cargo.toml` to gate compilation on feature presence.
- Default features are empty; zero domains are compiled by default.

## Open Questions

- [ ] What is the crates.io publication order, and are all three tiers published in a single release cycle, or is the pure tier promoted first with a gap?
- [ ] What is the minimum set of "critical path" feature combinations for CI validation (e.g., `wifi+mqtt`, `lora+esp32s3`, `provisioning+esp32c6`)?
- [ ] Does `rustyfarian-esp-idf-network` need a `[package.metadata.docs.rs]` section to control docs.rs builds, or does the default `riscv32imac-esp-espidf` target suffice?
- [ ] Should the pure tier be re-exported by the HAL tiers, or do consumers import both crates (one for pure types, one for implementations)?
- [ ] Are the six per-domain crates (`wifi-pure`, `lora-pure`, etc.) deleted from the workspace after consolidation, or retained as internal-only path dependencies?

## Proposed Feature Tables

### juggler (no_std by default + optional `std`, host-testable, 6 domains + 1 support feature)

| Feature        | What it gates                                                                                     | Default | Optional deps                         | Notes                                                                                                |
|:---------------|:--------------------------------------------------------------------------------------------------|:--------|:--------------------------------------|:-----------------------------------------------------------------------------------------------------|
| `wifi`         | `wifi-pure` module re-export                                                                      | No      | (none; pure logic only)               | Core Wi-Fi state machine, config validation.                                                         |
| `mqtt`         | `rustyfarian-network-pure` MQTT module (no_std base types only; see `std` feature for std items)  | No      | (none)                                | MQTT state machine, connection logic.                                                                |
| `lora`         | `lora-pure` module                                                                                | No      | (none; uses `heapless` for RF config) | LoRa/LoRaWAN types, coding rates, spreading factors.                                                 |
| `espnow`       | `espnow-pure` module                                                                              | No      | (none)                                | ESP-NOW frame types, MAC addresses.                                                                  |
| `ota`          | `ota-pure` module                                                                                 | No      | (none)                                | OTA manifest parsing, update state.                                                                  |
| `provisioning` | `provisioning-pure` module                                                                        | No      | (none)                                | Provisioning schema profiles, field validators.                                                      |
| `mock`         | Test doubles for radio/MQTT                                                                       | No      | (mock implementations; never shipped) | `lora-pure --features mock`: mock radio impl, spy callbacks.                                         |
| `std`          | MQTT helper traits + utilities requiring std: `spawn_subscriber_thread`, `SubscribeClient`, `QoS` | No      | `dep:anyhow`                          | Gated on `#[cfg(feature = "std")]`; host-tested via `test-subscriber-thread` + `BlockingMockClient`. |

**Key dependencies (always present):**
- `heapless 0.9` (no_std-safe collections)
- `log 0.4`

**Optional dependencies (pulled only if a domain feature is enabled):**
- `std`: `anyhow 1.0` (error handling for std-dependent utilities).
- None for other domains; all logic is portable Rust (no_std compilable).

---

### rustyfarian-esp-idf-network (std, ESP-IDF HAL, 6 domains)

| Feature        | What it gates                                                    | Default  | Optional deps (proposed)                                                                     | Notes                                                          |
|:---------------|:-----------------------------------------------------------------|:---------|:---------------------------------------------------------------------------------------------|:---------------------------------------------------------------|
| `wifi`         | `rustyfarian-esp-idf-network-wifi` Wi-Fi manager + SoftAP        | No       | `esp-idf-svc 0.52`, `esp-idf-hal 0.46`, `embedded-svc 0.29`                                  | Always includes LED status feedback.                           |
| `mqtt`         | `rustyfarian-esp-idf-network-mqtt` MQTT client + builder         | No       | `dep:esp-idf-svc`, `esp-idf-hal`, `embedded-svc`                                             | Requires `wifi` for bootstrap (checked at build time or docs). |
| `lora`         | `rustyfarian-esp-idf-network-lora` SX1262 driver                 | No       | `dep:sx126x 0.3`, `dep:lorawan-device 0.12`, `dep:sha2 0.10`, `esp-idf-hal/critical-section` | Requires `wifi` (or can init standalone).                      |
| `espnow`       | `rustyfarian-esp-idf-network-espnow` ESP-NOW peer-to-peer        | No       | `esp-idf-svc`, `esp-idf-hal`                                                                 | Broadcast mode, no Wi-Fi required.                             |
| `ota`          | `rustyfarian-esp-idf-network-ota` OTA firmware update            | No       | `esp-idf-svc`, `esp-idf-hal`, `dep:esp-bootloader-esp-idf 0.5`                               | HTTP/HTTPS firmware download.                                  |
| `provisioning` | `rustyfarian-esp-idf-network-provisioning` SoftAP captive portal | No       | `dep:esp-idf-svc`, `esp-idf-hal`, depends on `wifi` feature                                  | Credential persistence via NVS; requires `wifi`.               |

**Key dependencies (always present):**
- `anyhow 1.0` (error handling)
- `log 0.4`
- `esp-idf-svc 0.52` (re-export of ESP-IDF APIs)
- `embedded-svc 0.29` (traits)
- `juggler 0.3` (shared types)

**Optional dependencies (proposed; each domain pulls its own set):**
- `wifi`: re-uses `esp-idf-svc`, `esp-idf-hal`, `embedded-svc` (already required)
- `mqtt`: re-uses the above + possibly a JSON/CBOR dep if message serialization is optional
- `lora`: `sx126x 0.3`, `lorawan-device 0.12`, `sha2 0.10`, needs `esp-idf-hal/critical-section`
- `espnow`: re-uses `esp-idf-svc`, `esp-idf-hal`
- `ota`: `esp-bootloader-esp-idf 0.5`, `esp-storage 0.9`
- `provisioning`: re-uses `esp-idf-svc`, `esp-idf-hal` + requires `wifi` feature

**Example usage:**
```toml
rustyfarian-esp-idf-network = { version = "0.4", features = ["wifi", "mqtt", "ota"] }
```

---

### rustyfarian-esp-hal-network (no_std bare-metal, 4 domains × 4 chips)

| Feature            | What it gates                                           | Default  | Optional deps (proposed)                                                                                        | Notes                                                   |
|:-------------------|:--------------------------------------------------------|:---------|:----------------------------------------------------------------------------------------------------------------|:--------------------------------------------------------|
| **Domains:**       |                                                         |          |                                                                                                                 |                                                         |
| `wifi`             | Async STA + AP Wi-Fi via `esp-radio`                    | No       | `dep:esp-radio =0.18.0`, `dep:esp-rtos =0.4.0`, `embassy-net =0.8`, `embassy-executor =0.10`                    | Always async-only; requires `embassy` feature.          |
| `lora`             | Bare-metal SX1262 driver stub (Phase 2)                 | No       | `dep:sx126x 0.3`, `dep:lorawan-device 0.12`, `heapless 0.9`                                                     | No hardware driver yet; types only.                     |
| `ota`              | OTA firmware update client over HTTP                    | No       | `embassy-net`, `embassy-executor`, `embassy-time =0.5`, `dep:esp-bootloader-esp-idf 0.5`, `dep:esp-storage 0.9` | Requires `embassy` feature + async executor.            |
| `provisioning`     | SoftAP captive portal (v1)                              | No       | `embassy-net`, `embassy-executor`, (edge-net family TBD), requires `wifi`                                       | Requires `wifi` and `embassy` features.                 |
| **Support:**       |                                                         |          |                                                                                                                 |                                                         |
| `embassy`          | Enables async/await, `embassy-executor`, `embassy-time` | No       | `embassy-executor =0.10`, `embassy-time =0.5`, `static_cell 2.1`, `embedded-io-async 0.6`                       | **Required for all features.** Compile error if absent. |
| **Chip features:** |                                                         |          |                                                                                                                 |                                                         |
| `esp32`            | Target ESP32 (Xtensa, 2 cores)                          | No       | `esp-hal/esp32`, `esp-println/uart`, `esp-storage/esp32`                                                        |                                                         |
| `esp32c3`          | Target ESP32-C3 (RISC-V, 1 core)                        | No       | `esp-hal/esp32c3`, `esp-println/uart`, `esp-storage/esp32c3`                                                    |                                                         |
| `esp32c6`          | Target ESP32-C6 (RISC-V, 2 cores)                       | No       | `esp-hal/esp32c6`, `esp-println/uart`, `esp-storage/esp32c6`                                                    |                                                         |
| `esp32s3`          | Target ESP32-S3 (Xtensa, 2 cores)                       | No       | `esp-hal/esp32s3`, `esp-println/uart`, `esp-storage/esp32s3`                                                    |                                                         |

**Key dependencies (always present):**
- `juggler 0.3` (shared types)
- `esp-hal =1.1.0` (core HAL)
- `heapless 0.9`
- `log 0.4`

**Optional dependencies (proposed; each domain + feature pair pulls its own set):**
- `wifi`: `esp-radio =0.18.0`, `esp-rtos =0.4.0`, `embassy-net =0.8`, `embassy-executor =0.10` (gates on `embassy`)
- `lora`: `sx126x 0.3`, `lorawan-device 0.12`, `heapless` (already present)
- `ota`: `esp-bootloader-esp-idf =0.5.0`, `esp-storage =0.9`, `embassy-net`, `embassy-executor`, `embassy-time =0.5` (gates on `embassy`)
- `provisioning`: `esp-radio`, `esp-rtos`, `embassy-net`, `embassy-executor`, (edge-net family for portal HTTP/DHCP/DNS — version TBD in Phase 2 spike), requires `wifi` feature (gates on `embassy`)
- Each chip: forwards to `esp-hal/<chip>`, `esp-storage/<chip>`, `esp-println/uart`

**Example usage:**
```toml
# Wi-Fi + OTA, targeting ESP32-C6
rustyfarian-esp-hal-network = { version = "0.4", features = ["wifi", "ota", "esp32c6", "embassy"] }

# LoRa only, targeting ESP32-S3
rustyfarian-esp-hal-network = { version = "0.4", features = ["lora", "esp32s3"] }
```

---

## Handling of test features

Currently, the workspace uses `-p lora-pure --features mock` to gate test-double implementations.
After consolidation:

- `juggler` gains a `mock` feature that is never released (only used locally during testing).
- `mock` is not listed in the published crate metadata; it is a local development feature.
- Host tests use `-p juggler --features lora,mock` to enable the lora module and mock radio impl.
- CI excludes `mock` from any published artifacts (checked via `cargo publish --dry-run`).

Examples that require mock implementations (e.g., `examples/test_lora_pure.rs` if one exists) must gate on `#[cfg(feature = "mock")]`.

---

## Migration Guide — Old Paths to New Paths

Downstream consumers must re-point all imports using the mapping below.
The mapping is representative and covers all principal public entry points; it will be completed during implementation as types are consolidated.

**Naming and feature notes:**
- The consolidated pure crate is named `juggler` (a fair-themed name mirroring `rustyfarian-ws2812`'s shared crate `pennant`); the crate name reflects that it juggles many concurrent wireless protocols.
- The two HAL crates take the project-domain postfix `-network` to form `rustyfarian-esp-idf-network` and `rustyfarian-esp-hal-network`, exactly mirroring `rustyfarian-ws2812`'s naming pattern; this scopes the namespace and future-proofs for sibling projects.
- The `std` feature on `juggler` gates MQTT helpers (`spawn_subscriber_thread`, `SubscribeClient`, `QoS` enum, `format_broker_url`) that require `std::thread` and `anyhow`; it is host-tested and does not depend on any HAL.

| Old crate path                                                             | New path                                                                            | Notes                                                  |
|:---------------------------------------------------------------------------|:------------------------------------------------------------------------------------|:-------------------------------------------------------|
| `rustyfarian_esp_idf_wifi::WiFiManager`                                    | `rustyfarian_esp_idf_network::wifi::WiFiManager`                                    | Feature: `wifi`                                        |
| `rustyfarian_esp_idf_mqtt::{MqttBuilder, MqttHandle, MqttConnectionState}` | `rustyfarian_esp_idf_network::mqtt::{MqttBuilder, MqttHandle, MqttConnectionState}` | Feature: `mqtt`                                        |
| `rustyfarian_esp_idf_lora::EspIdfLoraRadio`                                | `rustyfarian_esp_idf_network::lora::EspIdfLoraRadio`                                | Feature: `lora`                                        |
| `rustyfarian_esp_idf_espnow::EspNowManager`                                | `rustyfarian_esp_idf_network::espnow::EspNowManager`                                | Feature: `espnow`                                      |
| `rustyfarian_esp_idf_ota::OtaUpdate`                                       | `rustyfarian_esp_idf_network::ota::OtaUpdate`                                       | Feature: `ota`                                         |
| `rustyfarian_esp_idf_provisioning::{ProvisioningPortal, SchemaProfile}`    | `rustyfarian_esp_idf_network::provisioning::{ProvisioningPortal, SchemaProfile}`    | Features: `provisioning`, `wifi`                       |
| `rustyfarian_esp_hal_wifi::WiFiManager`                                    | `rustyfarian_esp_hal_network::wifi::WiFiManager`                                    | Features: `wifi`, `embassy`, `esp32c3` (or other chip) |
| `rustyfarian_esp_hal_lora::EspHalLoraRadio`                                | `rustyfarian_esp_hal_network::lora::EspHalLoraRadio`                                | Feature: `lora`                                        |
| `rustyfarian_esp_hal_ota::OtaUpdate`                                       | `rustyfarian_esp_hal_network::ota::OtaUpdate`                                       | Features: `ota`, `embassy`                             |
| `rustyfarian_esp_hal_provisioning::ProvisioningPortal`                     | `rustyfarian_esp_hal_network::provisioning::ProvisioningPortal`                     | Features: `provisioning`, `wifi`, `embassy`            |
| `wifi_pure::{WiFiConfig, ApConfig}`                                        | `juggler::wifi::{WiFiConfig, ApConfig}`                                             | Feature: `wifi`                                        |
| `lora_pure::{LoraConfig, SpreadingFactor}`                                 | `juggler::lora::{LoraConfig, SpreadingFactor}`                                      | Feature: `lora`                                        |
| `espnow_pure::EspNowFrame`                                                 | `juggler::espnow::EspNowFrame`                                                      | Feature: `espnow`                                      |
| `ota_pure::UpdateManifest`                                                 | `juggler::ota::UpdateManifest`                                                      | Feature: `ota`                                         |
| `provisioning_pure::{SchemaProfile, LoraFields, MqttFields}`               | `juggler::provisioning::{SchemaProfile, LoraFields, MqttFields}`                    | Feature: `provisioning`                                |
| `rustyfarian_network_pure::mqtt::*`                                        | `juggler::mqtt::*`                                                                  | Feature: `mqtt`                                        |

**Implementation note:** The exact type names and module structure will be finalized during Phase 1–3 as the crates are consolidated; this table is representative and will be completed as a comprehensive checklist in the PR description.

---

## Feature Combination CI Matrix

To validate that features are independent and do not leak dependencies, the CI pipeline must test a representative (not exhaustive) matrix of combinations.

**juggler:**
- [ ] `--no-default-features` (zero features enabled)
- [ ] `--features wifi` (and similarly for each domain: `mqtt`, `lora`, `espnow`, `ota`, `provisioning`)
- [ ] `--all-features` (all domains + `mock`)

**rustyfarian-esp-idf-network (riscv32imac-esp-espidf via `just verify`):**
- [ ] `--no-default-features` (zero features)
- [ ] `--features wifi`
- [ ] `--features lora`
- [ ] `--features wifi,mqtt` (multimodal dependency check)
- [ ] `--all-features` (all 6 domains)

**rustyfarian-esp-hal-network (per-chip build via `just build-example`):**
For each of `esp32c3`, `esp32c6`, `esp32s3`:
- [ ] `--features embassy,wifi,<chip>` (Wi-Fi-only path)
- [ ] `--features embassy,lora,<chip>` (LoRa-only path)
- [ ] Spot-check one chip with `--all-features` (all 6 domains × 1 chip)

**Goal:** Prove that `default = []` is practical (no silent massive-dep inclusion), that multi-feature combinations resolve cleanly, and that per-chip builds work.

**Expected outcome:** Each test compiles cleanly with no unused-dep warnings (via `cargo udeps` post-merge) and no feature-interaction bugs.

---

## Build and Size Guardrails

After each tier consolidation (Phases 1, 2, 3), capture and record:

1. **Clean build time** (from-scratch, no cache): before and after the merge, for a representative example per tier.
2. **Incremental build time:** recompile after a single-line change in core logic.
3. **Binary size:** measure one ESP-IDF example (`idf_c3_connect`) and one HAL example (`hal_c6_connect_async`), both in release mode, before and after.

Record these metrics in the PR description so reviewers can spot unexpected regressions (e.g., a domain feature unexpectedly pulling in a large transitive dependency).
Rough order-of-magnitude comparisons ("no change", "+5%", "–10%") suffice; exact numbers are less important than visibility.

---

## Dependency Hygiene Checks

After each tier consolidation, run:

- [ ] `cargo udeps` (per crate) — identifies unused declared dependencies; any present after merge indicates over-broad internal imports.
- [ ] `cargo machete` (workspace) — finds unused imports in source; ensures cleanup after moving code.
- [ ] `cargo deny` (workspace) — verifies all licenses are approved and no RUSTSEC advisories are unacknowledged.
- [ ] Spot-check the `Cargo.toml` of the new consolidated crate: no optional dep should appear in `[dependencies]` (only under `[features]` as `dep:name`).

**Important:** transitive dependencies should *not* be listed in `Cargo.toml` unless explicitly needed; Cargo's feature resolver handles the rest.
Any optional dependency that needs to be always-present (e.g., `heapless` in `juggler`) goes in `[dependencies]`; others go in `[features]` with `dep:` prefix.

---

## Testing Strategy After Merge

### Per-new-crate smoke tests

For each newly consolidated crate (`juggler`, `rustyfarian-esp-idf-network`, `rustyfarian-esp-hal-network`), create a minimal example that exercises the crate the way an external consumer would:

**juggler:**
- A simple `#[cfg(test)]` unit test (e.g., in `tests/`) that imports all domain modules and instantiates a few key types.
- Example: `use juggler::wifi::*; let cfg = WiFiConfig::default();`

**rustyfarian-esp-idf-network:**
- A minimal binary (not an example, but a simple `fn main() {}` in a Rust test) that depends on the crate with feature flags and successfully imports key types.
- Validates that the consolidated crate is correctly re-exporting the old interfaces.

**rustyfarian-esp-hal-network:**
- Per-chip compilation check: build the library with a single feature (`wifi` + `esp32c3`, `lora` + `esp32s3`, etc.) to verify no feature-interaction breakage.

### Workspace-external integration check

After all three tiers are consolidated and before declaring readiness for publication:

1. Run `cargo publish --dry-run` for each of the 3 crates in order (`juggler`, `rustyfarian-esp-idf-network`, `rustyfarian-esp-hal-network`).
   - Verify no missing `version =` on internal dependencies.
   - Check that the packaged tarball includes all intended source files.
   - Confirm `Cargo.toml.orig` and `.crates2.json` are generated correctly.

2. Create a scratch consumer crate outside the workspace:
   - Add `juggler = { path = "../crate-consolidation-spike/crates/juggler" }` (simulate local publish).
   - Add `rustyfarian-esp-idf-network = { path = "../crate-consolidation-spike/crates/rustyfarian-esp-idf-network", features = ["wifi", "mqtt"] }` (simulate local publish).
   - Verify it compiles and can instantiate types from the consolidated crates.
   - This catches missing `pub use` re-exports and accidental private-item exposure before publication.

---

## Merge-Readiness Checklist

This is the explicit definition-of-done gate for each phase and the entire consolidation:

### Pre-merge validation (per phase)

- [ ] Clear 16-old → 3-new crate mapping documented (e.g., "lora-pure becomes juggler::lora")
- [ ] Migration guidance for downstream consumers ready (the migration table completed for all types)
- [ ] Feature flags reviewed for minimal default footprint (`default = []` confirmed, no transitive-dep pollution)
- [ ] Build/test metrics captured (before vs after clean build, incremental, binary size)
- [ ] Semver impact explicitly called out (this is breaking → 0.4.0; release notes prepared)
- [ ] CI validates representative usage of each new publishable crate (feature matrix green)
- [ ] `cargo udeps` / `cargo machete` / `cargo deny` all clean
- [ ] `cargo publish --dry-run` passes for the affected crate(s) in correct order
- [ ] Workspace-external smoke test compiles (scratch consumer crate proves no missing re-exports)

### Post-Phase-3 publication readiness

- [ ] All 3 crates pass `cargo publish --dry-run` (in order: pure, esp-idf, esp-hal)
- [ ] Per-crate README sections written and `[package]` metadata complete
- [ ] crates.io account access confirmed + test publish to staging (if available)
- [ ] Version consistency: all 3 crates at `0.4.0` (or agreed-upon semver)

---

## Publish Automation

### Phase 5 — Publication via a coordinated `just` recipe

The workspace `justfile` gains a `release-publish` recipe (or extension of the existing release flow) that enforces publication order and validates versions:

**Pseudocode:**
```sh
just release-publish VERSION=0.4.0
# 1. Verify all 3 crates have [package] version = "0.4.0"
# 2. Run `cargo publish --dry-run` for juggler
# 3. On success, `cargo publish` juggler
# 4. Repeat for rustyfarian-esp-idf-network and rustyfarian-esp-hal-network in order
# 5. Create a git tag `v0.4.0` and push
```

**Constraints:**
- Publication must occur in strict order: `juggler` first, then the two HAL crates (either parallel or sequential, but never out of order).
- Each `cargo publish` must succeed before the next begins.
- If any crate fails, halt and require manual intervention (do not continue to the next crate).

**Cross-reference:** The `docs/release-plan.md` document (created separately) captures the full publication checklist, rollback procedure, and post-release validation steps (e.g., verify crates.io reflects the new versions within 5 minutes, run a final integration test against published artifacts).

---

## Migration Sequencing

**Prerequisite:** ADR 016 is accepted and this feature doc is signed off.

### Phase 1 — Pure tier consolidation ✓ DONE (2026-06-19)
- [x] Consolidate 6 pure crates → 1 `juggler` crate in `crates/juggler/`
  - Renamed `crates/rustyfarian-network-pure/` → `crates/juggler/`
  - Moved `wifi-pure`, `lora-pure`, `espnow-pure`, `ota-pure`, `provisioning-pure` as submodules
  - Created `lib.rs` with `pub mod wifi { pub use wifi_pure::*; }` pattern
  - Updated Cargo.toml: `features = ["wifi", "mqtt", "lora", "espnow", "ota", "provisioning", "mock", "std"]`, `default = []`
- [x] Updated `Cargo.toml` workspace to point to `crates/juggler` only; deleted 6 old crate dirs
- [x] Updated all 10 internal consumers (5 esp-idf crates, 4 esp-hal crates, 1 workspace) to depend on `juggler`
- [x] Rewired `justfile` recipes: consolidated per-domain test recipes into features on a single `test` recipe
- [x] Ran `just fmt && just verify` — pure tier host tests pass clean; riscv32 check target green
- [x] Ran `just test` (all host tests) — all 8 pure-tier test suites passing (wifi, mqtt, lora, espnow, ota, provisioning, mock, std)
- [x] Updated `AGENTS.md` crate-list table to show `juggler` with 7 domain features + 1 support feature instead of 6 rows

### Phase 2 — ESP-IDF tier consolidation
- [ ] Consolidate 6 ESP-IDF crates → 1 `rustyfarian-esp-idf-network` crate in `crates/rustyfarian-esp-idf-network/`
  - Move source from `crates/rustyfarian-esp-idf-network-{wifi,mqtt,lora,espnow,ota,provisioning}/src/` into `src/wifi/`, `src/mqtt/`, etc.
  - Create `lib.rs` with re-export pattern
  - Update Cargo.toml: list domain features, all gated `#[cfg(feature = "...")]`
  - Add `default-features = false` to dependency on `juggler = { path = "../juggler" }`
  - Set `default = []`
- [ ] Update all `examples/idf_*.rs` to add `required-features` in their `[[example]]` block (e.g. `required-features = ["wifi"]`)
- [ ] Update `scripts/build-example.sh` to resolve example → required features (extract from Cargo.toml metadata)
- [ ] Run `just fmt && just verify` — check target passes clean
- [ ] Build a subset of ESP-IDF examples: `just build-example idf_c3_connect`, `just build-example idf_c3_provision_mqtt`, `just build-example idf_esp32s3_join` — all link clean
- [ ] Update `AGENTS.md` and `README.md` references from 6 ESP-IDF crates to 1 with feature table

### Phase 3 — Bare-metal tier consolidation
- [ ] Consolidate 4 HAL crates → 1 `rustyfarian-esp-hal-network` crate in `crates/rustyfarian-esp-hal-network/`
  - Move source from `crates/rustyfarian-esp-hal-network-{wifi,lora,ota,provisioning}/src/` into `src/wifi/`, `src/lora/`, etc.
  - Create `lib.rs` with re-export pattern
  - Update Cargo.toml: list domain + chip features, all gated `#[cfg(feature = "...")]`
  - Add `default = []`
  - Gate all domains on `feature = "embassy"` (or provide explicit `compile_error!` if absent)
- [ ] Update all `examples/hal_*.rs` to add `required-features`
- [ ] Update `scripts/build-example.sh` to handle HAL examples with chip extraction
- [ ] Run `just fmt && just verify` — check target passes clean
- [ ] Build a subset of HAL examples: `just build-example hal_c3_connect_async`, `just build-example hal_c6_provision`, `just build-example hal_esp32s3_join_async` — all link clean with correct target/MCU
- [ ] Update `AGENTS.md` and `README.md` references from 4 HAL crates to 1 with domain + chip feature table

### Phase 4 — Prepare for publication
- [ ] Add per-crate README sections:
  - `crates/juggler/README.md` — features, shared types, host-test coverage
  - `crates/rustyfarian-esp-idf-network/README.md` — ESP-IDF-specific setup, Wi-Fi + MQTT + LoRa examples
  - `crates/rustyfarian-esp-hal-network/README.md` — bare-metal setup, chip feature matrix, async ecosystem notes
- [ ] Add `description`, `keywords`, `categories` to each crate's `Cargo.toml` (LOCKED at planning pass; examples provided below)
- [ ] Fix `crates/rustyfarian-esp-hal-network-provisioning/Cargo.toml` metadata (if it still exists post-consolidation, which it will not; skip if already consolidated)
- [ ] Add `[package.metadata.docs.rs]` to both HAL crates, or verify the default build works
- [ ] Run `cargo publish --dry-run` for all three crates — no errors, output validates package contents
- [ ] Update `docs/ROADMAP.md` to reference the 3 consolidated crates instead of 16
- [ ] Create `docs/release-plan.md` with:
  - Publication order (pure first, then esp-idf, then esp-hal)
  - Semver versioning (all three at 0.4.0)
  - Dry-run checklist
  - Rollback procedure

### Phase 5 — First publication
- [ ] Publish to crates.io (in order: `juggler`, then `rustyfarian-esp-idf-network`, then `rustyfarian-esp-hal-network`)
- [ ] Verify on crates.io: all three appear, metadata is correct, docs build (or note build failures for HAL targets as expected)
- [ ] Create a GitHub release tag `v0.4.0` with release notes covering the 16→3 consolidation and any breaking API changes
- [ ] Update root `README.md` to reference the published crates (version 0.4.0) and feature tables

---

## Files / Recipes Known to Require Rewiring

After consolidation, the following files must be updated to reflect the 16→3 crate map:

### Workspace build files
- [ ] `Cargo.toml` (workspace root): remove 6 per-tier crate entries from `members = ["crates/*"]` (or keep all crates as internal modules, then remove post-consolidation); update `[workspace.dependencies]` to remove per-domain crate names, add only the 3 consolidated crates
- [ ] `justfile`: update recipes `test-lora`, `test-mqtt`, `test-wifi`, `test-ota`, `test-espnow`, `test-provisioning` — each becomes a feature-gate on a single `juggler` target or is merged into one `test` recipe with `--features` flags
- [ ] `scripts/build-example.sh`: rewrite to extract required-features from the example's Cargo.toml `[[example]]` metadata and resolve to chip + HAL tier
- [ ] `scripts/flash.sh`: no changes needed (uses `scripts/build-example.sh` output)
- [ ] `scripts/ensure-bootloader.sh`: no changes needed

### Documentation
- [ ] `README.md`: update crate inventory table and usage examples to show 3 crates + feature flags instead of 16
- [ ] `docs/ROADMAP.md`: update crate references and timeline events (e.g., "Phase 5 LoRa" → "Phase 5 rustyfarian-esp-hal-network LoRa")
- [ ] `AGENTS.md` § Architecture: replace the 16-row table with the 3-tier structure + domain/chip feature matrix
- [ ] `VISION.md`: update capability summary if it currently lists per-crate scope

### Configuration
- [ ] `.cargo/config.toml`: no changes required (per-tier target routing is unchanged)
- [ ] `deny.toml`: no changes required (dependency graph is unchanged, only the crate boundaries move)

### CI / publishing
- [ ] Add or update CI job to test feature combinations: `cargo build -p rustyfarian-esp-idf-network --features wifi,mqtt`, `cargo build -p rustyfarian-esp-hal-network --features lora,esp32s3`, etc.
- [ ] Create `docs/release-plan.md` with publication checklist

---

## Proposed Crate Metadata (Locked at planning pass)

Each crate's `Cargo.toml` will carry:

### juggler

```toml
description = "Platform-agnostic types, validation, and state machines for Wi-Fi, MQTT, LoRa, ESP-NOW, OTA, and provisioning on ESP32"
keywords = ["esp32", "wifi", "mqtt", "lora", "embedded"]
categories = ["embedded", "no-std"]
```

### rustyfarian-esp-idf-network

```toml
description = "ESP-IDF (std) drivers for Wi-Fi, MQTT, LoRa, ESP-NOW, OTA, and provisioning on ESP32"
keywords = ["esp32", "esp-idf", "wifi", "mqtt", "lora"]
categories = ["embedded", "hardware-support"]
```

### rustyfarian-esp-hal-network

```toml
description = "Bare-metal (esp-hal) async drivers for Wi-Fi, LoRa, OTA, and provisioning on ESP32-C3, ESP32-C6, ESP32-S3, and ESP32"
keywords = ["esp32", "esp-hal", "embedded", "no-std", "async"]
categories = ["embedded", "hardware-support", "no-std"]
```

---

## Risks

### Risk: Breaking rename for downstream consumers

**Impact:** High (external users will need to migrate)

**Mitigation:** 
- At 0.3.0 publication (2026-06-16), the workspace is fresh to crates.io; the consolidation breaking release at 0.4.0 affects only internal-facing projects (first-party firmware repos using git deps).
- Release notes and migration guide clearly document the 16→3 consolidation and the required `Cargo.toml` changes.
- Minor version bump from `0.3.0` to `0.4.0` signals a breaking change pre-1.0.

### Risk: Feature-matrix combinatorics explosion in CI

**Impact:** Medium (CI runtime grows; hard to maintain all combinations)

**Mitigation:**
- CI tests only a sparse "critical path" matrix of ~5–8 feature combinations per tier, not all 2^6 permutations.
- A local pre-commit check (optional, via `just lint-features`) can test all combinations locally before pushing.
- Accept the risk that an edge-case feature interaction (e.g., `lora+provisioning+esp32s3`) is not caught until integration testing.

### Risk: docs.rs builds on incompatible target

**Impact:** Low (bare-metal crates may not build docs)

**Mitigation:**
- docs.rs defaults to the `riscv32imac-esp-espidf` target when the crate does not specify `[package.metadata.docs.rs]`.
- For the two HAL crates, either (a) disable docs.rs builds on ESP targets and accept that the docs show feature flags only, or (b) use `[package.metadata.docs.rs]` to build on a compatible target.
- The pure crate builds on any target, so it has no risk.

### Risk: API surface complexity

**Impact:** Low (good documentation and module structure mitigate)

**Mitigation:**
- Each domain is exported via a separate `pub mod wifi { }` namespace, so naming collisions are impossible.
- Rustdoc clearly marks each domain's entry point and provides examples per domain.
- The root README links to per-domain documentation.

---

## Session Log

- 2026-06-18 — Feature doc created; ADR 016 LOCKED for signature; Phase 1–5 tasks defined; critical-path testing matrix TBD in Phase 2.
- 2026-06-18 — External review feedback incorporated: added migration table (old → new crate paths), feature-combo CI matrix, build/size guardrails, dependency hygiene checklist, testing strategy (per-crate smokes + workspace-external integration), merge-readiness definition-of-done, and publish automation (`just release-publish` recipe pseudo-spec). ADR 016 sections: crate boundary contract (prohibited-dependency rules), semver impact statement, facade-crate rejection rationale.
- 2026-06-19 — Final naming and feature decisions locked: (1) Pure crate renamed `rustyfarian-pure` → `juggler` (fair-themed, mirrors `pennant`; juggles many protocols); HAL crates stay literal (`rustyfarian-esp-idf-network`, `rustyfarian-esp-hal-network`). (2) Naming scope documented as working assumption in ADR 016. (3) `juggler` is `no_std` by default with optional `std` feature gating MQTT helpers (`spawn_subscriber_thread`, `SubscribeClient`, `QoS`, `format_broker_url`) + `anyhow`; host-tested, no HAL deps; does not break boundary contract. (4) Phase 1 COMPLETE: `crates/juggler` created, 10 consumers rewired, 6 old dirs deleted, all tests green, `AGENTS.md` updated. Phases 2–5 remain open.
