# Feature: SoftAP Provisioning v1

Feature doc for the SoftAP captive-portal provisioning triad accepted by
[ADR 013](../adr/013-softap-provisioning-acceptance.md).
The implementation plan below was drafted 2026-06-11 and implemented the same day (Phases 1–5).
All open questions are resolved and signed off; the Decisions table records the locked architecture across three sub-tables (ADR 013, planning pass, and implementation).
The Design section remains an illustrative sketch; where the code deviated, the Session Log records it.

## Decisions

### Locked by ADR 013

|                                                                                         Decision | Reason                                                                                                                                                                           | Rejected Alternative                                                                                                                                          |
|-------------------------------------------------------------------------------------------------:|:---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|:--------------------------------------------------------------------------------------------------------------------------------------------------------------|
|                                            Provisioning lives in `rustyfarian-network` workspace | Runtime dependency surface is dominated by Wi-Fi/NVS plumbing already provided here (same argument as ADR 011 for OTA)                                                           | In-beekeeper module (violates "drivers live in shared crates"); separate `rustyfarian-provisioning` repo (duplicates Wi-Fi pinning)                           |
|               Two crates at acceptance: `provisioning-pure` + `rustyfarian-esp-idf-provisioning` | Matches the established `*-pure` + `rustyfarian-esp-idf-*` triad used by wifi/lora/espnow/ota                                                                                    | Single combined crate (breaks host-testability of the form/state-machine logic)                                                                               |
|                                           Bare-metal `rustyfarian-esp-hal-provisioning` deferred | No bare-metal downstream has requested it; speculative work would mean a `no_std` HTTP server with no consumer                                                                   | Build the bare-metal crate up front for "dual-HAL completeness"                                                                                               |
|                                                      Captive-portal HTTP server is internal-only | Preserves the README's "general-purpose HTTP clients out of scope" line; mirrors ADR 011's private OTA HTTP client                                                               | Export the HTTP server as a reusable workspace API (would need a wider vision change)                                                                         |
|        Four-field provisionable schema (Wi-Fi creds + LoRaWAN OTAA keys + OTA URL + device name) | Matches the requesting downstream's needs verbatim; also the union of NVS fields every Rustyfarian field device stores today                                                     | Wi-Fi credentials only (forces beekeeper to build half of what it asked for); generic host-defined schema (scatters validation rules across every downstream) |
|                                                                BLE provisioning stays a non-goal | No downstream has asked for it; ESP-IDF BLE stack is a substantial new dependency; SoftAP solves the same problem on hardware every device already uses                          | Accept BLE provisioning alongside SoftAP                                                                                                                      |

### Locked at planning pass (signed off 2026-06-11)

|                                                                                         Decision | Reason                                                                                                                                                                           | Rejected Alternative                                                                                      |
|-------------------------------------------------------------------------------------------------:|:---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|:----------------------------------------------------------------------------------------------------------|
| SoftAP lifecycle is added to `rustyfarian-esp-idf-wifi`, not built inside the provisioning crate | The constraint "no parallel Wi-Fi stack" implies AP mode belongs next to the existing STA lifecycle; `rustyfarian-esp-idf-wifi` is STA-only today (`Configuration::Client` only) | AP lifecycle private to `rustyfarian-esp-idf-provisioning` (duplicates Wi-Fi setup and TX-power handling) |
|                OTAA credential validation delegates to `lora-pure::LoraConfig::from_hex_strings` | One authoritative hex/length/byte-order implementation; provisioning maps its failure to a typed field error                                                                     | Second hex parser in `provisioning-pure` (drift risk, double maintenance)                                 |
|                          Concrete error enums in `provisioning-pure`, following `ota-pure` style | Validation errors must be structured and matchable by the HTTP layer and host tests; the exact enum shape is sketched in the Design section                                      | `&'static str` errors as in `wifi-pure` (loses structure, forces string comparison in tests)              |

These rows began as a planning-pass proposal and were signed off alongside the resolved open questions on 2026-06-11.

### Locked at implementation (2026-06-11)

These rows capture the durable outcomes promoted from the resolved open questions plus the implementation deviations.
The amendments that produced them are itemised in the 2026-06-11 implementation Session Log entry.

