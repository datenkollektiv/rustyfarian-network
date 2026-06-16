# Feature: Bare-Metal (esp-hal) Provisioning v1

> **Status: design + planning only — no `rustyfarian-esp-hal-provisioning` code in this PR.**
> The PR that introduces this doc lands ADR 015 and the planning content below; Phases 0–4 are follow-up PRs that begin only after ADR 015 is accepted and the open-question proposals are signed off.
> The State checklist at the bottom is the authoritative progress record — at the time this doc landed, only "Design drafted (ADR 015 proposed)" is true.

Feature doc for the bare-metal SoftAP captive-portal provisioning crate proposed by [ADR 015](../adr/015-esp-hal-provisioning.md).
The crate, `rustyfarian-esp-hal-provisioning`, brings the `WifiMqttDevice` profile already shipped on the ESP-IDF tier ([ADR 014](../adr/014-wifi-mqtt-provisioning-profile.md) / [feature doc](wifi-mqtt-provisioning-profile-v1.md)) to the bare-metal esp-hal stack, for the `rustyfarian-rgb-clock` downstream moving onto that stack.

This is a hybrid doc: it carries the decisions ADR 015 locks (once accepted), the open questions and proposed answers that will drive the implementation, a signature-only design sketch, and a phased plan.
ADR 015 and the proposed answers below are **Proposed — awaiting maintainer sign-off**; the maintainer asked for both load-bearing technology choices (network substrate, flash store) to be evaluated at ADR level rather than picked by gut feel, so the ADR recommends and the maintainer decides.
One sentence per line throughout this doc.

## Decisions

### Locked by ADR 015 (once accepted)

