# ADR 026: Wi-Fi Regulatory Country

## Status

Accepted (2026-11-11).
Builds on [ADR 022](022-tier-parity-one-async-api.md) (same names on both tiers) and [ADR 023](023-shared-logic-ladder.md) (shared logic in `juggler`).

## Context

The radio's regulatory country decides which 2.4 GHz channels it may transmit on.
The two tiers disagreed about it.
ESP-IDF 5.3.3 starts in country `01` (channels 1-11, 802.11d on) and keeps the country it was given in flash, so it survives a reflash.
esp-radio 1.0.0-beta.1 sets a hard-coded `CountryInfo` (CN, channels 1-13, manual) once, at controller start, so the esp-hal tier allowed channels 12 and 13 that a world-safe radio refuses.

The 0.7.0 bench showed the cost.
The ESP-IDF scout scanned the default channels 1-13, ESP-IDF rejected channel 12 with `ESP_ERR_INVALID_ARG`, and every not-found scan ended with a local failure instead of a clean not-found.
The same mismatch affects a SoftAP: a channel outside the country silently becomes channel 1 on ESP-IDF.

The allowed set is runtime state, not a compile-time constant, because 802.11d lets an associated station adopt the country of its access point.
An unassociated scout cannot learn that country by listening, so it must be told.

### Approaches evaluated

**Per-tier defaults** (IDF `01`, esp-radio CN 1-13): keeps each tier as it is but leaves the scan and the SoftAP disagreeing with the radio, and a fleet of mixed tiers cannot agree on a channel.

**esp-radio `CountryInfo`**: esp-radio sets the country once, before the application runs, and hard-codes the channel range; a later `esp_wifi_set_country_code` is not overwritten, but the firmware cannot pick the range through the controller config.

**Shared `extern "C"` in `juggler`**: one copy of the FFI, but `juggler` would no longer be pure and host-testable, and the struct layout is specific to each `esp-wifi-sys` release.

**Write the country on every boot**: simple, but every ESP-IDF boot would write flash for a value that rarely changes.

**Reject channel lists that name a forbidden channel**: fails at construction, although the list is only checked against a country the radio may later adopt.

**Keep the silent SoftAP fallback**: the captive portal would come up on channel 1 with no error.

## Decision

### Default and API

The default country is `Country::WORLD`, the code `01` with channels 1-11 and 802.11d on, on both tiers.
Both tiers offer the same names, with the same arguments and returns:

- `Country` (`parse`, `new`, `with_ieee80211d`, `code`, `as_str`, `ieee80211d`, `channels`, `c_code`), `CountryError`, `ChannelSet` and `Regulatory`, re-exported from `juggler::wifi` by the `wifi` and `espnow` modules of both tiers.
- `CountryPort` and `apply_country` in `juggler::wifi`, which implement the read-before-write rule once; the tiers use them internally and do not re-export them.
- `wifi::set_country(Country)` and `wifi::regulatory()` for a firmware that builds its own controller.
- `WiFiConfig::with_country`, `ApConfig::with_country` and `ProvisioningBuilder::with_country`.
- `EspIdfEspNow::set_country` and `EspHalEspNow::set_country`, which work on a radio the library owns and return `RadioNotOwned` or `ControllerNotOwned` otherwise.

`Country::parse` is `const`, so an example turns `option_env!("WIFI_COUNTRY")` into a build error for an unsupported code.

### Shared logic and tier adapters

All logic lives in `juggler::wifi::country`: the code table (`01`, `US`, `CA` and `TW` allow 1-11, `JP` allows 1-14, the others 1-13), the channel set, `Regulatory::from_raw` and `apply_country`.
Each tier implements `CountryPort` over its own FFI only.
ESP-IDF uses the bindgen `esp_wifi_set_country_code` and `esp_wifi_get_country`.
esp-hal declares the same two functions as an inline `extern "C"` next to its TX-power clamp and checks the private `WifiCountryRaw` layout (12 bytes, alignment 4) with compile-time asserts.

### Apply on every radio start

Each tier applies the country right after its controller exists and before its configuration and start:

- ESP-IDF applies it in `WiFiManager::new`, `SoftApManager::start` and the owned ESP-NOW radios.
  `init` and `init_with_capacity` only read the country, because the caller owns the radio.
- esp-hal applies it in `init_async`, `init_softap_async` and `EspHalEspNow::new_owned`, before the TX power and the channel.

