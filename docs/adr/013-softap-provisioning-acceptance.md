# ADR 013: SoftAP Provisioning Acceptance

## Status

Accepted; amended by [ADR 014](014-wifi-mqtt-provisioning-profile.md) (2026-06-12), which generalises the original four-field schema described below into a closed set of named `SchemaProfile`s.
The four-field framing in this document is historical and reflects the v1 acceptance decision unchanged; the runtime contract is the two-profile schema documented in ADR 014.

## Context

The `rustyfarian-beekeeper` project's Milestone 5 (Field Provisioning) needs a
captive-portal SoftAP provisioning flow so beekeepers can configure devices in
the field without a build toolchain.
A feature request was filed asking that the capability live in `rustyfarian-network` as a reusable crate
pair, rather than be re-implemented per downstream firmware.

The request collides with two existing positions in this workspace:

1. `VISION.md` lists "Provisioning / SoftAP mode — no captive portal, BLE
   provisioning, or Wi-Fi setup flows" as a non-goal.
2. The README (v0.2.1, 2026-05-06) restates the same out-of-scope wording.

Both positions predate the request and predate the precedent set by ADR 011,
which accepted OTA — itself originally a non-goal — into this workspace on the
grounds that OTA's runtime dependency surface is dominated by Wi-Fi plumbing
already provided here.
Provisioning has the same property: it is a SoftAP lifecycle managed by
`rustyfarian-esp-idf-wifi` plus an HTTP form, an NVS writing, and a state machine —
all of which fit the established `*-pure` + `rustyfarian-esp-idf-*` triad.

Four decisions need to be locked before the implementation work begins:

1. **Whether to accept the scope expansion at all**, given the explicit non-goal
   in `VISION.md`, or to decline and point the requester at one of the
   alternatives listed in the feature request (in-beekeeper module; separate
   `rustyfarian-provisioning` repo).
2. **Which HAL tiers the provisioning crates target** — IDF-only versus the full
   dual-HAL triad established by ADR 005.
3. **Whether the captive-portal HTTP server is internal-only or a reusable
   workspace surface**, with direct implications for the README's HTTP scope.
4. **Which provisionable values the API supports** — Wi-Fi only, the full
   beekeeper set (Wi-Fi credentials + LoRaWAN OTAA keys + OTA server URL +
   device name), or a generic host-defined schema.

A fifth scope question — **BLE provisioning alongside SoftAP** — is answered by
this ADR's framing: BLE remains a non-goal.
SoftAP is what beekeeper needs; BLE has no requesting downstream.

## Decision

### 1. SoftAP provisioning is accepted as a workspace concern, deferred to Long-term

`rustyfarian-network` formally accepts provisioning as a domain it owns,
overriding the current `VISION.md` non-goal and the README's out-of-scope line
on provisioning.
The acceptance follows the same workspace-cohesion argument as ADR 011: the
runtime dependency surface (Wi-Fi association, SoftAP lifecycle, NVS) is
already provided here, and every Rustyfarian field device will need
provisioning eventually.

Scheduling is **Long-term**, not Near or Midterm.
Implementation does not preempt the currently blocked Phase 5 TTN v3 OTAA
validation, the active `esp-hal` Wi-Fi async polish, or the open MQTT
subscribe-during-shutdown race fix.
The roadmap entry sits below those workstreams and is picked up when capacity
opens or when the beekeeper Milestone 5 timeline forces the issue —
whichever comes first.

The alternatives identified in the feature request are rejected:

- **Build a `provisioning` module inside `rustyfarian-beekeeper`** — violates
  the "drivers live in shared crates" boundary that all four existing triads
  (`wifi`, `lora`, `espnow`, `ota`) honour.
- **Create a dedicated `rustyfarian-provisioning` repo** — duplicates Wi-Fi
  pinning and Cargo workspace setup for a feature whose primary collaborator is
  `rustyfarian-esp-idf-wifi`; same critique that drove ADR 011.

### 2. The provisioning triad is IDF-only at acceptance time

Two crates are added to `crates/` when the work is picked up:

- `provisioning-pure` — `no_std`, platform-independent form parsing,
  credential validation, and provisioning state machine; host-testable.
- `rustyfarian-esp-idf-provisioning` — thin ESP-IDF wrapper for SoftAP
  lifecycle, captive portal HTTP form, and NVS credential persistence.

