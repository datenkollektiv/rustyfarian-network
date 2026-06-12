# Feature: Wi-Fi + MQTT Provisioning Profile v1

Feature doc for the second provisioning schema profile proposed by [ADR 014](../adr/014-wifi-mqtt-provisioning-profile.md).
The profile adds a `WifiMqttDevice` schema (Wi-Fi credentials + MQTT broker + OTA URL + device name, no LoRaWAN) to the SoftAP provisioning triad already shipped on the `soft-ap` branch under [ADR 013](../adr/013-softap-provisioning-acceptance.md) / [feature doc](softap-provisioning-v1.md).

This is a hybrid doc: it records the decisions locked by ADR 014 (accepted 2026-06-12) and the open questions and phased plan that drove the implementation.
ADR 014 and the seven proposed answers below were signed off on 2026-06-12 and implemented the same day; durable outcomes are promoted into the "Locked at implementation" Decisions sub-table.
The Design section is an illustrative sketch and a *delta* on the existing v1 implementation, not a fresh contract; where the code deviated, the Session Log records it.
The verification gates are deferred to the maintainer machine (no Rust toolchain in the implementation sandbox); see the State checklist.

## Decisions

### Locked by ADR 014 (once accepted)

|                                                                                                         Decision | Reason                                                                                                                                                                                           | Rejected Alternative                                                                                                                                                                                           |
|-----------------------------------------------------------------------------------------------------------------:|:-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|:---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
|                                  A second provisioning profile is accepted: `WifiMqttDevice` = Core + MQTT + OTA | `rustyfarian-rgb-clock` needs Wi-Fi + MQTT + OTA + device name and no LoRaWAN; ADR 013's Consequences already anticipated a "no LoRaWAN" host; closed named profiles keep validation centralised | Route MQTT creds through opaque extras (no redaction/validation); a third crate or per-downstream fork (duplicates the portal); ad-hoc optional LoRa fields (ambiguous boot states, no typed profile contract) |
|             Schema generalises to a closed set of named profiles built from field groups (Core/LoRaWAN/MQTT/OTA) | Exactly two profiles (`LorawanFieldDevice`, `WifiMqttDevice`) keep ADR 013's "validation is load-bearing" property while expressing both shapes from one type                                    | Generic host-defined schema (reaffirmed rejection from ADR 013 §4 — scatters validation across downstreams)                                                                                                    |
|                              MQTT field group = broker host+port, optional username/password, optional client ID | Every field maps onto an existing `MqttConfig` method (`new(host, port, client_id)` / `with_auth(username, password)`); validation delegates per the wifi-pure/lora-pure precedent               | A bespoke MQTT field set not grounded in the consumer (collects values the MQTT crate cannot use)                                                                                                              |
|                                                              Plain `mqtt://` only; MQTT-over-TLS is out of scope | Matches `format_broker_url`'s single hard-coded scheme and the workspace plain-transport posture (same as ADR 011 plain-HTTP OTA)                                                                | Accept `mqtts://` (promises a transport the consumer cannot honour today)                                                                                                                                      |
|                      The profile mechanism lives in `provisioning-pure`; the same two crates serve both profiles | No new crate; `parse_form` becomes profile-parameterised and `ProvisioningConfig` carries optional field groups (honours ADR 013 §2/§3)                                                          | A parallel crate or surface for the new profile                                                                                                                                                                |
|             NVS schema version bumps to 2 with an explicit `profile` discriminator; v1 records read as `lorawan` | A device must know its profile at boot; absent-`profile` v1 records read as `lorawan` so deployed beekeeper devices are not re-provisioned                                                       | No discriminator (a v2 reader cannot tell the profiles apart); a full migration pass (unnecessary — default-by-absence is zero-cost)                                                                           |
|                                                                        BLE provisioning and TLS remain non-goals | Carried forward from ADR 013 §5 and the plain-transport posture; no downstream has asked for either                                                                                              | Accept BLE provisioning or MQTT-over-TLS in v1                                                                                                                                                                 |

### Locked at implementation (2026-06-12)

These rows promote the resolved Q1–Q7 outcomes plus the implementation deviations into durable decisions.
The amendments that produced them are itemised in the 2026-06-12 implementation Session Log entry.

