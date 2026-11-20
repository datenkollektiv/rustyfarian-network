# ADR 014: Wi-Fi + MQTT Provisioning Profile

## Status

Accepted — maintainer signed off 2026-06-12; implemented the same day on the `soft-ap` branch.

The canonical sign-off anchor is the State checklist in the feature doc ([wifi-mqtt-provisioning-profile-v1.md](../features/archive/wifi-mqtt-provisioning-profile-v1.md)), which records acceptance.
No issue tracker exists in this workspace by convention.

## Context

A new ESP32 downstream, `rustyfarian-rgb-clock`, needs SoftAP provisioning.
It is the first device in the workspace that wants Wi-Fi credentials, an MQTT broker, an OTA URL, and a device name — and no LoRaWAN at all.
The provisioning triad shipped on the `soft-ap` branch ([ADR 013](013-softap-provisioning-acceptance.md), [feature doc](../features/archive/softap-provisioning-v1.md)).
It only knows one schema: the downstream four-field set (Wi-Fi credentials + LoRaWAN OTAA keys + OTA URL + device name).

ADR 013 §4 deliberately locked that single four-field schema and rejected a generic host-defined schema, because centralised form validation is the load-bearing piece of `provisioning-pure`.
ADR 013 also left an extras escape hatch: opaque `x_*` name/value pairs carried alongside the canonical fields.

Those extras are unsuitable for MQTT credentials.
They have no typed validation, no `Debug` redaction, and no secret handling in the portal.
The portal pre-fills extras as plain non-secret values.
Their NVS key name is capped at 13 bytes by the `x_` prefix rule.
Routing an MQTT password through the extras mechanism would defeat the credential-hygiene work ADR 013 was built around.

ADR 013's Consequences anticipated exactly this case.
Its "Negative" section noted that hosts needing fundamentally different provisioning data — "no LoRaWAN, no OTA" —
would otherwise "carry unused surface in their NVS layout, or layer their own provisioning UI alongside rather than on top".
`rustyfarian-rgb-clock` is that host.

The workspace North Star (`VISION.md:5`: "Any ESP32-IDF project can add Wi-Fi and MQTT in minutes, with confidence")
makes a Wi-Fi + MQTT provisioning profile the most on-brand profile the workspace could offer.
A device that provisions Wi-Fi and an MQTT broker through the shared portal is the literal embodiment of the North Star.

Five decisions need to be locked before implementation begins.
The feature doc ([wifi-mqtt-provisioning-profile-v1.md](../features/archive/wifi-mqtt-provisioning-profile-v1.md)) carries the seven implementation-level open questions that these decisions imply.

## Decision

### 1. A second provisioning schema profile is accepted: Wi-Fi + MQTT + OTA + device name

The schema model generalises from "the four-field schema" to a closed, workspace-defined set of named **profiles** built from reusable **field groups**.

The field groups are:

- **Core** — `wifi_ssid`, `wifi_pass`, `dev_name`.
- **LoRaWAN** — `dev_eui`, `join_eui`, `app_key`.
- **MQTT** (new) — broker host + port, optional username/password, optional client ID.
- **OTA** — `ota_url`.

Exactly two profiles exist:

- `LorawanFieldDevice` = Core + LoRaWAN + OTA — today's behaviour, unchanged.
- `WifiMqttDevice` = Core + MQTT + OTA (OTA optional since the 2026-09-26 amendment below) — the new profile `rustyfarian-rgb-clock` needs.

Generic host-defined schemas remain rejected, reaffirming ADR 013 §4.
Centralised validation is the load-bearing piece of `provisioning-pure`, and a generic schema scatters validation rules across every downstream.
A profile is not a generic schema — it is a closed, workspace-curated combination of field groups whose validation still lives in the pure crate.

Rejected alternatives:

- **Route MQTT credentials through the existing opaque extras** — extras have no typed validation, no `Debug` redaction, no portal secret handling, and a 13-byte key cap.
  This defeats the credential-hygiene work that motivated ADR 013.
