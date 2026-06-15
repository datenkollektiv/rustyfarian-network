# ADR 015: Bare-Metal (esp-hal) Provisioning Crate

## Status

Proposed — awaiting maintainer sign-off.

The maintainer explicitly requested an ADR-level evaluation of the two load-bearing technology choices — the captive-portal network substrate and the flash credential store — rather than a gut-feel pick.
This ADR recommends; the maintainer decides.

The canonical sign-off anchor is the State checklist in the feature doc ([esp-hal-provisioning-v1.md](../features/esp-hal-provisioning-v1.md)), which records acceptance.
No issue tracker exists in this workspace by convention.

## Context

A new bare-metal downstream needs SoftAP captive-portal provisioning: `rustyfarian-rgb-clock` is moving onto the esp-hal stack.
It is the same downstream that drove [ADR 014](014-wifi-mqtt-provisioning-profile.md)'s `WifiMqttDevice` profile on the ESP-IDF tier, and it now wants the same field-configuration capability on bare-metal so a field operator can set Wi-Fi credentials, an MQTT broker, an OTA URL, and a device name with no toolchain present.

[ADR 013 §2](013-softap-provisioning-acceptance.md) reserved the name `rustyfarian-esp-hal-provisioning` but explicitly declined to build it at acceptance time.
Its gate was the workspace's standard one: the bare-metal crate "becomes plausible only when downstream firmware on the bare-metal stack surfaces a concrete need".
`rustyfarian-rgb-clock`'s bare-metal milestone is that need, so this ADR lifts the ADR 013 §2 deferral.

The bare-metal estate the crate joins is materially different from the ESP-IDF estate the existing triad was built against, and that difference is what makes this a real ADR rather than a port:

- `rustyfarian-esp-hal-wifi` is async-only.
  `esp-radio 0.18` removed the synchronous controller and direct `smoltcp` integration, so the crate exposes a single async entry point — `WiFiManager::init_async` returning an `AsyncWifiHandle { controller, stack, runner }` wired into an `embassy-net` stack — and the `compile_error!` guard in `rustyfarian-esp-hal-wifi` rejects any chip feature enabled without `embassy`.
  It is STA-only today: it configures `esp_radio::wifi::sta::StationConfig` and has no AP path.
- The ESP-IDF tier got three things for free from ESP-IDF that bare-metal does not have: an HTTP server (`EspHttpServer`), a DHCP server (the SoftAP DHCPS), and an NVS key-value store.
  `embassy-net` provides a TCP/UDP socket layer and a DHCP *client*, but no HTTP server, no DHCP *server*, and no key-value store.
- The only TCP code anywhere in the bare-metal estate is `rustyfarian-esp-hal-ota`'s hand-rolled, strict, no-alloc HTTP/1.1 GET *client* over `embassy_net::tcp::TcpSocket` (`crates/rustyfarian-esp-hal-ota/src/http.rs`).
  There is no UDP code anywhere bare-metal, and no bare-metal MQTT at all.
- There is no config-persistence facility on bare-metal.
  `esp-storage` is pulled in only for OTA partition access; examples carry build-time configuration via `option_env!`.

So a bare-metal provisioning portal has to supply, from the Rust ecosystem, three protocols the ESP-IDF tier never had to think about (HTTP server, DHCP server, captive-portal DNS) plus a flash credential store — none of which has an in-workspace precedent beyond a single hand-rolled HTTP *client*.
Those are the two genuinely open technology questions, and the maintainer asked for both to be evaluated at ADR level.

A further honest caveat shapes the scope: bare-metal MQTT does not exist yet.
The ROADMAP gates `rustyfarian-esp-hal-mqtt` (minimq-based) behind a future async-MQTT decision ADR (`docs/ROADMAP.md`, Long term).
A `WifiMqttDevice` provisioning crate on bare-metal therefore provisions credentials that its consumer will only be able to use later; `rustyfarian-rgb-clock`'s bare-metal milestone needs both this crate and that future MQTT crate.
This ADR states that dependency plainly rather than pretending the consumer already exists.