|                                                                                                          Decision | Reason                                                                                                                                                                                |
|-----------------------------------------------------------------------------------------------------------------:|:------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
|              Q1 — single `mqtt_uri` input split into separate `mqtt_host` / `mqtt_port` NVS keys (`mqtt_port` string) | One form field, zero re-parsing on the boot critical section; port failures on both paths (`u16` parse and `!= 0` validator) map to the `mqtt_uri` field error as `InvalidUrl`        |
|                         Q2 — optional `mqtt_client`; blank derives a host-side client ID truncated to 23 bytes on a char boundary | `MqttConfig::new` needs a client ID but the library does not invent identifiers; the example's `derive_client_id` helper truncates the device name on a UTF-8 boundary                |
|                                Q3 — `mqtt_user` / `mqtt_pass` optional with an asymmetric guard: anonymous, user-only, and both-present accepted; password-without-user rejected | Maps onto `MqttConfig::with_auth`; username-only ACLs are legitimate; `mqtt_pass` is secret-class (redacting `Debug`, no prefill, re-entered each submission)                         |
|             Q4 — `SchemaProfile` enum (with `as_str` / `from_str`) and grouped `Option<LoraFields>` / `Option<MqttFields>` | Closed two-profile set with profile-aware `parse_form`; cross-profile fields fold to one Form-level `UnexpectedForProfile`; `MAX_FIELD_ERRORS = 9`; group structs carry redacting `Debug` |
|                                                  Q5 — two complete templates `portal_lorawan.html` / `portal_wifi_mqtt.html` | Flat `include_str!` + substitution, no template mini-language; the old combined `portal.html` was deleted                                                                            |
|         Q6 — NVS schema v2 with a `profile` discriminator; absent-`profile` v1 records load as `lorawan`; `save` removes the inactive group | A device knows its profile at boot; deployed beekeeper devices are not re-provisioned; `save` writes only the active group so `load` round-trips `None` for the other; MQTT length caps set to 64 |
|                  Q7 — `PortalConfig` gains `profile`; prefill / field-label / render paths profile-aware; `/status` adds `"profile"` | Prefill only fires when the stored profile matches the portal profile; the `/status` document schema bumped 1→2 on an axis independent of the NVS `schema_ver`                        |

## Constraints

- Builds entirely on the existing `soft-ap` branch triad — no SoftAP, portal, DNS, or store rework beyond threading the profile through.
- `provisioning-pure` stays `no_std`.
- `provisioning-pure` depends on `rustyfarian-network-pure` for `validate_client_id` / `CLIENT_ID_MAX_LEN`; because that crate is `std` today, ADR 014 §2 locks a `#![cfg_attr(not(feature = "std"), no_std)]` extraction (default-enabled `std` feature gating the two std items) that lands before Phase 1 (see Q2).
- Secrets handling is identical to the LoRaWAN profile: `mqtt_pass`, `wifi_pass`, and `app_key` are redacted in `Debug`, never pre-filled into HTML, and re-entered on every submission.
- The library never decides provisioning-mode entry; that is the host application's decision, carried forward from the v1 constraints.
- `mqtt_uri` accepts the plain `mqtt://` scheme only, matching the OTA URL's `http://`-only rule.
- One logical commit unit, single-namespace factory reset, and `schema_ver`-written-last torn-write safety are preserved unchanged.
- One sentence per line throughout this doc.

## Open Questions

Each carries a **Proposed** answer awaiting maintainer sign-off alongside ADR 014.

### Q1. Broker address form UX — Proposed: a single `mqtt_uri` input parsed into host + port

The portal collects one `mqtt_uri` input validated as `mqtt://{host}:{port}`: the scheme is locked to `mqtt://`, the port is mandatory and in `1..=65535`, and the host is non-empty.
At submit the parser splits it into host and port; `ProvisioningConfig` exposes `mqtt_host()` and `mqtt_port()`.
NVS stores `mqtt_host` and `mqtt_port` as **separate** keys so the load-then-connect path needs zero re-parsing — the same argument that made the v1 store keep LoRa values as ready-to-use hex strings.

Rejected: two separate form fields (host + port) — more inputs, and a port typed as `0` or garbage is just as likely either way, so the single field is no less safe and is one fewer thing to get wrong.
Rejected: storing the URI verbatim and re-parsing on every boot — wasteful and re-introduces a parse-failure path on the boot critical section.

### Q2. Client ID — Proposed: optional `mqtt_client`, host derives one when blank