- **A third crate or a per-downstream fork of the triad** — duplicates the SoftAP lifecycle, the captive portal, the DNS responder, and the NVS store for one new field group.
  The whole point of ADR 013 was to keep provisioning in one place.
- **Make every LoRaWAN field individually optional, ad hoc** — produces ambiguous boot states (which subset is required?) and offers no typed profile contract a host can match on.
  A closed set of named profiles is unambiguous; a bag of optional fields is not.

### 2. The MQTT field group is broker host + port, optional auth, optional client ID

The MQTT group is grounded in the actual consumer, `rustyfarian-esp-idf-mqtt`:

- **Broker host + port** — maps onto `MqttConfig::new(host, port, client_id)`.
  Presented in the portal as a single `mqtt_uri` form field (see feature doc Q1) and stored as separate `mqtt_host` + `mqtt_port` NVS keys.
- **Optional username/password auth pair** — maps onto `MqttConfig::with_auth(username, password)`.
- **Optional client ID** — maps onto the `client_id` argument of `MqttConfig::new`.

Validation delegates wherever a pure validator already exists.
Extending the established `no_std`-leaf delegation pattern (`wifi-pure`, `lora-pure`) to the MQTT group requires `rustyfarian-network-pure` to acquire a `no_std`-safe surface first.
Unlike those leaves, it is `std` today.
That is because `mqtt.rs:266` `spawn_subscriber_thread` uses `std::thread` and `std::sync`, and `mqtt.rs:18` `format_broker_url` returns `String`.
The client-ID rule is the 23-byte MQTT 3.1.1 cap (`CLIENT_ID_MAX_LEN = 23` in `rustyfarian-network-pure::mqtt`).

#### Sub-decision: `rustyfarian-network-pure` gains a `no_std`-safe surface

`rustyfarian-network-pure` is made `no_std`-compatible via `#![cfg_attr(not(feature = "std"), no_std)]` with a default-enabled `std` feature.
That feature gates only the two std-dependent items — `spawn_subscriber_thread` (`mqtt.rs:266`) and `format_broker_url` (`mqtt.rs:18`); everything else compiles under `no_std`.
The validators — `validate_client_id`, `CLIENT_ID_MAX_LEN`, and the topic validators — sit in the `no_std` core, as do `backoff.rs` and `status_colors.rs`, which have no std usage.
`provisioning-pure` depends on `rustyfarian-network-pure` with `default-features = false`, picking up the validators without dragging in `std`.
The MQTT consumers (`rustyfarian-esp-idf-mqtt`) keep the default `std` feature and are unaffected.

This extraction landed **before the MQTT validators** of the feature plan (2026-06-12), since those validators delegate into it.
The implementation also moved `QoS` and the `SubscribeClient` trait behind the `std` gate because the trait's `anyhow` dependency forced it.
It also made `anyhow` itself optional behind `std`.

Rejected alternatives:

- **A new `mqtt-pure` crate** — crate proliferation for a handful of validators; the `cfg_attr` feature gate keeps them in their existing home.
  Revisit only if a bare-metal MQTT downstream appears, mirroring the reserved-name pattern of ADR 013 §2.
- **Duplicate the 23-byte rule in `provisioning-pure`** — validator drift, the exact failure mode the delegation pattern exists to prevent.

Plain `mqtt://` only.
TLS stays out of scope, matching `format_broker_url`'s single hard-coded `mqtt://` scheme in `rustyfarian-network-pure` and the workspace's plain-transport posture.
That is the same posture ADR 011 took for plain-HTTP OTA.

### 3. The profile mechanism lives in `provisioning-pure`; the same two crates serve both profiles

No new crate is added.
`provisioning-pure` and `rustyfarian-esp-idf-provisioning` serve both profiles.

`parse_form` becomes profile-parameterised (a breaking signature change, acceptable because every public API in these crates is declared experimental).
`ProvisioningConfig` carries optional field groups so one type expresses both profiles.

This honours ADR 013 §2 (two crates at acceptance) and §3 (the portal is internal-only): the new profile threads through the existing crates rather than spawning a parallel surface.