A bare-metal `rustyfarian-esp-hal-provisioning` is **not** added at acceptance
time.
The dual-HAL convention from ADR 005 is honoured by reserving the name and
keeping the `*-pure` crate `no_std` so it can be reused by a future esp-hal
triad without an API break.
The bare-metal crate becomes plausible only when downstream firmware on the
bare-metal stack surfaces a concrete need — the same gating criterion the rest of
the workspace uses.

### 3. The captive-portal HTTP server is internal to `rustyfarian-esp-idf-provisioning`

The HTTP server that backs the captive portal is a private implementation
detail of `rustyfarian-esp-idf-provisioning`.
It is not exported, not re-exported, and not advertised as a reusable
workspace surface.
This mirrors ADR 011's treatment of the bare-metal OTA HTTP client: private
transport scoped to one crate, not a workspace HTTP API.

The README's stance — "general-purpose application-layer clients (HTTP, CoAP,
WebSocket) are out of scope" — is preserved.
The README's provisioning non-goal is the one that changes; the HTTP non-goal
does not.

`esp-idf-svc::http::server::EspHttpServer` is the expected backing
implementation, scoped behind a `pub(crate)` module.
A `/status` JSON endpoint is exposed only for the duration of the provisioning
session; it is not promoted to a general device-diagnostics API.

### 4. The provisionable schema is the beekeeper full set

The provisioning API supports four field categories at acceptance time:

- Wi-Fi station credentials (SSID and password).
- LoRaWAN OTAA credentials (DevEUI, JoinEUI/AppEUI, AppKey).
- OTA server URL.
- Device name (human-readable label, used for the SoftAP SSID and logs).

These are the fields the requesting downstream (`rustyfarian-beekeeper`) needs;
they are also the union of what every Rustyfarian field-deployable workspace
crate stores in NVS today.
The "Wi-Fi credentials-only" alternative was rejected because it would force
beekeeper — the requester — to build half of the captive portal it asked for
on top of a workspace-provided base, splitting the form across two codebases
for no architectural gain.

The "generic host-defined schema" alternative was rejected because the
form-validation logic is the load-bearing piece of `provisioning-pure`;
extracting it to host applications scatters validation rules across every
downstream and turns the pure crate into a CRUD shell.

A future host application that needs additional fields adds them as a
host-defined extension to the form (the captive portal accepts opaque
`name=value` pairs alongside the known set); the four canonical fields above
have first-class validation and typed accessors in `provisioning-pure`.
A schema extension mechanism is a feature-doc concern when the work is picked
up, not an ADR concern now.

### 5. BLE provisioning remains out of scope

The `VISION.md` non-goal "BLE provisioning" survives this ADR.
No downstream has asked for BLE provisioning; the ESP-IDF BLE stack
(`esp-idf-svc::bt`, NimBLE) would be a substantial new dependency surface; and
the SoftAP path solves the same field-configuration problem with hardware that
every Rustyfarian device already uses.
If a future downstream surfaces a concrete BLE-provisioning need, a separate
ADR revisits the same gating used elsewhere in the workspace.

## Rationale

### On accepting a non-goal

`VISION.md` is a living document, not a treaty.
Three of its non-goals have been promoted to active goals on the same gating
criterion: real downstream firmware surfaces a concrete need that the
workspace can meet without duplicating its own foundations.
`esp-hal` LoRa was promoted by ADR 004 (`rustyfarian-beekeeper` LoRa
exploration); `esp-hal` Wi-Fi was promoted in the 2026-03-12 vision update
(LoRa-blocked-on-hardware redirect); OTA was promoted by ADR 011 (the
`rustyfarian-ferriswheel-demo` OTA MVP).
This ADR follows the same pattern for the same reason: the requesting
downstream has a published roadmap milestone, the technical fit is clean, and
declining would push the implementation into a worse architectural location
(in-application module or a sibling repo that duplicates Wi-Fi pinning).

Deferring to Long-term, rather than Midterm, honours a separate principle:
the workspace prefers to finish the workstreams it has started before opening
new ones.
Phase 5 TTN validation, the bare-metal Wi-Fi polish, and the MQTT
subscribe-race fix are all in flight; adding provisioning to Near or Midterm
would dilute attention without changing the beekeeper timeline materially.

### On IDF-only at acceptance time

ADR 005 commits the workspace to a dual-HAL pattern, but the same ADR
explicitly notes that the pattern is realised "per peripheral, when a downstream
needs each side".
LoRa lived as IDF-only for months before `rustyfarian-esp-hal-lora` was added;
ESP-NOW today is IDF-only and has no requesting bare-metal downstream.
Reserving the `rustyfarian-esp-hal-provisioning` name without writing the crate
keeps the convention honest and avoids speculative `no_std` HTTP server work
that nobody has asked for.