The portal collects an optional `mqtt_client` input.
When present it is validated against the 23-byte MQTT 3.1.1 rule.
When blank, the **host** derives a client ID at boot (for example from the device name, sanitised and truncated) — the library does not invent identifiers, mirroring the "host decides" posture used for provisioning-mode entry and reboot.

`DEVICE_NAME_MAX_LEN` is 24, which is greater than the 23-byte client-ID cap, so any naive "client ID = device name" derivation must truncate.
That note belongs in the rustdoc of whatever accessor returns the client ID, so a host author does not ship a 24-byte device name expecting a valid client ID.

> **Where does the 23-byte rule come from? — locked by ADR 014 §2.**
> The canonical validator is `rustyfarian-network-pure::mqtt::validate_client_id` with `CLIENT_ID_MAX_LEN = 23`.
> `rustyfarian-network-pure` is `std` today — `mqtt.rs:266` `spawn_subscriber_thread` uses `std::thread` / `std::sync` and `mqtt.rs:18` `format_broker_url` returns `String` — so `provisioning-pure` (which is `no_std`) cannot depend on it as written.
> ADR 014 §2 locks the mechanism: `rustyfarian-network-pure` gains `#![cfg_attr(not(feature = "std"), no_std)]` with a default-enabled `std` feature that gates only those two items; the validators (`validate_client_id`, `CLIENT_ID_MAX_LEN`, the topic validators) sit in the `no_std` core, and `provisioning-pure` depends on it with `default-features = false`.
> This extraction lands **before Phase 1**, since Phase 1's MQTT validators delegate into it.
> A separate `mqtt-pure` crate and duplicating the 23-byte rule in `provisioning-pure` were both rejected (see ADR 014 §2).

Rejected: a required client-ID field — `MqttConfig::new` requires a client ID at build time, but the provisioning layer should not force a field author to invent one; the host can derive it.
Rejected: auto-derivation *inside* the library — the library does not invent identifiers, by the same principle that keeps reboot and mode-entry host-owned.

### Q3. Auth pair — Proposed: `mqtt_user` / `mqtt_pass`, optional with an asymmetric guard

The portal collects `mqtt_user` and `mqtt_pass`.
Anonymous connection = both absent or both empty.
A password without a username is rejected as a field error on `MqttPass` (a password with no account to attach it to is almost certainly a mistake).
A username without a password **is** allowed — some brokers run username-only ACLs — so this asymmetry is deliberate, not an oversight.
`mqtt_pass` is secret-class: redacted in `Debug`, never pre-filled, re-entered each submission.
The pair maps onto `MqttConfig::with_auth(username, password)`.

Rejected: strict both-or-neither — rejects the legitimate username-only broker case.
Rejected: allow password-without-user — no broker semantics make that meaningful; it is a typo guard.

### Q4. Profile representation in code — Proposed: a `SchemaProfile` enum and grouped accessors

`SchemaProfile` is a two-variant enum (`LorawanFieldDevice`, `WifiMqttDevice`) with `fn fields(self) -> &'static [Field]` returning that profile's canonical field list.
`parse_form` gains a profile parameter: `parse_form(body, profile)`.
This is a breaking signature change, acceptable because every public API in these crates is declared experimental.

`ProvisioningConfig` gains optional field-group structs: `lora: Option<LoraFields>` and `mqtt: Option<MqttFields>`.
Group access is via methods `lora() -> Option<&LoraFields>` and `mqtt() -> Option<&MqttFields>`; the Core and OTA accessors (`wifi_ssid`, `wifi_password`, `ota_url`, `device_name`) stay flat and always present.
`ProvisioningConfig` also gains `fn profile(&self) -> SchemaProfile`.
Without it the runtime invariant — the provisioned profile's group is `Some` and the other is `None` — is unasserted on the type; hosts match on the returned profile instead of probing which group happens to be present, and the store round-trips the NVS `profile` discriminator against it.
`to_lora_config` moves onto `LoraFields`, since it is only meaningful when the LoRaWAN group is present.

A canonical field belonging to the *other* profile appearing in a submission is not folded into extras; it is recorded as a single **Form-level** `UnexpectedForProfile` error (a new `ValidationError` variant), with the first body-level error winning, consistent with the existing Form-level folds.
Silent extras-folding of such a field would reproduce the ambiguous-acceptance problem ADR §1 rejects.
Per-field reporting is unnecessary: cross-profile fields are never rendered in the active profile's form, so a body carrying one is hand-crafted rather than user error, and a single Form-level signal is sufficient.