### 4. NVS schema version bumps to 2 with an explicit `profile` discriminator key

The NVS layout gains a `profile` discriminator key (string-valued: `lorawan` or `wifi_mqtt`) and the `SCHEMA_VERSION` constant bumps from `1` to `2`.

`mqtt_port` is stored as a string, like every other value key in the namespace.
It is written once and read once per boot, and the string path adds zero new store surface.
The read-then-connect path reuses the existing `read_str` / `set_str` already in use for every other value.
This is decided plainly and does not depend on confirming `EspNvs::set_u16` / `get_u16` (those were unverified in-repo and are deliberately not relied upon).

Canonical namespace keys — `schema_ver`, `profile`, and the field keys — are reserved.
Host extensions must use the `x_*` prefix carried forward from ADR 013.
No collision with a host extension is possible today, and `profile` joins the reserved set explicitly.

Existing provisioned devices are not re-provisioned.
`load` treats `schema_ver == 1` with an absent `profile` key as the `lorawan` profile.
Downstream LoRaWAN devices already in the field keep working untouched.

The commit-guard ordering from ADR 013 is preserved: the `profile` key is written before `schema_ver`, so a torn write never reads as provisioned with an unknown profile.

### 5. BLE provisioning and TLS remain non-goals

BLE provisioning stays out of scope, carried forward unchanged from ADR 013 §5 and `VISION.md`.

MQTT-over-TLS stays out of scope, matching decision 2 and the workspace plain-transport posture.

## Rationale

### On a second profile rather than a generic schema

ADR 013 rejected a generic host-defined schema and accepted a single opinionated four-field set.
That was the right call for one downstream.
A second downstream with a genuinely different shape (no LoRaWAN) forces the question ADR 013 explicitly deferred to "Negative" consequences.

The cheapest answer that preserves ADR 013's load-bearing property — centralised validation — is not a generic schema and not a fork.
It is a *closed* set of named profiles assembled from validated field groups.
The pure crate still owns every validation rule; the host just selects which curated combination it wants.
Two profiles is a set, not an open extension point; a third profile is a future ADR, not a config flag.

### On grounding the MQTT group in the consumer

The MQTT field group is not designed in the abstract.
Every field maps onto a method already in `MqttConfig`: `new(host, port, client_id)` and `with_auth(username, password)`.
That keeps the provisioning surface honest — the portal can only collect what the MQTT crate can actually consume — and it mirrors how the LoRaWAN group is grounded in `LoraConfig::from_hex_strings`.

### On plain `mqtt://` only

`format_broker_url` hard-codes a single `mqtt://` scheme and its own doc-comment says "a future TLS variant would change the prefix here".
Accepting a `mqtts://` scheme in the portal would promise a transport the consumer cannot honour today, the same trap the OTA URL validator avoids by rejecting `https://` (ADR 011 plain-HTTP scope).

### On schema v2 with a discriminator

A device must know, at boot, which profile it was provisioned with, so it loads the right field group and builds the right runtime config.
A `profile` discriminator key is the minimal addition that answers this.
Bumping `schema_ver` to 2 while reading absent-`profile` v1 records as `lorawan` is a zero-migration upgrade: deployed downstream devices are correct by default.

## Consequences

### Positive

- **`rustyfarian-rgb-clock` gets a first-class profile** — Wi-Fi + MQTT + OTA + device name, validated and persisted by the shared triad, with no fork and no abuse of the extras mechanism.
- **The North Star is directly served** — a Wi-Fi + MQTT provisioning profile is the most on-brand provisioning the workspace can ship (`VISION.md:5`).
- **Validation stays centralised** — the new MQTT validators live in `provisioning-pure` alongside the Wi-Fi and LoRaWAN ones; no downstream re-implements them.
- **Backward compatible** — schema v2 reads v1 records as the `lorawan` profile; no field device is re-provisioned.

### Negative