|                                                                                                                                                                                   Decision | Reason                                                                                                                                                                                                                                                                                                                       | Rejected Alternative                                                                                                                                                                         |
|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------:|:-----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|:---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
|                                                                                                     `rustyfarian-esp-hal-provisioning` is accepted; the ADR 013 §2 deferral gate is lifted | `rustyfarian-rgb-clock` moving onto the esp-hal stack is the concrete bare-metal downstream ADR 013 §2 waited for; v1 scope is `WifiMqttDevice` only, C3+C6, embassy/async-only                                                                                                                                              | Decline and keep the name reserved (would relocate the work into the application, the location ADR 013 §1 rejected); build both profiles (no bare-metal LoRaWAN consumer exists)             |
|                                                                                                                          SoftAP (AP-mode) lifecycle is added to `rustyfarian-esp-hal-wifi` | Mirrors ADR 013's no-parallel-Wi-Fi-stack lock and the v1 `SoftApManager` precedent; reuses `wifi-pure` `ApConfig` / `validate_ap_config` / `TxPowerLevel` unchanged; pinned to `esp-radio =0.18.0`                                                                                                                          | Own AP lifecycle inside the provisioning crate (a second Wi-Fi stack, duplicating the TX-power clamp and AP-event subscription)                                                              |
|                                      Captive-portal substrate is a `pub(crate)` private implementation detail; `edge-net` is the preferred first candidate, validated by the Phase 2 spike | The architectural commitment is the private-substrate boundary (ADR 011/013 pattern), not the crate family; edge-net is preferred because one version-consistent no-alloc family covers all three protocols incl. the DHCP server `embassy-net` lacks; on spike failure the hand-rolled runner-up proceeds without a new ADR | picoserve (HTTP only — DHCP/DNS still needed elsewhere, partial coverage is the killer); all-hand-rolled is not rejected but the documented fallback the §3 rule promotes if the spike fails |
|        Flash credential store: a simple single-record store, A/B torn-write-safe, in a dedicated non-NVS partition, with an open-time minimum-size guarantee and no cross-tier NVS interop | Not a general KV DB — one record written single-digit times per lifetime; A/B + CRC is the smallest torn-write-safe shape; dedicated non-NVS partition because the NVS binary format cannot be reused (no crate, and `esp-bootloader-esp-idf` 0.5.0 exposes no NVS read/write API); planning figures locked below            | sequential-storage (async-trait mismatch needs an adapter; wear-leveling unneeded); ekv (LSM overkill); ESP-IDF NVS format (eliminated — no crate exists)                                    |
|                                                                           The v1/v2 security contract carries over as a behavioural conformance list, plus two added secret-handling lines | The nonce / no-prefill / body-cap / lengths-only-logging / no-store / escaping / library-never-reboots / commit-guard contract is tier-independent; the IDF portal is the reference implementation; the two added lines (no reflection of submitted values, early credential-buffer drop) are in the checklist below         | Relax any item for bare-metal (would diverge the two tiers' credential hygiene for no reason)                                                                                                |

### Locked at planning pass (2026-06-12)

These rows hold the planning-level figures the ADR 015 durable contracts imply.
They were locked by the second review pass, which moved them out of the ADR sub-decisions and into this doc with their locked status preserved — they are not reopened as questions.
This doc is the single normative home for these figures; ADR 015 references them.

|                                                                                                                                                                                                                                                                                                                                                              Decision | Reason                                                                                                                                                                                                                                                                                                            | Rejected Alternative                                                                                                                                       |
|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------:|:------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|:-----------------------------------------------------------------------------------------------------------------------------------------------------------|
| Example naming: hal examples carry no `_async` suffix (`hal_c3_provision_mqtt` / `hal_c6_provision_mqtt`); the hal script package arm is `*provision*` placed before any `*mqtt*` arm; a `*provision*` embassy feature-append arm adds `embassy`; `hal_c3_ap_smoke` gets an `*ap*` package arm mapping to `rustyfarian-esp-hal-wifi` and an `*ap*` feature-append arm | The crate is async-only per ADR 015 §1, so an `_async` suffix is signal-free; the IDF-side ordering lesson is that `*provision*` must match before `*mqtt*` (an `..._provision_mqtt` example would otherwise route to the MQTT crate); the embassy feature must be appended without relying on the `*_async*` arm | `_async`-suffix naming — it would reuse the existing `*_async*` feature-append arm, but in an async-only estate the suffix encodes no information          |
|                                                                                                                                                               Store geometry: 8 KiB — two 4 KiB sectors — positioned immediately following the OTA data region, before any user partitions; only the exact byte offset is deferred to the chosen partition table (Q2) | Two sectors are the minimum for A/B torn-write safety; placing it right after OTA data keeps the user-partition region contiguous; the offset is a partition-table concern, not an API concern                                                                                                                    | A single sector (no A/B safety); squatting the standard `nvs` region at `0x9000` (a foreign-format collision, fragile against any tool expecting real NVS) |
|                                                                                                                                                                                                                                                   `ProvisioningStore::open(flash, base_offset: u32, total_bytes: u32)` returns a hard `Err` when `total_bytes < 8192` | The store size is part of the `open` signature so the minimum-size guarantee is enforced at open; this strengthens the suggested debug-assert, which would vanish in release builds, into a check that holds in both profiles on a cold path where the cost is irrelevant                                         | A debug-assert only — it vanishes in release builds, leaving silent corruption possible on a misconfigured partition                                       |
|                                                                                                                                                                                                                                           Profile discriminator on flash is the `SchemaProfile::as_str` bytes written directly, length-prefixed per the record layout | Self-describing, survives enum-variant additions and reordering, and parallels ADR 014's NVS string discriminator (`"lorawan"` / `"wifi_mqtt"`), so the two tiers' storage formats line up even without interop                                                                                                   | A `u8` enum mapping — more compact, but it couples the on-flash contract to enum ordering that a future refactor can silently break                        |
|                                                                                                                                                                                                        edge-net spike-target versions: `edge-http 0.7.0`, `edge-dhcp 0.7.0`, `edge-captive 0.7.0`, `edge-nal-embassy 0.8.1` (over `edge-nal 0.6` / `embassy-net 0.8`) | These are the exact family versions the Phase 2 integration spike brings up; pinning them here gives the spike a concrete target and keeps the ADR free of version detail                                                                                                                                         | Leaving versions unpinned until Phase 2 — the spike needs a concrete, version-consistent target to evaluate, and the family moves on one release cadence   |

These rows began as ADR 015 sub-decisions; the 2026-06-12 second review relocated them here with their locked status intact.

## Constraints

- Everything in `provisioning-pure` is reused unchanged — `SchemaProfile`, `LoraFields`, `MqttFields`, `parse_form`, and the validators all carry over `no_std`-safe, keeping the ADR 013 §2 no-API-break promise.
- Embassy / async only, matching the `rustyfarian-esp-hal-wifi` estate; there is no sync path and the chip-without-`embassy` combination is a `compile_error!` in the Wi-Fi crate.
- The AP path targets the `esp-radio =0.18.0` surface (`Interfaces { ap, .. }`, `wifi::Config::AccessPoint`, `WifiController::subscribe`), **not** the `1.0.0-beta.0` surface that removed `Interfaces` and renamed `wifi::new` to `WifiController::new`.
- Flash writes happen only with the radio quiesced — credentials are written after the portal commits, with the radio stopped or immediately before the host reboots, with the `esp-storage` `critical-section` feature enabled (the cache-disabled-during-flash-write hazard, `esp-idf#10079`).
- The security contract checklist below is a conformance gate, not a suggestion.
- The `wifi_mqtt` portal HTML renders identically to the ESP-IDF `portal_wifi_mqtt.html` where possible — whether the template is *shared* or *copied* is Q1.
- The library never decides provisioning-mode entry, never reboots, and never erases on its own — host decisions, carried forward from the v1 constraints.
- `mqtt_uri` accepts the plain `mqtt://` scheme only; `ota_url` accepts `http://` only — matching the ESP-IDF tier and the workspace plain-transport posture.
- Secrets (`wifi_pass`, `mqtt_pass`) are redacted in `Debug`, never pre-filled into HTML, and re-entered on every submission — identical to the ESP-IDF rules.

## Open Questions

Each carries a **Proposed** answer awaiting maintainer sign-off alongside ADR 015.

### Q1. Template sharing — Proposed: move the shared portal templates into `provisioning-pure`

The ESP-IDF `portal_wifi_mqtt.html` is platform-neutral — it is HTML with placeholder substitution, nothing ESP-IDF-specific.
Propose moving the shared templates into `provisioning-pure` as `include_str!` consts (HTML is just bytes, `no_std`-safe), so both tiers render from one source of truth and the bare-metal portal renders identically to the ESP-IDF one by construction.

This is a deliberate tradeoff: moving templates into `provisioning-pure` expands the crate from model/validation-only to "schema plus its canonical rendering".
That expansion is justified because the templates ARE part of the schema contract — the input names are generated from `Field::form_name`, the existing single source of truth — so the alternative of per-tier copies trades that coupling for silent drift between the two portals.

Rejected: copy the template into the hal crate — two copies drift, and the Constraint that the two portals render identically becomes a manual discipline instead of a compile-time fact.
Rejected: a cross-crate `include_str!` path hack pointing into the ESP-IDF crate's source tree — fragile against crate moves and breaks an out-of-workspace build.

### Q2. Store partition / offset — Proposed: a dedicated `rf_prov` data partition

Propose a dedicated, documented `rf_prov` data partition carried in a custom `espflash` partition table.
The size and position policy are locked at planning (see the store-geometry row in the Decisions table): 8 KiB / two 4 KiB sectors, immediately following the OTA data region, with only the exact byte offset deferred to the chosen partition table.
`esp-bootloader-esp-idf` / `espflash` can carry the custom table.

Rejected: reuse the standard ESP-IDF `nvs` region at `0x9000` (24 KiB) — squatting it with a foreign (non-NVS) format is a format collision, fragile against any tool that expects real NVS there.

### Q3. Record layout — Proposed: magic + version + profile discriminator + field TLVs + CRC32 + sequence counter

Propose a single record per sector with: a `u32` magic, a `u8` `layout_ver`, a profile discriminator, the field values as TLVs, a CRC32 over the payload, and a monotonic `u32` sequence counter for A/B arbitration (`load` picks the sector with the higher valid sequence).
The profile discriminator is locked at planning (see the discriminator row in the Decisions table) to the `SchemaProfile::as_str` bytes written directly, length-prefixed per the record layout — not a `u8` enum mapping.
The encode / decode / CRC / arbitration logic lives as pure host-testable functions, the same shape as the OTA HTTP parser.
A profile-discriminator round-trip test writes a profile-A record via the pure encode functions, loads it back, and asserts the returned record's discriminator matches what was written, so a profile mismatch at the consumer layer surfaces as a match failure rather than a silent reinterpretation.
This closes a gap the IDF tier's store currently has — there are no store unit tests there.
Secrets are stored plaintext — flash encryption is a host / partition concern, the same posture ADR 013 took for the ESP-IDF NVS store.

Rejected: a multi-record append log — that is wear-leveling machinery for a single-digit-writes-per-lifetime store (the rejection that also eliminated the KV crates).

### Q4. Crate API shape — Proposed: mirror the ESP-IDF builder / session surface, adapted to async

Propose mirroring the ESP-IDF `ProvisioningBuilder` / `ProvisioningSession` surface, adapted to embassy/async: a builder that takes a `PortalConfig` and the bare-metal Wi-Fi / network resources, a `start(..)` that brings up the AP, DHCP server, DNS catch-all, and HTTP server, and a session whose primary async terminator is a richer outcome sketch (`wait_outcome` returning a `ProvisioningOutcome`) alongside the `wait_committed` IDF-parity convenience.
The state machine has a `FactoryResetPending` terminal in addition to `Committed`, so a single success-path wait cannot express every way a session ends; the outcome enum names the alternatives (see the Design section).
The SoftAP embassy-net stack is configured with `embassy_net::Config::ipv4_static` carrying `AP_IP`/24, gateway = `AP_IP`, and no DNS servers.
The host spawns the embassy tasks the substrate needs (the `embassy-net` runner, plus the portal's serve loops); the builder documents exactly what the host `Spawner` must own.
Signatures only are sketched in Design below.

Rejected: a blocking `wait_committed` like the ESP-IDF tier's `Condvar`-backed one — there is no blocking path on this estate; the async estate wants an `.await`.
Rejected: a free-function entry point with no builder — loses the `with_status_entry` / `on_event` extensibility the ESP-IDF builder carries.

### Q5. DHCP lease scope — Proposed: a single `192.168.4.0/24` with a small pool and a pinned AP IP

Propose matching the ESP-IDF tier's pinned subnet for documentation and UX parity: AP IP `192.168.4.1` as a const, a `/24` (`192.168.4.0/24`), and a small lease pool (for example four leases — a captive portal serves one phone at a time).
The pinned AP IP also backs the DNS catch-all's single answer and the portal's probe-route redirects.

Rejected: a DHCP-assigned or randomised AP subnet — the ESP-IDF tier pins `192.168.4.x`, so matching it keeps the two tiers' field UX and documentation identical.

### Q6. justfile / build routing — Proposed: add `check-provisioning-hal*` recipes, examples, and a `*provision*` hal script arm

Propose:

- `check-provisioning-hal` — a stub check (`--no-default-features`), matching `check-wifi-hal` / `check-ota-hal`.
- `check-provisioning-hal-embassy` — the real check on both chips with `-Zbuild-std=core,alloc`, matching `check-wifi-hal-embassy` / `check-ota-hal-embassy`.
- Examples `hal_c3_provision_mqtt` and `hal_c6_provision_mqtt`.
- A `*provision*` arm in the `hal` case of `scripts/build-example.sh` and `scripts/flash.sh`.

`scripts/build-example.sh` and `scripts/flash.sh` already have a `*provision*` arm in their **idf** case (mapping to `rustyfarian-esp-idf-provisioning`), but the **hal** case does **not** — its package detection only matches `*join*` and `*connect*|*wifi*` (`scripts/build-example.sh:61`, `scripts/flash.sh:85`).
The exact edit: add `*provision*) pkg="rustyfarian-esp-hal-provisioning" ;;` as the **first** arm of the hal `case "$example"` package-detection block in both scripts, before the `*connect*|*wifi*` arm.
The ordering matters preemptively: the lesson from the ESP-IDF side is that `*provision*` must be matched before `*mqtt*` (an example named `..._provision_mqtt` would otherwise route to the MQTT crate); the hal case has no `*mqtt*` arm today, but `hal_c3_provision_mqtt` contains both substrings, so putting `*provision*` first is the safe ordering to apply from the start.
The Phase 0 AP-bring-up example `hal_c3_ap_smoke` also needs routing: add an `*ap*) pkg="rustyfarian-esp-hal-wifi" ;;` package arm and an `*ap*) hal_features="${hal_features},embassy" ;;` feature-append arm to both scripts.
The `*ap*` glob collides with none of the existing hal arms or example names — today's package arms are `*join*` and `*connect*|*wifi*`, the feature-append arms are `*_rgb*|hal_c6_*_led*` and `*_async*`, and the existing hal example names (`hal_c3_join`, `hal_esp32_join`, and the `*connect*` / `*wifi*` names) contain no `ap` substring.
The example name carries no `_async` suffix, locked at planning (see the example-naming row in the Decisions table) because the crate is async-only per ADR 015 §1, which makes the suffix signal-free; the existing `*_async*` feature-append therefore does not fire.
The embassy feature is added by a dedicated `*provision*) hal_features="${hal_features},embassy" ;;` feature-append arm alongside the package arm, so the build picks up `embassy` without an `_async` suffix.
The example-naming, `*provision*`-ordering, embassy-append, and `*ap*` routing decisions are the locked planning row; the exact script edits above are the operational detail this doc owns.

Rejected: reuse the existing example-name conventions without a script edit — the hal case genuinely has no provisioning package mapping, so the build would fail package detection.

## Design — candidate signatures (illustrative, not an API commitment)

Signatures only, no bodies, no comments inside snippets.
This is an illustrative sketch; the contract is the ADR plus the resolved open questions, not these exact signatures.
Implementation may revise ownership, lifetimes, and task boundaries (`SoftApHandle`, the builder/session surface, the store) while preserving the architectural constraints and the security contract.

### SoftAP addition to `rustyfarian-esp-hal-wifi` (candidate `SoftApHandle`)

```rust
pub struct SoftApHandle {
    pub controller: WifiController<'static>,
    pub stack: embassy_net::Stack<'static>,
    pub runner: embassy_net::Runner<'static, Interface<'static>>,
}

impl WiFiManager {
    pub fn init_softap_async(config: HalApConfig<'_>) -> Result<SoftApHandle, WifiError>;
}

pub trait ApConfigExt<'a> {
    fn with_ap_peripherals(
        self,
        timg0: esp_hal::peripherals::TIMG0<'static>,
        sw_interrupt: esp_hal::peripherals::SW_INTERRUPT<'static>,
        wifi: esp_hal::peripherals::WIFI<'static>,
    ) -> HalApConfig<'a>;
}
```

### Provisioning builder and session (`rustyfarian-esp-hal-provisioning`) — candidate builder/session

```rust
pub struct PortalConfig<'a> {
    pub ssid_prefix: &'a str,
    pub ap_password: Option<&'a str>,
    pub channel: u8,
    pub device_name: &'a str,
    pub firmware_version: &'a str,
    pub profile: SchemaProfile,
}

pub struct ProvisioningBuilder<'a> {
    /* config + on_event + status entries, mirroring the ESP-IDF builder */
}

impl<'a> ProvisioningBuilder<'a> {
    pub fn new(config: PortalConfig<'a>) -> Self;
    pub fn on_event<F: Fn(ProvisioningEvent)>(self, f: F) -> Self;
    pub fn start(
        self,
        spawner: embassy_executor::Spawner,
        ap: SoftApHandle,
        store: ProvisioningStore,
    ) -> Result<ProvisioningSession, ProvisioningError>;
}

pub struct ProvisioningSession {
    /* shared state behind embassy-sync primitives */
}

pub enum ProvisioningOutcome {
    Committed(ProvisioningConfig),
    FactoryResetRequested,
    HostAborted,
}

impl ProvisioningSession {
    pub fn state(&self) -> ProvisioningState;
    pub fn ap_ip(&self) -> embassy_net::Ipv4Address;
    pub async fn wait_outcome(&self) -> ProvisioningOutcome;
    pub async fn wait_committed(&self) -> ProvisioningConfig;
}
```

`wait_outcome` is the candidate primary terminator: the state machine's `FactoryResetPending` terminal means a session can end without ever committing, so a single success-path wait cannot name `FactoryResetRequested` or a host abort.
`wait_committed` remains the IDF-parity convenience for the common configure-then-reboot loop; both are candidates, and the implementation may keep one, both, or refine the variant set (for example adding a timeout/teardown-failure variant if it reads naturally).

### Store module (`rustyfarian-esp-hal-provisioning`) — candidate store

```rust
pub struct ProvisioningStore<F> {
    /* F: embedded_storage::nor_flash::NorFlash over two 4 KiB sectors */
}

impl<F: embedded_storage::nor_flash::NorFlash> ProvisioningStore<F> {
    pub fn open(flash: F, base_offset: u32, total_bytes: u32) -> Result<Self, StoreError>;
    pub fn is_provisioned(&mut self) -> bool;
    pub fn load(&mut self) -> Result<Option<StoredConfig>, StoreError>;
    pub fn save(&mut self, config: &ProvisioningConfig) -> Result<(), StoreError>;
    pub fn erase_all(&mut self) -> Result<(), StoreError>;
}
```

```rust
pub fn encode_record(config: &ProvisioningConfig, seq: u32, buf: &mut [u8]) -> Result<usize, StoreError>;

pub fn decode_record(bytes: &[u8]) -> Result<DecodedRecord, StoreError>;
```

`open` takes `total_bytes` as part of its signature and returns a hard `Err` when `total_bytes < 8192` (see the `open`-signature row in the Decisions table); this strengthens the suggested debug-assert, which would vanish in release builds, into a check that holds in both profiles on a cold path where the cost is irrelevant.

### edge-net wiring boundaries (`pub(crate)`)

```rust
pub(crate) struct PortalServer { /* wraps edge-http server */ }

pub(crate) struct DhcpServer { /* wraps edge-dhcp Server<F, N> over edge-nal-embassy UDP */ }

pub(crate) struct CaptiveDns { /* wraps edge-captive over edge-nal-embassy UDP */ }
```

No `edge-*` type appears in any public signature above; the family is an implementation detail behind these `pub(crate)` wrappers (the ADR 011 / ADR 013 private-transport pattern).

## Implementation Phases

Every phase ends with `just fmt` then `just verify`; additional gates listed per phase.
On-hardware captive-portal smoke is a trailing manual validation, not a per-phase gate.

### Phase 0 — SoftAP (AP-mode) in `rustyfarian-esp-hal-wifi` + an AP example

Add the AP-mode lifecycle (`init_softap_async` / `SoftApHandle`) against the `esp-radio =0.18.0` surface, the AP TX-power clamp, and the `ApStaConnected` / `ApStaDisconnected` subscription, reusing `wifi-pure` `ApConfig` unchanged.
Configure the SoftAP embassy-net stack with `embassy_net::Config::ipv4_static` carrying `AP_IP`/24, gateway = `AP_IP`, and no DNS servers.
Add a minimal AP-bring-up example named `hal_c3_ap_smoke`.

Gate: `just check-wifi-hal-embassy`; `just build-example hal_c3_ap_smoke` on C3.

### Phase 1 — store module (host-tested)

Add the `ProvisioningStore` and the pure `encode_record` / `decode_record` / CRC / A/B-arbitration functions, with host tests covering torn-write recovery (higher valid sequence wins), CRC rejection of corrupted records, round-trip of a `WifiMqttDevice` config, and a profile-discriminator round-trip (write a profile-A record, load it back, assert the discriminator matches — a mismatch must surface as a match failure, never a silent reinterpretation).
This profile-discriminator test closes a gap the IDF tier's store currently has, which carries no store unit tests.

Gate: `just check-provisioning-hal-embassy`; host tests run on the host target.

### Phase 2 — portal, opening with an edge-net integration spike gate

Phase 2 opens with an integration spike: bring up `edge-http` + `edge-dhcp` + `edge-captive` (at the versions in the edge-net planning row) on the Phase 0 `hal_c3_ap_smoke` scaffold, before any portal-logic wiring.
Evaluation criteria for the spike: executor / task-model fit against the embassy estate, a single-client captive-portal flow exercised on a real phone, the binary-size delta on C3, and error handling under AP / DNS / DHCP timing.
The spike outcome is recorded in the Session Log.
On spike failure, the hand-rolled fallback proceeds per ADR 015 §3 without a new ADR — the architectural commitment is the private-substrate boundary, not the crate family.

Once the substrate is validated (or the fallback is chosen), wire it behind the `pub(crate)` wrappers, thread the `WifiMqttDevice` profile through, and render the shared template (Q1).
Enforce the full security contract checklist below.
Enable esp-storage's `critical-section` feature in the new crate's dependency declaration — `esp-storage = { workspace = true, features = ["critical-section"] }` plus the chip feature — the same critical-section-backend territory as the sx126x critical-section lore in `CLAUDE.md`.

Gate: `just check-provisioning-hal-embassy`.

### Phase 3 — examples + build routing + justfile

Add `examples/hal_c3_provision_mqtt.rs` and `hal_c6_provision_mqtt.rs`, add the `check-provisioning-hal` / `check-provisioning-hal-embassy` recipes, and add the `*provision*` arms to the hal case of `scripts/build-example.sh` and `scripts/flash.sh` (Q6, `*provision*` before any `*mqtt*` arm).

Gate: `just build-example hal_c3_provision_mqtt`.

### Phase 4 — docs / CHANGELOG / ROADMAP + the CLAUDE-lore correction note

Tick the State boxes and Session Log, add a `CHANGELOG.md` entry, add the ROADMAP entry for the bare-metal provisioning crate, and update `VISION.md` / `README.md` for the fourth `esp-hal` crate.
Note (developer-local, not a public-doc edit): correct the stale `vendor/esp-radio` row in `CLAUDE.md`'s *Common Resolution Failures* table — `vendor/esp-radio` does not exist; the TX-power workaround is the inline `extern "C" esp_wifi_set_max_tx_power` clamp.

Gate: `just lint-docs`.

## Security contract checklist (conformance gate)

The ESP-IDF portal (`crates/rustyfarian-esp-idf-provisioning`) is the reference implementation for each item.

- [ ] Per-session nonce on every mutating POST (`/save`, `/factory-reset`).
- [ ] No secret pre-fill — `wifi_pass` and `mqtt_pass` never rendered into HTML, re-entered every submission.
- [ ] Request-body cap (2 KB, matching `MAX_BODY_LEN = 2048`), oversized bodies rejected.
- [ ] Lengths-only logging of credential material.
- [ ] `Cache-Control: no-store` on portal responses.
- [ ] HTML / JSON escaping of rendered values.
- [ ] Library never reboots and never erases on its own — host decisions via the `Committed` / `FactoryResetRequested` events.
- [ ] Commit-guard ordering on the store (discriminator / version written last) and an open-AP warning when no AP password is set.
- [ ] No reflection of submitted credential values in validation errors — the `ValidationError` variants carry lengths and expectations (`TooLong { max }`, `InvalidHex { expected_len }`), never input bytes, by construction; this is a stated contract with a test obligation, so a future variant cannot quietly start echoing input.
- [ ] Credential-holding buffers dropped as early as practical after commit or failure — honest scope: guaranteed zeroization (e.g. the `zeroize` crate) is out of scope for v1, since Rust drop semantics do not scrub memory; an early drop bounds the window but adding `zeroize` is a future hardening decision, not a promise this checklist makes.

## State

- [x] Design drafted (ADR 015 proposed) — 2026-06-12
- [ ] ADR 015 accepted
- [ ] Open-question proposals signed off
- [x] Phase 0 — SoftAP (AP-mode) in `rustyfarian-esp-hal-wifi` + AP example — landed 2026-06-15 (commit `704dd85`); `hal_c3_ap_smoke` builds and runs on ESP32-C3
- [x] Phase 1 — store module (host-tested) — landed 2026-06-15 (commit `73e45c0`); 38 host tests across `record.rs` + `store.rs`; subsequent fixup passes on 2026-06-15 / 2026-06-16 tightened decode bounds-checks, locked the on-flash layout (`record_len:u16` insertion), and closed UTF-8 / duplicate-tag / missing-required-field gaps
- [ ] Phase 2 — portal (substrate hardening + promotion + templates) — substrate spike gate cleared 2026-06-15 with the §3 hand-rolled fallback (DHCP + DNS catch-all + HTTP server behind the `provisioning-spike` Cargo feature in `rustyfarian-esp-hal-wifi`); pre-promotion hardening (Phase 2A), portal logic + template wiring + security-checklist conformance, and promotion into `rustyfarian-esp-hal-provisioning` (Phase 2B) all remain
- [ ] Phase 3 — examples + build routing + justfile
- [ ] Phase 4 — docs / CHANGELOG / ROADMAP + CLAUDE-lore correction note
- [ ] Verification gates green — `just fmt` / `just verify` / `just check-provisioning-hal-embassy` / `just build-example hal_c3_provision_mqtt` (gated on Phase 0–3 landing)
- [ ] On-hardware captive-portal smoke test (ESP32-C3, phone browser) — trailing manual validation

## Session Log

- 2026-06-12 — Bare-metal provisioning request from `rustyfarian-rgb-clock`, the downstream moving onto the esp-hal stack and the same one that drove ADR 014's `WifiMqttDevice` profile on the ESP-IDF tier.
  An ecosystem research pass evaluated the captive-portal network substrate (edge-net family vs picoserve vs all-hand-rolled) and the flash credential store (hand-rolled A/B record vs sequential-storage vs ekv vs ESP-IDF NVS format); all crate versions were verified against docs.rs / GitHub and the workspace Cargo.lock on 2026-06-12 (both `embedded-io-async` majors 0.6.1 + 0.7.0 and both `embassy-sync` 0.7.2 + 0.8.0 coexist in the lock, so the edge-net family is not version-blocked).
  At the maintainer's explicit request, both technology choices were elevated to ADR-level decisions with full comparison tables rather than gut-feel picks, so ADR 015 recommends and the maintainer decides.
  Awaiting maintainer sign-off of ADR 015 and the proposed answers above; the State checklist is the sign-off anchor (no issue tracker by convention).
- 2026-06-12 — Maintainer review passes; twelve findings are adopted.
  The profile discriminator is locked to the `SchemaProfile::as_str` bytes written directly (not a `u8` mapping); a profile round-trip test is added to Q3 and Phase 1; the SoftAP `embassy_net::Config::ipv4_static` AP-IP plumbing is named in Q4 and Phase 0; no-`_async`-suffix example naming plus a `*provision*` embassy feature-append arm is locked; the store geometry is locked to 8 KiB / two 4 KiB sectors immediately following the OTA data region; `open()` gains a `total_bytes` parameter with hard-`Err` validation below 8192 — strengthened from the suggested debug-assert; the ADR switches to symbol-name citations with two anchor fixes (`compile_error!` at lib.rs:90, `ApConfig` at lib.rs:303); the heapless comparison-table wording is symmetrized; the `esp-bootloader-esp-idf` 0.5.0 no-NVS confirmation is added; an esp-storage `critical-section` bullet is added to Phase 2; and `hal_c3_ap_smoke` is named with its routing arm.
  Both docs remain Proposed pending the maintainer's acceptance of the two recommended technology choices.
- 2026-06-12 — Second review passes; eight findings are adopted.
  Structural reconciliation: the planning-level details the earlier review had locked as ADR 015 sub-decisions were moved out of the ADR and into this doc as "Locked at planning pass (2026-06-12)" Decisions-table rows — their locked status is preserved, not reopened as questions, and ADR 015 narrows to durable architecture that references these rows.
  The five relocated locks are the example-naming / build-routing arms, the store geometry (8 KiB / two 4 KiB sectors after OTA data), the `open(flash, base_offset, total_bytes)` hard-`Err`-below-8192 signature, the `SchemaProfile::as_str`-bytes discriminator, and the edge-net spike-target versions (`edge-http 0.7.0` / `edge-dhcp 0.7.0` / `edge-captive 0.7.0` / `edge-nal-embassy 0.8.1`).
  The captive-portal substrate decision is softened from a settled recommendation to "edge-net is the preferred first candidate", to be validated by a Phase 2 integration spike, with a documented hand-rolled fallback that proceeds without a new ADR if the spike fails on executor fit, single-client ergonomics, or binary size — the locked architecture is the private-substrate boundary, not the crate family.
  The Design section is reframed as candidate signatures (illustrative, not an API commitment), applied across `SoftApHandle`, the builder/session, and the store.
  A richer session-outcome sketch is added: `ProvisioningOutcome { Committed(ProvisioningConfig), FactoryResetRequested, HostAborted }` with `wait_outcome`, motivated by the state machine's `FactoryResetPending` terminal, alongside the `wait_committed` IDF-parity convenience.
  The Q1 template-sharing tradeoff is acknowledged explicitly — moving templates into `provisioning-pure` expands it from model/validation-only to "schema plus its canonical rendering", justified because input names derive from `Field::form_name`, the existing single source of truth, where per-tier copies would trade that coupling for silent drift.
  Two secret-handling contract lines are added to ADR §5 and the conformance checklist: no reflection of submitted credential values in validation errors (carried by construction in the `ValidationError` variants, with a test obligation) and early drop of credential buffers (with an honest no-`zeroize`-in-v1 scope note).
  Both docs remain Proposed; nothing from the earlier review pass was un-locked — the locks were relocated, not removed.
- 2026-06-15 — Phase 0 (SoftAP in `rustyfarian-esp-hal-wifi`, `hal_c3_ap_smoke` example) and the Phase 2 substrate spike land on branch `bare-metal-softap` (commits `704dd85`, `d793337`, `b808a3d`, `2342b9e`).
  The Phase 2 spike took the ADR 015 §3 hand-rolled fallback: rather than wiring `edge-http` / `edge-dhcp` / `edge-captive`, three private modules (`dhcp.rs`, `dns_catchall.rs`, `http_server.rs`) ship inside `rustyfarian-esp-hal-wifi` behind a `provisioning-spike` Cargo feature, validated end-to-end on a real phone.
  The hand-rolled path was chosen ahead of an edge-net side-by-side because the architectural commitment is the private-substrate boundary, not the crate family, and three small bespoke modules with host tests proved a faster path to a phone-on-AP demo than dragging in a new family.
  Two non-obvious lessons surfaced and are now recorded in `docs/project-lore.md` ("esp-hal April 2026 Stack"): (1) workspace-invariant constants belong bound at construction, not threaded as per-call arguments — the DHCP server's `LeaseTable::allocate(pool_start, ...)` shape shipped to hardware because host tests called `allocate` directly and never exercised the call-site indirection; (2) `StaticCell<[u8; N]>` requires a compile-time-constant `N`, so runtime `config.buf_size: usize` fields cannot back the allocation — `const` defaults are the right shape for spike code, const generics for production.
  The HTTP server applied lesson (1) prospectively (no `Connection: close` parameter — server-wide invariant) and codifies the pattern for the eventual portal promotion.
  Phase 1 (store module) and the Phase 2 portal promotion both remain open; ADR 015 status is unchanged (still Proposed) — the spike validates the §3 fallback but the formal acceptance signal is the State checklist.
- 2026-06-15 — Phase 1 implementation kickoff after a 5-specialist plan-review pass refined the design.
  Locked before code: `MAGIC = 0x52465052` ("RFPR"), `LAYOUT_VER = 1`, all multi-byte ints `to_le_bytes` / `from_le_bytes`, CRC32 IEEE 802.3 reflected via `const fn` lookup table covering `[magic..last_byte_before_crc]`, TLV tag table `WIFI_SSID = 0x01` / `WIFI_PASS = 0x02` / `MQTT_HOST = 0x03` / `MQTT_PORT = 0x04` / `MQTT_USER = 0x05` / `MQTT_PASS = 0x06` / `MQTT_CLIENT = 0x07` / `OTA_URL = 0x08` / `DEVICE_NAME = 0x09` with retirement-not-reuse policy.
  API refinements vs the candidate sketch: `load()` returns `Result<Option<ProvisioningConfig>, StoreError>` (reuses the existing manually-redacted `Debug` impl in `provisioning-pure`, avoids a parallel `StoredConfig` heapless mirror); `is_provisioned()` returns `Result<bool, StoreError>` not bare `bool` (flash reads are fallible); `open(flash, base_offset, total_bytes)` validates size ≥ 8192 AND `base_offset % F::ERASE_SIZE == 0` AND `F::ERASE_SIZE == 4096`, each with its own `StoreError` variant; `encode_record` / `decode_record` are `pub(crate)` not `pub` (don't leak the on-flash format as a public API); `save()` owns a private `heapless::Vec<u8, 4096>` encode buffer overwritten with `0xFF` before drop; `decode_record` bounds-checks `len` before reading CRC (torn-length-field guard); `encode_record` pads written buffer to 4-byte multiple (esp-storage `WRITE_SIZE = 4`).
  `StoreError` variants locked upfront — `TooSmall { required: u32, provided: u32 }` / `NotAligned` / `UnsupportedGeometry { erase_size: u32 }` / `BadMagic` / `BadVersion { found: u8, expected: u8 }` / `BadCrc` / `ShortRecord { need: usize, have: usize }` / `UnknownProfile { len: u8 }` / `BufferTooSmall { need: usize, have: usize }` / `Flash` with manual `Debug` — lengths and expectations only, never input bytes (security-contract obligation).
  The `Flash` variant intentionally carries no `F::Error` payload: a parameterised `Flash(F::Error)` would impose a viral `where F::Error: Debug` bound on every `NorFlash` impl in the dep graph, which the candidate-signatures section had not anticipated.
  The typed flash failure cause is observable at the call site via downstream logging instead.
  Phase 1 lands ~25 host tests (split between `record.rs` and a `MockNorFlash`-backed `store.rs` suite) plus a `validation_errors_carry_no_input_bytes` companion test in `provisioning-pure::error` (the security contract belongs in both crates).
  Pre-flight workspace edit: amend the workspace `[workspace.dependencies] provisioning-pure` line with `default-features = false` (closes the same workspace-inheritance silent-feature-bleed loop the `rustyfarian-network-pure` fix closed two days ago — without it the new crate's `default-features = false` override is silently ignored).
  Parallel finding flagged for a separate ticket: `crates/rustyfarian-esp-idf-provisioning/src/store.rs:141` derives `Debug` over plaintext `wifi_password` / `mqtt_pass` — same redaction gap the bare-metal store closes by construction, but out of Phase 1 scope.
- 2026-06-15 — Phase 1 lands; one on-flash layout refinement emerged during implementation.
  The locked header was `[magic:u32][layout_ver:u8][seq:u32][len:u8][profile_str][TLV*][crc32:u32]` — a single `len:u8` for the profile string only.
  At decode time the store reads back a full 4 KiB sector; the trailing bytes from the end of the record to the end of the sector are `0xFF` (the erased-flash state).
  Without an explicit total-record-length marker, the decoder's greedy TLV scan could not distinguish real TLVs from the `0xFF` padding, so every save→load round-trip failed — and the embedded-systems reviewer flagged the latent gap as "the plan does not explicitly enumerate a bounds check on `len` before the CRC read".
  Resolution: insert a `record_len: u16 LE` field at byte offset 9, between `seq` and `profile_len`, growing the fixed header from 10 to 12 bytes.
  The decoder now reads `record_len` early, bounds-checks it against the slice length, and locates the CRC word deterministically at `record_len - 4` without scanning for sentinel patterns.
  The new layout is `[magic:u32][layout_ver:u8][seq:u32][record_len:u16][profile_len:u8][profile_str][TLV*][crc32:u32]`; all multi-byte integers remain little-endian on flash; the security and atomicity properties are unchanged.
  Phase 1 also adopts two compile-time exhaustiveness locks (security-driven, surfaced by Wave-3 verification): the `store_errors_carry_no_input_bytes` and `validation_errors_carry_no_input_bytes` tests each carry a `_exhaustiveness_lock` inner fn whose `match` over the error enum will fail compilation if a future variant is added without being covered by the security sweep — closing the contract against silent drift.
  The `save()` encode-buffer overwrite was hardened to run on every exit path (success and every error from `encode_record` / `flash.erase` / `flash.write`) by wrapping the fallible region in a closure.
  Parallel finding (still out of scope): `crates/rustyfarian-esp-idf-provisioning/src/store.rs:141` continues to derive `Debug` over plaintext credentials — the bare-metal store closes the gap by construction; the IDF tier needs a separate fix.
- 2026-06-15 — Post-merge PR-review fixups land on top of Phase 1 (commit `73e45c0`).
  The deeper review of the bare-metal-softap PR (5 commits, ~5.9k insertions) surfaced four store-side gaps the Wave-3 verification team had under-weighted, all addressed in this fixup pass: (1) `open()` did not bound-check `base_offset + STORE_SIZE` against `total_bytes` or `flash.capacity()` — a misconfigured partition would fail late on first save with a generic `Flash` error; new `StoreError::OffsetOutOfBounds { end, limit }` variant fires on a cold path with `limit = total_bytes.min(capacity)`; (2) non-empty `ProvisioningConfig::extras` were silently dropped at encode because the v1 record format allocates no TLV tags for opaque extras — encoder now rejects with `StoreError::ExtrasNotSupported { count }`, locked by `extras_rejected_at_encode` test; (3) `save()` did two full-sector reads through `next_seq()` then two more through `target_sector_for_save()` (4 reads + 1 write per save), now coalesced into a single `plan_save()` returning both; (4) `write_tlv` / `encode_record` carry `debug_assert!` tripwires on `value.len() as u8` and `profile_bytes.len() as u8` against future field-cap revisions past 255 bytes (every shipped field is ≤ 128 today, but `OTA_URL_MAX_LEN = 128` is the nearest ceiling).
  Doc-drift fix: this Session Log previously described the `Flash` variant as `Flash(F::Error)`; the shipped variant carries no payload to avoid a viral `where F::Error: Debug` bound across every `NorFlash` impl in the dep graph (the structured cause is observable at the call site via downstream logging). The relevant lock-up table row above has been corrected.
  `pick_active` doc updated to call out the seq-saturation edge: once `u32::MAX` is reached, the `>=` tie in arbitration keeps the frozen sector active while saves continue writing to the standby with the saturated `seq`, so subsequent writes are effectively orphaned — operationally impossible (~4B saves on a single-digit-writes-per-lifetime store), but documented as a known property rather than the previously implied "no behavioural cost".
  AGENTS.md architecture-table row corrected: the `provisioning-pure` row now lists `rustyfarian-esp-hal-provisioning` rather than `(planned)`, and column padding is re-aligned for visual diff.
  Phase 2 entry conditions captured (deferred from the PR review): (a) reduce per-`save` peak stack from ~4–5 KiB (one `heapless::Vec<u8, 4096>` encode buffer + one transient `[u8; 4096]` read buffer in `try_read_sector`) by reading only a fixed prefix containing `record_len`, then a targeted read of `record_len` bytes — doubles flash transactions but cuts peak stack on the hot path, relevant given the workspace's main-task stack-overflow lore; (b) the three `provisioning-spike` modules in `rustyfarian-esp-hal-wifi` (DHCP / DNS / HTTP) need targeted hardening before promotion: DHCP must drop or NAK a bindingless REQUEST (no Option 50, ciaddr=0) per RFC 2131 §4.3.2 rather than upgrading to allocation+ACK; DHCP's `rx_pkt` (548 B) must match or exceed the socket RX buffer size (1024 B) — 549–1024 B packets silently truncate today; DNS should defensively `let n = n.min(rx_pkt.len())` before `decode_query`; HTTP `build_response(...).unwrap_or(0)` can emit a zero-byte response and the 405 path may RST a still-uploading POST body; pre-plant a `// SECURITY: never log request bodies` marker at the routing site before any /save route lands.
  Justfile `test-dhcp` / `test-http` / `test-dns` are three recipes running the same `cargo test -p rustyfarian-esp-hal-wifi --features provisioning-spike` command — Phase 2 will collapse them with module filters as part of the promotion to `rustyfarian-esp-hal-provisioning`.
- 2026-06-16 — Second-pass spike-server fixups: a deeper first-hand read of `dhcp.rs` / `dns_catchall.rs` / `http_server.rs` surfaced one genuinely new bug and seven misleading-doc / clippy / Phase 2-note items.
  Fixed in-band: (1) **DHCP /24-boundary validation hole** — the startup check compared `pool_end - pool_start + 1 == POOL_SIZE` but never verified that the pool lay within a single /24 or that `pool_start.0[3] + POOL_SIZE - 1 ≤ 255`, so a config like `pool_start=192.168.4.250, pool_end=192.168.5.4` would pass the size check yet `addr_of(6)` would `wrapping_add` the last octet to `.0` instead of advancing the third octet. Lifted the inline check into a pure `pub(crate) fn validate_pool_geometry(pool_start, pool_end, pool_size) -> Result<(), PoolGeometryError>` with named variants (`SizeMismatch { configured, required }`, `CrossesSubnet`, `LastOctetOverflow`), now locked by four host tests (`pool_geometry_accepts_default_within_24_pool`, `pool_geometry_rejects_size_mismatch`, `pool_geometry_rejects_crosses_24_boundary`, `pool_geometry_rejects_last_octet_overflow`); (2) cleaned up the four stale comments — `Method::Post` enum no longer claims a 404 response (the router emits 405), the DNS opcode comment no longer says it's "copied via mask from request" (the decoder accepts only opcode 0 so it's hard-set, not masked), the DNS over-long-name comment now describes the actual 13th-label-bounds-check rejection rather than the root-terminator that's never reached, and the HTTP "empty 500" log now matches the code path that actually fires (the buffer-too-small branch silently closes the socket with no response, not a 500); (3) cleaned up nine clippy warnings that the workspace `just verify` did not surface because the `provisioning-spike` Cargo feature is non-default — three `collapsible_if` in `decode_options` collapsed into `match` guards, three `Option::map_or(default, …)` → `is_some_and` / `is_none_or` per current clippy lints, two `core::iter::repeat(b).take(n)` → `core::iter::repeat_n(b, n)` in tests, and two `#[allow(clippy::result_unit_err)]` on `build_response` / `route` with rationale (the only failure mode is "ran out of buffer bytes" and the caller has no recovery path until Phase 2 expands the response set past 200/405).
  Verified both environmental blocking unknowns the PR-review pass had called out: `cargo clippy --features provisioning-spike --tests -- -D warnings` is clean for `rustyfarian-esp-hal-wifi` (the `unnecessary_map_or` lint family is now addressed across all three sites the review named), and `just build-example hal_c3_ap_smoke` builds clean on `riscv32imc-unknown-none-elf` (15 s release-mode link).
  Phase 2 hardening list extended (deferred per the review's calibrated severity): (a) DHCP's `confirm(offered_ip)` call is redundant after `allocate` already wrote the lease entry — fold the timestamp refresh into one place; (b) `allocate` commits a lease at OFFER time, so a DISCOVER without REQUEST holds a slot for the full lease — fine for one phone, churns the 11-slot table under a DISCOVER flood with spoofed MACs; (c) embedded stack-footprint sizing: `http_server::run` puts req_buf (2048) + resp_buf (2048) = ~4 KiB on the task stack, the store's `save` similarly holds a 4 KiB `heapless::Vec` plus transient 4 KiB sector reads — three spawned tasks plus the store all want generous task stacks, worth a sizing note given the workspace's prior main-task stack-overflow lore; (d) `StackResources::<4>` is exactly sized (DHCP + DNS + HTTP = 3 + 1 spare); a fourth socket silently exhausts it, so the `<4>` and the three spawned tasks are a coupled invariant that should be commented when Phase 2 promotes; (e) the async `run()` state machines have zero test coverage — only the pure codecs and `LeaseTable` are unit-tested today. The DHCP REQUEST → {ACK, NAK} branch in particular is pure decision logic that should be lifted into a testable function during Phase 2 promotion — that is where the bindingless-REQUEST defensive case (and any future-introduced cousins of the /24-boundary bug) would be locked.
  Two spike-server pass-1 callouts were re-graded by this pass-2: the DHCP `rx_pkt` 548-vs-1024 socket buffer mismatch is **not** a panic risk — `embassy-net`'s `recv_from` copies `min(datagram, buf)` and returns the actual `n`, so `&rx_pkt[..n]` is always in-bounds; worst case is a silently-truncated >548-byte packet, which a captive-portal phone never sends. Downgraded to a Phase 2 hardening nit. The HTTP "accepts POSTed credentials" framing was a pass-1 misreading — the module and example docs are accurate; the only stale spot was the `Method::Post` enum comment fixed above.
- 2026-06-16 — Third-pass PR-feedback fixups (closes a correctness gap the prior verification waves missed).
  **Decode permissiveness — corrected.** The PR reviewer surfaced that `decode_record` initialised every required-field slot to an empty slice / `None` and then `build_config` silently turned those into empty strings, even synthesising `mqtt_port = Some(1883)` when the TLV was absent. A CRC-valid but field-incomplete record decoded into a syntactically valid-looking config rather than being rejected. Closed by switching every required-field slot to `Option<&[u8]>`, gating the post-loop construction on `MissingRequiredField { tag: u8 }` for the four Core fields (wifi_ssid / wifi_pass / ota_url / device_name) and additionally `mqtt_host` / `mqtt_port` when the profile is `WifiMqttDevice`, and dropping the `mqtt_port.unwrap_or(1883)` fallback from `build_config` entirely (the gate guarantees `Some`, so the construction now `expect`s with rationale). Four new tests lock the contract: `decode_rejects_missing_wifi_ssid`, `decode_rejects_missing_ota_url`, `decode_rejects_missing_mqtt_host_for_wifi_mqtt_profile`, `decode_rejects_missing_mqtt_port_for_wifi_mqtt_profile` — built via a small `build_record_with_tlvs` helper that emits a CRC-valid record with a caller-supplied TLV subset.
  **Duplicate TLV handling — explicit reject.** Added `StoreError::DuplicateTag { tag: u8 }` and a `set_once` helper that turns the per-tag slot fill into a presence-guarded write. The encoder never emits duplicates, so a duplicate in a CRC-valid record by definition implies a buggy or adversarial producer; refuse rather than last-wins. Locked by `decode_rejects_duplicate_tag` (two `TAG_WIFI_SSID` entries in one record → `DuplicateTag { tag: TAG_WIFI_SSID }`). Both new variants are added to the `_exhaustiveness_lock` so any future variant that ships without test coverage fails compilation.
  **`PoolGeometryError::LastOctetOverflow` — removed as structurally unreachable.** The reviewer was right: under the helper's caller contract (`pool_end - pool_start + 1 == pool_size`), an overflow on the last octet either lands `pool_end` in the next /24 (caught by `CrossesSubnet`) or yields a `configured != pool_size` (caught by `SizeMismatch`), so no separate variant could fire. Dropped the variant, the dead `pool_geometry_rejects_last_octet_overflow` test (which never reached the branch it claimed to test), and added `pool_geometry_accepts_boundary_at_255` that pins the `pool_start[3] = 250 + pool_size = 6 → end .255` boundary case as `Ok(())` plus the `pool_size = 7` over-claim as `SizeMismatch`.
  **Stack usage — explicit `# Stack usage` doc heading on `ProvisioningStore::save`.** Documents the ~4–5 KiB peak (4 KiB encode buffer in `save` + 4 KiB read buffer in `try_read_sector`), names the Phase 2 optimisation (read a 12-byte header prefix first, then `record_len` bytes), and gives integrators a budgeting number — "at least 6 KiB on top of their own requirements until that change lands" — so the limitation is visible from the public API, not only from the feature doc.
  Test count: hal-provisioning 31 → 36 (+ 5 new); DHCP 58 (one removed for the dropped variant, one added at the .255 boundary). All gates clean: `just fmt` · `just verify` · `cargo clippy --features provisioning-spike --tests -- -D warnings` · `just check-provisioning-hal-embassy` (both riscv32 targets) · `just test-provisioning-hal` · `just test-dhcp` · `just build-example hal_c3_ap_smoke` (15s release link).
- 2026-06-16 — Fourth-pass fixups (closes the last decoder-permissiveness item the reviewer flagged on a fresh pass; resolves the vestigial-`Result` clarity nit at the same time).
  **Invalid UTF-8 — fail closed.** `decode_record` previously routed every string-typed TLV value through `bytes_to_hstring`, which silently coerced invalid UTF-8 to an empty string via `core::str::from_utf8(b).unwrap_or("")`. The encoder only ever writes validated `heapless::String` content, so non-UTF-8 bytes in a CRC-valid record imply a buggy or adversarial producer and should refuse rather than normalise to empty. Added `StoreError::InvalidUtf8 { tag: u8 }`, rewrote `bytes_to_hstring` to take the offending TLV's tag and return `Result<HString<N>, StoreError>`, and threaded the `?` through every required-and-optional string TLV in `build_config`. Heapless-capacity truncation remains silent and acceptable (field caps match encode-time caps, so well-formed records round-trip without loss).
  **`build_config` `Result` regained a purpose.** With the required-field gate having moved up into `decode_record` last round, `build_config`'s `Result<ProvisioningConfig, StoreError>` had no error paths and was vestigial; the reviewer flagged it as a clarity nit. The UTF-8 validation above now provides real `Err` returns through this same signature, so the indirection is no longer scaffolding.
  Locked by two new tests: `decode_rejects_invalid_utf8_in_required_field` (a record with `0xFF 0xFE 0xFD` in `TAG_WIFI_SSID` is rejected with `InvalidUtf8 { tag: TAG_WIFI_SSID }`) and `decode_rejects_invalid_utf8_in_optional_mqtt_user` (covers the optional-TLV path where `bytes_to_opt_hstring` is the indirection). The `_exhaustiveness_lock` was extended to cover the new variant so any future variant without test coverage fails compilation.
  Three of the reviewer's pass-3 "still bothers me" items were already addressed by the previous fixup pass (committed as `c6fe657`): required fields are `Option<&[u8]>` with a `MissingRequiredField` gate, `mqtt_port` no longer defaults to 1883, the misleadingly-named `profile_discriminator_wrong_profile_returns_err` was renamed to `lorawan_profile_string_decodes_as_lorawan_field_device` with the exploratory comment block trimmed, and duplicate TLVs are rejected as `DuplicateTag { tag }`. The reviewer's snippets were from the pre-`c6fe657` commit state; this entry confirms the gap they call out is now closed.
  Test count: hal-provisioning 36 → 38 (+ 2 new). All gates clean.
- 2026-06-16 — Phase 2A — Pre-promotion hardening of the spike substrate modules in `rustyfarian-esp-hal-wifi`.
  Eight of the deferred Phase 2 hardening items from the 2026-06-15 session-log review landed in one pass; the rest remain on the Phase 2B promotion checklist.
  **DHCP** (`crates/rustyfarian-esp-hal-wifi/src/dhcp.rs`): the MSG_REQUEST decision tree was lifted out of the async `run` loop into a pure `pub(crate) fn decide_request(&DhcpMessage, &mut LeaseTable, server_ip, now_secs) -> RequestOutcome` returning `Ack(Ipv4Addr)` / `Nak` / `Drop` / `Ignore`.
  Six new host tests lock the RFC 2131 §4.3.2 branches: SELECTING with Option 50 → Ack, RENEWING with ciaddr → Ack, bindingless REQUEST with no record → Drop (the §4.3.2 silent-server rule), bindingless REQUEST with an existing binding → lenient Ack-refresh, Option 54 names a different server → Ignore, and an unsatisfiable Option 50 → Nak.
  The redundant `LeaseTable::confirm` call after `allocate` was folded away; `confirm` is removed as dead code.
  The DHCP receive buffer (`rx_pkt`) was bumped from 548 B (BOOTP minimum) to 1024 B to match the underlying UDP socket RX buffer — option-heavy DHCP REQUESTs were silently truncated by the 548-byte cap.
  **DNS** (`dns_catchall.rs`): added a defensive `let n = n.min(rx_pkt.len());` clamp before `decode_query` to document the embassy-net invariant.
  **HTTP** (`http_server.rs`): the `route(...).unwrap_or_else(|()| 0)` zero-byte response path was replaced with a deterministic `write_minimal_500(&mut resp_buf)` fallback that emits a hard-coded `HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n` instead of silently closing without sending any bytes.
  A `drain_body_with_deadline(socket, remaining, 500ms)` helper now runs between response flush and `socket.close()`, reading and discarding any in-flight POST body so the close FIN does not race the upload and trigger RST — the mitigation the 2026-06-15 review called out for the 405 path and the future `/save` route.
  A `// SECURITY: never log request bodies` marker was planted at the routing dispatch site so the eventual `/save` route lands behind a visible guard already in source.
  A `# Stack usage` doc-comment on `http_server::run` names the per-task peak (~4 KiB: 2 KiB `req_buf` + 2 KiB `resp_buf`) and cross-references the DHCP / DNS / `ProvisioningStore::save` peaks, giving integrators a 6 KiB budget number to size spawned-task stacks against.
  The `StackResources::<4>` capacity in `init_softap_async` got an INVARIANT comment naming its coupling to the three spike-module socket tasks (DHCP UDP + DNS UDP + HTTP TCP) plus the one spare; the parallel spawn site in `examples/hal_c3_ap_smoke.rs` carries the matching note.
  **Justfile**: the three `test-dhcp` / `test-http` / `test-dns` recipes (running identical `cargo test -p rustyfarian-esp-hal-wifi --features provisioning-spike` commands) are collapsed into one `test-provisioning-spike`, with the three legacy names retained as one-line back-compat aliases.
  **Tests**: hal-wifi spike test count 58 → 65 (+6 `decide_request` branches, +1 `minimal_500_is_a_valid_complete_response`).
  All gates clean: `just fmt` · `just verify` · `cargo clippy --target aarch64-apple-darwin -p rustyfarian-esp-hal-wifi --no-default-features --features provisioning-spike --tests -- -D warnings` · `just build-example hal_c3_ap_smoke` (16 s release link on `riscv32imc-unknown-none-elf`).
  Deferred to Phase 2B portal promotion: (a) `ProvisioningStore::save` peak-stack reduction (12-byte-prefix-then-`record_len`-bytes targeted read — names the public `# Stack usage` block already shipped, but the implementation work belongs with the promotion); (b) decoupling `LeaseTable::allocate` candidate-lookup from commit, so a NAK'd REQUEST does not consume a pool slot under the client's MAC; (c) lifting the DHCP DECLINE arm into `decide_request`-style pure logic for symmetry (low value today — single-arm match with one `entries[idx] = None` line); (d) the analogous async-run-state-machine coverage for DNS and HTTP routing (the lift demonstrated the pattern on DHCP; replication is mechanical).
- 2026-06-16 — Phase 2A — PR #72 review fixups (six items, all small).
  Reviewer's main concern was that `decide_request` mutates `LeaseTable` even on the Nak path and the doc didn't make clear whether that was deliberate long-term or temporary scaffolding.
  Strengthened the `# Mutation note` header on `decide_request` to read **"NAK-still-mutates is preserved, not endorsed"** and added an explicit cross-reference to the deferred Phase 2B `probe` + `commit_lease` API.
  Locked the current side effect with a dedicated host test (`decide_request_nak_still_records_mac_lease`) so any silent drift fails CI loudly; the test carries the rewrite plan in its body (`Phase 2B probe/commit will replace this assertion with assert!(table.find_by_mac(&mac).is_none())`).
  Re-enriched the `RequestOutcome::Ignore` log call to name **both** the mismatched client `server_id` (Option 54 in the REQUEST) and the local `server_ip` — the original detail dropped by the lift; the format string also documents the unreachable-`unwrap_or` fallback so future Ignore-emitting branches cannot panic the logger.
  Promoted the DHCP receive-buffer length to a `pub(crate) const SOCKET_RX_BUF_LEN = 1024` at module scope and threaded it through the `StaticCell` RX/TX socket buffers and the on-stack `rx_pkt` array, so the socket-buffer / receive-buffer coupling is enforced at the source instead of by comment.
  Promoted the HTTP body-drain deadline to a `const REQUEST_BODY_DRAIN_DEADLINE_MS: u64 = 500` with a doc-comment naming the client-correctness-vs-single-socket-pinning trade-off the previous magic number elided.
  Added an HTTP integration test (`route_response_too_large_falls_back_to_minimal_500`) that exercises the actual `route(...).unwrap_or_else(|()| write_minimal_500(...))` fallback path inside `run` rather than only `write_minimal_500` in isolation — locks the integration point against a future refactor that might swap the fallback for `0` again.
  Reviewer items deliberately deferred to Phase 2B (the reviewer marked these as "consider", not blocking): a uniform UDP `recv_from` clamp helper across DHCP/DNS, a credential-safe HTTP request-logger that refuses raw body bytes by construction, a shared `SUBSTRATE_SOCKET_COUNT` constant to compile-time-couple `StackResources::<N>` to the number of spawned substrate tasks, and the daily-progress-vs-milestone split of this feature doc.
  **Tests**: hal-wifi spike test count 65 → 67 (+ `decide_request_nak_still_records_mac_lease`, + `route_response_too_large_falls_back_to_minimal_500`).
  All four gates remain clean: `just fmt` · `just verify` · `cargo clippy --target aarch64-apple-darwin -p rustyfarian-esp-hal-wifi --no-default-features --features provisioning-spike --tests -- -D warnings` · `just build-example hal_c3_ap_smoke` (11 s release link on `riscv32imc-unknown-none-elf`).
- 2026-06-16 — Phase 2A — PR #72 pass-2 review polish (two nits from the reviewer's "approve, with optional non-blocking" list).
  The reviewer's second-pass approval flagged `SOCKET_RX_BUF_LEN` as misleading (the constant now backs both UDP RX and TX socket buffers plus the on-stack `rx_pkt`) and re-raised the `StackResources::<4>` comment-coupling that the pass-1 review had also called out.
  Renamed `SOCKET_RX_BUF_LEN` to `SOCKET_BUF_LEN` (single symmetric const) at all 7 reference sites in `dhcp.rs`.
  Promoted the AP-stack socket-table size to `pub const SUBSTRATE_SOCKET_COUNT: usize = 4` at the crate root, threaded it through both `StaticCell<StackResources<{..}>>` and `StackResources::<{..}>::new()` call sites in `init_softap_async`, and rewrote the parallel comment in `examples/hal_c3_ap_smoke.rs` to reference the constant by name rather than restating the magic number — a future bump from 4 to 5 now requires a single-source change, not three coordinated edits.
  Deliberately skipped (the reviewer's other "consider" items): a shared UDP `recv_from` clamp helper across DHCP/DNS (single-line inline is already self-explanatory), HTTP policy-constant grouping (cosmetic), Ignore-log `{:?}` simplification (trace-log readability is better with the current explicit format), and the feature-doc archival-vs-roadmap split (process question, defer to Phase 4 post-promotion).
  Tests unchanged at 67; all four gates remain clean (`just build-example hal_c3_ap_smoke` 12 s release link).
