# ADR 018: OTA Consumer Runtime in the Library

## Status

Accepted — 2026-10-05.

## Context

The first OTA consumer, `rustyfarian-rgb-clock`, implements an MQTT-triggered A/B update on top of `rustyfarian-esp-idf-network`, totalling ~2,000 lines of application-specific code (command parsing, manifest validation, boot-time reconciliation, runtime health policy, NVS persistence, status reporting).

A second consumer (Watchtower v2) would re-implement most of this layer from scratch.
The rgb-clock's code is logically separable into layers: a wire contract (JSON parsing, error codes), a decision core (boot reconciliation, version refusal), record durability (NVS layout and write sequences), and runtime health policy.

The decision-core layer is already extracted into `juggler::ota`, adding `decide_offer`, `reconcile`, and `OtaError::code()` with no breaking changes to existing types.

The wire contract and runtime are still duplicated per consumer.

## Decision

### 1. The library owns the OTA consumer layer, split into four tiers (A–D)

| Tier                      | Scope                                                                                        | Container               |
|:--------------------------|:---------------------------------------------------------------------------------------------|:------------------------|
| **A. Download hardening** | Cooperative deadline, time-limit checking at operation boundaries, `ActivationPermit` gating | juggler `ota`           |
| **B. Wire contract**      | JSON parsing, status serialization, reason codes                                             | juggler `ota-wire`      |
| **C. Record store**       | NVS layout, attempt and report durability, boot-facts reader                                 | IDF tier `ota`          |
| **D. Runtime**            | Background task loop, MQTT callback handoff, health hook, manifest fetch, rollback retries   | IDF tier + `mqtt`,`ota` |

All sub-projects ship together in v0.6.0 after hardware validation (first consumer adoption + library runbook validation + power-loss scenarios).

The esp-hal tier implements A only (download hardening).
The decision-core boundary remains: esp-hal has no MQTT (ADR 015), so B, C, D stay in the IDF tier.

### 2. The consumer keeps three responsibilities

- **Health predicate:** is the newly-booted image healthy? (app-specific criteria: firmware version checks, signature verification, feature detection, uptime metrics)
- **Topics:** MQTT topic names for commands and status (clock: `ota/command`, `ota/status`; Watchtower: `rustyfarian/watchtower/<device-id>/ota/*`)
- **Version numbering:** version strings and parsing rules (clock: `"1.2.3"`, semver; consumer may prefer `"2026-10-05-abc1234"`, git tags, or others)

### 3. Reversals of ota-decision-core decisions

The decision core originally stated:
- **Introduction:** "Persistence (NVS), the MQTT wire contract and the health policy stay in the consumer"
- **Decisions table, row "Pure decisions in `juggler::ota`, not I/O":** "A shared NVS record store" is rejected
- **Decisions table, row "`reconcile` returns an action, never performs it":** "A trait over the record store" is rejected
- **Decisions table, row "MQTT command parsing and health policy stay in the consumer":** "MQTT command parsing and health policy stay in the consumer"
- **Decisions table, row "Write sequences stay in the consumer"**

ADR 018 reverses those decisions:
- The library now owns C (NVS record store and write sequences), not the consumer
- The library now owns B (MQTT wire contract — command parsing, status serialization, reason codes), not the consumer
- The library now owns D (runtime health policy around an app-supplied predicate), not the consumer
- The consumer retains the health predicate itself (the decision whether to mark valid or rollback), topic names, and version numbering

This is supported by the evidence of a second consumer that would otherwise duplicate all four layers.

### 4. Tier responsibilities & boundaries (detailed)

