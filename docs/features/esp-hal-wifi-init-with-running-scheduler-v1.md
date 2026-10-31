---
gate: none
desk-work: none
---

# Feature: esp-hal Wi-Fi init with a running scheduler v1

`WiFiManager::init_async` and `init_softap_async` called `esp_rtos::start` themselves, so a firmware whose `main` already starts the scheduler (the knob, which must boot without Wi-Fi when no credentials exist) could not use them.
Worse, both validated the configuration first: a rejected SSID left the caller without `TIMG0`, `FROM_CPU_INTR0` and a scheduler, and every embassy timer hung.
Request U2e from `rustyfarian-knob`, folded into the ADR 021 change (U2d) so both ship in one PR.

## Decisions
| Decision                                                                                                                                  | Reason                                                                                                 | Rejected Alternative                                                                  |
|:------------------------------------------------------------------------------------------------------------------------------------------|:-------------------------------------------------------------------------------------------------------|:--------------------------------------------------------------------------------------|
| The scheduler is the firmware's: fourth point of ADR 021, next to console, panic handler, critical-section                                | Same ownership rule, one place to read it                                                              | A new ADR for one paragraph                                                           |
| Config-driven: `with_peripherals` keeps starting the scheduler, new `with_wifi_scheduler_running(WIFI)` assumes it runs; one `init_async` | Keeps `with_ampdu`, power save and TX power on both paths, no duplicated body                          | A second constructor `init_async_started(ssid, pw, wifi)` without the builder options |
| AP path gets `with_ap_wifi_scheduler_running(WIFI)` the same way                                                                          | One private `Scheduler` enum serves both; the knob's portal boot needs it next                         | STA only, AP later as U2f-style follow-up                                             |
| Both `init_*` and `ProvisioningBuilder::start` start the scheduler before validating                                                      | A `ConfigureFailed` or builder preflight error must never strand the firmware without a scheduler      | Validate first (the trap) or return the peripherals in the error                      |
| `hal_esp32s3_mqtt_lwt` starts the scheduler in `main` and uses the new variant                                                            | Exercises the knob's boot shape upstream on the board the knob maintainer owns                         | A twelfth example                                                                     |
| `ProvisioningBuilder::start_scheduler_running(spawner, wifi, store, rng)` next to `start`, one shared body                                | PR review: the portal fallback of a scheduler-owning firmware must not start it twice                  | Narrow portal-boot support to direct `ApConfig` callers                               |
| `hal_c3_provision_mqtt` starts the scheduler in `main`, uses the builder variant, provisioned branch awaits                               | Exercises the knob's full boot shape (portal or station after `esp_rtos::start`) on the C3 on the wire | Leave both provisioning examples on `start`                                           |

## Constraints
- Additive API only: `with_peripherals` and `with_ap_peripherals` keep their signatures and behaviour.
- `esp_rtos::start` runs once per boot in either variant; a second call panics in `esp-rtos`.
- `just check-hal`, `just clippy-hal` (all chips), `just test`, `just verify` and all 11 `just build-example hal_*` stay green.
- The rest of ADR 021 (console, panic handler, dev-dependencies) is unchanged.

## State
- [x] Design approved
- [x] Core implementation
- [x] Tests passing
- [x] Documentation updated
- [x] Hardware gate (builder path): `hal_c3_provision_mqtt` with the scheduler in `main` brought the portal up on the C3 Super Mini, 2026-10-31
- [x] Hardware gate: knob maintainer ran `hal_esp32s3_mqtt_lwt` with the firmware-owned scheduler on the CrowPanel, 2026-10-31 (record in `docs/runbooks/esp-hal-mqtt-hardware-test.md`)

## Session Log
- 2026-10-31 — Feature doc created from request U2e; implementation folded into the open `configurable-console` change
- 2026-10-31 — Implemented: `Scheduler` enum in both configs, `*_scheduler_running` variants, start-before-validate, S3 MQTT example adapted, ADR 021 amended, review applied; `just verify` + 11 examples green; CrowPanel re-run owed by the knob maintainer
- 2026-10-31 — C3 Super Mini regression run: `hal_c3_provision_mqtt` brings the SoftAP up through the reordered `init_softap_async` (scheduler first, then validation), portal, DNS and DHCP bound; the `*_scheduler_running` variant itself is still only exercised by the S3 example
- 2026-10-31 — Knob maintainer's CrowPanel run passed (console, scheduler, MQTT, 11/11 ticks, LWT, heap 25 928 B); record appended to the hal MQTT runbook; not covered: `with_ap_wifi_scheduler_running` on hardware and the invalid-SSID error path; feature complete
- 2026-10-31 — PR review: builder did not reach the new AP path; added `ProvisioningBuilder::start_scheduler_running`, `Scheduler` is `pub(crate)`, `hal_c3_provision_mqtt` moved to the firmware-owned scheduler; C3 portal run pending
- 2026-10-31 — C3 Super Mini: `hal_c3_provision_mqtt` with `esp_rtos::start` in `main` and `start_scheduler_running` brings the portal up (AP, DHCP, DNS, HTTP served to a phone); not exercised: the provisioned-branch idle after a portal commit, the invalid-SSID path; feature complete
- 2026-10-31 — PR review 2: the builder's own preflight (profile, SSID, password) ran before the scheduler start; `start_with` now calls `ensure_started` first and hands `init_softap_async` a `Running` marker; docs aligned