Capacity: `WifiMqttDevice` has eight canonical fields — `wifi_ssid`, `wifi_pass`, `mqtt_uri`, `mqtt_user`, `mqtt_pass`, `mqtt_client`, `ota_url`, `dev_name` — one more than LoRaWAN's seven.
So `MAX_FIELD_ERRORS` becomes 9: the maximum canonical count across profiles (eight) plus one form-level error.
Because the cross-profile case folds to a single Form-level error rather than per-field, this stays at 9 — a per-field scheme would force 12 (the union of canonical fields across both profiles, plus one).
All four capacity-proof comment sites must be amended to state "at most one error per canonical field (up to eight for `WifiMqttDevice`) plus one form-level error":

- `crates/provisioning-pure/src/config.rs:30-33` (the `MAX_FIELD_ERRORS` doc).
- `crates/provisioning-pure/src/error.rs:150-156` (the `FieldErrors` type doc).
- `crates/provisioning-pure/src/form.rs:160-167` (the `parse_form` doc).
- `crates/provisioning-pure/src/form.rs:306-310` (the `push_field_error` capacity comment).

The fixed `[Slot; 7]` working array and the `const CANONICAL: [Field; 7]` array become per-profile: the `slots` buffer is sized to the maximum canonical count (eight) and the canonical list is selected from `SchemaProfile::fields`.

Rejected: flat `Option`-returning LoRa accessors that panic when the group is absent — a footgun; grouped `Option<&...>` accessors make presence explicit at the type level.

### Q5. Portal template strategy — Proposed: two complete templates selected per profile

Ship two complete HTML templates, `portal_lorawan.html` and `portal_wifi_mqtt.html`, selected by `include_str!` per profile, both using the existing placeholder-substitution mechanism.
Accept the duplicated head/CSS/nonce/footer markup (roughly 40 lines).

Rejected: a `{{#LORA}} … {{/LORA}}` conditional-block stripping scheme — that is a template-engine mini-language nobody asked for, and the v1 portal deliberately uses flat `include_str!` plus simple substitution.
Two static files are auditable; a homegrown conditional language is not.

### Q6. NVS layout — Proposed: new MQTT keys plus a `profile` discriminator, schema v2

New keys, all within the 15-byte NVS key cap and collision-free against the existing `rf_prov` keys:

- `mqtt_host` (9 bytes).
- `mqtt_port` (9 bytes).
- `mqtt_user` (9 bytes).
- `mqtt_pass` (9 bytes).
- `mqtt_client` (11 bytes).
- `profile` (string `lorawan` | `wifi_mqtt`).

`SCHEMA_VERSION` bumps to `2`.
`load` treats `schema_ver == 1` with an absent `profile` key as the `lorawan` profile — a documented migration that re-provisions no device.
`is_provisioned` stays `schema_ver` + `wifi_ssid` present (the SSID is in the Core group, common to both profiles).
`save` writes `profile` before `schema_ver`, preserving the v1 commit-guard ordering so a torn write never reads as provisioned with an unknown profile.

`mqtt_port` is stored as a string, consistent with every other value in the namespace and with the existing `read_str` / `set_str`: it is written once and read once per boot, and the string path adds zero new store surface.
`EspNvs` u16 support (`set_u16` / `get_u16`) was unverified in-repo and is deliberately not relied upon.

The version suffixes are independent axes: the feature-doc suffix `v1` is the first revision of *this feature doc*, while NVS `schema_ver = 2` is the second on-flash layout revision; the asymmetry is expected, not a typo.

Rejected: storing `mqtt_uri` verbatim as one key — re-parsed on every boot (see Q1).

### Q7. `PortalConfig` / builder threading — Proposed: add a `profile` field and make the render paths profile-aware

`PortalConfig` gains a `profile: SchemaProfile` field.
The prefill, field-label, and render paths in `portal.rs` become profile-aware (selecting the template from Q5 and the field set from Q4).
`/status` JSON gains an additive optional `"profile"` field, consistent with the v1 `/status` contract where everything beyond `schema` / `state` / `provisioned` is optional.

Rejected: a separate builder per profile — duplicates the start/shutdown/DNS wiring for one config field.

