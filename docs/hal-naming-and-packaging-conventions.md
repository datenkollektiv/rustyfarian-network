# HAL Naming and Packaging Conventions in the Rust Embedded Ecosystem

Research into how the ESP and broader embedded-Rust ecosystem handles naming and packaging for crates that need to support multiple hardware abstraction layers (HALs).
Evaluated against the `rustyfarian-ws2812` project philosophy: pure logic in `no_std` crates, thin hardware wrappers, and unit-testable on a laptop.

---

## Executive Summary

- The ESP ecosystem has converged on **two distinct namespaces** — `esp-idf-*` (std, ESP-IDF/FreeRTOS) and `esp-hal-*` / `esp-*` (no_std, bare-metal) — which are treated as separate ecosystems, not as feature variants of a single crate.
- The broader embedded-Rust community follows the same pattern: **separate crates per execution model**, as demonstrated by the `embedded-hal` v1.0 split into `embedded-hal`, `embedded-hal-async`, and `embedded-hal-nb`.
- Cargo's feature flags are **suitable for additive, non-exclusive options** (e.g., `defmt` support, optional peripheral enablement) but are the **wrong tool for mutually exclusive hardware backends** due to Cargo's feature unification semantics.
- The Rust API Guidelines (C-FEATURE rule) mandate feature names to be direct and additive; features representing hardware backend selection violate this rule because selecting one backend logically disables another.
- Third-party crates that target the ESP ecosystem should **not** use the `esp-idf-*` or `esp-hal-*` prefix as it implies membership of the official `esp-rs` organisation; they should prefix with their own project or organisation name.
- The `rustyfarian-ws2812` workspace already follows the ecosystem-validated pattern (separate crates, project-prefixed names), and no reconsideration is warranted.

---

## The ESP Ecosystem Naming Split

The `esp-rs` organisation maintains two entirely separate software stacks for Rust on ESP chips.
These are not variants of the same crate — they differ in runtime model, dependency graph, and target triple.

| Namespace           | Runtime model                     | `std` | Representative crates                             |
|:--------------------|:----------------------------------|:-----:|:--------------------------------------------------|
| `esp-idf-*`         | ESP-IDF / FreeRTOS, C FFI bridge  |  yes  | `esp-idf-hal`, `esp-idf-svc`, `esp-idf-sys`       |
| `esp-hal` / `esp-*` | Bare-metal, no OS                 |  no   | `esp-hal`, `esp-wifi`, `esp-alloc`, `esp-println` |

`esp-idf-hal` provides safe Rust wrappers over the `esp-idf-sys` FFI bindings and implements `embedded-hal` traits including async variants.
`esp-hal` is the officially supported bare-metal HAL (with paid Espressif developer time) and also implements `embedded-hal` and `embedded-hal-async`.
Neither crate uses feature flags to switch between the two backends — they are separate packages with separate version histories.

From the `esp-hal` 1.0 beta announcement (February 2025), Espressif explicitly directs developers to choose one stack and use the corresponding crate suite.
The two stacks share no crate-level dependency; compatibility happens through the common `embedded-hal` trait surface.

---

## WS2812/SmartLED Precedent: Separate Crates in Practice

The WS2812 driver ecosystem is a direct parallel to the `rustyfarian-ws2812` situation and demonstrates the separate-crates convention in use.

<details>
<summary><strong>ws2812-esp32-rmt-driver (esp-idf / std)</strong></summary>

A widely-used WS2812B driver using the ESP32 RMT peripheral, targeting esp-idf.
Its README explicitly states: "This library is intended for use with espidf.
For bare-metal environments (i.e., use with `esp-hal`), use the official crate `esp-hal-smartled`."
The crate uses feature flags only for **additive** concerns: `smart-leds-trait`, `embedded-graphics-core`, and `std` vs `alloc` memory strategies.
It does not attempt to support `esp-hal` via a feature flag.

- Crate: `ws2812-esp32-rmt-driver`
- Targets: `esp-idf-hal` stack only
- Latest: v0.13.1, October 2025

</details>

<details>
<summary><strong>esp-hal-smartled / esp-hal-smartled2 (esp-hal / no_std)</strong></summary>

