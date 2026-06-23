# Feature: SoftAP SSID override v1

Let a provisioning consumer set the SoftAP captive-portal SSID **verbatim**, instead of
being limited to the auto-derived `{prefix}-{MAC}` name. Sourced from the rgb-clock
bring-up: the AP currently shows as `Rustyfarian-DBBD` (prefix `Rustyfarian` +
last two MAC bytes), and the consumer wants a bare custom name (e.g. `MyClock`).

**Status:** esp-idf lands end-to-end in this release (the override drives the actual SoftAP SSID).
esp-hal lands the `ssid_override` field + validation only — full radio-SSID parity (override actually sets the AP name, and the `{prefix}-{MAC}` default) is **deferred** to [`hal-provisioning-ap-ownership-v1.md`](./hal-provisioning-ap-ownership-v1.md).

## Scope and end state

Today `PortalConfig.ssid_prefix` is the only knob; `juggler::provisioning::derive_softap_ssid`
always appends `-{MAC[4]:02X}{MAC[5]:02X}` (`ssid.rs`). When done:

- `PortalConfig` (on **both** `rustyfarian-esp-idf-network` and `rustyfarian-esp-hal-network`)
  gains `ssid_override: Option<&str>`.
- `Some(name)` → (esp-idf) the AP SSID broadcasts exactly `name`, with no transformation, suffixing, or truncation; (esp-hal) the override is validated but does not yet drive the radio SSID — radio parity is deferred to `hal-provisioning-ap-ownership-v1.md`. Invalid values are rejected on both (never silently altered).
- `None` → today's `{prefix}-{MAC}` behavior, byte-for-byte unchanged.
- The resolution + validation logic lives once in `juggler` (pure, host-tested), called by
  both HAL paths — so the validation and error-handling cannot drift. The runtime radio-SSID
  behavior differs: esp-idf applies the override end-to-end; esp-hal validates but defers
  radio ownership to `hal-provisioning-ap-ownership-v1.md`.
- An invalid override (empty or `> 32` bytes) makes `start()` return an error — never a
  silent truncation, never a panic.

## Proposed public API

*Illustrative sketch; names may shift slightly during implementation.*