## Design

Signatures only, no bodies, no comments inside snippets.
This is a *delta* on [softap-provisioning-v1.md](softap-provisioning-v1.md)'s Design section — read that first; only the changes are sketched here.

### Profile and field groups (`provisioning-pure`)

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaProfile {
    LorawanFieldDevice,
    WifiMqttDevice,
}

impl SchemaProfile {
    pub fn fields(self) -> &'static [Field];
}

#[derive(Clone, PartialEq, Eq)]
pub struct LoraFields {
    pub dev_eui_hex: heapless::String<EUI_HEX_LEN>,
    pub join_eui_hex: heapless::String<EUI_HEX_LEN>,
    pub app_key_hex: heapless::String<APP_KEY_HEX_LEN>,
}

impl LoraFields {
    pub fn dev_eui_hex(&self) -> &str;
    pub fn join_eui_hex(&self) -> &str;
    pub fn app_key_hex(&self) -> &str;
    pub fn to_lora_config(&self, region: lora_pure::Region) -> lora_pure::LoraConfig;
}

#[derive(Clone, PartialEq, Eq)]
pub struct MqttFields;

impl MqttFields {
    pub fn host(&self) -> &str;
    pub fn port(&self) -> u16;
    pub fn username(&self) -> Option<&str>;
    pub fn password(&self) -> Option<&str>;
    pub fn client_id(&self) -> Option<&str>;
}
```

### Amended `ProvisioningConfig` accessors and `parse_form`

```rust
impl ProvisioningConfig {
    pub fn wifi_ssid(&self) -> &str;
    pub fn wifi_password(&self) -> &str;
    pub fn ota_url(&self) -> &str;
    pub fn device_name(&self) -> &str;
    pub fn lora(&self) -> Option<&LoraFields>;
    pub fn mqtt(&self) -> Option<&MqttFields>;
    pub fn profile(&self) -> SchemaProfile;
    pub fn extras(&self) -> &[ExtraField];
}

pub fn parse_form(body: &str, profile: SchemaProfile) -> Result<ProvisioningConfig, FieldErrors>;
```

### Store key additions (`rustyfarian-esp-idf-provisioning`)

| Key           | Type                | Bytes | Group |
|:--------------|:--------------------|------:|:------|
| `profile`     | string              |     7 | all   |
| `mqtt_host`   | string              |     9 | MQTT  |
| `mqtt_port`   | string (see Q6)     |     9 | MQTT  |
| `mqtt_user`   | string              |     9 | MQTT  |
| `mqtt_pass`   | string (secret)     |     9 | MQTT  |
| `mqtt_client` | string              |    11 | MQTT  |

`SCHEMA_VERSION` becomes `2`; `profile` is written before `schema_ver`.

### `PortalConfig` addition

```rust
pub struct PortalConfig<'a> {
    pub ssid_prefix: &'a str,
    pub ap_password: Option<&'a str>,
    pub channel: u8,
    pub device_name: &'a str,
    pub firmware_version: &'a str,
    pub profile: SchemaProfile,
}
```

## Implementation Phases

Every phase ends with `just fmt` then `just verify`; additional gates listed per phase.
New example name: `idf_c3_provision_mqtt` (the `*provision*` routing case already exists in both `scripts/build-example.sh` and `scripts/flash.sh`, so the name matches with no script change — verified).

### Phase 1 — `provisioning-pure` profile mechanism + MQTT validators + tests

Add `SchemaProfile`, the `LoraFields` / `MqttFields` group structs, the profile-parameterised `parse_form`, the new `Field` variants (`MqttUri`, `MqttUser`, `MqttPass`, `MqttClient`), the `ValidationError::UnexpectedForProfile` variant, the MQTT validators, and the `MAX_FIELD_ERRORS = 9` capacity bump with all four comment sites amended (Q4).
This phase assumes the ADR 014 §2 `no_std` extraction of `rustyfarian-network-pure` has already landed, so the MQTT validators delegate to `validate_client_id` / `CLIENT_ID_MAX_LEN` directly.

Test coverage to enumerate:

- `mqtt_uri` shape boundaries: valid `mqtt://h:1883`; port `0` parses as a `u16` but is rejected by the validator (mirroring `validate_broker_port`'s `!= 0` rule); port `65536` fails `u16` parsing itself (a different error path) — both must map to the `mqtt_uri` field error; port missing rejected; host empty rejected; non-`mqtt://` scheme rejected.
- Auth-pair rules: both absent (anonymous); both present (auth); password without user rejected on `MqttPass`; user without password accepted.
- Client-ID boundaries: 23 accepted, 24 rejected, blank accepted (host-derived).
- Profile-mismatch fields: a field that is canonical in the *other* profile but not the active one is recorded as a single Form-level `UnexpectedForProfile` error (first body-level error wins), not folded into extras and not reported per-field.
- `Debug` redaction of `mqtt_pass` (and the existing `wifi_pass`) for the `WifiMqttDevice` profile.

