# ADR 005: Crate Naming Convention for Dual-HAL Drivers

## Status

Accepted

## Context

During internal review of ADR 004 in this project, an incorrect naming rationale was discovered.
That ADR claimed the crate should retain the name `rustyfarian-esp-idf-lora` despite hosting `esp-hal` support because a sister project's crate (`rustyfarian-esp-idf-ws2812`) uses a single Cargo package with feature flags to support both stacks.

This claim is factually incorrect.
The `rustyfarian-ws2812` workspace uses **separate crates** (`rustyfarian-esp-idf-ws2812` and `rustyfarian-esp-hal-ws2812`), not a single crate with feature flags.
That decision is formally documented in ADR 005 of the `rustyfarian-ws2812` project.

Therefore, `rustyfarian-esp-idf-lora` lacks a valid precedent for the single-crate-with-features approach.
This ADR documents the research conducted to evaluate both approaches, confirms that the separate-crates pattern is the correct one, and establishes naming guidance for all future `rustyfarian-*` HAL driver crates.

### The two approaches are under evaluation

**Approach A — Single crate with feature flags**

```toml
[dependencies]
rustyfarian-esp-idf-lora = { version = "0.1", features = ["esp-hal"] }
```

The crate name embeds `esp-idf` but a feature flag selects `esp-hal` behavior at build time.

**Approach B — Separate crates per HAL**

```toml
# For std / ESP-IDF users:
rustyfarian-esp-idf-lora = "0.1"

# For no_std / esp-hal users:
rustyfarian-esp-hal-lora = "0.1"
```

Each crate targets exactly one HAL and signals that clearly in its name.

## Decision

Adopt **Approach B (separate crates per HAL)** for all `rustyfarian-*` hardware driver crates,
consistent with ADR 005 of the `rustyfarian-ws2812` project and with the conventions of the broader embedded-Rust ecosystem.

The canonical structure for any new dual-HAL peripheral driver is:

```
<peripheral>-pure          — no_std, no hardware dependency; shared types, traits, protocol logic
rustyfarian-esp-idf-<peripheral>   — std driver; esp-idf-hal; anyhow errors
rustyfarian-esp-hal-<peripheral>   — no_std driver; esp-hal; custom error enum
```

The `*-pure` crate is optional when there is no meaningful pure logic to share,
but should be created whenever configuration types, trait definitions, or protocol state machines
are reusable across both HAL implementations.

## Rationale

### Cargo feature flags cannot safely represent mutually exclusive backends

Cargo's dependency resolver takes the union of all features enabled for a crate across the entire
dependency graph.
If two packages in the same workspace or application enable different backends via feature flags,
both backends are activated simultaneously — producing broken builds with confusing diagnostics.

The Cargo Book states features must be **additive**: enabling one must never disable or break
another.
Selecting between `esp-idf-hal` and `esp-hal` violates this requirement by definition, since
the two HALs require different target triples, different runtime models, and cannot coexist in
a single build.

Cargo's resolver v2 (already in use in this workspace) does not solve this problem.
It only prevents feature unification across build-time vs run-time boundaries,
not across two workspace members that both depend on the same crate at run time.

The Cargo Book's documented alternatives for mutually exclusive concerns are:
(1) split into separate packages, (2) choose one option via precedence logic,
or (3) use runtime configuration.
In an embedded context, only option (1) is applicable.

### The crate name is a semantic contract

The `esp-idf-*` prefix carries strong meaning in the ecosystem: std, FreeRTOS, C FFI bridge,
`anyhow` errors, POSIX-like threading.
A crate named `rustyfarian-esp-idf-lora` that also supports `esp-hal` via a feature flag
contradicts its own name.

On crates.io, only crate names — not feature names — are indexed for search.
A user searching for "esp-hal lora" will not find a crate named `rustyfarian-esp-idf-lora`,
regardless of what features it exposes.

### Semver independence matters more than crate count

`esp-hal` and `esp-idf-hal` have independent release cadences and independent breaking-change
policies.
Specifically, `esp-hal` 1.0's published policy is that peripheral APIs behind the `unstable`
feature gate (which covers RMT, SPI, GPIO, and everything a radio driver depends on)
may break in minor releases.