The community counterpart to `ws2812-esp32-rmt-driver`.
Implements `SmartLedsWrite` wrapping an `esp-hal` RMT channel.
Targets bare-metal exclusively; does not support esp-idf.
The `esp-hal-smartled2` fork (published to crates.io as `esp-hal-smartled2`) is described as "based on the official no-std esp-hal, unlike `ws2812-esp32-rmt-driver` which is based on the unofficial esp-idf SDK."
The naming convention embeds the HAL signal: `esp-hal-smartled` vs `ws2812-esp32-rmt-driver`.

- Crates: `esp-hal-smartled` (community), `esp-hal-smartled2` (fork)
- Targets: `esp-hal` stack only
- Latest: v0.28 and v0.23.1 respectively (2025)

</details>

The ecosystem-wide pattern is unambiguous: one crate per HAL, named to signal which stack it belongs to.
No examined WS2812 crate attempts to serve both stacks through a feature flag.

---

## The Broader embedded-hal Convention: Separate Crates for Execution Models

The `embedded-hal` project itself made the authoritative structural choice when releasing v1.0 in January 2024.
Rather than gating blocking, async, and non-blocking variants behind feature flags in a single crate, the working group split them into distinct packages:

- `embedded-hal` — blocking trait definitions
- `embedded-hal-async` — async trait definitions (leverages stable async traits from Rust 1.75)
- `embedded-hal-nb` — non-blocking I/O via the `nb` crate
- `embedded-hal-bus` — bus-sharing utilities (SPI, I2C multiplexing)

The rationale from the working group: "We have put the traits for different execution models into separate crates.
This allows for a separate and more tailored evolution."
Each crate has its own version history and can evolve independently.
Driver authors declare explicit dependencies on only the execution model(s) they require.

The `lora-phy` crate follows the same approach.
The `lora-rs` workspace is organised as multiple focused packages (`lora-modulation`, `lora-phy`, `lorawan-encoding`, `lorawan-device`, `lorawan-macros`), all `no_std`, with hardware integration handled through trait implementations rather than feature flags.
Board-specific support is provided by implementing the `InterfaceVariant` trait in a separate integration layer, not by gating it behind a feature in the core crate.

---

## Why Feature Flags Are Wrong for Hardware Backend Selection

<details>
<summary><strong>The Cargo feature unification problem</strong></summary>

Cargo's dependency resolver takes the **union** of all features enabled for a given crate across the entire dependency graph.
If package A enables `feature = "esp-idf"` and package B enables `feature = "esp-hal"` on the same driver crate, both features are activated simultaneously — even if they are mutually exclusive at the hardware level.

The practical consequence: you cannot use feature flags to select between two backends when both could be pulled in by different crates in the same workspace or application.
The Cargo book states: "Features should be additive. That is, enabling a feature should not disable functionality, and it should usually be safe to enable any combination of features."
Mutually exclusive hardware backends violate this requirement by definition.

The Cargo book's recommended alternatives for mutually exclusive cases are: (1) split into separate packages, (2) choose one option via precedence logic, or (3) use runtime configuration.
For hardware backends, only option (1) is applicable in an embedded context.

</details>

<details>
<summary><strong>Workspace-level compounding</strong></summary>

The feature unification problem is amplified in Cargo workspaces.
When multiple workspace members depend on the same crate with different feature sets, Cargo unifies those features for the entire workspace build.
A workspace that contains both an `esp-idf`-based crate and an `esp-hal`-based crate would force both feature sets to be active simultaneously if they were expressed as features of a shared driver crate — breaking the build.

The Cargo resolver v2 (which this workspace already uses: `resolver = "2"`) improves the situation for platform-specific and dev-dependencies, but does not solve the fundamental problem of mutually exclusive user-visible features.
Resolver v2 only avoids unifying features across build-time vs run-time boundary, not across two workspace members that both depend on the same crate at run time.

</details>

<details>
<summary><strong>Rust API Guidelines: C-FEATURE rule</strong></summary>

The Rust API Guidelines specify (C-FEATURE): "Do not include words in the name of a Cargo feature that convey zero meaning, as in `use-abc` or `with-abc`. Name the feature `abc` directly."
More importantly, features should be additive and named positively.
A feature named `esp-idf` that, when combined with `esp-hal`, produces a build error is anti-additive and violates the spirit of the guidelines.

The guidelines give `std` as the canonical example of an additive feature: the crate is `no_std` by default, and the `std` feature enables additional capabilities.
This pattern works because enabling `std` never breaks a build that did not enable it.
Selecting between two incompatible hardware backends does not fit this model.

</details>

---

## Naming Conventions: Ecosystem, Organisation, and Driver Layers

### Layer-based naming