Five decisions need to be locked before implementation begins.
This ADR holds the durable architecture; the planning-level figures those decisions imply are locked in the feature doc ([esp-hal-provisioning-v1.md](../features/esp-hal-provisioning-v1.md)) as "Locked at planning pass" Decisions-table rows, and the implementation-level open questions live there too.

## Decision

### 1. `rustyfarian-esp-hal-provisioning` is accepted; the ADR 013 §2 deferral gate is lifted

The reserved bare-metal crate is built.
`rustyfarian-rgb-clock` moving onto the esp-hal stack is the concrete bare-metal downstream ADR 013 §2 was waiting for, and it is the same downstream that drove ADR 014's profile on the ESP-IDF tier.

Scope for v1:

- The `WifiMqttDevice` profile only — Core + MQTT + OTA + device name, the profile `rustyfarian-rgb-clock` needs.
- ESP32-C3 and ESP32-C6, the two chips the bare-metal estate is hardware-validated on.
- Embassy / async only, matching the `rustyfarian-esp-hal-wifi` estate the crate joins — there is no sync path to support.

The `LorawanFieldDevice` profile is deferred until a bare-metal LoRaWAN provisioning consumer exists, mirroring ADR 013 §2's own reserved-but-not-built pattern: `provisioning-pure` already carries both profiles `no_std`-safely (`crates/provisioning-pure/src/profile.rs`), so the second profile is a later feature-doc concern, not an API break.

Honest caveat carried forward from Context: bare-metal MQTT does not exist yet (ROADMAP gates `rustyfarian-esp-hal-mqtt` behind a future async-MQTT ADR), so v1 provisions credentials that its consumer will use later.

Rejected alternatives:

- **Decline and keep the name reserved** — the ADR 013 §2 gate is now met; declining would push the implementation into `rustyfarian-rgb-clock` itself, violating the "drivers live in shared crates" boundary that every existing triad honours (the same critique ADR 013 §1 made of the in-application alternative).
- **Build both profiles in v1** — no bare-metal LoRaWAN provisioning consumer exists, so the LoRaWAN portal template and field wiring would be speculative; ADR 013 §2's reserved-not-built discipline applies in the other direction here.

### 2. SoftAP (AP-mode) lifecycle is added to `rustyfarian-esp-hal-wifi`, not the provisioning crate

The bare-metal AP-mode lifecycle lives in `rustyfarian-esp-hal-wifi`, alongside the existing STA path, exactly as the ESP-IDF `SoftApManager` lives in `rustyfarian-esp-idf-wifi` rather than in `rustyfarian-esp-idf-provisioning`.

This mirrors ADR 013's locked "no parallel Wi-Fi stack" position and the v1 `SoftApManager` precedent.
The provisioning crate consumes the Wi-Fi crate's AP handle; it does not own radio lifecycle.
The reusable `wifi-pure` `ApConfig` / `validate_ap_config` / `TxPowerLevel` surface is shared unchanged across both tiers — the AP path is already pure-validated and host-tested, so the bare-metal addition is a binding, not new validation logic.

The workspace pins `esp-radio =0.18.0`, and the AP path commits to that version surface.
The `1.0.0-beta.0` release (2026-06-03) reshaped the AP/interfaces API, so the implementation must target the pinned `=0.18.0` surface.
The feature doc enumerates the exact API symbols and documents the beta trap so a future esp-radio bump does not silently break the AP path.

Rejected alternative:

- **Own AP lifecycle inside `rustyfarian-esp-hal-provisioning`** — produces a second Wi-Fi stack, the exact thing ADR 013 locked against; it would also duplicate the TX-power clamp and the AP-event subscription that already belong in the Wi-Fi crate.

### 3. Captive-portal network substrate: edge-net is the preferred first implementation candidate