Gate: `just test-provisioning`.

### Phase 2 — store schema v2 + migration + portal templates + builder threading

Bump `SCHEMA_VERSION` to `2`; add the MQTT keys and the `profile` discriminator (Q6); write `profile` before `schema_ver`; read absent-`profile` v1 records as `lorawan`.
Add `portal_lorawan.html` and `portal_wifi_mqtt.html`, selected per profile (Q5).
Thread `profile` through `PortalConfig`, the prefill / field-label / render paths, and the additive `/status` `"profile"` field (Q7).

- `mqtt_pass` joins the no-prefill secret set alongside `wifi_pass` and `app_key`: `load_prefill` / `Prefill` and the new `portal_wifi_mqtt.html` template must never carry an `{{MQTT_PASS}}` placeholder, so the secret is re-entered on every submission, identical to the v1 rules.

### Phase 3 — example `idf_c3_provision_mqtt` + build routing

Add `examples/idf_c3_provision_mqtt.rs` demonstrating the `WifiMqttDevice` host contract: open the store, check `is_provisioned`, run the builder with `SchemaProfile::WifiMqttDevice` if unprovisioned, `wait_committed`, log, reboot.
Verify the `*provision*` routing in `build-example.sh` / `flash.sh` matches the new name (it does — confirmed during drafting).

Gate: `just build-example idf_c3_provision_mqtt`.

### Phase 4 — docs / CHANGELOG / ROADMAP follow-through

Tick the State boxes and Session Log here; promote the locked answers into a "Locked at implementation" Decisions sub-table; add a `CHANGELOG.md` entry; update any "four-field" wording in `docs/ROADMAP.md`, `CHANGELOG.md`, `VISION.md`, and the README to reflect two profiles; add the ROADMAP entry.

Gate: `just lint-docs`.

## State

- [x] Design drafted (ADR 014 proposed)
- [x] ADR 014 accepted (2026-06-12)
- [x] Open-question proposals signed off (2026-06-12)
- [x] Phase 0 — `rustyfarian-network-pure` `no_std` extraction (ADR 014 §2)
- [x] Phase 1 — profile mechanism + MQTT validators + tests
- [x] Phase 2 — store schema v2 + templates + builder threading
- [x] Phase 3 — example + build routing
- [x] Core implementation (Phases 0–3, 2026-06-12)
- [x] Host tests written (`provisioning-pure` profile + MQTT coverage)
- [x] Phase 4 — docs / CHANGELOG / ROADMAP
- [ ] Verification gates green — `just fmt` / `just verify` / `just test-provisioning` / `just build-example idf_c3_provision_mqtt` (and `idf_c3_provision`) plus the `cargo build -p rustyfarian-network-pure --no-default-features` `no_std` check pending on the maintainer machine (no Rust toolchain in the implementation sandbox)
- [x] On-hardware smoke test of the `wifi_mqtt` load path (ESP32-C3, 2026-06-12)

## Session Log

- 2026-06-12 — Feature request from `rustyfarian-rgb-clock` (a new ESP32 device, first mention in this repo) for a Wi-Fi + MQTT + OTA + device-name provisioning profile with no LoRaWAN.
  ADR 014 and this implementation plan drafted via an agent pass against the `soft-ap` branch triad and the existing MQTT crate surface.
  Awaiting maintainer sign-off of ADR 014 and the seven proposed answers above.
