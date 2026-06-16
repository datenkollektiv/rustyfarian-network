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

### Locked at Phase 2A hardening (2026-06-16)

These rows capture named values that landed in `rustyfarian-esp-hal-wifi` during the Phase 2A pre-promotion hardening pass (PR #72, commits `5d7ec70` + `c88a1b3`).
They are part of the v1 contract — Phase 2B promotion moves the source of truth into `rustyfarian-esp-hal-provisioning` but cannot silently widen them.

|                                                                                                                                                 Decision | Reason                                                                                                                                                                                                                                                                                 | Rejected Alternative                                                                                                                                                                        |
|---------------------------------------------------------------------------------------------------------------------------------------------------------:|:---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|:--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| `SOCKET_BUF_LEN = 1024` — the per-direction UDP socket buffer length, shared by both `StaticCell` RX/TX buffers and the on-stack `rx_pkt` in `dhcp::run` | Option-heavy DHCP REQUESTs (DDNS, vendor-class identifiers, PXE) routinely exceed the BOOTP 548-byte minimum; coupling the socket buffer and the on-stack receive buffer at the source prevents the 548-vs-1024 truncation hazard from being reintroduced by a one-sided tuning change | A 548-byte (BOOTP minimum) receive buffer (silently truncates option-heavy DHCP frames); a per-call buffer override on the socket (couples two sites by discipline rather than enforcement) |
|                                                            `SUBSTRATE_SOCKET_COUNT = 4` — `embassy-net` `StackResources<N>` size for the SoftAP scaffold | One socket per substrate task (DHCP UDP + DNS UDP + HTTP TCP) plus one spare; `embassy-net` returns `SocketAlreadyOpen` / `OutOfResources` on exhaustion rather than blocking, so the table size must match the number of long-lived sockets                                           | A magic `4` in `init_softap_async` paired with parallel comments in the example — the 2026-06-16 PR #72 review flagged the discipline coupling twice, prompting the const promotion         |
|                                    `REQUEST_BODY_DRAIN_DEADLINE_MS = 500` — HTTP body-drain bounded deadline between response flush and `socket.close()` | Trade-off: long enough that a phone on the captive portal almost never stalls past it (clean FIN, not RST that browsers surface as "connection reset"); short enough that a stuck client cannot pin the single TCP socket indefinitely                                                 | An inline magic `500` (no rationale in source — reviewer flagged on PR #72); an unbounded drain (pins the single TCP socket on any stuck client)                                            |

### Locked at Phase 2B planning gate (2026-06-16)

These rows resolve the open questions Q1 (template sharing) and Q4 (builder/session API), signed off via the `/feature` update dialog on 2026-06-16.

|                                                                                                                                                                                                                                                                                                                                                                                                                                                  Decision | Reason                                                                                                                                                                                                                                                                                                        | Rejected Alternative                                                                                                                                                                                                                                                        |
|----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------:|:--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|:----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
|                                                                                                                                                                                                                                                                                                                       Q1 — Shared portal HTML templates (both `WifiMqttDevice` and LoRaWAN profiles) live in `provisioning-pure` as `include_str!` consts | The templates ARE part of the schema contract (input names derive from `Field::form_name`, the single source of truth); both tiers render identically by construction; one source survives schema additions and field renames                                                                                 | Per-tier copies (drift between IDF and bare-metal portals); a cross-crate `include_str!` path hack into the IDF crate (fragile against crate moves, breaks out-of-workspace builds); LoRaWAN-template-deferred (no symmetry payoff, storage cost is bytes-in-flash not RAM) |
| Q4 — Public builder/session surface: `PortalConfig { ssid_prefix, ap_password, channel, device_name, firmware_version, profile }`; `ProvisioningBuilder::new(...).on_event(...).start(spawner, ap, store)` returning `ProvisioningSession`; `ProvisioningOutcome { Committed(ProvisioningConfig), FactoryResetRequested, HostAborted }` with both `wait_outcome` (richer terminator) and `wait_committed` (IDF-parity convenience) as session terminators | Mirrors the ESP-IDF tier's `ProvisioningBuilder` / `ProvisioningSession` shape (consistent UX across HALs); the outcome enum names the `FactoryResetPending` terminal so a single success-path wait cannot silently drop alternatives; `wait_committed` carries forward for callers porting from the IDF tier | A free-function entry point (loses `with_status_entry` / `on_event` extensibility); a single-`wait_committed`-only terminator (cannot express `FactoryResetRequested` or `HostAborted`); a different `PortalConfig` shape than the IDF tier (cross-HAL onboarding cost)     |

### Locked at Phase 2B implementation (2026-06-16)

|                                                                       Decision | Reason                                                                                                                                                                                                                                      | Rejected Alternative                                                                                                   |
|-------------------------------------------------------------------------------:|:--------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|:-----------------------------------------------------------------------------------------------------------------------|
| `DEFAULT_TX_BUF = 6144` in `http_server.rs` (bumped from 2048 at Phase 2A) | Real portal HTML for the `WifiMqttDevice` profile is 4-6 KiB rendered with placeholders substituted; the 2 KiB spike default would trip the `MINIMAL_500` fallback on every portal GET response, making the captive portal non-functional | Keeping 2048 (spike default) — incompatible with real portal HTML; a per-call override (rejected because `StaticCell<[u8; N]>` requires const N, not a runtime field) |
| v1 `ProvisioningSession` has no `shutdown()` method; session ends by host reboot after `Committed` or by host-driven erase after `FactoryResetRequested`. The four spawned embassy tasks (net runner, wifi-event watcher, DHCP, DNS, HTTP) live for the boot lifetime. | Pre-emptive cancellation requires threading a `CancellationToken` through every spawned task, a substantial cross-cutting refactor that the v1 captive-portal scenario (commit then reboot) does not need. | A `shutdown()` mirroring the IDF tier (drops HTTP server, stops SoftAP) — would require adding cancellation plumbing to every task without a v1 use case driving the API. |

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
- Only `SchemaProfile::WifiMqttDevice` is implemented in v1; `LorawanFieldDevice` is rejected at `start()` with `ProvisioningError::ProfileNotSupported`. LoRaWAN portal logic is a v2 expansion.
- The SoftAP scaffold `embassy-net` `StackResources` capacity is hard-coupled to the number of spawned substrate tasks via the `SUBSTRATE_SOCKET_COUNT` crate-root const; adding a fourth long-lived socket-owning task without bumping the const in lockstep exhausts the table and `embassy-net` returns `SocketAlreadyOpen` / `OutOfResources` rather than blocking.
- Integrators sizing the spawned-task stacks must budget at least **14 KiB** for the HTTP task — the steady-state frame is ~8.3 KiB (`req_buf` 2 KiB + `resp_buf` 6 KiB (`DEFAULT_TX_BUF = 6144`) + executor overhead); during `POST /save` the call to `ProvisioningStore::save` transiently adds ~4.6 KiB (4 KiB encode buffer + ~512 B read buffer), raising the worst-case peak to ~13 KiB; the `# Stack usage` doc-comment on `portal::run_portal_dyn` (and on `ProvisioningStore::save`) is the integrator-visible reference. In bare-metal embassy (`esp-rtos` thread-mode executor) all async tasks share the main thread's stack — size it via the linker script or `CONFIG_ESP_MAIN_TASK_STACK_SIZE`.

## Open Questions

Each carries a **Proposed** answer awaiting maintainer sign-off alongside ADR 015.

### Q1. Template sharing — **Resolved 2026-06-16: accepted** (locked at the Phase 2B planning gate Decisions row above)

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

### Q4. Crate API shape — **Resolved 2026-06-16: candidate signatures accepted as the binding public surface** (locked at the Phase 2B planning gate Decisions row above)

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

- [x] Per-session nonce on every mutating POST (`/save`, `/factory-reset`) (locked by `nonce_mismatch_returns_403_without_invoking_parse_form` + `nonce_compare_is_length_independent_constant_time` in `portal.rs`).
- [x] No secret pre-fill — `wifi_pass` and `mqtt_pass` never rendered into HTML, re-entered every submission (locked by `wifi_mqtt_template_carries_no_password_placeholder` in `provisioning-pure::templates` + `render_with_loaded_config_omits_password_bytes` in `portal.rs`).
- [x] Request-body cap (2 KB, matching `DEFAULT_REQUEST_SIZE_CAP == 2048`), oversized bodies rejected (locked by `post_save_body_exceeding_max_body_len_returns_413` in `portal.rs` + `const _: () = assert!(DEFAULT_REQUEST_SIZE_CAP == 2048)` static assert in `portal.rs`).
- [x] Lengths-only logging of credential material (locked by `logged_output_carries_no_credential_bytes` in `portal.rs`; grep recipe `just check-no-credential-logging` proposed for Batch F).
- [x] `Cache-Control: no-store` on portal responses (locked by `every_built_response_carries_cache_control_no_store` in `portal.rs`).
- [x] HTML / JSON escaping of rendered values (locked by `html_escape_covers_all_five_significant_chars` in `provisioning-pure::html_json_escape` + `render_template_escapes_every_substituted_field` in `portal.rs`).
- [x] Library never reboots and never erases on its own — host decisions via the `Committed` / `FactoryResetRequested` events (locked by `library_does_not_call_esp_hal_reset` + `portal_handlers_do_not_call_erase_all` in `tests/library_invariants.rs`; grep recipe `just check-library-never-reboots` proposed for Batch F).
- [x] Commit-guard ordering on the store (CRC written last, covering magic + version + discriminator + TLVs) and an open-AP warning when no AP password is set (locked by `crc_is_the_final_word_of_every_encoded_record` + `torn_write_with_valid_header_but_no_crc_decodes_as_none` in `record.rs` + `open_ap_emits_warning` in `session.rs`).
- [x] No reflection of submitted credential values in validation errors — the `ValidationError` variants carry lengths and expectations (`TooLong { max }`, `InvalidHex { expected_len }`), never input bytes, by construction; locked by `validation_errors_carry_no_input_bytes` in `provisioning-pure::error` and the `_exhaustiveness_lock` inner fn so a future variant without coverage fails compilation.
- [x] Credential-holding buffers dropped as early as practical after commit or failure — honest scope: guaranteed zeroization (e.g. the `zeroize` crate) is out of scope for v1, since Rust drop semantics do not scrub memory; an early drop bounds the window but adding `zeroize` is a future hardening decision, not a promise this checklist makes (locked by `req_buf_overwritten_between_requests` in `portal.rs`).

## State

- [x] Design drafted (ADR 015 proposed) — 2026-06-12
- [x] ADR 015 accepted — 2026-06-16 (de-facto by Phases 0–2A landing against ADR 015 plus the Q1+Q4 planning-gate sign-off; no separate ADR-status edit required by convention — the State checklist is the sign-off anchor)
- [x] Open-question proposals signed off — 2026-06-16 (Q2/Q3/Q5/Q6 at the 2026-06-12 planning pass; Q1 and Q4 at the Phase 2B planning gate via `/feature` update dialog)
- [x] Phase 0 — SoftAP (AP-mode) in `rustyfarian-esp-hal-wifi` + AP example — landed 2026-06-15 (squashed into commit `67cc65d` on rebase); `hal_c3_ap_smoke` builds and runs on ESP32-C3
- [x] Phase 1 — store module (host-tested) — landed 2026-06-15 (squashed into commit `67cc65d` on rebase); 38 host tests across `record.rs` + `store.rs`; subsequent fixup passes on 2026-06-15 / 2026-06-16 tightened decode bounds-checks, locked the on-flash layout (`record_len:u16` insertion), and closed UTF-8 / duplicate-tag / missing-required-field gaps
- [x] Phase 2 — portal (substrate hardening + promotion + templates) — substrate spike gate cleared 2026-06-15 with the §3 hand-rolled fallback (DHCP + DNS catch-all + HTTP server behind the `provisioning-spike` Cargo feature in `rustyfarian-esp-hal-wifi`); **Phase 2A pre-promotion hardening landed 2026-06-16 (PR #72, commits `5d7ec70` + `c88a1b3`)** — DHCP REQUEST lift to `decide_request`, RFC 2131 §4.3.2 lock-down tests, `SOCKET_BUF_LEN` / `SUBSTRATE_SOCKET_COUNT` / `REQUEST_BODY_DRAIN_DEADLINE_MS` constants, HTTP minimal-500 fallback + body-drain, security-marker plant, stack-footprint docs; **Phase 2B portal logic + template wiring + security-checklist conformance + promotion into `rustyfarian-esp-hal-provisioning` landed (pending Phase 2B commit)**
- [x] Phase 3 — examples + build routing + justfile — `hal_c3_provision_mqtt` and `hal_c6_provision_mqtt` examples build clean on both chip targets (pending Phase 2B commit)
- [x] Phase 4 — docs / CHANGELOG / ROADMAP + CLAUDE-lore correction note — Phase 2B landing (2026-06-16)
- [x] Verification gates green — `just fmt` / `just verify` / `just check-provisioning-hal-embassy` / `just build-example hal_c3_provision_mqtt` (Phase 0–3 landed, Phase 4 in progress)
- [ ] On-hardware captive-portal smoke test (ESP32-C3, phone browser) — trailing manual validation

## Session Log

Newest entries at the bottom.
The Decisions table, Constraints list, and Open Questions are the durable contract — this log records milestones and the reasoning behind them, not the full delta of every pass.
For prior-art detail see git log (commit refs below), `docs/project-lore.md` "esp-hal April 2026 Stack" for non-obvious lessons, and ADR 015 for the architectural rationale.

- 2026-06-12 — **Feature doc created** from `rustyfarian-rgb-clock` (downstream moving onto esp-hal stack).
  Maintainer elevated the two load-bearing technology choices (captive-portal substrate, flash credential store) to ADR-level decisions with full comparison tables.
- 2026-06-12 — **Two maintainer review passes** adopted 20 findings combined.
  Highlights: planning-level details relocated from ADR 015 sub-decisions into "Locked at planning pass" Decisions rows; substrate decision softened to "edge-net preferred, hand-rolled fallback proceeds without new ADR" (the locked architecture is the private-substrate boundary, not the crate family); Design reframed as candidate signatures, not API commitment; `ProvisioningOutcome { Committed, FactoryResetRequested, HostAborted }` sketched with both `wait_outcome` and `wait_committed` terminators; two secret-handling lines added to the security contract.
- 2026-06-15 — **Phase 0 (SoftAP) + Phase 2 substrate spike** land (work later squashed into commit `67cc65d` during the 2026-06-16 rebase).
  Took the ADR 015 §3 hand-rolled fallback: three private modules (`dhcp.rs`, `dns_catchall.rs`, `http_server.rs`) behind a `provisioning-spike` Cargo feature, validated end-to-end on a real phone.
  Two non-obvious lessons recorded in `docs/project-lore.md` "esp-hal April 2026 Stack" (workspace-invariant constants bound at construction; `StaticCell<[u8; N]>` requires compile-time `N`).
- 2026-06-15 — **Phase 1 (store module)** lands across kickoff + 3 PR review passes (work later squashed into commit `67cc65d` during the 2026-06-16 rebase); 38 host tests.
  Pre-locked: `MAGIC = "RFPR"`, `LAYOUT_VER = 1`, TLV tag table, `StoreError` variants carrying lengths-and-expectations only (security-contract obligation).
  `Flash` variant carries no `F::Error` payload — avoids a viral `where F::Error: Debug` bound across every `NorFlash` impl in the dep graph.
  On-flash layout refined mid-implementation: `record_len:u16` inserted at offset 9 to disambiguate real TLVs from `0xFF` flash padding (decoder otherwise could not bound the CRC read).
  Two `_exhaustiveness_lock` tests (`store_errors_carry_no_input_bytes`, `validation_errors_carry_no_input_bytes`) close the security contract against silent drift.
  Subsequent PR passes added: required-field gate (`MissingRequiredField`), `DuplicateTag` reject, `InvalidUtf8 { tag }` fail-closed on non-UTF-8, `OffsetOutOfBounds`, `ExtrasNotSupported`, `plan_save` read-coalescing, `# Stack usage` doc heading.
  Parallel finding (out of scope): `rustyfarian-esp-idf-provisioning::store` derives `Debug` over plaintext credentials; the bare-metal store closes the gap by construction, IDF tier needs a separate fix (tracked in the roadmap Near-term band).
- 2026-06-16 — **Spike-server pass-2** (pre-Phase 2A).
  Fixed the DHCP /24-boundary validation hole (lifted to pure `validate_pool_geometry` with host tests); cleaned 4 stale comments; cleared 9 clippy warnings that workspace `just verify` missed because `provisioning-spike` is a non-default Cargo feature.
  Phase 2A hardening list captured for later (the five items the next pass actually delivered).
- 2026-06-16 — **Phase 2A — pre-promotion hardening** of spike substrate lands across 2 commits (PR #72: `5d7ec70` initial hardening + review fixups, `c88a1b3` pass-2 polish); 58 → 67 tests.
  DHCP REQUEST decision lift to pure `decide_request(&DhcpMessage, &mut LeaseTable, server_ip, now_secs) -> RequestOutcome { Ack, Nak, Drop, Ignore }` with 7 RFC 2131 §4.3.2 host tests including the `decide_request_nak_still_records_mac_lease` lock-down (the `# Mutation note` documents Nak-still-mutates as "preserved, not endorsed" pending Phase 2B probe/commit).
  Redundant `LeaseTable::confirm` folded away; DHCP `rx_pkt` 548 → 1024 B.
  HTTP `write_minimal_500` deterministic fallback (no more zero-byte responses) + `drain_body_with_deadline` between flush and close (RST avoidance) + `// SECURITY: never log request bodies` marker at routing dispatch + `# Stack usage` doc on `run`.
  Three named constants promoted: `SOCKET_BUF_LEN = 1024`, `SUBSTRATE_SOCKET_COUNT = 4`, `REQUEST_BODY_DRAIN_DEADLINE_MS = 500` — now locked at the feature-doc level (Decisions table).
  Two reviewer passes drove the constant naming and the Nak-mutation doc strengthening.
  Deferred to Phase 2B: `ProvisioningStore::save` peak-stack reduction (12-byte-prefix targeted read); decouple `LeaseTable::allocate` candidate-lookup from commit; lift DHCP DECLINE arm; async-run-loop coverage for DNS/HTTP; shared UDP clamp helper; credential-safe request-logger.
- 2026-06-16 — **Phase 2B planning gate** via `/feature` update dialog (doc-only).
  Q1 (shared templates in `provisioning-pure` as `include_str!` consts, both profiles) and Q4 (candidate builder/session signatures as binding public surface) locked as Decisions rows; ADR 015 marked accepted (de-facto, by the State-checklist-as-sign-off-anchor convention); security-checklist item 9 ticked (locked by `validation_errors_carry_no_input_bytes`).
  All 6 Open Questions are now resolved; the Phase 2B implementation contract is closed at the feature-doc level.
- 2026-06-16 — **Session log condensed** (commit `5e88281` "Boy Scout Pass"); 444 → 367 lines, 13 entries → 7.
  Pre-condensation detail lives in the squashed source commits (`67cc65d`, `5d7ec70`, `c88a1b3`), `docs/project-lore.md` "esp-hal April 2026 Stack", and the Decisions / Constraints / Open Questions tables above — which are the durable contract this log indexes against.
- 2026-06-16 — **Phase 2B landing** — Batch G documentation pass.
  Promotion of `rustyfarian-esp-hal-provisioning` v0.1.0 with all 10 security-checklist items locked by named host tests (118 unit + 2 library invariant tests pass on the host toolchain).
  Portal HTML templates moved to `provisioning-pure::templates`, end-to-end source-of-truth for both tiers.
  Key compromises vs the planned Q4 surface: `PortalStore` trait + `Box::leak` for 'static binding; `ap_ip` returns `[u8;4]` not `Ipv4Address` (cross-HAL compat); MAC zeroed in `ClientConnected` event pending field-name confirmation from event-info API; `SpawnFailed` variant unreachable per embassy 0.10 spec (no runtime error path).
  CHANGELOG entries added; ROADMAP Ready band retirement; `CLAUDE.md` Common Resolution Failures row corrected (TX-power workaround is inline `extern "C" esp_wifi_set_max_tx_power` clamp, not vendor patch); AGENTS.md architecture table verified.
