# Feature: WifiMqttDevice boot helper v1

A thin, additive convenience layer in `rustyfarian-esp-idf-network` (`provisioning` feature) that owns the store read and portal lifecycle, so a `WifiMqttDevice` consumer writes only the app-specific seams. Sourced from a downstream feature request (rustyfarian-rgb-clock); see `review-queue/rustyfarian-network-provision-or-load.md` for the original proposal.

## Scope and end state

When done, a `WifiMqttDevice` consumer no longer copies `derive_client_id` and `mqtt_config_from_stored` out of `examples/idf_c3_provision_mqtt.rs`. Instead:

- An owned `WifiMqttBoot` bundle resolves a stored record into ready-to-borrow `WiFiConfig`/`MqttConfig` (no `Box::leak`, no `'static` gymnastics in the consumer).
- A loader + portal-runner pair drives the lifecycle: `WifiMqttBoot::load` (modem-free store read) and `run_wifi_mqtt_portal` (portal lifecycle with modem), each returning results the consumer matches on to decide boot / restart / erase.
- `examples/idf_c3_provision_mqtt.rs` is rewritten on top of the new API as the reference, demonstrating the line reduction (consumer ~150 → ~50 lines).
- The built-in client-id policy lives in one host-tested place (`juggler::mqtt`).

The library still never calls `restart()`/`erase` itself — those stay caller decisions.
The consumer owns the branch logic (store-read outcome) and the restart/erase logic; the boot layer owns the portal lifecycle and teardown signaling.
Despite the historical shorthand `provision_or_load` (still used in the branch and commit name), the accepted v1 API is the two-call split documented below — `WifiMqttBoot::load` + `run_wifi_mqtt_portal`.

## Proposed public API

```rust
// All items gated: #[cfg(all(feature = "provisioning", feature = "mqtt"))]

pub struct WifiMqttBoot { /* owns the resolved Wi-Fi + MQTT strings */ }

impl WifiMqttBoot {
    pub fn load(nvs: EspDefaultNvsPartition) -> anyhow::Result<WifiMqttLoadOutcome>;
    pub fn wifi_config(&self) -> WiFiConfig<'_>;
    pub fn mqtt_config(&self) -> MqttConfig<'_>;
}

#[non_exhaustive]
pub enum WifiMqttLoadOutcome {
    Ready(WifiMqttBoot),
    NotProvisioned,
    OtherProfile(SchemaProfile),
}

#[non_exhaustive]
pub enum PortalOutcome {
    JustProvisioned,
    FactoryResetRequested,
    PortalExitedWithoutCommit,
}

pub struct BootConfig<'a> {
    pub portal: PortalConfig<'a>,
    pub portal_timeout: Option<Duration>,
    pub on_event: Option<Arc<dyn Fn(ProvisioningEvent) + Send + Sync + 'static>>,
    pub client_id_fn: Option<Box<dyn Fn(&StoredConfig) -> anyhow::Result<String> + Send + 'static>>,
}

pub fn run_wifi_mqtt_portal(
    modem: Modem<'static>,
    sys_loop: EspSystemEventLoop,
    nvs: EspDefaultNvsPartition,
    config: BootConfig<'_>,
) -> anyhow::Result<PortalOutcome>;
```

*Illustrative API sketch; names and signatures may still shift before implementation.*

`OtherProfile` carries the existing public `SchemaProfile` enum directly (no new narrower type).
Use the variant name `NotProvisioned` (not `Unprovisioned`) consistently throughout.

**On `client_id_fn`:** The public override hook takes `&StoredConfig` (full-record access) and returns `anyhow::Result<String>` (validation failure is a real outcome). It is `Box<...>` (not `Arc`) because it is set once and consumed during derivation, never cloned across tasks. The built-in default derives the ID by delegating to a pure helper in `juggler::mqtt` — a separate function with signature `fn(operator_id: Option<&str>, device_name: &str, fallback: &str) -> anyhow::Result<String>` (host-tested) — that reads operator_id and device_name from `StoredConfig` and calls the helper with the `"rustyfarian"` fallback. The two signatures are intentionally different: the narrow one is the tested pure core; the `&StoredConfig` one is the ESP-IDF override seam.