**A — Download hardening (juggler `ota` feature, both HAL tiers)**
- `Deadline` type: total time limit, checked at operation boundaries
- `ActivationPermit` type: token obtained only after the deadline check passes, required for finalization
- Helper to classify read errors: socket reset → `ServerUnreachable`, per-read timeout → `DownloadTimeout`
- Public `url_for_log` in IDF `ota` module (reused by D's manifest fetch)
- No decisions (decision core is separate and unchanged)

**B — Wire contract (juggler `ota-wire` feature, pure tier only)**
- JSON parsing for commands: `{"manifest_url": string, "sig"?: string}` and `{"action": "rollback", "from": string}` with `deny_unknown_fields`
- JSON parsing for manifests: `{version, sha256, url, target}` with `deny_unknown_fields`
- JSON serialization for status: `{status, version?, reason?, attempt_id?, epoch?}` with `#[serde(tag="status", rename_all="snake_case")]`
- Fixed-reason enum (`FailReason`) and mappings from `OfferDecision` and `OtaError::code()`
- No state changes; inputs are wire bytes, outputs are validated types
- No MQTT, no task spawning, no allocation besides `String` and `Vec`
- Enabled by both tier `ota` features for re-export parity; esp-hal users never see it (no MQTT)

**C — Record store (IDF tier `ota` feature)**
- NVS key-value layout: attempt counter, id/version/slot/flags, rollback reason, refused version, report and rollback-request layers (operator rollback without an attempt)
- Read path: read boot-facts (running slot, state, version) with retries, map to `BootFacts` struct
- Write sequences: reserve attempt id (bump counter durably first), store attempt, store boot-selected (post-apply), persist report/refusal (post-rollback), clear attempt, clear report
- Each write commits on its own; order is the crash-consistency guarantee
- New types: storage-agnostic (could migrate to another backend later), but v1 is concrete NVS only
- Depends on A (deadline is read-only for the fetch total limit in D)
- Reused by D's runtime loop

**D — Runtime (IDF tier + `mqtt` and `ota` features)**
- Background task loop: spawned by the app at startup
- Early-boot hook (`note_boot_slot`): record the running slot before Wi-Fi and async setup, so it survives a crash later in boot
- Command channel: created before MQTT client, consumed by the worker task
- MQTT callback only parses and `try_send`s; rejects if queue is full with `busy` or `worker_unavailable` (per ADR 017)
- Worker task: fetch manifest via HTTP, decide (via decision core), download and apply (via `OtaSession`)
- Health policy loop: runs on the app's main thread with an app-supplied predicate; enforces minimum dwell and rollback deadline
- Report delivery: runs on the reporter thread; at-least-once with retries on `Timeout`/`Disconnected`
- Health-flag pair: `refuse_mark_valid` (set at boot by reconciliation on a version mismatch, cleared only by successful mark_valid) and `admission_open` (set at boot by reconciliation on an eligible validated slot; opened by boot reconciliation on eligible validated boot, by health policy on other boots)
- Configurable: task stack sizes, minimum healthy dwell, failure deadline, rollback retry count and interval, manifest fetch total limit
- Restart: only via app-supplied callback after successful apply (`swap_pending` + grace delay) and after rollback; library never calls `esp_restart` directly

### 5. Dependency flow

```
Consumer (health predicate, topics, version)
    ↓
D: runtime + health policy (juggler + IDF tier)
    ↓ depends on
C: record store (IDF tier)
    ↓ reads
B: wire contract (juggler `ota-wire`, enables A)
    ↓ uses
A: deadline + error mapping (juggler `ota`)
    ↓ reads
Decision core: `decide_offer`, `reconcile`, `OtaError::code()` (juggler `ota`, unchanged)
```

esp-hal tier depends on A only; B, C, D are IDF-only.

Both tier `ota` features enable `juggler/ota-wire` to provide consistent re-exports, but esp-hal users never activate it at link time (no MQTT in esp-hal).

## Consequences

### Positive

- **Reduced consumer duplication:** a second consumer gains all four layers (wire parsing, durability, runtime, health policy) from the library instead of copying ~2,000 lines
- **Proven wire format:** identical JSON behaviour to the clock eliminates format-drift risk (same serde stacks, same escaping, same error handling)
- **Shared NVS layout:** consumers can migrate their OTA data from one device to another by sharing the same NVS layout; future v2 with more than one refused version can extend the schema uniformly
- **Consistent error codes:** all consumers report the same `code()` strings to backends, simplifying telemetry aggregation
- **Health policy isolation:** the app's predicate stays small and testable; the library owns deadlines, retries, and reboot sequencing
- **Both tiers benefit from A:** deadline checking on HAL and IDF, cooperative timeouts, unified error classification

### Negative

- **Large new library surface:** B, C, D add ~1,500 lines of juggler and IDF-tier code
- **NVS layout is stable:** extending the schema (e.g., multiple refused versions) requires a migration step and a version bump; v1 is locked to a single refused version and single report record
- **Health policy is rigid:** the library owns minimum dwell, failure deadline, rollback retry count; configurable values are exposed, but the policy structure cannot be swapped (YAGNI — if a second consumer needs a different policy, add a variant or trait then)
- **Reboot ownership:** the library reboots (after `swap_pending` on IDF, after successful activation on both tiers); an app that wants to defer reboot for app-level cleanup must add a pre-reboot hook
- **IDF-only:** B, C, D stay in the IDF tier because the esp-hal tier has no MQTT (ADR 015); a future bare-metal firmware with a different command transport (e.g., BLE) would still need to re-implement D's runtime if that transport is incompatible with MQTT

### Rationale

**On reversing decision-core decisions:**
The evidence that triggered the reversals is a second consumer (Watchtower v2) that would re-implement the same layers.
Decision-core explicitly said persistence and wire contracts stay in the consumer; two independent implementations prove that was too conservative.
The boundary is now: the library owns all four layers *except* the health predicate (app-specific), topic names (app-specific), and version numbering (app-specific).

**On NVS layout stability:**
The rgb-clock validated persistence on hardware (2026-06-23 provisioning, 2026-09-28 rollback cases).
The NVS keys and structure are now a stable contract.
Extending it (e.g., a `refused_versions` table instead of a single `rej_ver`) would require a schema version and migration, which can be added when a second consumer needs it.
v1 is intentionally minimal: one refused version, one report record, one operator-rollback layer.

**On boot-facts reader:**
C does not own the bootloader's view of flash slots (that's `OtaSession` and `OtaManager` in the tiers).
C reads *facts* (running slot, state, version) and assembles them into `BootFacts` for the decision core to consume.
The app supplies `running_version` (from firmware headers); the bootloader supplies slot state and id.
This keeps storage concerns small (NVS, not otadata) and testable on the host.