```rust
// Both PortalConfig structs gain one field (idf + hal):
pub struct PortalConfig<'a> {
    pub ssid_prefix: &'a str,
    /// When `Some`, used verbatim as the complete SoftAP SSID; `ssid_prefix` and the
    /// MAC-derived suffix are ignored. Must be 1..=32 UTF-8 bytes and not
    /// whitespace-only, or `start()` fails. Using an override disables the default
    /// per-device MAC uniqueness (see the collision caveat).
    pub ssid_override: Option<&'a str>,
    pub ap_password: Option<&'a str>,
    pub channel: u8,
    pub device_name: &'a str,
    pub firmware_version: &'a str,
    pub profile: SchemaProfile,
}

// New resolver, shared, pure (juggler::provisioning::ssid) — alloc-free, no_std:
pub fn resolve_softap_ssid(                                           // override ? validate+use : derive
    ssid_override: Option<&str>,
    prefix: &str,
    mac: &[u8; 6],
) -> Result<heapless::String<32>, &'static str>;
// Reuses existing `juggler::wifi::validate_ssid(ssid: &str) -> Result<(), &'static str>`
// for length and empty checks (1..=32 bytes). Also rejects whitespace-only overrides.
// `derive_softap_ssid(prefix, mac)` stays as-is and backs the `None` path.
```

Consumer usage (rgb-clock):

```rust
portal: PortalConfig {
    ssid_prefix: "Rustyfarian",   // ignored when ssid_override is Some
    ssid_override: Some("MyClock"),
    ...
}
// AP SSID == "MyClock"   (Some(_) → verbatim)
// ssid_override: None    → "Rustyfarian-DBBD" (unchanged default)
```


## Decisions

|                                                                                                                                                         Decision | Reason                                                                                                                                                                                                            | Rejected Alternative                                                                                                                                                                                                               |
|-----------------------------------------------------------------------------------------------------------------------------------------------------------------:|:------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|:-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
|                   **Full verbatim override** — `ssid_override: Option<&str>` on `PortalConfig`; `Some` is used as the entire SSID, `None` keeps `{prefix}-{MAC}` | Gives a bare custom name (`MyClock`) — exactly what the consumer asked for — while leaving the default path untouched. Smallest additive surface.                                                                 | `append_mac_suffix: bool` (keeps prefix, only drops the suffix — can't produce arbitrary names). Or an `Ssid { Literal \| Prefixed }` enum (most expressive but a breaking replacement of `ssid_prefix` and larger surface).       |
|                  **Resolution in `juggler` (`resolve_softap_ssid`); both HALs call it; reuse existing `validate_ssid`** | One pure, host-tested place; validation and error semantics can't diverge across HALs — directly applying the lesson from the DHCP-DNS bug. Runtime radio-SSID behavior differs (esp-idf end-to-end vs. esp-hal deferred), but the *validation contract* is unified. No second validator needed.      | Per-HAL duplication of the override/validation logic (the exact drift we are trying to design out).                                                                                                                                |
|                                                              **Reject an invalid override via `Err`** (empty, whitespace-only, or `> 32` bytes); `start()` fails | Mirrors the established validate-then-reject philosophy (client-id policy, `ap_password` length). A silently truncated or blank SSID is a confusing field bug and provisioning footgun.                           | Silent truncation to 32 bytes, or `debug_assert`/panic.                                                                                                                                                                            |
|                                                                      **Keep `derive_softap_ssid` unchanged; `resolve_softap_ssid` wraps it for the `None` path** | Backward compatible; existing `ssid.rs` tests stay green; the derived branch stays infallible.                                                                                                                    | Folding the MAC-derivation into `resolve_softap_ssid` and deleting `derive_softap_ssid` (needless churn, breaks existing callers/tests).                                                                                           |
| **Additive field: `ssid_prefix` becomes required-but-ignored when `ssid_override: Some`** | Deliberate v1 tradeoff: the field remains in struct literals for backward compatibility and to keep the derived path unambiguous. The alternative (an `Ssid` enum unifying the model) is the noted Future direction. | Removing `ssid_prefix` entirely and using an `Ssid` enum now (forces a major breaking change and blocks v1 release).                                                                                                |

## Constraints

- **802.11 / ESP SSID cap: 1..=32 UTF-8 bytes.** Reuse the existing `juggler::wifi::validate_ssid` for length and empty checks. SSID length is validated in UTF-8 bytes (≤ 32), not Unicode scalar count; a non-ASCII name may exceed the 32-byte limit even when it looks short.
- **Whitespace-only override is rejected.** A `Some(ssid_override)` whose trimmed value is empty (e.g. `"   "`) is rejected; verbatim does not mean "accept obviously-broken input." `resolve_softap_ssid` trims and rejects the blank case before calling `validate_ssid`.
- **Content / character validation scope for v1.** v1 validation enforces ONLY (a) non-empty, (b) not whitespace-only, (c) ≤ 32 UTF-8 bytes. It intentionally does NOT normalize, restrict printable vs. control-character content, or reject embedded NULs. Tightening these constraints (e.g. rejecting control characters or NUL, which lower C layers may care about) is a possible future change, not v1.
- **`None` is byte-for-byte identical to today.** No behavior change on the default path.
- **Source-breaking for struct-literal construction.** Adding a field means every `PortalConfig { .. }` literal must add `ssid_override: None`. Acceptable pre-1.0 (the items are already marked "Experimental: API may change before 1.0"); ship as a minor bump with a one-line migration note in the CHANGELOG. (Mitigation deferred — see Open Questions.)
- **Lands on BOTH `rustyfarian-esp-idf-network` and `rustyfarian-esp-hal-network`.** Shared validation logic in `juggler`; no per-HAL copies. esp-idf applies the override end-to-end; esp-hal validates only (radio application deferred to `hal-provisioning-ap-ownership-v1.md`).
- **`juggler` stays `no_std` / alloc-free.** `resolve_softap_ssid` returns `heapless::String<32>`; the error is `&'static str` (no `anyhow`, no `alloc`). The esp-idf `start()` maps the `Err` to `anyhow`; esp-hal maps it to a new `ProvisioningError::InvalidSsid` variant (mirroring `PasswordTooShort`).
- **Public-surface intent.** `juggler::wifi::validate_ssid` is pre-existing public (reused, not newly exposed). The NEW `resolve_softap_ssid` is `pub` because both HAL crates call it across the crate boundary; downstreams may also pre-resolve an SSID before calling `start()`. This intent makes the public surface deliberate, not accidental.
- **Collision caveat.** A verbatim override bypasses the per-device MAC suffix, so multiple devices sharing one override will share one SSID — the consumer owns uniqueness when they opt out of the default. This tradeoff is part of the shipped contract and documented on the `ssid_override` field.

