# Feature: SoftAP Provisioning v1

Stub feature doc capturing the architectural decisions already locked by
[ADR 013](../adr/013-softap-provisioning-acceptance.md).
Implementation work is **Long-term** and does not start until capacity opens
or `rustyfarian-beekeeper` Milestone 5 forces the issue.
Whoever picks the work up should walk through `/feature` to add implementation
details (form-validation rules, NVS key layout, captive-portal state machine,
SoftAP SSID derivation) before writing code.

## Decisions

|                                                                                  Decision | Reason                                                                                                                                                  | Rejected Alternative                                                                                                                                          |
|------------------------------------------------------------------------------------------:|:--------------------------------------------------------------------------------------------------------------------------------------------------------|:--------------------------------------------------------------------------------------------------------------------------------------------------------------|
|                                     Provisioning lives in `rustyfarian-network` workspace | Runtime dependency surface is dominated by Wi-Fi/NVS plumbing already provided here (same argument as ADR 011 for OTA)                                  | In-beekeeper module (violates "drivers live in shared crates"); separate `rustyfarian-provisioning` repo (duplicates Wi-Fi pinning)                           |
|        Two crates at acceptance: `provisioning-pure` + `rustyfarian-esp-idf-provisioning` | Matches the established `*-pure` + `rustyfarian-esp-idf-*` triad used by wifi/lora/espnow/ota                                                           | Single combined crate (breaks host-testability of the form/state-machine logic)                                                                               |
|                                    Bare-metal `rustyfarian-esp-hal-provisioning` deferred | No bare-metal downstream has requested it; speculative work would mean a `no_std` HTTP server with no consumer                                          | Build the bare-metal crate up front for "dual-HAL completeness"                                                                                               |
|                                               Captive-portal HTTP server is internal-only | Preserves the README's "general-purpose HTTP clients out of scope" line; mirrors ADR 011's private OTA HTTP client                                      | Export the HTTP server as a reusable workspace API (would need a wider vision change)                                                                         |
| Four-field provisionable schema (Wi-Fi creds + LoRaWAN OTAA keys + OTA URL + device name) | Matches the requesting downstream's needs verbatim; also the union of NVS fields every Rustyfarian field device stores today                            | Wi-Fi credentials only (forces beekeeper to build half of what it asked for); generic host-defined schema (scatters validation rules across every downstream) |
|                                                         BLE provisioning stays a non-goal | No downstream has asked for it; ESP-IDF BLE stack is a substantial new dependency; SoftAP solves the same problem on hardware every device already uses | Accept BLE provisioning alongside SoftAP                                                                                                                      |

## Constraints

- Must build on `rustyfarian-esp-idf-wifi` SoftAP lifecycle — no parallel Wi-Fi stack.
- Must use `esp-idf-svc` NVS for credential persistence — no custom flash layout.
- HTTP server is `pub(crate)` inside `rustyfarian-esp-idf-provisioning`; not re-exported.
- `provisioning-pure` is `no_std` so a future `rustyfarian-esp-hal-provisioning` can adopt it without an API break.
- Compile-time `.env` values via `option_env!` remain a valid fallback when NVS is empty (same pattern as `idf_esp32s3_join`).
- Provisioning-mode entry (no NVS credentials, button hold, repeated Wi-Fi failure) is the host application's decision, not the library's.

## Open Questions

- [ ] SoftAP SSID derivation rule — MAC suffix vs configurable prefix-plus-MAC; default for beekeeper is `Beekeeper-XXXX`.
- [ ] NVS namespace and key layout — single namespace per device or one per field category.
- [ ] Captive-portal "save and reboot" semantics — single-shot config commit vs incremental field saves.
- [ ] `/status` JSON endpoint schema — battery, LoRa state, firmware version are listed in the request, but the contract is not pinned.
- [ ] Factory-reset hook API shape — callback, channel, or NVS flag the host application polls.
- [ ] Form-validation error reporting — single error string vs per-field error map returned by `provisioning-pure`.

## State

- [x] Design approved (ADR 013)
- [ ] Core implementation
- [ ] Tests passing
- [ ] Documentation updated

## Session Log

- 2026-06-11 — Feature doc stub created alongside ADR 013; original feature
  request was archived into this doc by acceptance and the review-queue file
  was deleted.
- 2026-06-11 — Walked through Decisions, Constraints, Open Questions, and State
  via `/feature`; confirmed all sections are correct as written and that the
  6 open questions are intentionally left for the implementer when work starts.