**On health policy in D:**
The app supplies a `Fn(&BootFacts) -> bool` predicate.
The library runs `predicate() → mark_valid()` or `predicate() → rollback()` with configurable dwell time and retry policy.
A future variant (v0.7+) could add a pluggable `HealthPolicy` trait, but for v1 the fixed policy matches both known consumers' needs.

## Alternatives Considered

### Rejected: persistent record store as a pluggable trait

**Superseded 2026-10-06:** C adopted a key-level `OtaKv` trait after all; see the Amendment.

Evaluated and rejected in decision-core (Decisions table, row "`reconcile` returns an action, never performs it").
A trait would shift all persistence concerns into juggler, but each tier (IDF using NVS, bare-metal using flash sectors) needs different write-sequence guarantees.
A v1 concrete NVS implementation in the IDF tier keeps storage logic local and testable in isolation.
A trait can be added in v0.7+ if a third consumer needs a different backend (e.g., flash sectors, HTTP API).

### Rejected: wire contract in a separate crate

The wire contract could live in a separate `juggler-ota-wire` crate to make the dependency optional.
Instead, it lives in `juggler` behind an opt-in `ota-wire` feature, keeping the consolidation from ADR 016 intact and avoiding a third publishable crate.

### Rejected: hand-rolled JSON parser to avoid the serde dependency

Evaluated and rejected in the B design (feature doc).
Hand-rolling ~800 lines of escaping and UTF-8 handling for security-sensitive input is error-prone and risks format drift.
The serde+serde_json stack is mature and identical to the clock's, so byte-compatibility is guaranteed by construction.

### Rejected: HTTPS enforcement in B

HTTPS and certificate validation belong to sub-project D (manifest fetch), not the wire contract (B).
B does not validate URLs; D's HTTP client enforces scheme and host verification.
This keeps B simple and testable on the host.

## Amendment

**2026-10-06 — Implementation complete, key-level OtaKv trait adopted for host testing:**

All sub-projects (A–D) are implemented and host-tested.
Hardware validation pending via the library runbook at `docs/runbooks/ota-hardware-test.md`.
The example `idf_c3_ota_runtime` accepts build-time overrides `OTA_DEMO_UNHEALTHY` and `OTA_FAILURE_DEADLINE_SECS` to enable health-policy testing scenarios without code changes.
Health policy runs on the app's main thread; report delivery runs on the reporter thread (not the worker).
Admission is opened by boot reconciliation on an eligible validated boot and by the health policy otherwise — never "cleared by the health loop".

**OtaKv trait adoption:** supersedes "Rejected: persistent record store as a pluggable trait" above.
C implements a key-level `OtaKv` trait in juggler, not a record-level one, so the per-key write order (the crash-consistency guarantee) lives in host-testable code.
The IDF crate cannot run host tests; a record-level trait would push that order back into untested code.
`OtaStore<K: OtaKv>` holds all record semantics, write ordering and boot orchestration as generic code under the `ota-wire` feature (`no_std`).
It has two implementors: `EspNvsKv` (IDF production) and the `cfg(test)` `FaultKv` (host fault injection).
Reads take `&self`, writes `&mut self`; the consumer serialises access with a `Mutex<OtaStore<_>>`.
Every `set` or `remove` is one durable commit; `Ok(None)` means exactly "key absent", and type mismatch, over-long value, non-ASCII and flash errors are `Err`.
A different backend (e.g. flash sectors) can be added later without touching the record logic.

**NVS layout:** 16 keys, with no `schema_ver` key; an existing `schema_ver` from an earlier build is never read.
The clock's current layout is adopted as-is.
The older clock layouts of commits `ef7818b..a14f555` (`rb_from` / `rb_conf`, records without ids) are not migrated: a record without an id is reported corrupt and cleared by `repair_corrupt`.

## References

- [ADR 015](015-esp-hal-provisioning.md) — esp-hal tier decision: no MQTT, no system-level resource management
- [ADR 016](016-crate-consolidation-for-publishing.md) — three-crate consolidation (`juggler`, `rustyfarian-esp-idf-network`, `rustyfarian-esp-hal-network`)
- [ADR 017](017-mqtt-callback-threading-contract.md) — MQTT callback threading and the channel-based handoff pattern
- `docs/runbooks/ota-hardware-test.md` — hardware validation runbook for the OTA consumer runtime (21 physical scenarios (P1–P21) and 5 host scenarios (H1–H5))
- `docs/features/archive/ota-decision-core-v1.md` — archived decision-core feature doc; this ADR reverses decisions from its introduction and "Decisions" section
- `docs/features/ota-consumer-runtime-v1.md` § "Sub-project B: Pure wire contract" — B design rationale, rejected alternatives, and tests
- `docs/features/ota-consumer-runtime-v1.md` § "Sub-project C: Record store and boot reconciliation" — Storage API decision and complete C requirements