The captive-portal substrate is a PRIVATE implementation detail behind `pub(crate)` wrappers — no substrate type appears in the crate's public API — following the ADR 011 / ADR 013 private-transport pattern (the OTA HTTP client and the IDF portal HTTP server are both private to their crates).
The architectural commitment is that private-substrate boundary, not any particular crate family.

The portal needs three protocols bare-metal does not provide: an HTTP server, a DHCP *server*, and a captive-portal DNS catch-all.
The `edge-net` family by ivmarkov — one version-consistent no-alloc stack covering all three (`edge-http`, `edge-dhcp`, `edge-captive` over `edge-nal-embassy`) — is the preferred first implementation candidate, to be validated by the Phase 2 integration spike before being treated as final.
The feature doc pins the exact spike-target versions of the family as a "Locked at planning pass" row.

Fallback rule: if the spike fails on executor fit, single-client ergonomics, or binary size, the documented runner-up (all-hand-rolled) proceeds WITHOUT a new ADR — because the architectural commitment is the private-substrate boundary, not the crate family.

| Option                                                                        | New deps                                                                                    | Covers                                                     | heapless                                                           | Code to own                                    | Verdict                               |
|:------------------------------------------------------------------------------|:--------------------------------------------------------------------------------------------|:-----------------------------------------------------------|:-------------------------------------------------------------------|:-----------------------------------------------|:--------------------------------------|
| **edge-net family** (edge-http + edge-dhcp + edge-captive + edge-nal-embassy) | ~6 (`edge-http`, `edge-dhcp`, `edge-captive`, `edge-nal`, `edge-nal-embassy`, `domain`)     | HTTP server + DHCP server + DNS catch-all — all three      | 0.9 (already in lock via the workspace pin)                        | thin `pub(crate)` wiring                       | **Preferred first candidate**         |
| **picoserve**                                                                 | `picoserve` + `serde` + `thiserror 2` + `pin-project` + `picoserve_derive` + `heapless 0.8` | HTTP **only** — DHCP server and DNS still needed elsewhere | 0.8 (coexists with workspace 0.9, already in lock via embassy-net) | DHCP server + DNS catch-all on top             | Rejected — partial coverage           |
| **All hand-rolled**                                                           | 0                                                                                           | nothing pulled in; three protocols hand-written            | n/a                                                                | HTTP server + DHCP server + DNS (~3 protocols) | Documented runner-up — review surface |

Key arguments for the recommendation:

- **One family covers all three protocols**, including the DHCP server `embassy-net` lacks — `embassy-net` ships only a DHCP client.
  `edge-dhcp 0.7` is a real no-alloc DHCP server (`Server<F, N>` lease database, `ServerOptions`, `io::server::run` over the `edge-nal` UDP abstraction), and its own `server.rs` documents that the RFC 2131 §4.1 broadcast-destination handling works over plain UDP sockets — no raw sockets are needed, so `embassy-net`'s `udp` feature suffices.
- **Version-consistent and no-alloc** — the four crates move together on one release cadence, all `no_std`, `heapless 0.9` (matching the workspace pin), `embassy-sync 0.7` / `embassy-time 0.5` / `embedded-io-async 0.7`.
  Cargo.lock already carries `embedded-io-async 0.7.0` and `embassy-sync 0.7.2` transitively via `embassy-net 0.8`, so the family is not version-blocked; both majors coexist at a binary-size cost only.
- **Maintained by the esp-rs ecosystem author** — ivmarkov is the author of `esp-idf-svc` / `esp-idf-hal`, so the family is built by someone who knows the ESP toolchain intimately.

Honest negatives, recorded so the maintainer weighs them:

- **New supply-chain surface** — roughly six crates enter the dependency graph (`cargo deny` review applies at acceptance).
- **Single-client HTTP** — `edge-http`'s server has historically served one client at a time; acceptable for a captive portal, where one phone configures one device.
- **Abstraction layering** — `edge-nal` is an extra network-abstraction layer between the portal and `embassy-net`.

Rejected and runner-up alternatives at the decision level:

