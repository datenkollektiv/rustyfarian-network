---
gate: none
desk-work: available
---

# Feature: Wi-Fi regulatory country v1

Both tiers apply one configurable regulatory country on every radio start, and the ESP-NOW scan skips channels the country forbids.

## Decisions
| Decision | Reason | Rejected Alternative |
|:--|:--|:--|
| Default `Country::WORLD` ("01", channels 1-11, 802.11d on) on both tiers | Legal everywhere, ESP-IDF default, upstream esp-radio direction | Keep per-tier defaults (IDF 01, esp-radio CN 1-13) |
| All logic in `juggler::wifi::country` (type, table, `apply_country`, scan filter) | One implementation for all tiers (ADR 023) | Per-tier copies |
| Tiers implement `CountryPort` over FFI only | Keeps juggler pure and host-testable | Shared `extern "C"` in juggler |
| Apply via `esp_wifi_set_country_code` on both tiers | Validates per-country rules, present in the hal Wi-Fi blobs | esp-radio `CountryInfo` (hard-codes nchan 13, manual) |
| Read before write, write only on change and never while the station is associated | IDF stores the country in flash; an associated station reports the AP's country | Write every boot |
| Forbidden channels are skipped, counted in `NotFound::not_allowed` | The allowed set is runtime state (802.11d) | Reject `with_channels` lists up front |
| SoftAP channel outside the country is a config error | IDF silently falls back to channel 1 | Keep the silent fallback |
| ESP-NOW start settings in shared `juggler::espnow::EspNowConfig` | Both tiers take the same config (ADR 022) | Per-tier `_country` constructors |
| Reply naming a forbidden home channel ends `ScanError::HomeNotAllowed` | Diagnoses a coordinator on 12/13 seen from a 01 scout | Report plain `NotFound` |

## Constraints
- Tier parity (ADR 022): same names, arguments and returns on both tiers; only error types differ.
- Ships in 0.7.0; breaking field and variant additions are listed in the CHANGELOG.
- The hal `extern "C"` layout is checked against esp-wifi-sys 0.3.0 on every esp-radio bump.
- `init_with_radio_scanning` scans before a country can be set and always runs under WORLD.

## Open Questions
- [ ] Does ESP-IDF write flash on a boot where the country already matches (bench check)? (deferred)
- [ ] Does stop/start after suspend keep the country on both tiers (read-back on the bench)? (deferred)

## State
- [x] Design approved
- [x] Core implementation
- [x] Tests passing
- [x] Documentation updated
- [x] Hardware check passed (`c3/espnow-cancel`)

## Session Log
- 2026-11-11 — Feature doc created from the approved design; core, both tier adapters, ADR 026 and the bench check implemented; review fixes (owned ESP-NOW constructors take the start country, no write skipped while associated, empty `WIFI_COUNTRY` means WORLD on both tiers); ESP-NOW start settings moved into shared `juggler::espnow::EspNowConfig`; bench check `c3/espnow-cancel` passed on both tiers with the country line at boot and not-found scans skipping 12/13
- 2026-11-12 — Archived; the two open bench questions are deferred