The embedded Rust ecosystem uses a consistent vocabulary to signal a crate's role in the stack:

| Layer                    | Naming pattern                       | Examples                           |
|:-------------------------|:-------------------------------------|:-----------------------------------|
| PAC (peripheral access)  | `<chip>-pac`                         | `nrf52840-pac`, `stm32f4-pac`      |
| HAL                      | `<chip/family>-hal`                  | `stm32f4xx-hal`, `rp2040-hal`      |
| BSP (board support)      | `<board>-bsp`                        | `microbit-bsp`                     |
| Platform-agnostic driver | chip/device name, no platform prefix | `scd30`, `bme280`, `ssd1306`       |
| Raw FFI bindings         | `<name>-sys`                         | `esp-idf-sys`                      |
| Ecosystem suite          | `<project>-<role>`                   | `esp-hal`, `esp-wifi`, `esp-alloc` |

Driver crates that implement `embedded-hal` traits are named after the device they drive, not the platform they run on.
Discoverability is achieved by adding the `embedded-hal` keyword on crates.io.

### Third-party ESP crates must not claim the `esp-idf-*` or `esp-hal-*` namespace

The `esp-idf-*` prefix is strongly associated with the official `esp-rs` organisation.
Using it for a third-party crate creates namespace confusion and risks future conflicts as Espressif expands its official crate suite.
The same applies to bare `esp-hal-*` prefixes.

The community norm, confirmed by the `rustyfarian-ws2812` project's own naming history (documented in ADR 005), is to prefix third-party ESP crates with the project or organisation name:

| Anti-pattern     | Correct pattern              | Rationale                                          |
|:-----------------|:-----------------------------|:---------------------------------------------------|
| `esp-idf-ws2812` | `rustyfarian-esp-idf-ws2812` | Avoids confusion with official `esp-idf-*` suite   |
| `esp-hal-ws2812` | `rustyfarian-esp-hal-ws2812` | Avoids confusion with official `esp-hal` ecosystem |
| `esp-ws2812-rmt` | `rustyfarian-esp-hal-ws2812` | Generic name claimed by organisation members       |

The Rust API Guidelines note that the `<project>-` prefix pattern (as in `tokio-*`, `aws-sdk-*`) is the accepted way for an organisation to namespace a suite of related crates.
The `rustyfarian-esp-idf-*` and `rustyfarian-esp-hal-*` naming follows this convention correctly.

The `-rmt` suffix was intentionally dropped in the current naming: it is an implementation detail, not a user-visible characteristic.
Implementation details belong in keywords and documentation, not in the crate name.

---

## Authoritative Sources: Summary of Positions

| Source                              | Position                                                                                                                       |
|:------------------------------------|:-------------------------------------------------------------------------------------------------------------------------------|
| Cargo Book (Features chapter)       | Features must be additive; mutually exclusive features should be avoided; recommended alternative is separate packages         |
| `embedded-hal` v1.0 blog post       | Different execution models belong in separate crates; within a crate, use features only for additive options like `defmt`      |
| Rust API Guidelines (C-FEATURE)     | Feature names must be direct, additive, and not negatively framed; no `use-` or `with-` prefixes                               |
| The Embedded Rust Book (HAL naming) | HAL crates named after chip/family with `-hal` suffix, dashes not underscores                                                  |
| ESP ecosystem practice              | `esp-idf-*` and `esp-hal-*` are distinct namespaces; separate crates, not feature-gated variants                               |
| WS2812 ecosystem practice           | `ws2812-esp32-rmt-driver` (esp-idf) and `esp-hal-smartled` (esp-hal) are separate crates with no shared feature flag mechanism |
| `lora-phy` / `lora-rs`              | Separate crates per concern; trait-based hardware integration, not feature flags                                               |
| Effective Rust (Item 26)            | Feature creep introduces combinatorial CI burden; keep features additive and minimal                                           |

---

## Comparison Table

