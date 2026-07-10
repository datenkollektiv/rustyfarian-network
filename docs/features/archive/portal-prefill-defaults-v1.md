# Feature: Portal prefill defaults v1

Seed the SoftAP captive-portal provisioning forms with `.env`-derived, non-secret pre-fill values, so testing and re-provisioning workflows no longer require retyping every field on a fresh or factory-reset device.

**Status:** lands end-to-end on both `rustyfarian-esp-hal-network` and `rustyfarian-esp-idf-network` (v0.4.0+); pre-fill only affects the initial `GET /` render, never masks user input on rejected `POST /save` submissions.

## Scope and end state

Today the provisioning portal renders empty form fields and requires typing all values (WLAN SSID, MQTT host, LoRaWAN EUIs) during every provision cycle.
When done:

- A new shared type `juggler::provisioning::PortalDefaults<'a>` carries only non-secret fields: `wifi_ssid`, `dev_eui`, `join_eui`, `mqtt_host`, `mqtt_port`, `mqtt_user`, `mqtt_client`, `ota_url`. Secrets (`wifi_pass`, `mqtt_pass`, `app_key`) are structurally omitted — never pre-filled.
- Both `PortalConfig` structs gain a `defaults: PortalDefaults<'a>` field.
- `load_prefill` falls back to `Prefill::from_defaults(defaults, profile)` (instead of `Prefill::empty()`) when the store is unprovisioned, empty, or on a wrong profile. A stored config still takes precedence.
- Examples populate `PortalDefaults` via `option_env!` over existing `.env` keys (`WIFI_SSID`, `MQTT_HOST`, `MQTT_PORT`, `MQTT_USER`, `MQTT_CLIENT_ID`, `OTA_URL`, `LORAWAN_DEV_EUI`, `LORAWAN_APP_EUI`).
- Build-time correctness: new `crates/rustyfarian-esp-hal-network/build.rs` emits `cargo:rerun-if-env-changed` for each key; `crates/rustyfarian-esp-idf-network/build.rs` extends the existing triggers.

The HTML portal templates are unchanged — no `{{WIFI_PASS}}` / `{{MQTT_PASS}}` / `{{APP_KEY}}` placeholders exist, and existing no-secret tests pass unchanged.
The "secrets never rendered into HTML" invariant is preserved.

## Proposed public API

*Illustrative sketch; names and signatures as built.*

```rust
// In juggler::provisioning::defaults (re-exported from juggler::provisioning
// and from both HAL crates' provisioning modules).
//
// Borrowed, no_std, Copy + Default. Every field is a plain `&str`; an empty
// string ("") means "no default — render the field empty". There is no
// `Option` wrapper and no numeric port — `mqtt_port` is a decimal string,
// mirroring how the stored config and the form field carry it.

pub struct PortalDefaults<'a> {
    pub wifi_ssid: &'a str,
    pub dev_eui: &'a str,       // LoRaWAN profile
    pub join_eui: &'a str,      // LoRaWAN profile (from LORAWAN_APP_EUI)
    pub mqtt_host: &'a str,     // Wi-Fi+MQTT profile
    pub mqtt_port: &'a str,     // Wi-Fi+MQTT profile ("" = unset)
    pub mqtt_user: &'a str,
    pub mqtt_client: &'a str,
    pub ota_url: &'a str,
}
// `PortalDefaults::default()` / `PortalDefaults::none()` → all fields "".

// Both PortalConfig structs gain one field (shown for esp-idf; esp-hal is
// identical). Note there is no `dev_name` default — the device name flows
// through the existing `device_name` field and its `{{DEV_NAME}}` fallback.
pub struct PortalConfig<'a> {
    pub ssid_prefix: &'a str,
    pub ssid_override: Option<&'a str>,
    pub ap_password: Option<&'a str>,
    pub channel: u8,
    pub device_name: &'a str,
    pub firmware_version: &'a str,
    pub profile: SchemaProfile,
    pub defaults: PortalDefaults<'a>,  // NEW
}

// The fallback lives in each crate's private `Prefill::from_defaults` +
// `load_prefill` (NOT a juggler function — the store types differ per HAL).
// A stored config for the active profile takes precedence; otherwise
// `load_prefill` returns `Prefill::from_defaults(defaults, profile)`.
```