## Open Questions

- [ ] **Typed `SsidError` enum (deferred).** A typed `juggler` error (e.g. `enum SsidError { Empty, TooLong, WhitespaceOnly }`) would improve host-test assertions and HAL error mapping, but adopting it means re-typing the existing `&str`-returning validators (`validate_ssid`, `validate_client_id`, `validate_password`) for consistency — a cross-cutting refactor out of scope for v1. Logged as a future improvement. Note: even though v1 uses `&'static str` (no typed enum yet), parity-of-MEANING is preserved across HALs — the `&'static str` reason from validation (e.g. "SSID must not be empty", "SSID exceeds maximum length of 32 bytes", "SSID is whitespace-only") is the shared semantic vocabulary: esp-hal carries it in `ProvisioningError::InvalidSsid`, and esp-idf preserves it in the `anyhow` context/message. The deferred typed `SsidError` is a type-ergonomics improvement, not a semantics gap.
- [ ] **`PortalConfig` builder / `Default` (deferred).** Ease the source-breaking field addition so future field additions don't churn every consumer literal. Ship v1 with the field + the CHANGELOG migration note; open a follow-up for builder/`Default` ergonomics.

## State

- [x] Design approved
- [ ] Core implementation
- [ ] Tests passing
- [ ] Documentation updated

## Acceptance criteria

1a. **esp-idf (met in this release):** `ssid_override: Some("MyClock")` causes the AP to broadcast exactly `MyClock` (no prefix, no MAC suffix) end-to-end.
1b. **esp-hal (validation only; radio-SSID parity deferred):** `start()` accepts and validates the override (rejects invalid values with `ProvisioningError::InvalidSsid`), but does NOT yet apply it to the radio. Full override-to-broadcast parity is deferred to `hal-provisioning-ap-ownership-v1.md`.
2. **Both HALs:** `ssid_override: None` yields byte-identical output to today's `derive_softap_ssid({prefix}, mac)` (unchanged default path).
3. **Both HALs (validation contract met):** An empty, whitespace-only, or `> 32`-byte override makes `start()` return `Err` (esp-idf: `anyhow`; esp-hal: `ProvisioningError::InvalidSsid`) — never truncated, never a panic.
4. **Pure function validation:** `resolve_softap_ssid` in `juggler` is host-tested with explicit cases: `"A"` accepted; exactly 32 ASCII bytes accepted; 33 ASCII bytes rejected; a multibyte string under 32 chars but over 32 bytes rejected; whitespace-only (`"   "`) rejected; a 31-byte prefix with derived suffix accepted (existing behavior unchanged); empty-prefix derived behavior unchanged; an override containing hyphens is treated as an ordinary literal (no special-casing of `-`); duplicate override names across devices are allowed by design (ties to collision caveat); and the `None` path equals `derive_softap_ssid` (covering existing multibyte-prefix and empty-prefix cases).
5. `resolve_softap_ssid` reuses the existing `juggler::wifi::validate_ssid` — no second SSID-length validator added.
6. `ssid_override` field docs explicitly state that using an override disables per-device MAC uniqueness and may cause SSID collisions across devices.
7. **Build + release:** `just verify` green; `just build-example idf_c3_provision_mqtt` builds; esp-hal provisioning example builds (validation only); CHANGELOG entry added, including the `ssid_override: None` migration line for existing `PortalConfig` literals, plus a note linking to `hal-provisioning-ap-ownership-v1.md` for full esp-hal radio parity.