|                                                                                                          Decision | Reason                                                                                                                                                                                                       |
|------------------------------------------------------------------------------------------------------------------:|:-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
|                              SoftAP SSID is `{prefix}-XXXX` from `derive_softap_ssid(prefix, mac)` in `provisioning-pure` | Last-two-MAC-bytes hex suffix guarantees uniqueness for several unprovisioned devices on one table; lives in the pure crate so it is host-testable and reusable by a future `esp-hal` triad (Q1)             |
|                            Single NVS namespace `rf_prov`, one key per field, hex-string LoRa values, `schema_ver` written last | One logical commit unit, single-namespace factory reset, zero re-encoding on load-then-join; `schema_ver` last means torn writes never look provisioned (Q2). An `extras_idx` key indexes `x_*` extras (deviation below) |
|                                       Single-shot validate-all / commit-all NVS write; host-driven reboot, no in-process handoff | Incremental saves create ambiguous boot states; reboot-based handoff sidesteps ESP-IDF AP↔STA mode-switch bugs (Q3)                                                                                          |
|                            `/status` is `{schema, state, provisioned}` required plus optional `device_name`/`firmware_version`/`uptime_s`/`extra` | Minimal static-plus-session schema, host-injected extras via `with_status_entry`, no live telemetry (no ADC / LoRa-stack access); session-scoped per ADR 013 §3 (Q4)                                          |
|                                   Factory reset is `ProvisioningStore::erase_all()` plus `ProvisioningEvent::FactoryResetRequested` | Library signals reset intent only; host owns the destructive operation and the reboot (Q5)                                                                                                                   |
|                                  Per-field error list: `Field` × `ValidationError`, `heapless::Vec<FieldError, 8>` | Portal HTML must highlight the offending input; at most one error per canonical field plus one form-level error makes `MAX_FIELD_ERRORS = 8` exact (Q6)                                                       |
|                            `pub(crate)` UDP catch-all DNS responder bound to `0.0.0.0:53`, lifecycle-bound to the session | Without a wildcard responder the OS captive-portal sheet rarely appears; `std::net::UdpSocket` needs zero new dependencies; manual `192.168.4.1` stays documented as fallback (Q7)                            |

## Constraints

- Must build on `rustyfarian-esp-idf-wifi` SoftAP lifecycle — no parallel Wi-Fi stack.
- Must use `esp-idf-svc` NVS for credential persistence — no custom flash layout.
- HTTP server is `pub(crate)` inside `rustyfarian-esp-idf-provisioning`; not re-exported.
- `provisioning-pure` is `no_std` so a future `rustyfarian-esp-hal-provisioning` can adopt it without an API break.
- Compile-time `.env` values via `option_env!` remain a valid fallback when NVS is empty (same pattern as `idf_esp32s3_join`).
- Provisioning-mode entry (no NVS credentials, button hold, repeated Wi-Fi failure) is the host application's decision, not the library's.
- The library never calls `esp_restart()`; rebooting after commit is the host's decision.
- NVS values are stored in plaintext; flash/NVS encryption is a partition concern owned by the host firmware, not this crate.
- Event callbacks run on the `httpd` task and must return quickly and never block (same rule as `MqttBuilder::on_connect`; the rule is cited from the MQTT crate rustdoc, where the SUBACK-deadlock lore lives).
- Secrets are never echoed back into HTML: `GET /` pre-fills non-secret fields only, and `wifi_pass` and `app_key` must be re-entered on every submission.
- Each session carries a per-session nonce (hidden `_nonce` field) checked on `POST /save` and `POST /factory-reset`; a mismatch is rejected `403`, defending the open AP against request forgery.
- `ota_url` accepts `http://` only, matching the ADR 011 plain-HTTP OTA scope.

## Open Questions — Resolved (signed off 2026-06-11)

All seven questions were reviewed by the agent-team pass, signed off, and implemented; the durable outcomes are promoted into the "Locked at implementation" Decisions sub-table above.
The original proposals are retained below as the rationale of record.

### 1. SoftAP SSID derivation — Resolved: configurable prefix and last-two-MAC-bytes hex suffix