## Decisions

|                                                                                                                                                                                                     Decision | Reason                                                                                                                                                                                                                                                                                                                                                                                    | Rejected Alternative                                                                                                                                                                                                                                                                                                                                         |
|-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------:|:------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|:-------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------|
| **Two-call split** — `WifiMqttBoot::load(nvs) -> Result<WifiMqttLoadOutcome>` (modem-free store read) + `run_wifi_mqtt_portal(modem, sys_loop, nvs, BootConfig) -> Result<PortalOutcome>` (portal lifecycle) | The modem only moves into the path that consumes it; ownership reads naturally. `ProvisioningBuilder::start` consumes `Modem<'static>` by value (`mod.rs:283`), so the modem is genuinely available on the already-provisioned path. `nvs.clone()` is Arc-cheap (example already clones at `:65`).                                                                                        | Single `provision_or_load(modem, …)` with an `AlreadyProvisioned { modem, sys_loop, nvs }` giveback variant. The helper would over-claim peripherals it might not use, then hand them back through the enum — a code smell that bloats the common match arm. The proposal itself conceded the split "has cleaner ownership."                                 |
|                                                                           **Owned `WifiMqttBoot` bundle hands out borrows** (`wifi_config(&self) -> WiFiConfig<'_>`, `mqtt_config(&self) -> MqttConfig<'_>`) | `WiFiConfig`/`MqttConfig` borrow `&str`; the owning strings must outlive them. An owned bundle that hands out borrows solves the lifetime problem without `Box::leak`.                                                                                                                                                                                                                    | Returning borrowed configs directly (lifetime can't be satisfied) or leaking strings to `'static` (the pattern `MqttHandle` uses for the client — avoided here for config).                                                                                                                                                                                  |
|                                                                                                                                       **Built-in client-id policy is the default; `client_id_fn` overrides** | One tested place for the 23-byte UTF-8-boundary truncation + non-empty fallback; pure logic in `juggler::mqtt`, host-testable. Override hook is cheap insurance for consumer-specific defaults.                                                                                                                                                                                           | Forcing every consumer to supply a policy (re-introduces the copy-paste) or hard-coding with no override.                                                                                                                                                                                                                                                    |
|                                                                                                                                                       **Forward `ProvisioningEvent` verbatim and unchanged** | `ProvisioningEvent` is a public, exhaustive enum; adding a variant is a breaking change and contradicts the "non-breaking" constraint. The portal-runner's return (`PortalOutcome`) is the authoritative teardown signal — it fires only after `shutdown()` completes on the consumer's thread (not on the httpd task). No app concepts leak upstream.                                    | Adding a terminal `PortalStopped` event (violates non-breaking contract) or a second typed state layer (adds extra concepts upstream).                                                                                                                                                                                                                       |
|                                                                       **Animation/indicator lifecycle stays entirely in the consumer** — crate exposes only the `on_event` hook + the `PortalOutcome` return | Honours "no LED colours/topics/ring counts upstream." `on_event` (already `Arc<dyn Fn + Send + Sync>`, `mod.rs:287`) is the *start/modulate* seam; the returned `PortalOutcome` is the *authoritative stop/teardown* seam.                                                                                                                                                                | `start_animation`/`stop_animation` hooks on `BootConfig` — two callbacks firing on different threads (httpd/wifi tasks) are harder to reason about than one event stream + one return value.                                                                                                                                                                 |
| **Profile mismatch in `WifiMqttLoadOutcome`; factory-reset in `PortalOutcome`** — crisp phase boundary | Profile mismatch is a load-time concept: a device provisioned under a non-`WifiMqttDevice` profile yields `WifiMqttLoadOutcome::OtherProfile(_)` (only from `load()`). Factory-reset is a portal-runtime concept, tracked in `PortalOutcome::FactoryResetRequested` (only from `run_wifi_mqtt_portal()`). Load queries "what's in storage?"; portal answers "how did execution terminate?" — separate concerns. `OtherProfile(_)` is NOT an error — it signals a valid stored record for a different device role/profile; callers should branch on it, not convert it back into `Err`.           | Merging both into one enum (conflates load-time vs runtime). Or omitting profile-mismatch entirely (leaves the unprovisioned-on-wrong-profile case unspecified).                                                                                                                                                                                              |
|                                                                                                                                                         **Client-id validation and operator-ID passthrough** | Operator-supplied IDs are validated on input and rejected if invalid (not silently truncated). Derived IDs (from `device_name`) are truncated to the 23-byte MQTT 3.1.1 cap on a UTF-8 boundary, with the empty fallback applied only to the derived path. Output of `client_id_fn` is also validated; the helper never hands invalid IDs to `MqttConfig`.                                | Passthrough with over-cap truncation (self-contradictory — passthrough and truncation can't both apply). Or no validation on `client_id_fn` output (leads to runtime MqttConfig errors instead of helper errors).                                                                                                                                            |
|                                                                                                                                                            **Client-id derivation lives in `juggler::mqtt`** | Per the workspace pure-first rule, the client-id logic is pure, host-testable, and lives in the pure crate. The pure helper signature is `fn(operator_id: Option<&str>, device_name: &str, fallback: &str) -> anyhow::Result<String>`. The ESP-IDF `WifiMqttBoot` delegates to this helper internally; consumers can override the entire derivation via `client_id_fn: Option<Box<dyn Fn(&StoredConfig) -> anyhow::Result<String>>>` on `BootConfig`. | All logic in ESP-IDF only (violates pure-first). Or duplication across multiple helpers.                                                                                                                                                                                                                                                                     |
| **PortalOutcome vs Err contract** — expected lifecycle exits vs operational failures | Expected control-flow exits are outcomes: `PortalOutcome::JustProvisioned`, `FactoryResetRequested`, `PortalExitedWithoutCommit`. Operational failures (SoftAP start, store open, DNS/httpd failures) are `Err`. Importantly: a **successful commit yields `JustProvisioned`** even if the subsequent best-effort `shutdown()` errors (logged, not surfaced); non-commit paths still propagate shutdown errors as `Err`. Tradeoff: callers can no longer distinguish "commit + shutdown clean" from "commit + shutdown degraded" — this is intentional (caller restarts anyway, tearing down). A future `JustProvisioned { shutdown_warned: bool }` could restore that observability without breaking, provided the shutdown error is logged with enough structure to diagnose. | Surfacing all outcomes as `Result<Outcome, Error>` (blurs control-flow vs operational errors). Or always masking shutdown errors (loses diagnostics but over-hides failure severity).                                                                                                                                                                           |
| **Derived-ID fallback is `"rustyfarian"`** | A weak, shared last-resort fallback applied only when `device_name` is empty — rare, since `device_name` is normally a non-empty compile-time constant. Caveat: a static fallback shared across many unprovisioned devices risks MQTT client-id collisions; collision-sensitive deployments should supply `client_id_fn` to derive a MAC-unique id. Future refinement: the ESP-IDF side could default the fallback to a MAC-suffixed id (mirroring the SoftAP SSID pattern). | No fallback (causes panics on empty device_name). Or a per-device random/MAC-based fallback in the library (adds complexity; overrides operator choice).                                                                                                                                                                                                         |
| **Borrowed-config use pattern** | `wifi_config()`/`mqtt_config()` are created on demand and handed straight to `WiFiManager::new`/`MqttBuilder::new`; both borrows may be held simultaneously and the accessors may be called repeatedly — they return shared `&` borrows of strings owned by `WifiMqttBoot`, which must outlive them. | Leaking config strings to `'static` (introduces global state). Or owning configs in consumers (violates the "single ownership" invariant).                                                                                                                                                                                                                     |

## Example consumer flow

```rust
let boot = match WifiMqttBoot::load(nvs.clone())? {
    WifiMqttLoadOutcome::Ready(b) => b,
    WifiMqttLoadOutcome::NotProvisioned => {
        let outcome = run_wifi_mqtt_portal(peripherals.modem, sys_loop, nvs, boot_config)?;
        indicator_cancel.store(true, Ordering::Relaxed);
        indicator.join();  // join before restart — never leave a WS2812 frame mid-transfer
        match outcome {
            PortalOutcome::JustProvisioned
            | PortalOutcome::FactoryResetRequested      // caller erases, then restarts
            | PortalOutcome::PortalExitedWithoutCommit  // or retry, sleep, AP-mode, fallback UX…
            => esp_idf_svc::hal::reset::restart(),
        }
    }
    WifiMqttLoadOutcome::OtherProfile(_) => {
        // provisioned under another profile — caller decides (factory-reset to re-provision or other action)
        esp_idf_svc::hal::reset::restart();
    }
};
let wifi = boot.wifi_config();
let mqtt = boot.mqtt_config();
// hand straight to WiFiManager::new(wifi) / MqttBuilder::new(mqtt)
```

Restarting on every arm is this example's policy, not a library default — restart/erase remain caller decisions (see Constraints).

## Constraints

- **Additive, non-breaking.** All new names; existing `ProvisioningBuilder`, `ProvisioningStore`, `PortalConfig`, `StoredConfig`, `ProvisioningSession` untouched. Minor version bump; items marked "Experimental: API may change before 1.0."
- **Feature gating.** The boot module's MQTT-touching items (`WifiMqttBoot`, `mqtt_config()`, the runner) are gated `#[cfg(all(feature = "provisioning", feature = "mqtt"))]`. The `provisioning` feature implies `wifi` but NOT `mqtt`, and must NOT implicitly enable `mqtt`.
- **No new Cargo feature** — lives under the existing `provisioning` feature.
- **The library never reboots or erases itself.** `restart()`/erase stay caller decisions.
- **Modem ownership.** The portal consumes `Modem<'static>`; the two-call split keeps the modem out of the load path so it's never stranded or double-moved.
- **Lifetimes.** Borrowed `WiFiConfig`/`MqttConfig` must be outlived by `WifiMqttBoot`; no `Box::leak`, no consumer-side `'static` gymnastics.
- **`on_event` is `Send + Sync`** (runs on the `httpd` task). The indicator hook keeps that bound.
- **Targets.** Must compile for `riscv32imac-esp-espidf` and `riscv32imc-esp-espidf` under `features = ["wifi", "mqtt", "provisioning"]`.
- **Three-way session wait (real change to session code).** The session wait must return `enum SessionWait { Committed(ProvisioningConfig), FactoryResetRequested, TimedOut }`. Today `wait_committed(None)` waits EXCLUSIVELY for a committed config — a factory-reset changes state but neither notifies the condvar nor causes the wait loop to inspect it, so the promised `FactoryResetRequested` outcome cannot occur on an indefinite (`portal_timeout: None`) wait. The fix requires the factory-reset path to notify the condvar AND the wait loop to inspect that state change.

### Event / animation contract (to document on the API)

- **Events = react/modulate** (any thread): `PortalStarted`, `ClientConnected`, `SubmissionRejected`, `Committed`, `FactoryResetRequested`.
- **`PortalOutcome` = authoritative teardown** (consumer's thread): set the cancel flag and **join the indicator thread before `restart()`** so a WS2812 transfer isn't left mid-frame (LED latches its last colour until reboot).
- **`PortalStarted` fires *after* the AP is already up** (`mod.rs:342`, synchronous inside `start()`). Start any "coming up…" animation *before* calling the runner; use `PortalStarted` only to transition to the "portal ready / awaiting client" state.

## Open Questions

- [ ] Confirm the `juggler::mqtt` pure-helper signature `fn(Option<&str>, &str, &str) -> anyhow::Result<String>` (note it is now fallible).

## State

- [x] Design approved
- [ ] Core implementation
- [ ] Tests passing
- [ ] Documentation updated

## Acceptance criteria

1. `WifiMqttBoot::load` + `run_wifi_mqtt_portal` compile for `riscv32imac-esp-espidf` and `riscv32imc-esp-espidf` under `features = ["wifi", "mqtt", "provisioning"]`; MQTT items gated `#[cfg(all(feature = "provisioning", feature = "mqtt"))]`.
2. Rustdoc on the gated items clearly states they require `provisioning` + `mqtt` (so users don't hit confusing cfg-hidden references).
3. Client-id policy host-tested in `juggler::mqtt`: derived-ID truncation on a UTF-8 boundary, empty→fallback (`"rustyfarian"`), valid operator-ID passthrough, INVALID operator-ID rejected (error), and `client_id_fn` output validated/rejected when invalid.
4. `WifiMqttBoot::{wifi_config, mqtt_config}` borrow soundly — no `Box::leak`, no `'static` gymnastics in the consumer.
5. Profile mismatch: a record stored under a non-`WifiMqttDevice` profile yields `WifiMqttLoadOutcome::OtherProfile(_)`, not an error or a panic.
6. Factory-reset termination: an indefinite (`portal_timeout: None`) runner returns `PortalOutcome::FactoryResetRequested` when the portal's factory-reset is triggered (i.e. the condvar is notified and the wait loop inspects the state).
7. Auth mapping: `mqtt_config()` maps (user+pass)→`with_auth`, (user only)→`with_username_only`, (neither)→anonymous.
8. Errors from store `load()` and session `shutdown()` propagate (no `unwrap`/swallow) — except: a successful commit yields `PortalOutcome::JustProvisioned` even when the subsequent `shutdown()` errors (logged, not surfaced).
9. `idf_c3_provision_mqtt.rs` is rewritten on the new API as the reference, showing the line reduction and the cancel-flag + thread-join indicator pattern.
10. The rewritten `idf_c3_provision_mqtt.rs` no longer contains local equivalents of `derive_client_id` or `mqtt_config_from_stored` (the copy-paste it exists to eliminate).
11. The library still never calls `restart()`/`erase` itself.

## Session Log

- 2026-06-22 — Third review pass (final). Unified `client_id_fn` to `Box<dyn Fn(&StoredConfig) -> anyhow::Result<String>>` and split it explicitly from the fallible `juggler::mqtt` pure helper `fn(Option<&str>, &str, &str) -> anyhow::Result<String>`; documented the shutdown-observability tradeoff and `JustProvisioned { shutdown_warned }` evolution path; softened the example's restart policy with explanatory comments and post-code note; stated `OtherProfile` caller policy (branch, not error); marked the API sketch illustrative; added the no-duplicate-policy acceptance criterion. Open questions reduced to 1 (confirm pure-helper signature is fallible).
- 2026-06-22 — Second review pass. Added public-API sketch and example consumer flow sections; fixed the profile-mismatch / factory-reset enum-boundary inconsistency (profile-mismatch in `WifiMqttLoadOutcome` only; factory-reset in `PortalOutcome` only); added PortalOutcome-vs-Err contract decision; documented commit-durability-over-shutdown behaviour; resolved the derived-ID fallback to `"rustyfarian"` with collision caveat in Decisions; added naming-drift note to intro; added rustdoc requirement to acceptance criteria. Updated variant names: `NotProvisioned` (consistent). Open questions narrowed to `juggler::mqtt` signature confirmation only.
- 2026-06-22 — Feature doc revised after design review. Resolved decisions: dropped `PortalStopped` as breaking (return is authoritative teardown), typed `WifiMqttLoadOutcome` to express profile mismatch, renamed `BootOutcome`→`PortalOutcome`, added three-way `SessionWait` for factory-reset handling, added feature-gating constraint, fixed client-id rule (reject invalid operator ID, truncate derived only), relocated derivation to `juggler::mqtt`. Doc renamed off the rejected single-entry name. Open questions narrowed to the fallback string and juggler signature.
- 2026-06-22 — Feature doc created from the rgb-clock review-queue proposal after a design discussion. Chose the two-call split over the single-entry giveback; added the `PortalStopped` terminal event and the event-vs-outcome animation contract; flagged the profile-mismatch / factory-reset `BootOutcome` variants.