## Implementation notes

- **Resolver failure domain:** The `None` (derived) path via `derive_softap_ssid` is infallible under existing invariants; the `Some` (override) path is the ONLY new error source. This helps callers and test planning focus on override validation.
- **Shared logic placement:** The resolution + validation logic lives only in `juggler` (pure, host-tested), called by both HAL paths via `resolve_softap_ssid`. This guarantees the two SoftAP paths cannot diverge. No per-HAL duplication of the override/validation logic.
- **Public surface organization:** `resolve_softap_ssid` is public in `juggler::provisioning::ssid`, re-exported from `juggler::provisioning`, so both `rustyfarian-esp-idf-network` and `rustyfarian-esp-hal-network` import it cleanly.

## Future direction (non-goal for v1)

Keep `ssid_override` on `PortalConfig` for v1.
If more SSID modes later appear (custom name + suffix, full-MAC or hash suffix, format templates, branding), an `Ssid { Derived { prefix }, Literal(&str) }` enum may supersede `ssid_prefix` + `ssid_override`.
Explicitly out of scope for v1.

## Session Log

- 2026-06-23 — Feature doc created via /feature dialog. Decided: full verbatim `ssid_override: Option<&str>` (over a suffix-toggle bool or an `Ssid` enum); shared `resolve_softap_ssid` in `juggler` driving both HAL paths (no divergence); reject-invalid-via-Err; keep `derive_softap_ssid` for the `None` path. Open: builder/Default to soften the field addition; typed esp-idf error parity.
- 2026-06-23 — Design review polish. Reuse existing `validate_ssid` (don't re-add); new resolver renamed `resolve_softap_ssid`; whitespace-only rejected; UTF-8-byte semantics documented; `&'static str` error for v1 with esp-hal `ProvisioningError::InvalidSsid` (not `SsidInvalid`); typed `SsidError` and builder/`Default` deferred as follow-ups; collision caveat promoted to acceptance criterion; explicit validation test cases enumerated; future `Ssid` enum noted as non-goal.
- 2026-06-23 — Final review polish (contract clarity). Named the `ssid_prefix`+`ssid_override` ignored-field tradeoff as intentional (Decision row added); made the content/char validation scope explicit (length + non-empty + non-whitespace only; no content/control-char/NUL restriction in v1); split the juggler-only architectural goal out of acceptance criteria into new Implementation notes section; stated public-surface intent (`validate_ssid` pre-existing public, `resolve_softap_ssid` public for cross-crate use); made the resolver failure domain explicit (None path infallible, Some path is only error source); documented semantic-vocabulary parity across HALs (meaning preserved via `&'static str` reasons, deferred typed `SsidError` is ergonomics-only); tightened the verbatim promise (no transformation/truncation/silencing); enumerated edge cases (32-byte multibyte, 31-byte prefix unchanged, empty-prefix unchanged, hyphens literal, duplicates allowed, derived-path `None` unchanged).
- 2026-06-23 — Truthfulness fix: rescoped to reflect as-built split. Added Status line: esp-idf end-to-end; esp-hal field+validation only with radio-SSID parity deferred to `hal-provisioning-ap-ownership-v1.md`. Split Acceptance criterion 1 into 1a (esp-idf, met) and 1b (esp-hal, validation-only/deferred). Qualified Scope line 15 (runtime split), Scope line 17-19 (validation logic shared, runtime behavior differs), and Decisions table (validation contract unified, runtime may differ). Clarified criteria 2 and 3 (both HALs), criterion 4 (pure function), and criterion 7 (build, plus CHANGELOG link). Updated Constraints to clarify esp-idf end-to-end vs. esp-hal validate-only. Cross-linked `hal-provisioning-ap-ownership-v1.md` throughout.