- `derive_softap_ssid(prefix, mac)` produces `{prefix}-{XXXX}` where `XXXX` is the uppercase hex of the AP MAC's last two bytes, truncated so the SSID fits 32 bytes.
  Workspace default prefix is `Rustyfarian`; beekeeper passes `Beekeeper` to get its `Beekeeper-XXXX`.
  The MAC suffix guarantees uniqueness when several unprovisioned devices sit on the same table — the exact beekeeper field scenario.
  The derivation lives in `provisioning-pure` so it is host-testable and reusable by a future `esp-hal` triad.
  Rejected: fixed `Beekeeper-` prefix (couples a workspace crate to one downstream user's branding).

### 2. NVS namespace and key layout — Resolved: single namespace `rf_prov`, one key per field, hex-string LoRa values

- Namespace `rf_prov` with keys `schema_ver` (u8, value 1), `wifi_ssid`, `wifi_pass`, `lora_dev_eui`, `lora_join_eui`, `lora_app_key`, `ota_url`, `dev_name`; opaque extras stored as `x_{name}` with `name` validated to ≤13 chars (NVS key limit is 15).
  A single namespace makes the commit one logical unit and factory reset a single-namespace erase.
  LoRaWAN values are stored as the validated MSB-first hex strings matching `LoraConfig::from_hex_strings` input, so load-then-join needs zero re-encoding.
  `schema_ver` future-proofs layout migrations.
  Rejected: namespace per field category (more handles and erase complexity for zero isolation benefit, since fields commit together per question 3).

### 3. Save semantics — Resolved: single-shot validate-all, commit-all, host-driven reboot

- One form whose POST is validated as a complete set; only a fully valid submission is written to NVS, in one pass, after which the session enters `Committed` and the host is notified via the event callback.
  Incremental per-field saves create ambiguous boot states (Wi-Fi saved, LoRa keys missing) that every downstream would need recovery rules for.
  Single-shot also eliminates in-process AP-to-STA radio handoff: commit, host reboots, normal boot loads NVS — sidestepping ESP-IDF mode-switch bugs.

### 4. `/status` JSON schema — Resolved: minimal static-plus-session schema, host-injected extras, no live telemetry

- Schema: `{"schema":1,"device_name":"…","firmware_version":"…","state":"awaiting_submission","provisioned":false,"uptime_s":42,"extra":{…}}`.
  Required fields are `schema`, `state`, and `provisioned`; `device_name`, `firmware_version`, `uptime_s`, and `extra` are optional conveniences, and the exact payload above is illustrative, not a frozen external contract.
  Live battery and LoRa telemetry are explicit v1 non-goals — the provisioning crate has no ADC or LoRa-stack access, and adding one would violate ADR 013's two-crate decision.
  The builder accepts `with_status_entry(key, value)` string pairs rendered under `"extra"`, so beekeeper injects a battery reading it measured itself.
  This keeps `/status` session-scoped per ADR 013 §3.

### 5. Factory-reset hook — Resolved: event callback enum plus explicit `erase_all()` on the store

- Two surfaces: `ProvisioningStore::erase_all()` for host-triggered resets, and `ProvisioningEvent::FactoryResetRequested` delivered via the builder's `on_event` callback when the portal's reset button is pressed.
  Callback matches the `MqttBuilder` precedent; a host wanting channel semantics wraps the callback around its own sender in one line.
  The library only ever signals reset intent; the host application remains solely responsible for invoking destructive reset operations.
  Rejected: NVS-flag polling (flash wear, latency, and it inverts the constraint by letting the library make the reset decision).

### 6. Form-validation error reporting — Resolved: per-field error list with concrete enums

- `parse_form` returns `heapless::Vec<FieldError, 8>` pairing a `Field` enum (seven canonical fields plus `Form` for body-level problems) with a `ValidationError` enum (`Missing`, `Empty`, `Duplicate`, `TooLong`, `InvalidHex`, `InvalidUrl`, `MalformedBody`, `TooManyFields`).
  Per-field beats a single string because the portal HTML must highlight the offending input.
  Duplicate keys are rejected rather than silently resolved last-wins or first-wins; a well-formed portal form never produces them, so rejection is unambiguous.
  Duplicate canonical keys error as `Duplicate` on their own field; duplicate extra keys fold into a single `Form`-level `Duplicate`.
  The parser therefore records at most one error per canonical field plus one form-level error, so the capacity of 8 is exact, not a guess.
  Both enums implement `core::fmt::Display` so the IDF crate renders messages without `alloc` in the pure crate.

### 7. Captive-portal DNS (new question) — Resolved: ship a minimal `pub(crate)` UDP catch-all responder in v1

- Without a wildcard DNS responder answering every A query with the AP IP, the OS captive-portal sheet rarely appears automatically, undermining the "no toolchain in the field" premise.
  ESP-IDF `std` provides `std::net::UdpSocket`, so the responder is roughly 100 lines with zero new dependencies.
  It is `pub(crate)` and lifecycle-bound to the session, breaching the "no general-purpose DNS API" line no more than the private HTTP server breaches the HTTP one.
  OS captive-portal heuristics vary by platform and are not guaranteed; the catch-all improves detection but does not ensure the portal sheet opens on every client.
  Manual navigation to `192.168.4.1` therefore stays documented as the fallback instruction.

## Design

The snippets below are an illustrative API sketch, not a frozen contract; implementation may deviate where the code demands it, with deviations recorded in the Session Log.
Doc convention: signatures only, no implementation bodies, no comments inside snippets.

### `wifi-pure` additions (stays `no_std`)

`validate_ap_config` reuses `validate_ssid`, checks the WPA2 minimum password length (8) and channel range 1–13.
Error style stays `&'static str` for internal consistency with the rest of `wifi-pure`.

```rust
pub const AP_PASSWORD_MIN_LEN: usize = 8;
pub const AP_MAX_CONNECTIONS_DEFAULT: u8 = 4;

#[derive(Debug, Clone)]
pub struct ApConfig<'a> {
    pub ssid: &'a str,
    pub password: Option<&'a str>,
    pub channel: u8,
    pub max_connections: u8,
    pub tx_power: TxPowerLevel,
}

impl<'a> ApConfig<'a> {
    pub fn open(ssid: &'a str) -> Self;
    pub fn wpa2(ssid: &'a str, password: &'a str) -> Self;
    pub fn with_channel(self, channel: u8) -> Self;
    pub fn with_max_connections(self, max: u8) -> Self;
    pub fn with_tx_power(self, level: TxPowerLevel) -> Self;
}

pub fn validate_ap_config(config: &ApConfig<'_>) -> Result<(), &'static str>;
```

### `rustyfarian-esp-idf-wifi` SoftAP extension

This satisfies the "must build on `rustyfarian-esp-idf-wifi` SoftAP lifecycle" constraint: the AP lifecycle lives here and the provisioning crate consumes it as a normal dependency.
The implementation uses `Configuration::AccessPoint` from `esp-idf-svc 0.52` and applies the same `esp_wifi_set_max_tx_power` path as the STA manager.
The ESP32-C3 Super Mini antenna lore plausibly applies to AP beacons too, so the default is `TxPowerLevel::Medium` with `Low` documented for C3 Super Mini boards.
`ap_mac` backs the SSID derivation; `ap_ip` backs the DNS responder (the default ESP-IDF AP netif is 192.168.4.1 but it is read, never assumed).
No mixed AP+STA mode in v1, per the reboot-based handoff in question 3.

```rust
pub struct SoftApManager;

impl SoftApManager {
    pub fn start(
        modem: Modem<'static>,
        sys_loop: EspSystemEventLoop,
        nvs: Option<EspDefaultNvsPartition>,
        config: ApConfig<'_>,
    ) -> anyhow::Result<Self>;

    pub fn ap_ip(&self) -> anyhow::Result<Ipv4Addr>;
    pub fn ap_mac(&self) -> anyhow::Result<[u8; 6]>;
    pub fn station_count(&self) -> anyhow::Result<u16>;
    pub fn stop(self) -> anyhow::Result<()>;
}
```

### `provisioning-pure` (new crate, `no_std`)

Crate layout mirrors `ota-pure`: `lib.rs`, `error.rs`, `form.rs`, `config.rs`, `state.rs`, `ssid.rs`.
Dependencies: `heapless`, `wifi-pure` (for `validate_ssid`/`validate_password`/`SSID_MAX_LEN`), `lora-pure` (for `LoraConfig::from_hex_strings` and `Region`).

```rust
pub const DEVICE_NAME_MAX_LEN: usize = 24;
pub const OTA_URL_MAX_LEN: usize = 128;
pub const EXTRA_FIELDS_MAX: usize = 8;
pub const EXTRA_KEY_MAX_LEN: usize = 13;
pub const EXTRA_VALUE_MAX_LEN: usize = 64;
pub const MAX_FIELD_ERRORS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    WifiSsid,
    WifiPassword,
    DevEui,
    JoinEui,
    AppKey,
    OtaUrl,
    DeviceName,
    Form,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationError {
    Missing,
    Empty,
    Duplicate,
    TooLong { max: usize },
    InvalidHex { expected_len: usize },
    InvalidUrl,
    MalformedBody,
    TooManyFields,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldError {
    pub field: Field,
    pub error: ValidationError,
}

pub type FieldErrors = heapless::Vec<FieldError, MAX_FIELD_ERRORS>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtraField {
    pub key: heapless::String<EXTRA_KEY_MAX_LEN>,
    pub value: heapless::String<EXTRA_VALUE_MAX_LEN>,
}
```

Both enums implement `core::fmt::Display`.
`Field::form_name()` returns the HTML input name (`wifi_ssid`, `dev_eui`, …) so the portal HTML, the parser, and the tests share one source of truth.

`parse_form` is single-stage: it percent-decodes an `application/x-www-form-urlencoded` body, maps known input names to fields, collects unknown pairs as opaque extras (the ADR 013 §4 extension mechanism), and validates everything, accumulating per-field errors instead of failing fast.
OTAA validation calls `LoraConfig::from_hex_strings` and maps failure to `InvalidHex { expected_len }` — no second hex parser.
Wi-Fi validation delegates to `wifi-pure`.
URL validation is a deliberately shallow shape check (`http://` prefix, non-empty host, length cap) consistent with ADR 011's plain-HTTP OTA scope.
Duplicate canonical keys produce `Duplicate` on their own field; duplicate extra keys fold into a single `Form`-level `Duplicate`; the parser thus records at most one error per canonical field plus one form-level error, making `MAX_FIELD_ERRORS = 8` exact.
The `Debug` impl redacts the Wi-Fi password and AppKey, following the `LoraConfig` precedent.

```rust
#[derive(Clone, PartialEq, Eq)]
pub struct ProvisioningConfig;

impl ProvisioningConfig {
    pub fn parse_form(body: &str) -> Result<Self, FieldErrors>;
    pub fn wifi_ssid(&self) -> &str;
    pub fn wifi_password(&self) -> &str;
    pub fn dev_eui_hex(&self) -> &str;
    pub fn join_eui_hex(&self) -> &str;
    pub fn app_key_hex(&self) -> &str;
    pub fn ota_url(&self) -> &str;
    pub fn device_name(&self) -> &str;
    pub fn extras(&self) -> &[ExtraField];
    pub fn to_lora_config(&self, region: lora_pure::Region) -> lora_pure::LoraConfig;
}

pub fn derive_softap_ssid(prefix: &str, mac: &[u8; 6]) -> heapless::String<32>;
```

The state machine is terminal at `Committed`, like `OtaState::Booted`.

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisioningState {
    AwaitingSubmission,
    Persisting,
    Committed,
    FactoryResetPending,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisioningInput {
    ValidSubmission,
    InvalidSubmission,
    PersistOk,
    PersistFailed,
    FactoryReset,
}

impl ProvisioningState {
    pub fn apply(self, input: ProvisioningInput) -> Result<ProvisioningState, InvalidTransition>;
    pub fn as_str(self) -> &'static str;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidTransition {
    pub state: ProvisioningState,
    pub input: ProvisioningInput,
}
```

Transition table: `AwaitingSubmission` + `ValidSubmission` → `Persisting`; `AwaitingSubmission` + `InvalidSubmission` → `AwaitingSubmission`; `Persisting` + `PersistOk` → `Committed`; `Persisting` + `PersistFailed` → `AwaitingSubmission`; `FactoryReset` is accepted from `AwaitingSubmission` only; every input in `Committed` is an `InvalidTransition`.
`as_str` feeds the `/status` `state` field.

### `rustyfarian-esp-idf-provisioning` (new crate)

Crate layout mirrors `rustyfarian-esp-idf-ota`: public `lib.rs` facade, `pub(crate) mod portal` (EspHttpServer wiring plus embedded HTML via `include_str!`), `pub(crate) mod dns`, public store re-exported at the root.
Re-exports the pure types (`ProvisioningConfig`, `ProvisioningState`, `Field`, `FieldError`, `ValidationError`, `derive_softap_ssid`).

```rust
#[derive(Debug, Clone)]
pub struct PortalConfig<'a> {
    pub ssid_prefix: &'a str,
    pub ap_password: Option<&'a str>,
    pub channel: u8,
    pub device_name: &'a str,
    pub firmware_version: &'a str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProvisioningEvent {
    PortalStarted,
    ClientConnected,
    SubmissionRejected,
    Committed,
    FactoryResetRequested,
}

pub struct ProvisioningBuilder<'a>;

impl<'a> ProvisioningBuilder<'a> {
    pub fn new(config: PortalConfig<'a>) -> Self;
    pub fn with_status_entry(self, key: &'a str, value: &'a str) -> Self;
    pub fn on_event<F>(self, f: F) -> Self
    where
        F: Fn(ProvisioningEvent) + Send + Sync + 'static;
    pub fn start(
        self,
        modem: Modem<'static>,
        sys_loop: EspSystemEventLoop,
        nvs: EspDefaultNvsPartition,
    ) -> anyhow::Result<ProvisioningSession>;
}

pub struct ProvisioningSession;

impl ProvisioningSession {
    pub fn state(&self) -> ProvisioningState;
    pub fn wait_committed(&self, timeout: Option<Duration>) -> Option<ProvisioningConfig>;
    pub fn shutdown(self) -> anyhow::Result<()>;
}
```

`start` derives the SSID, brings up `SoftApManager`, the `pub(crate)` DNS responder thread, and the `pub(crate)` `EspHttpServer`; the session owns all three plus the shared state.
`wait_committed` is the blocking convenience the host's provisioning-mode main loop sits in; on success the host typically logs and reboots.
`shutdown` drops the HTTP server first, then DNS, then `SoftApManager::stop()`, so nothing answers on a dead netif.
Any `EspSubscription` used for `ClientConnected` AP events is stored in the session struct (dropped subscriptions fire zero times — known lore).

`ProvisioningStore` is public because the host boot path needs `is_provisioned`/`load` on every normal boot and `erase_all` for its factory-reset trigger, independent of any portal session.

```rust
pub struct StoredConfig {
    pub wifi_ssid: String,
    pub wifi_password: String,
    pub dev_eui_hex: String,
    pub join_eui_hex: String,
    pub app_key_hex: String,
    pub ota_url: String,
    pub device_name: String,
    pub extras: Vec<(String, String)>,
}

pub struct ProvisioningStore;

impl ProvisioningStore {
    pub fn open(partition: EspDefaultNvsPartition) -> anyhow::Result<Self>;
    pub fn is_provisioned(&self) -> anyhow::Result<bool>;
    pub fn load(&self) -> anyhow::Result<Option<StoredConfig>>;
    pub fn save(&mut self, config: &ProvisioningConfig) -> anyhow::Result<()>;
    pub fn erase_all(&mut self) -> anyhow::Result<()>;
}
```

Portal routes: `GET /` (form, pre-filled from NVS if present), `POST /save`, `GET /status`, `POST /factory-reset`, plus common OS captive-portal probe endpoints (such as `/generate_204`, `/hotspot-detect.html`, `/ncsi.txt`) answering `302 → http://{ap_ip}/`.
The concrete probe list is an implementation detail expected to evolve with OS behaviour; only the four functional routes are part of the design.
The `EspHttpServer` `Configuration` sets `max_uri_handlers` explicitly (default is 8; this design uses about that many).

## Implementation Phases

Phases 1→2 and 3 are parallelisable; 4 needs 1–3; 5 needs 4; 6 is last.
Every phase ends with `just fmt` and `just verify`; additional gates listed per phase.
New example name: `idf_c3_provision` (follows `idf_<chip>_<purpose>`; C3 matches the existing Wi-Fi example fleet and the cheap Super Mini field hardware).

### Phase 1 — `wifi-pure` AP config types

Add `ApConfig`, `validate_ap_config`, AP constants, and host tests to `crates/wifi-pure`.
Gate: `just test-wifi`.

### Phase 2 — `rustyfarian-esp-idf-wifi` SoftAP lifecycle

Add `SoftApManager` wrapping `Configuration::AccessPoint`, TX-power application, and rustdoc with the C3 antenna caveat; re-export `ApConfig` alongside the existing pure re-exports.

### Phase 3 — `provisioning-pure` crate

New crate `crates/provisioning-pure`; root `Cargo.toml` workspace dependency entry; `justfile` gains `check-provisioning-pure` and `test-provisioning`, with `test-provisioning` joining the `test` aggregate.
Host-test coverage: percent-decoding (`+` as space, `%zz` malformed, multibyte UTF-8), each field's accept/reject boundary (SSID 32/33, password 64/65, EUI 15/16/17 chars, AppKey 31/32/33, mixed-case hex, URL shape), error accumulation across several bad fields in one body, extras capture and overflow at `EXTRA_FIELDS_MAX`, duplicate-key rejection (canonical and extra keys, `Duplicate` on the affected field), the full state-machine transition table including every `InvalidTransition`, SSID derivation truncation at 32 bytes, and `Debug` redaction of the Wi-Fi password and AppKey.
CodeQL rule applies: fixture constants named `TEST_PSK`-style, never `password`-named literals.
Gate: `just test-provisioning`.

### Phase 4 — `rustyfarian-esp-idf-provisioning` crate

New crate with `lib.rs` (builder, session, events), `store.rs` (NVS layout from question 2), `pub(crate) portal.rs`, `pub(crate) dns.rs`; root `Cargo.toml` and `justfile` entries.
Watch items from lore: store every `EspSubscription` in the session struct; no blocking host calls inside `httpd` handlers; set `max_uri_handlers` explicitly.

### Phase 5 — Hardware example + build routing

`examples/idf_c3_provision.rs` demonstrating the full host contract: `ProvisioningStore::open` → `is_provisioned` → if false (or the `option_env!` fallback is absent) run the builder → `wait_committed` → log → restart.
`scripts/build-example.sh` gains a `*provision*` routing case — mandatory, the script errors on unknown keywords today.
Any `sdkconfig.defaults` additions (httpd task stack, main task stack) go in the **workspace-root** file per the embuild resolution lore, followed by an `esp-idf-sys` build-dir clean.
Gate: `just build-example idf_c3_provision`.
Manual validation (not a merge gate): an on-hardware captive-portal smoke test with iOS and Android — the DNS catch-all behaviour is only verifiable on real phones and may trail the merge as a follow-up check.

### Phase 6 — Docs, CHANGELOG, roadmap follow-through

Tick the open-question checkboxes and State entries here; promote the locked answers (NVS table, `/status` schema) into the Decisions table; `CHANGELOG.md` entry; README crate inventory row; `docs/ROADMAP.md` status update.
Consider a `/project-lore` entry if the captive-portal probe-endpoint testing surfaces anything non-obvious.
Gate: `just lint-docs`.

## State

- [x] Design approved (ADR 013)
- [x] Implementation plan drafted (2026-06-11)
- [x] Open-question proposals signed off (2026-06-11)
- [x] Core implementation (Phases 1–5, 2026-06-11)
- [x] Host tests written (`provisioning-pure` + `wifi-pure` AP coverage)
- [ ] Verification gates green — `just verify` / `just test-provisioning` / `just build-example idf_c3_provision` pending on the maintainer machine (no Rust toolchain in the implementation sandbox)
- [x] Documentation updated (2026-06-11)

## Session Log

- 2026-06-11 — Feature doc stub created alongside ADR 013; original feature
  request was archived into this doc by acceptance and the review-queue file
  was deleted.
- 2026-06-11 — Walked through Decisions, Constraints, Open Questions, and State
  via `/feature`; confirmed all sections are correct as written and that the
  6 open questions are intentionally left for the implementer when work starts.
- 2026-06-11 — Implementation plan added via agent-team planning pass
  (codebase analysis + architecture design): API-level design for the crate
  pair plus the `rustyfarian-esp-idf-wifi` SoftAP extension, proposed answers
  to all six open questions plus a new seventh on captive-portal DNS, and a
  six-phase task breakdown with verification gates.
- 2026-06-11 — Review-feedback pass: normative-status framing added (hybrid-doc
  note, illustrative-sketch language, planning-pass Decision rows share the
  proposal sign-off gate), duplicate form keys now rejected via a `Duplicate`
  variant with the error-capacity rationale documented, `/status` split into
  required/optional fields, factory-reset ownership made explicit, DNS
  platform-variance caveat added, probe routes marked illustrative, and the
  iOS/Android smoke test reclassified as trailing manual validation.
- 2026-06-11 — Second review-feedback pass: Decisions split into "Locked by
  ADR 013" and "Provisional (planning pass)" sub-tables, DNS captive-portal
  claim softened from absolute to probabilistic, and duplicate extra-key
  accounting clarified (folds into a single `Form`-level `Duplicate`,
  preserving the exact `MAX_FIELD_ERRORS = 8` bound).
- 2026-06-11 — Review-and-implement pass: an agent-team review
  (architecture / security / API-feasibility) adopted all seven proposals with
  amendments, then Phases 1–5 were implemented in full (`wifi-pure` AP types,
  `rustyfarian-esp-idf-wifi` SoftAP lifecycle, `provisioning-pure`,
  `rustyfarian-esp-idf-provisioning`, and the `idf_c3_provision` example with
  build-script routing).
  Adopted amendments and implementation deviations (authoritative list):
  - Security blocker fixed: `GET /` pre-fills non-secret fields only; `wifi_pass`
    and `app_key` are never echoed into HTML and must be re-entered on every
    submission.
  - Per-session nonce (hidden `_nonce` field, checked on `POST /save` and
    `POST /factory-reset`, `403` on mismatch) defends the open AP against request
    forgery; form keys beginning with `_` are reserved and silently ignored by
    `parse_form`.
  - No-credential-logging rule adopted from the Wi-Fi crate (lengths only);
    `"<redacted>"` Debug token; `parse_form` hardened (never panics on malformed
    percent-escapes, all heapless pushes fallible, 2048-byte POST body cap → `413`).
  - `lora-pure`'s `from_hex_strings` returns `Option` without field attribution,
    so `provisioning-pure` does per-field hex length / charset validation itself
    and uses `from_hex_strings` only as the final constructor in `to_lora_config`
    (the `expect` is documented as unreachable-by-construction, guarded by a
    round-trip test); the form's `join_eui` maps to the `app_eui` parameter.
  - `AccessPointConfiguration.max_connections` is `u16` (cast from `ApConfig`'s
    `u8`); `station_count` wraps the unsafe `esp_wifi_ap_get_sta_list` sys call;
    the SSID-before-start chicken-and-egg is solved by a new `softap_mac()` efuse
    helper (`esp_read_mac`), so derivation needs no started interface.
  - `on_event` keeps `Send + Sync` (the callback is shared across httpd handlers
    via `Arc`), a documented divergence from `MqttBuilder`'s `Send`-only
    precedent; the "callbacks run on the httpd task" rule cites the MQTT crate
    rustdoc (the SUBACK lore lives there, not in `project-lore.md`).
  - `ApConfig` Debug is hand-written to redact the password; `Clone` hand-written
    alongside.
  - `Field::form_name` returns a `"_form"` sentinel for `Field::Form`
    (collision-free via the reserved-underscore rule); `InvalidTransition` lives
    in `state.rs`; over-cap extra keys / values fold into a single Form-level
    `TooManyFields`, preserving the `MAX_FIELD_ERRORS = 8` proof; `wifi_pass` must
    be present but may be empty (open STA networks); `ota_url` accepts `http://`
    only (ADR 011 scope); Debug redacts `wifi_password` + `app_key` but not the
    EUIs (device identifiers).
  - NVS store adds an `extras_idx` index key (`EspNvs` 0.52 cannot enumerate keys)
    so `load` / `erase_all` can find `x_*` extras — a deviation from pure
    one-key-per-field; `schema_ver` is written last so torn writes never look
    provisioned.
  - Portal: `max_uri_handlers` set to 12 (10 handlers — 4 functional + 6 probe
    routes); httpd stack 10240 via server `Configuration`; DNS catch-all thread
    `prov-dns` with an 8 KB stack bound `0.0.0.0:53` and a 500 ms shutdown-poll
    timeout; rejected POSTs do not round-trip entered values in v1.
  - Toolchain unavailable in the implementation sandbox: all `just` gates deferred
    to the maintainer; `Cargo.lock` will update on first build.