- **The `Field` enum and the capacity proof grow** — `WifiMqttDevice` has eight canonical fields versus LoRaWAN's seven.
  `MAX_FIELD_ERRORS` becomes 9, and all four capacity-proof comment sites in `provisioning-pure` must be amended (see feature doc Q4).
- **The portal ships profile-specific HTML** — two complete templates selected per profile, accepting some duplicated head/CSS/nonce/footer markup (feature doc Q5).
- **`parse_form`'s signature breaks** — it gains a profile parameter.
  Acceptable: all public APIs are experimental.
- **`rustyfarian-network-pure` gains a `no_std`-safe surface** — `provisioning-pure` depends on it (`default-features = false`) for `validate_client_id`.
  That requires the `#![cfg_attr(not(feature = "std"), no_std)]` extraction in §2 to land before the MQTT validators.
  This is bounded work, not free.

### Implications

These landed with acceptance on 2026-06-12 and are recorded for the follow-through pass:

- `docs/ROADMAP.md` gains an entry for the Wi-Fi + MQTT profile.
- Any `VISION.md` / `README.md` / `CHANGELOG.md` wording that describes the schema as "four-field" is updated to reflect two profiles.
  (The "four-field" phrasing appeared in `docs/ROADMAP.md`, `CHANGELOG.md`, and ADR 013; it was not in `VISION.md` or the README, both of which were checked when the ADR landed.)
- The feature doc's seven open questions are signed off and the first four implementation phases are implemented.

## Amendment 2026-09-26 — OTA URL optional for WifiMqttDevice

The `ota_url` field is now optional for the `WifiMqttDevice` profile.
An absent or empty value is accepted and stored as an empty string; `ProvisioningConfig::ota_url()` returns `""`, meaning "no OTA configured".
A non-empty value is still validated (plain `http://` with a non-empty host, max 128 bytes; `https://` rejected per ADR 011).
`LorawanFieldDevice` keeps `ota_url` required — downstream field devices rely on OTA.
Motivation: `rustyfarian-rgb-clock` uses `WifiMqttDevice` but implements no OTA; users previously had to invent a placeholder URL.
NVS/flash layout unchanged — both stores already round-trip empty strings.

## Amendment 2026-10-27 — IDF in-memory type is `ProvisioningConfig`

The IDF tier now loads the stored record as `juggler::provisioning::ProvisioningConfig`, the same type the HAL tier and the portal commit use; `StoredConfig` is removed.
`ProvisioningConfig::ota_url()` returns `Option<&str>`, with `None` replacing the empty string described in the 2026-09-26 amendment.
The NVS key layout in §4 is unchanged, and records written by 0.6.x firmware load unchanged.
See `docs/features/archive/provisioning-boot-identity-v1.md`.

## Amendment 2026-11-16 — reserved portal-fallback counter keys in `rf_prov`

Two library-owned keys join the reserved canonical set in `rf_prov`: `wifi_fail_cred` counts credential failures and `wifi_fail_noap` counts network-absent failures.
Each holds a `u8` written with `set_u8` and read with `get_u8`, and the count saturates at 255.
A zero count is stored by removing its key, so a store without recorded failures has the key set of schema v2 as before.
`load` never reads the two keys, so the layout and read order in §4 are unchanged.
`save` (a commit) clears both keys best-effort, before the `profile` and `schema_ver` writes, so the commit-guard order from §4 is preserved.
`erase_all` removes both keys together with the record.
Records written by 0.6.x firmware have no counter keys and read as zero counts.
0.6.x firmware ignores the keys, so a downgrade boots the stored record.
The counters are advisory: they decide when the fallback portal opens and never erase or change the record (see `docs/features/wifi-portal-fallback-v1.md`).

## Amendment 2026-11-17 — journal keys for the Wi-Fi replacement in `rf_prov`