### On private HTTP

The same argument ADR 011 made for OTA applies verbatim: the README's
"HTTP out of scope" line is a feature, not an oversight.
A captive-portal HTTP server is materially the same kind of transport as the
OTA HTTP client — narrow, single-purpose, scoped to one crate's lifecycle.
Promoting it to a workspace API would either silently contradict the README or
force a scope expansion the workspace does not benefit from.

If a third internal HTTP user appears later (a second downstream-driven
feature inside this workspace), the extraction case becomes worth making at
that point.
Two private clients are acceptable; three are a signal to consolidate.

### On the four-field schema

The beekeeper feature request enumerates the exact four fields the downstream
needs: Wi-Fi credentials, LoRaWAN OTAA keys, OTA server URL, and device name.
Three of those four (Wi-Fi, LoRaWAN, OTA URL) are also stored in every other
Rustyfarian-style field device the maintainer has built or is planning.
Treating them as first-class in `provisioning-pure` lets the pure crate own
the validation logic that downstream firmware would otherwise re-implement
field by field.

The generic key-value alternative was tempting precisely because it sounds
flexible, but the flexibility is a tax: every host application would write the
same DevEUI hex parser, the same SSID length check, the same AppKey
length-and-hex validator.
Centralising those rules is what `*-pure` crates are for.

## Consequences

### Positive

- **Single workspace remains the answer to "where does Rustyfarian
  connectivity live"** — provisioning joins the four existing triads
  (`wifi`, `lora`, `espnow`, `ota`) without a new sibling repo.
- **`provisioning-pure` becomes a forcing function for credential validation
  hygiene** — DevEUI, AppKey, SSID, and OTA URL parsing/validation gets one
  authoritative implementation with host tests, rather than four
  re-implementations across downstream firmware.
- **HTTP scope stays honest** — the README's general-purpose HTTP out-of-scope
  line survives; only the provisioning line changes.
- **Beekeeper Milestone 5 unblocks cleanly when capacity opens** — the
  requesting downstream gets the architectural surface it asked for, in the
  shape it asked for, without having to host the implementation itself.

### Negative

- **`rustyfarian-network` scope grows further** — the workspace now covers
  Wi-Fi, MQTT, LoRa, ESP-NOW, OTA, and provisioning.
  `VISION.md` and the README must be updated to reflect this when the ADR
  lands; both files still describe the workspace as Wi-Fi/MQTT-first.
- **A Long-term commitment now exists with a downstream timeline attached** —
  beekeeper Milestone 5 will eventually push on this; if the rest of the
  Long-term roadmap shifts, the beekeeper schedule reacts.
- **The four-field schema is opinionated** — host applications that need
  fundamentally different provisioning data (no LoRaWAN, no OTA) carry unused
  surface in their NVS layout, or layer their own provisioning UI alongside
  rather than on top.
  Acceptable because every concrete Rustyfarian downstream in flight today
  uses the full set.

### Implications

- A roadmap entry is added under **Long term** in `docs/ROADMAP.md`:
  "Provisioning triad (`provisioning-pure` + `rustyfarian-esp-idf-provisioning`)
  — SoftAP captive portal, NVS credentials, four-field schema (ADR 013)".
  Existing Long-term entries are not reordered; provisioning sits below them.
- `VISION.md` is updated when the ADR lands: the provisioning non-goal is
  removed, and a long-term-goal bullet is added in its place, mirroring the
  framing OTA received from ADR 011.
  The BLE-provisioning portion of the non-goal is preserved as its own bullet.
- The README's "Out of scope" paragraph is updated to remove the
  provisioning/SoftAP clause and to retain the HTTP/CoAP/WebSocket clause
  unchanged.
- The bare-metal name `rustyfarian-esp-hal-provisioning` is reserved.
  No code, no `Cargo.toml` entry, no roadmap line until a bare-metal downstream
  asks for it.

## References

- `docs/features/softap-provisioning-v1.md` — feature doc carrying the request from
  `rustyfarian-beekeeper`.
- ADR 004 — `esp-hal` LoRa tier accepted (precedent for promoting a non-goal
  on downstream demand).
- ADR 005 — Crate naming convention for dual-HAL drivers.
- ADR 011 — OTA crate hosting and transport decisions (precedent for accepting
  a non-goal and keeping a private HTTP surface).
- `VISION.md` — non-goals updated when this ADR lands.
- `docs/ROADMAP.md` — Long-term entry added when this ADR lands.