| Question                                            | Feature Flags (single crate)         | Separate Crates                                  |
|:----------------------------------------------------|:-------------------------------------|:-------------------------------------------------|
| Mutually exclusive backends (esp-idf vs esp-hal)    | Not safe — Cargo feature unification | Clean isolation per crate                        |
| Additive optional capabilities (defmt, led-effects) | Correct approach                     | Over-engineering                                 |
| Independent version evolution                       | Coupled to one semver                | Each crate versioned independently               |
| CI matrix complexity                                | Exponential (2^N feature combos)     | Linear (one pipeline per crate)                  |
| Workspace with both stacks                          | Build breaks on feature unification  | No conflict; separate workspace members          |
| User experience                                     | One dependency                       | Two dependencies with clear names                |
| Ecosystem discoverability                           | Single entry point                   | Each crate independently searchable              |
| Third-party namespace hygiene                       | N/A                                  | Prefix prevents namespace collision              |
| Embedded-hal working group precedent                | Does not use for execution models    | Used for `embedded-hal`, `-async`, `-nb`, `-bus` |
| ESP ecosystem precedent                             | Does not use for HAL selection       | `esp-idf-hal` vs `esp-hal`, WS2812 driver split  |

---

## Strategic Recommendation

### The `rustyfarian-ws2812` workspace already follows the correct pattern

The separate-crates strategy adopted in ADR 005 (`rustyfarian-esp-idf-ws2812` + `rustyfarian-esp-hal-ws2812`) is fully aligned with:

- Cargo's feature additivity requirement
- The `embedded-hal` working group's structural precedent
- The ESP ecosystem's own naming convention
- The WS2812 driver community's established practice

No change to the packaging or naming approach is needed.

### Feature flags within each crate should remain additive

Both driver crates use feature flags correctly:

- `led-effects` — optional, additive dependency (disabled does not break the build)
- `esp32c6`, `unstable` — chip and stability gate forwarded to `esp-hal`; these are HAL-imposed requirements, not new concerns introduced by this project

The `esp32c6` and `unstable` features in `rustyfarian-esp-hal-ws2812` are worth monitoring.
If support for additional ESP32 chips is added, consider whether additional `espXXX` feature flags remain manageable or whether chip selection should be pushed entirely to the caller via `esp-hal`'s own feature flags without re-exposing them.
The current pinning to `esp-hal = "1.0.0"` mitigates this risk for now.

### The `rustyfarian-` prefix is the right long-term choice

As the `esp-hal` ecosystem matures and Espressif expands its official crate suite, namespace pressure on `esp-hal-*` and `esp-idf-*` names will increase.
The `rustyfarian-` prefix creates durable separation regardless of how the official ecosystem evolves.
If the project were ever contributed upstream (e.g., into an `esp-rs/esp-hal-community` driver collection), the rename would be the only required change — the architecture would remain unchanged.

### What this analysis validates (not just this project)

The research confirms a broader principle applicable to any embedded Rust project with dual-HAL ambitions:

Use a single crate when the two targets differ only in optional, additive capability (e.g., `std` vs `alloc` fallback, optional `defmt` formatting).
Use separate crates when the two targets require incompatible dependency trees, different target triples, or different error handling strategies.
ESP-IDF (`std`, anyhow errors, FreeRTOS threads) and esp-hal (`no_std`, enum errors, bare-metal) clearly fall in the latter category.

---

*Research conducted: March 2026.*

**Sources**

- [Cargo Book — Features](https://doc.rust-lang.org/cargo/reference/features.html)
- [Rust API Guidelines — Naming](https://rust-lang.github.io/api-guidelines/naming.html)
- [Rust API Guidelines Discussion #29](https://github.com/rust-lang/api-guidelines/discussions/29)
- [embedded-hal v1.0 blog post](https://blog.rust-embedded.org/embedded-hal-v1/)
- [The Embedded Rust Book — Portability](https://docs.rust-embedded.org/book/portability/)
- [The Embedded Rust Book — HAL Naming](https://doc.rust-lang.org/stable/embedded-book/design-patterns/hal/naming.html)
- [esp-hal 1.0 beta announcement](https://developer.espressif.com/blog/2025/02/rust-esp-hal-beta/)
- [The Embedded Rust ESP Development Ecosystem](https://blog.theembeddedrustacean.com/the-embedded-rust-esp-development-ecosystem)
- [ws2812-esp32-rmt-driver README](https://github.com/cat-in-136/ws2812-esp32-rmt-driver/blob/main/README.md)
- [esp-hal-smartled2 — crates.io](https://crates.io/crates/esp-hal-smartled2)
- [lora-phy — lib.rs](https://lib.rs/crates/lora-phy)
- [lora-rs GitHub](https://github.com/lora-rs/lora-rs)
- [Cargo Workspace Feature Unification Pitfall](https://nickb.dev/blog/cargo-workspace-and-the-feature-unification-pitfall/)
- [Effective Rust — Item 26](https://effective-rust.com/features.html)