Consumer usage (example) — `option_env!` yields `Option<&'static str>`, so
`.unwrap_or("")` maps an unset key to the "no default" empty string:

```rust
let defaults = PortalDefaults {
    wifi_ssid:   option_env!("WIFI_SSID").unwrap_or(""),
    mqtt_host:   option_env!("MQTT_HOST").unwrap_or(""),
    mqtt_port:   option_env!("MQTT_PORT").unwrap_or(""),
    mqtt_user:   option_env!("MQTT_USER").unwrap_or(""),
    mqtt_client: option_env!("MQTT_CLIENT_ID").unwrap_or(""),
    ota_url:     option_env!("OTA_URL").unwrap_or(""),
    ..PortalDefaults::default() // dev_eui / join_eui unset for this profile
};

let portal = PortalConfig {
    ssid_prefix: "Rustyfarian",
    ssid_override: None,
    ap_password: Some("provisioning"),
    channel: 6,
    device_name: "ESP32-C3",
    firmware_version: env!("CARGO_PKG_VERSION"),
    profile: SchemaProfile::WifiMqttDevice,
    defaults,  // NEW
};
```

## Decisions

| Decision                                                                                                                                                                                                                                                                                           | Reason                                                                                                                                                                                                                                                                                                                                                                                                       | Rejected Alternative                                                                                                                                     |
|:---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|:-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|:---------------------------------------------------------------------------------------------------------------------------------------------------------|
| **Non-secret fields only** — `PortalDefaults` carries `wifi_ssid`, `mqtt_host`, etc., but NOT `wifi_pass`/`mqtt_pass`/`app_key`                                                                                                                                                                    | Provisioning workflows never auto-fill passwords or cryptographic keys; users must type them fresh. Eliminating the temptation to set them in `.env` (and accidentally commit them) improves the security posture. The template has no placeholder for them, so this is structural, not accidental.                                                                                                          | Storing secrets in `.env` and rendering them into HTML (credential leakage risk, contradicts deployment practices).                                      |
| **Fallback only on store unprovisioned / empty / wrong-profile** — stored config takes absolute precedence                                                                                                                                                                                         | A user who re-provisions with `.env` still populated should not have their typed input overwritten by `.env`. Pre-fill is only for the initial blank render; a rejected POST re-render uses `Prefill::empty()` so just-typed input is never masked.                                                                                                                                                          | Always render `.env` values over stored (loses user input on form rejection).                                                                            |
| **Shared *type* in `juggler`, per-HAL prefill plumbing** — `juggler::provisioning::PortalDefaults` is the one shared surface; each crate has its own private `Prefill::from_defaults` + `load_prefill(store, profile, defaults)`                                                                   | The prefill vocabulary (field names, non-secret boundary) lives in one place, but the store types differ per HAL (`heapless`/flash on esp-hal, `String`/NVS on esp-idf), so the fallback plumbing cannot be a single pure function. Both crates share a `compose_mqtt_uri` helper shape to keep them comparable.                                                                                             | A single juggler `load_prefill` over an abstract store (would need a store trait spanning both HALs; over-abstraction).                                  |
| **WLAN SSID only (no password pre-fill)** — `wifi_ssid: &'a str` but no `wifi_pass` field                                                                                                                                                                                                          | Passwords are secrets; they must be typed by the human provisioning the device, never sourced from a file. Same rule as `app_key` and `mqtt_pass`.                                                                                                                                                                                                                                                           | Including `wifi_pass` in `PortalDefaults` (credential leakage risk).                                                                                     |
| **MQTT split into separate fields** — `mqtt_host: &'a str` + `mqtt_port: &'a str` (decimal string; not a pre-composed `mqtt_uri`)                                                                                                                                                                  | Both prefill paths (stored config and `.env` defaults) recompose `mqtt://host:port` via a shared `compose_mqtt_uri` helper, mirroring the parse-time split. Keeping host and port separate matches how the stored config and `.env` carry them; `mqtt_port` stays a string (no numeric parse at the config layer).                                                                                           | A single `mqtt_uri` string (can't cleanly recompose from the stored host+port split).                                                                    |
| **Build-time re-trigger on .env changes** — `crates/rustyfarian-esp-hal-network/build.rs` (new file) and `crates/rustyfarian-esp-idf-network/build.rs` (extended) emit `cargo:rerun-if-env-changed` for the full non-secret `PortalDefaults` vocabulary                                            | `option_env!` at compile time is zero-cost at runtime, but Cargo does not automatically re-build when `.env` changes. Explicit `cargo:rerun-if-env-changed` ensures a rebuild when pre-fill values change. Both trigger sets are kept **exhaustive over the shared vocabulary** (esp-hal declares the LoRaWAN EUI keys too, even though it serves only `WifiMqttDevice` today) so the policy does not drift. | Forgetting to rebuild (user edits `.env`, flashes the same binary, pre-fill stays stale); or asymmetric trigger sets that drift from the shared surface. |

## Constraints

- **No secret fields** — `PortalDefaults` has no `wifi_pass`, `mqtt_pass`, or `app_key` fields. Structurally impossible to leak. Existing HTML template tests (which verify no secret placeholders are rendered) remain green.
- **Opt-in.** Consumers may pass `PortalDefaults::default()` (all fields `""`) for no pre-fill; this is the backward-compatible baseline.
- **Best-effort, not validated.** Defaults are passed through to the initial form render as-is — an over-long or malformed default is not rejected here; it surfaces as an ordinary per-field `parse_form` error on submit, exactly as if typed. The esp-hal tier truncates its owned copies to fixed render buffers (no heap); this never widens what a submission may contain.
- **Stored config precedence.** If the store is valid and the profile matches, the stored config is loaded and pre-fill defaults are ignored entirely.
- **Rejected-POST re-render.** When the portal form receives a `POST /save` with invalid input, the re-rendered form uses `Prefill::empty()`, not `Prefill::from_defaults()`, so user edits are never masked by `.env` values.
- **On esp-hal:** `PortalDefaults` fields are borrowed references; the consuming example owns the static/const backing data (e.g. from `option_env!`). The owned copy carried by the HTTP task (`PortalDefaultsOwned`) uses `heapless::String` capped per field — no heap.
- **On esp-idf:** the public config takes `PortalDefaults<'a>`; `start()` converts it into an owned `PortalDefaultsOwned` (`String`) threaded into the httpd handler.
- **Per-HAL `load_prefill`.** Each crate owns its `load_prefill(store, profile, defaults)` (esp-hal is `no_std`/`heapless`; esp-idf is `std`/`String`). They return an owned `Prefill`; only the `PortalDefaults` *type* is shared via `juggler`.
- **Targets.** Must compile for `riscv32imac-esp-espidf`, `riscv32imc-esp-espidf`, `xtensa-esp32-none-elf`, and `xtensa-esp32s3-none-elf` with provisioning features enabled.

## Security posture

The HTML portal templates are unchanged; they have no `{{WIFI_PASS}}`, `{{MQTT_PASS}}`, or `{{APP_KEY}}` placeholders.
The no-secret tests which verify this remain green.
Pre-fill defaults are purely a form convenience feature; the security invariant is preserved by the structural absence of secret fields.

## Verification

- Host tests added: `PortalDefaults::default()`/`none()` yield all-empty (`juggler`); per-crate `Prefill::from_defaults(defaults, profile)` returns correct field assignments per profile (MQTT host+port recomposed via `compose_mqtt_uri`); empty defaults fallback remains empty; partial (e.g. MQTT host only) defaults omit the URI; `compose_mqtt_uri` requires both host and port.
- Cross-target example builds via `just build-example` (all provisioning examples).
- Hardware smoke test: with `.env` populated, flash a factory-reset device on esp-idf or esp-hal; verify the initial portal form shows non-secret pre-filled values (`WIFI_SSID`, `MQTT_HOST`, etc.); password and AppKey fields remain empty.
- Build-time re-trigger: edit `.env`, change a value, re-run `just build-example <name>` (no force-rebuild needed); binary includes the updated pre-fill.

## Open Questions

- None — all resolved as of implementation (2026-07-08).

## State

- [x] Design approved
- [x] Core implementation
- [x] Tests passing
- [x] Documentation updated

## Acceptance criteria

1. `PortalDefaults<'a>` compiles on both `rustyfarian-esp-hal-network` and `rustyfarian-esp-idf-network` with provisioning features enabled; carries only non-secret fields.
2. Both `PortalConfig` structs add a `defaults: PortalDefaults<'a>` field with no breaking changes to other fields.
3. Each crate's `Prefill::from_defaults(defaults, profile)` is host-tested for: all-empty defaults → empty prefill; per-profile field assignments (LoRaWAN EUIs vs. Wi-Fi+MQTT); MQTT host+port recomposition via the shared-shape `compose_mqtt_uri`; and stored-config precedence (stored wins, defaults ignored). Only the `PortalDefaults` type lives in `juggler`, not the fallback function.
4. Rejected-POST re-render uses `Prefill::empty()`, not `Prefill::from_defaults()`, so user input is never masked on form rejection.
5. HTML portal templates are unchanged; no secret placeholders are added; existing no-secret tests pass.
6. Examples (`hal_c3_provision_mqtt.rs`, `hal_c6_provision_mqtt.rs`, `idf_c3_provision_mqtt.rs`, `idf_c3_provision.rs`) populate `PortalDefaults` via `option_env!` over `.env` keys (`WIFI_SSID`, `MQTT_HOST`, `MQTT_PORT`, `MQTT_USER`, `MQTT_CLIENT_ID`, `OTA_URL`, `LORAWAN_DEV_EUI`, `LORAWAN_APP_EUI`).
7. Build-time re-trigger: `crates/rustyfarian-esp-hal-network/build.rs` (new) and `crates/rustyfarian-esp-idf-network/build.rs` (extended) emit `cargo:rerun-if-env-changed` exhaustively over the non-secret `PortalDefaults` vocabulary (esp-hal declares the LoRaWAN EUI keys too, for symmetry); examples rebuild when `.env` values change.
8. `just verify` passes; `just build-example` covers all provisioning examples (cross-target).
9. Hardware smoke test on esp-idf or esp-hal: factory-reset device with `.env` populated; initial portal form shows non-secret pre-fill (`WIFI_SSID`, `MQTT_HOST`, etc.); password and AppKey fields stay empty.
10. CHANGELOG entry added; no new Cargo features required (lives under existing `provisioning` feature).

## Session Log

- 2026-07-08 — Feature doc created and populated as a completed/landed feature (v0.4.0+). Design: non-secret `PortalDefaults<'a>` type shared via `juggler`; per-HAL `Prefill::from_defaults` + `load_prefill` fallback only on unprovisioned/wrong-profile; build-time re-trigger via `cargo:rerun-if-env-changed`. All acceptance criteria marked met. All state items ticked. No open questions.
- 2026-07-08 — PR #85 review follow-ups applied: esp-hal `build.rs` trigger set made exhaustive over the shared `PortalDefaults` vocabulary (added LoRaWAN EUI keys for symmetry); esp-idf gained a shared-shape `compose_mqtt_uri` helper used by both the stored and defaults prefill paths (with a test); the best-effort/unvalidated defaults contract documented on `PortalDefaults` and `PortalDefaultsOwned::from_borrowed`; doc corrected to match the as-built API (`&str` fields, per-HAL `load_prefill`).