Under a single-crate approach, every `esp-hal` minor bump forces a version bump on `esp-idf` users
even when nothing in the `esp-idf` code path changed.
Separate crates insulate each user population from the other's upgrade pressure,
preserving the semantic accuracy of the semver contract.

### The ecosystem has converged on separate crates

Independent research confirmed this pattern across multiple authoritative sources:

| Source | Pattern |
|:-------|:--------|
| `embedded-hal` v1.0 (Jan 2024) | Split into `embedded-hal`, `embedded-hal-async`, `embedded-hal-nb`, `embedded-hal-bus` — one crate per execution model, not feature flags |
| `esp-rs` organization | `esp-idf-hal` and `esp-hal` are entirely separate packages with separate version histories |
| WS2812 ecosystem | `ws2812-esp32-rmt-driver` (esp-idf) and `esp-hal-smartled` (esp-hal) — separate crates; the former's README explicitly directs `esp-hal` users to the latter |
| `lora-phy` / `lora-rs` | Workspace of focused `no_std` crates; hardware integration via trait implementation, not feature flags |
| Rust API Guidelines (C-FEATURE) | Features must be additive and named directly; mutually exclusive backends violate this rule |

Detailed source citations are available in
[`docs/hal-naming-and-packaging-conventions.md`](../hal-naming-and-packaging-conventions.md).

### The `rustyfarian-` prefix is required for third-party crates

The `esp-idf-*` and `esp-hal-*` prefixes are strongly associated with the official `esp-rs`
organization.
Using bare `esp-idf-*` or `esp-hal-*` names for third-party crates causes namespace confusion
and risks future naming conflicts as Espressif expands its official crate suite.

The `rustyfarian-` org prefix clearly identifies these as third-party crates,
following the community norm of prefixing with the project or organization name
(analogous to `tokio-*`, `aws-sdk-*`).

This principle was already established in ADR 005 of the `rustyfarian-ws2812` project and recorded in the naming history of
`rustyfarian-esp-idf-ws2812` (which was previously named `esp-idf-ws2812-rmt` before the
prefix was added).

## Consequences

### Positive

- **Naming honesty** — each crate name accurately describes the runtime environment it targets
- **Discoverability** — both HAL variants are independently searchable on crates.io
- **Semver independence** — `esp-hal` breaking changes do not affect `esp-idf` version history
- **CI isolation** — each crate is built against exactly one target; no feature-matrix combinatorics
- **User clarity** — `Cargo.toml` declarations are self-documenting; no "remember to set the feature" gotchas
- **Cargo compliance** — all features within each crate remain additive, as required by the Cargo Book

### Negative

- **Two crates to create and maintain** per peripheral (mitigated by keeping drivers thin and delegating shared logic to `*-pure` crates)
- **API drift risk** — the two drivers may diverge over time (mitigated by shared traits such as `StatusLed` and shared `*-pure` dependencies)

### Implications for extending LoRa support

A single `rustyfarian-esp-idf-lora` crate with an `esp-hal` feature flag is not a valid option.
It misrepresents its own name, hides from relevant crates.io searches, couples unrelated semver
histories, and violates Cargo's feature additivity requirement.

The correct structure for a dual-HAL LoRa driver following this workspace's conventions is:

```
lora-pure                          — no_std shared types (LoRaConfig, SpreadingFactor,
                                     Bandwidth, a LoRaDevice trait)
rustyfarian-esp-idf-lora           — std driver; esp-idf-hal; anyhow errors
rustyfarian-esp-hal-lora           — no_std driver; esp-hal; custom error enum
```

## References

- [ADR 005 (ws2812) — Dual-HAL Strategy](https://github.com/datenkollektiv/rustyfarian-ws2812/blob/main/docs/adr/005-dual-hal-strategy.md)
- [`docs/hal-naming-and-packaging-conventions.md`](../hal-naming-and-packaging-conventions.md) — full ecosystem research
- [Cargo Book — Features](https://doc.rust-lang.org/cargo/reference/features.html)
- [Rust API Guidelines — C-FEATURE](https://rust-lang.github.io/api-guidelines/naming.html)
- [embedded-hal v1.0 blog post](https://blog.rust-embedded.org/embedded-hal-v1/)
- [The Embedded Rust Book — HAL Naming](https://doc.rust-lang.org/stable/embedded-book/design-patterns/hal/naming.html)