`apply_country` reads the current country first.
It writes only when the code or the 802.11d policy differs, and it reads back afterwards.
A failed read still writes, a failed write is returned, and a read-back that differs from the request is logged as a warning.
Both tiers log the same line, `[wifi] country 01, channels 1-11, 802.11d on`.

A failed country call is fatal, and the error carries the driver code.

### Skip, do not reject

The ESP-NOW scan works from a `ChannelSet` (`ScanMachine::with_allowed_channels`, default `ALL`).
A channel the country forbids is skipped without a `set_channel` call, and it is not a fault.
When no configured channel is allowed, the scan ends `NotFound` with no radio work.
`ScanError::NotFound` carries `not_allowed`, the number of configured channels the country forbids, so the text reads `probing 11 of 13 configured channels (2 not allowed by the country)`.
A reply that names a forbidden home channel ends with `ScanError::HomeNotAllowed { home }` and the pinned channel is restored.
An explicit `with_channels` list that names a forbidden channel is skipped and counted, with one warning per scan.

A `SoftAP` channel outside the country is a configuration error, checked in `validate_ap_config`, because ESP-IDF would otherwise fall back to channel 1 without a message.

### Scope of the country

802.11d only affects a station that is associated.
A coordinator on a station (`WORLD` with 802.11d) finds a network in another country passively, joins it and adopts its country.
A scout that is not associated stays on 1-11 under `WORLD` and must be given the country of the network it looks for, with `set_country(Country::parse("DE")?)` or `WIFI_COUNTRY=DE`.
`with_ieee80211d(false)` pins the configured country even while associated.

### Breaking changes

The esp-hal default changes from CN 1-13 to `01`, so firmware that relied on channels 12 and 13 must set a country or stay on 1-11.
The new fields on `WiFiConfig`, `ApConfig` and `NotFound`, the new variants and `#[non_exhaustive]` on `ScanError` and `ScanFault` are breaking for code that matches or builds these types exhaustively.
They are listed in the CHANGELOG.

## Consequences

### Positive

- Both tiers agree on the channel set, so a scan and a SoftAP never try a channel the radio refuses.
- The bench sees a clean `NotFound` with the count of forbidden channels, instead of a local failure.
- The read-before-write rule keeps the ESP-IDF flash untouched on a boot where the country already matches and the station is not associated; while associated the radio may report the access point's country, so the write is never skipped then.
- The shared logic is host-tested in `juggler`, with mutation checks on the skip, the count, the skip-if-equal guard, the home-channel guard and the `01` table row.

### Negative

- ESP-IDF writes flash twice per boot on an owned radio built with `init_with_radio` that then calls `set_country` (once for `WORLD`, once for the requested country); `init_with_radio_config` and `init_with_radio_sta_config` apply the country once and avoid it.
- Both tiers take the same `juggler::espnow::EspNowConfig`; an owned radio pins the configured channel right after it applies the country, so an `ESPNOW_CHANNEL` of 12 or 13 needs `EspNowConfig::with_country` with a country that allows it, and `validate_owned` refuses it otherwise.
- The `init_with_radio_scanning` path scans before any country can be set and always runs under `WORLD`.
- `ProvisioningBuilder::with_country` and the ESP-IDF `WiFiManager` path must each be set by the firmware; `run_wifi_mqtt_portal` and `BootConfig` have no country option yet.
- The esp-hal extern layout matches the esp-wifi-sys 0.3.0 bindings for esp32, C3, C6 and S3 (no 5 GHz field), and must be re-checked on every esp-radio bump.

### Neutral

- The ESP-IDF tier keeps the country in flash and the library re-applies it on every boot, so a country set in an earlier build still stands until the next start.

## References

- [ADR 009 — ESP-NOW Channel Scanning](009-espnow-channel-scanning.md) (the scan that now skips forbidden channels)
- [ADR 022 — Tier Parity, One Async API](022-tier-parity-one-async-api.md)
- [ADR 023 — Shared Logic Ladder](023-shared-logic-ladder.md)
- [ADR 025 — ESP-NOW Probe Reply Carries the Coordinator's Channel](025-espnow-probe-reply-carries-channel.md) (the same scan, the reply rule)
- [Feature: Wi-Fi regulatory country v1](../features/archive/wifi-country-v1.md)
- `crates/juggler/src/wifi/country.rs` (`Country`, `ChannelSet`, `CountryPort`, `apply_country`), `crates/juggler/src/espnow/scan.rs` (`ScanMachine::with_allowed_channels`)