- 2026-06-12 — Maintainer review pass; nine findings adopted: the no_std mechanism for `rustyfarian-network-pure` lifted to an ADR §2 sub-decision (`#![cfg_attr(not(feature = "std"), no_std)]`, default `std` feature, extraction before Phase 1); §2 delegation argument reframed to lead with the requirement before the mechanism; `ProvisioningConfig::profile()` accessor added; profile-mismatch handling made explicit rejection via a Form-level `ValidationError::UnexpectedForProfile` (replacing extras-folding); `mqtt_port` storage locked to string; the `profile` NVS key reserved alongside the `x_*` rule; the port boundary test split into the `0`-rejected-by-validator and `65536`-fails-u16-parse paths; `mqtt_pass` added to the no-prefill secret set; and a version-axes note distinguishing feature-doc `v1` from NVS `schema_ver = 2`.
  The tracking-issue suggestion was declined per the no-issue-tracker convention; the feature-doc State checklist is the sign-off anchor.
- 2026-06-12 — ADR 014 accepted (maintainer signed off verbally; this counts as acceptance) and Phases 0–3 implemented the same day against the `soft-ap` branch.
  Implementation deviations and locked choices (authoritative list):
  - MQTT length caps set to 64 (`MQTT_HOST_MAX_LEN` / `MQTT_USER_MAX_LEN` / `MQTT_PASS_MAX_LEN`); the doc had left caps open, 64 was chosen.
  - `SchemaProfile` gained `as_str` / `from_str` alongside `fields()`, so the NVS `profile` discriminator round-trips through one source of truth.
  - `mqtt_uri` port failures map to `InvalidUrl` on **both** paths — the `0`-rejected-by-validator path and the `65536`-fails-`u16`-parse path.
  - The `LoraFields` / `MqttFields` group structs carry manual redacting `Debug` impls (redacting `app_key_hex` and `mqtt_pass` respectively), matching the `LoraConfig` / `ProvisioningConfig` precedent.
  - `QoS` and the `SubscribeClient` trait joined the `std` gate in `rustyfarian-network-pure` (the trait's `anyhow` dependency forced the pair), and `anyhow` was made optional behind the `std` feature; the `no_std` core keeps `validate_client_id`, `CLIENT_ID_MAX_LEN`, the topic validators, `backoff.rs`, and `status_colors.rs`.
  - The `/status` document schema bumped 1→2 on an axis independent of the NVS `schema_ver`, with the additive `"profile"` field; the version-axes note in Q6 covers the asymmetry.
  - `save()` writes only the active group's keys and removes the inactive group's keys, so `load` round-trips `None` for the absent group; v1 / absent-`profile` records load as `lorawan` with no re-provisioning.
  - Prefill is profile-aware: it recomposes `mqtt://host:port` and pre-fills only when the stored profile matches the portal profile; `mqtt_pass` joined the no-prefill secret set.
  - The `idf_c3_provision_mqtt` example constructs the downstream `MqttConfig` via a `rustyfarian-esp-idf-mqtt` dev-dependency (the established cross-crate example pattern) and uses a `derive_client_id` helper truncating the device name to 23 bytes on a char boundary when `mqtt_client` is blank; the existing `idf_c3_provision` example gained `profile: LorawanFieldDevice`.
  - The old combined `portal.html` was deleted in favour of `portal_lorawan.html` + `portal_wifi_mqtt.html`.
  - All gates deferred to the maintainer (no toolchain in the sandbox): `just fmt`, `just verify`, `just test-provisioning`, `just build-example idf_c3_provision_mqtt`, `just build-example idf_c3_provision`, and `cargo build -p rustyfarian-network-pure --no-default-features` to prove the `no_std` path (not covered by `just verify`).
    Follow-up suggestion (not actioned here): add a `justfile` recipe wrapping the `--no-default-features` `no_std` build so CI and the maintainer can gate it like the other checks.
- 2026-06-12 — On-hardware smoke test (ESP32-C3): `idf_c3_provision_mqtt` boots an already-provisioned `wifi_mqtt` device, confirming the schema-v2 `profile` discriminator round-trip from real NVS flash, absent optional MQTT keys loading as `None` (anonymous broker), the blank-`mqtt_client` device-name derivation (`client_id len=4` from name `Test`), lengths-only secret logging, and downstream `MqttConfig` construction.
  The flash itself exercised the corrected `*provision*`-before-`*mqtt*` routing in `scripts/build-example.sh` / `scripts/flash.sh`.
  Not yet exercised on hardware: the `portal_wifi_mqtt.html` submission path on a phone browser (same trailing manual validation as the v1 captive-portal smoke).