Three library-owned keys join the reserved canonical set in `rf_prov` for the fallback portal's Wi-Fi-only commit: `wifi_ssid_old` and `wifi_pass_old` hold the committed SSID and password backed up before a replacement, and `wifi_pending` is a `u8` marker that is set while a replacement is unfinished.
`KvProvisioningStore::save_wifi_credentials` copies the committed pair to the backups (`set_str`), sets the marker (`set_u8`), writes the new `wifi_ssid` and `wifi_pass` pair, and then removes the marker, which is the commit point.
The backups are removed and the counters cleared afterwards, both best-effort.
While `wifi_pending` is set, `is_provisioned` checks `wifi_ssid_old` and `load` reads the Wi-Fi pair from the backup keys, so a failure at any step leaves the old record readable; without the marker, both read the primary keys as before and ignore leftover backups.
ESP-IDF's `EspNvs::set_str` (esp-idf-svc 0.53) erases the key before writing it, so a failed string write leaves the key absent rather than unchanged, which is why `is_provisioned` also honours the journal.
A retry after a failed replacement keeps the backups of the first attempt.
`save` (a full commit) keeps `wifi_pending` until its value writes (the Wi-Fi pair, OTA URL, device name, profile group, extras and their index) have succeeded and removes it just before the `profile` and `schema_ver` writes, so a failed full commit never exposes the half-written pair of an earlier failed replacement, and a successful one is never shadowed; it removes the backups best-effort.
`erase_all` removes all three keys.
`save_wifi_credentials` returns `Err` and writes nothing when no record is stored.
The primary key layout and read order in §4 are unchanged.
0.6.x firmware ignores the three keys and reads the primary pair, so a downgrade during an unfinished replacement boots the primary pair, which may be half-written.
A 0.6.x commit does not clear `wifi_pending`, so an upgrade back reads the backup pair until this firmware's next `save` or `save_wifi_credentials` clears the marker.

## Amendment 2026-11-22 — the reserved `ddi_token` extra and the identity rule

The library reserves the extra key `ddi_token` (`juggler::provisioning::DDI_TOKEN_KEY`), stored under the `x_` prefix as `x_ddi_token`, for the fleet DDI profile's target token.
Unlike every other extra it is validated by the library whenever it is present (32 to 64 lowercase hex, reported on `Field::DdiToken`), so it is no longer opaque, and it stays redacted and scrubbed like every extra value.
`parse_form_with(body, profile, identity)` adds the `IdentityRule`, which binds the `WifiMqttDevice` profile only (a LoRaWAN form has no token input and its device no DDI path): under `Ddi` the token is required, the device name must be a device id (`^[a-z0-9][a-z0-9-]{2,19}$`, `juggler::identity`), `mqtt_user` must equal it and `mqtt_client` must be blank or equal it, so one device id serves the DDI controller id, the MQTT username and the MQTT client id (profile 0.4.0 §2).
`parse_form` keeps the open rule, and the NVS key layout of §4 does not change.
The Wi-Fi+MQTT template gains the `{{DDI_TOKEN_INPUT}}` slot, filled by the tiers with a password input that has no value under `Ddi` and with nothing otherwise, which keeps the rule that secrets are never pre-filled.

## References

- [ADR 013](013-softap-provisioning-acceptance.md) — SoftAP provisioning acceptance; locks the four-field schema and the extras escape hatch this ADR builds on and generalises.
- [ADR 011](011-ota-crate-hosting-and-transport.md) — OTA crate hosting and plain-HTTP transport; precedent for accepting a non-goal and keeping plain transport in scope.
- [docs/features/wifi-mqtt-provisioning-profile-v1.md](../features/archive/wifi-mqtt-provisioning-profile-v1.md)
  — feature doc carrying the seven implementation-level open questions and the phased plan.
- [docs/features/softap-provisioning-v1.md](../features/archive/softap-provisioning-v1.md) — the v1 provisioning triad this profile extends.
- `VISION.md:5` — the North Star this profile most directly serves.
- `crates/rustyfarian-esp-idf-mqtt/src/lib.rs` — `MqttConfig::new` / `with_auth`, the MQTT-group consumer.
- `crates/rustyfarian-network-pure/src/mqtt.rs` — `validate_client_id`, `CLIENT_ID_MAX_LEN`, `format_broker_url`.