- **picoserve** — rejected on partial coverage: it solves HTTP only and leaves the two harder protocols (a DHCP server `embassy-net` lacks, and a DNS responder) unaddressed, so it does not shrink the problem.
- **All hand-rolled** — not rejected but kept as the documented runner-up the fallback rule promotes if the spike fails: zero new dependencies and full control, at the cost of the largest review surface (three protocols, including a DHCP server with RFC 2131 §4.1 destination logic, versus the workspace's single-HTTP-client precedent).

The feature doc carries the full per-option rationale.

### 4. Flash credential store: RECOMMEND a hand-rolled A/B double-buffered single-record store in a dedicated non-NVS partition

This ADR locks the durable storage contract: a simple single-record store — not a general key-value database — of the torn-write-safe A/B double-buffered class, living in a DEDICATED non-NVS partition, with a minimum-size guarantee validated when the store is opened, and NO cross-tier NVS interop (a tier switch re-provisions the device).
The record holds one TLV+CRC entry per sector, written A/B for torn-write safety, over the synchronous `embedded-storage 0.3` `NorFlash` trait that `esp-storage` implements directly.

| Option                                        | Trait fit                                                                                                   | Wear-leveling                                        | heapless               | Verdict                          |
|:----------------------------------------------|:------------------------------------------------------------------------------------------------------------|:-----------------------------------------------------|:-----------------------|:---------------------------------|
| **Hand-rolled A/B record store**              | sync `embedded-storage 0.3` — `esp-storage` implements it directly                                          | not needed (single-digit writes per device lifetime) | n/a (pure byte layout) | **Recommended**                  |
| **sequential-storage 7.2.0**                  | async `embedded-storage-async 0.4.1` — needs a `BlockingAsync` adapter over `esp-storage`'s sync `NorFlash` | yes (solves a non-problem here)                      | 0.8 + 0.9              | Rejected — async mismatch        |
| **ekv 1.0.0**                                 | own `Flash` trait; `embassy-sync 0.6`, `heapless 0.8`                                                       | LSM-tree (overkill for one record)                   | 0.8                    | Rejected — disproportionate      |
| **ESP-IDF NVS binary format from bare-metal** | no crate exists                                                                                             | n/a                                                  | n/a                    | Eliminated — no upstream support |

Key arguments for the recommendation:

- **Matches the repo's hand-rolled, host-tested style** — the record layout (encode / decode / CRC / A/B arbitration) is pure byte manipulation, lives as host-testable functions exactly like the OTA HTTP parser, and pulls in zero new dependencies beyond `esp-storage` (already present) and a CRC routine.
- **Wear-leveling is solving a non-problem here** — provisioning writes happen a single-digit number of times across a device's lifetime, and NOR flash endures ~100k erase cycles; an LSM tree or a wear-leveling KV store is machinery for a write pattern this store will never have.
- **Torn-write safety is cheap** — two sectors, a monotonic sequence counter, and a CRC32 over the payload: `load` picks the sector with the higher valid sequence number, and an interrupted write leaves the previous record intact in the other sector.

Rejected alternatives, in full:

- **sequential-storage 7.2.0** (tweedegolf, edition 2024) — the mainstream key-value-over-flash crate with wear-leveling, and it supports `heapless 0.8`/`0.9` with optional `postcard`.
  But it depends on `embedded-storage-async 0.4.1`, while `esp-storage` implements the *synchronous* `NorFlash` trait, so it needs a `BlockingAsync` adapter; and its headline feature (wear-leveling) addresses a write pattern this store does not have.
- **ekv 1.0.0** (embassy-rs) — an LSM-tree KV store with its own `Flash` trait, `embassy-sync 0.6`, `heapless 0.8`.
  Not eliminated, but disproportionate: a log-structured merge tree for one credential record is poorly matched to the problem.
- **ESP-IDF NVS binary format read/write from bare-metal** — eliminated.
  No crate implements it (`esp-idf-nvs-partition-gen` only *generates* images), and reimplementing the NVS page/entry/CRC layout by hand is high effort with no upstream support to lean on.
  The consequence is stated honestly under Consequences: a device provisioned on one firmware tier cannot have its credentials read by the other tier, so a cross-tier firmware swap is a re-provisioning event.

On the dedicated non-NVS partition: the standard ESP-IDF partition table places `nvs` at offset `0x9000` (24 KiB), and squatting that region with a foreign (non-NVS) format is fragile, so the store lives in a dedicated, documented data partition rather than reusing `nvs`.
This is why the contract above mandates a dedicated non-NVS partition: the NVS binary format cannot be reused (no crate implements it, and `esp-bootloader-esp-idf` 0.5.0 — already a workspace dependency via the OTA crates — exposes partition-table and app-descriptor handling only, with no NVS read/write API), so a foreign format in a foreign partition is the only honest option, and `espflash` / `esp-bootloader-esp-idf` can carry the custom partition table.

The planning-level figures these contract terms imply — the 8 KiB / two-4-KiB-sector geometry, the placement-after-OTA-data policy, the `open(flash, base_offset, total_bytes)` signature with a hard `Err` below 8192, and the `SchemaProfile::as_str`-bytes discriminator — are locked in the feature doc as "Locked at planning pass" Decisions-table rows, each carrying its own rejected-alternative rationale.

### 5. The v1/v2 security contract carries over as a behavioural conformance list, with two added secret-handling lines

The security and credential-hygiene contract locked across ADR 013 and ADR 014 carries over to the bare-metal portal unchanged: per-session nonce on every mutating POST, no secret pre-fill, a small request-body cap with oversized bodies rejected, lengths-only logging, `Cache-Control: no-store`, HTML/JSON escaping, library-never-reboots/erases, and commit-guard ordering with an open-AP warning.
The full conformance checklist with the reference-implementation pointers lives only in the feature doc; the ESP-IDF portal (`crates/rustyfarian-esp-idf-provisioning`) is the reference implementation for each item, and the feature-doc checklist is the conformance gate.

Two contract lines are added for the bare-metal portal:

- Validation errors must never reflect submitted credential values back to the client.
  The existing `ValidationError` variants carry lengths and expectations (`TooLong { max }`, `InvalidHex { expected_len }`), never input bytes, by construction — this becomes a stated contract with a test obligation, so a future variant cannot quietly start echoing input.
- Credential-holding buffers are dropped as early as practical after commit or failure.
  Honest scope note: guaranteed zeroization (for example the `zeroize` crate) is out of scope for v1 — Rust drop semantics do not scrub memory, so an early drop bounds the window but does not erase the bytes; adding `zeroize` is a future hardening decision recorded as such, not a silent promise this contract makes.

## Rationale

### On lifting the ADR 013 §2 gate now

ADR 013 §2 did not decline the bare-metal crate on principle — it deferred it on the workspace's standard gate: build the bare-metal side when a bare-metal downstream needs it.
`rustyfarian-rgb-clock` is that downstream, and it is the same one that justified ADR 014's profile, so the demand is concrete and already familiar to the workspace.
Lifting the gate now is the same move ADR 004 (esp-hal LoRa) and ADR 011 (OTA) made when their downstreams appeared; declining would only relocate the work into the application, the location ADR 013 §1 already rejected.

### On adding AP mode to the Wi-Fi crate

ADR 013 locked "no parallel Wi-Fi stack" for the ESP-IDF tier, and the `SoftApManager` lives in `rustyfarian-esp-idf-wifi` for exactly that reason.
The bare-metal estate has the same shape — one Wi-Fi crate owning radio lifecycle — so the AP path belongs next to the STA path, sharing the `wifi-pure` `ApConfig` validation that is already host-tested.
The provisioning crate is a consumer of a Wi-Fi handle, not a radio owner.

### On preferring edge-net as the first candidate

The preference turns on coverage, not on any single protocol.
picoserve is the better-known HTTP server, but a captive portal is not just an HTTP server — it is an HTTP server *and* a DHCP server *and* a DNS catch-all, and `embassy-net` supplies none of those three.
The only option that covers all three from one coherent, version-consistent, no-alloc source is the edge-net family, which is why it is the preferred first candidate, at the cost of a supply-chain surface (`cargo deny`-reviewable) and a single-client HTTP server that a captive portal never strains.
The preference is held lightly on purpose: the Phase 2 integration spike validates it against real executor fit, single-client ergonomics, and binary size, and on failure the hand-rolled runner-up proceeds per the §3 fallback rule without a new ADR — because the locked architecture is the private-substrate boundary, not the crate family.
Hand-rolling all three is the highest-control option, but the workspace's hand-rolled precedent is one HTTP client, and three protocols — including a DHCP server with RFC 2131 §4.1 destination logic — is a much larger correctness surface than the precedent justifies, which is why it is the fallback rather than the first pick.

### On the hand-rolled store over the KV crates

The store holds one record, written a handful of times per device, on NOR flash that tolerates ~100k cycles.
Every mainstream KV-over-flash crate is built for the opposite write pattern (frequent writes, wear-leveling), and the closest fit (`sequential-storage`) additionally needs a sync-to-async adapter because `esp-storage` implements the synchronous `NorFlash` trait.
A two-sector A/B record with a sequence counter and a CRC is the smallest thing that is torn-write-safe, it pulls in nothing new, and its layout is pure and host-testable — the same shape as the OTA HTTP parser the workspace already trusts.

### On declining NVS interop

Reading the ESP-IDF NVS binary format from bare-metal would let a device keep its credentials across a tier swap, but no crate implements that format and hand-rolling the page/entry/CRC layout is high-effort with no upstream to lean on.
`esp-bootloader-esp-idf` 0.5.0 — already a workspace dependency via the OTA crates — exposes partition-table and app-descriptor handling only and documents no NVS read/write API, so no existing dependency provides NVS-format access either.
The honest trade is to accept that the two tiers store credentials in different formats and that swapping firmware tiers on a provisioned device is a re-provisioning event — a rare operation that does not justify reimplementing NVS.

## Consequences

### Positive

- **`rustyfarian-rgb-clock` gets a bare-metal provisioning surface** in the shape it already knows from the ESP-IDF tier, without hosting the implementation itself.
- **`provisioning-pure` is reused unchanged** — the ADR 013 §2 promise that the pure crate stays `no_std` so a future bare-metal tier can adopt it without an API break is kept; `SchemaProfile`, `LoraFields`, `MqttFields`, `parse_form`, and the validators all carry over as-is.
- **The captive portal is covered by one coherent no-alloc family** if the edge-net spike succeeds, including the DHCP server `embassy-net` does not provide; and if it fails, the documented hand-rolled fallback proceeds without an ADR revision, so the substrate choice never blocks the crate.
- **The store is small, dependency-light, and host-testable**, matching the workspace's established hand-rolled-and-tested posture.

### Negative

- **The workspace grows a fourth `esp-hal` crate** — `rustyfarian-esp-hal-provisioning` joins `-wifi`, `-lora`, `-ota`.
- **The dependency surface grows by the edge-net family** (~6 crates) if the Phase 2 spike confirms the preferred candidate — a `cargo deny` review item at that point; the hand-rolled fallback adds none.
- **The flash-while-radio constraint shapes the commit flow** — flash writes with the radio active risk a cache-disabled crash (the `esp-idf#10079` precedent; single-core C3/C6 ROM flash routines disable interrupts).
  The mitigation is independent of the store crate choice: enable the `esp-storage` `critical-section` feature and write credentials only at a quiescent moment — after the portal commits, with the radio stopped or immediately before the host reboots.
  Provisioning is inherently configure-then-restart, so the natural commit flow is *write store → emit `Committed` → host reboots* with the radio already quiesced.
  This is captured as a Constraint in the feature doc.
- **No NVS interop across tiers** — a device provisioned on the ESP-IDF tier cannot have its credentials read by the bare-metal tier or vice versa; a cross-tier firmware swap re-provisions the device.

### Implications

These are follow-through items, actioned at acceptance, not in this ADR:

- A `docs/ROADMAP.md` entry for the bare-metal provisioning crate.
- `VISION.md` / `README.md` / `CHANGELOG.md` updates reflecting the fourth `esp-hal` crate and the bare-metal provisioning capability.
- The stale `vendor/esp-radio` row in the local `CLAUDE.md` *Common Resolution Failures* table should be corrected when this work lands — `vendor/esp-radio` does not exist; the TX-power workaround is the inline `extern "C" esp_wifi_set_max_tx_power` clamp in `rustyfarian-esp-hal-wifi`.
  This is noted here only; the correction is a developer-local edit, not part of this public ADR.

## References

- [ADR 013](013-softap-provisioning-acceptance.md) — SoftAP provisioning acceptance; reserves the `rustyfarian-esp-hal-provisioning` name (§2) and locks the no-parallel-Wi-Fi-stack and private-HTTP positions this ADR builds on.
- [ADR 014](014-wifi-mqtt-provisioning-profile.md) — the `WifiMqttDevice` profile and the `SchemaProfile` generalisation this crate provisions; same downstream (`rustyfarian-rgb-clock`).
- [ADR 011](011-ota-crate-hosting-and-transport.md) — OTA crate hosting and plain-transport precedent; the private hand-rolled HTTP *client* this ADR contrasts with a portal's three-protocol surface.
- [docs/features/esp-hal-provisioning-v1.md](../features/esp-hal-provisioning-v1.md) — feature doc carrying the open questions, the signature-only design, and the phased plan.
- `crates/rustyfarian-esp-hal-wifi/src/lib.rs` — the async-only STA estate the AP path joins; the `compile_error!` embassy guard and the `extern "C" esp_wifi_set_max_tx_power` TX-power clamp.
- `crates/rustyfarian-esp-hal-ota/src/http.rs` — the only bare-metal TCP code: the hand-rolled strict no-alloc HTTP/1.1 *client* precedent.
- `crates/rustyfarian-esp-idf-provisioning/src` — the ESP-IDF reference portal: `lib.rs` (builder / session), `portal.rs` (HTTP + security contract), `dns.rs` (DNS catch-all), `store.rs` (NVS persistence and the v1→v2 migration).
- `crates/wifi-pure/src/lib.rs` — `ApConfig` / `validate_ap_config` / `TxPowerLevel`, reused unchanged across both tiers.
- `crates/provisioning-pure/src/profile.rs` — `SchemaProfile`, `LoraFields`, `MqttFields`; the `no_std` profile mechanism reused unchanged.
- `docs/ROADMAP.md` — the `rustyfarian-esp-hal-mqtt` future entry (Long term) the provisioned MQTT credentials' consumer is gated behind.
- [esp-radio CHANGELOG](https://github.com/esp-rs/esp-hal/blob/main/esp-radio/CHANGELOG.md) — the `1.0.0-beta.0` (2026-06-03) removal of `Interfaces` and rename of `wifi::new` to `WifiController::new`; the reason the AP path pins to `=0.18.0`.
- [edge-net](https://github.com/ivmarkov/edge-net) — `edge-http` / `edge-dhcp` / `edge-captive` / `edge-nal` / `edge-nal-embassy`, the preferred no-alloc captive-portal family pending the Phase 2 spike.
- [picoserve](https://crates.io/crates/picoserve) — the HTTP-only alternative evaluated and rejected on partial coverage.
- [sequential-storage](https://crates.io/crates/sequential-storage) and [ekv](https://crates.io/crates/ekv) — the KV-over-flash alternatives evaluated and rejected for the store.
- [esp-storage](https://crates.io/crates/esp-storage) — the `embedded-storage 0.3` `NorFlash` provider, with the `critical-section` feature backing the flash-while-radio mitigation.
- [espressif/esp-idf#10079](https://github.com/espressif/esp-idf/issues/10079) — the cache-disabled-during-flash-write hazard precedent.
