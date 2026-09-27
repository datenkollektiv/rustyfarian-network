---
id: 002
title: Provisioning waiters clone ProvisioningConfig onto the caller's stack and overflow an 8 KB main task
captured-on: 2026-09-27
doc-version: 5
status: closed
kind: defect
---

# Bug 002: Provisioning waiters clone `ProvisioningConfig` onto the caller's stack and overflow an 8 KB main task

## Symptom
Right after the portal logs `Provisioning event: Committed`, the ESP-IDF tier panics with `Guru Meditation Error: Core 0 panic'ed (Stack protection fault)` in task `main`, and the caller's code after `run_wifi_mqtt_portal` never runs.

## Suspected Cause
Verified against source, not a hunch.

- `SharedState::wait_committed` (`mod.rs` line 273) returns `guard.committed.clone()`.
- `SharedState::wait_outcome` (`mod.rs` lines 337–344) does the same clone and wraps it in `SessionWait::Committed`.
- `run_wifi_mqtt_portal` (`boot.rs` line 307) calls `wait_outcome`, then discards the payload as `SessionWait::Committed(_config)`, so the consumer's path clones a config it never reads.
- The report names `wait_committed`; the symbolised frames (`Vec<ExtraField, 8>::clone`, `Option<ProvisioningConfig>::clone`) match both sites, and the reported call path goes through `wait_outcome`.
- `ProvisioningConfig` is a by-value struct of `heapless` strings, about 1.3 KB on a 32-bit target (see table), and the return value is a second copy on the same stack.
- The workspace's own `sdkconfig.defaults` sets `CONFIG_ESP_MAIN_TASK_STACK_SIZE=32768`, which is why no in-repo example ever hit this.

| Field                         | Bytes (rv32, `len: usize` included) |
|:------------------------------|:------------------------------------|
| `wifi_ssid` + `wifi_password` | 36 + 68                             |
| `ota_url` + `device_name`     | 132 + 28                            |
| `lora: Option<LoraFields>`    | ~80                                 |
| `mqtt: Option<MqttFields>`    | ~260                                |
| `extras: Vec<ExtraField, 8>`  | 8 × 88 + 4 = 708                    |

## Linked Artefact
- `crates/rustyfarian-esp-idf-network/src/provisioning/mod.rs` — `wait_committed`, `wait_outcome`, `set_committed`.
- `crates/rustyfarian-esp-idf-network/src/provisioning/boot.rs` — `run_wifi_mqtt_portal`.
- `crates/juggler/src/provisioning/config.rs` — `ProvisioningConfig` layout.
- Filed by a downstream consumer in `review-queue/rustyfarian-network-wait-committed-stack-clone.md` (now under `review-queue/archive/`).

## Reproduction Confidence
high

## Severity
high

## Environment
- `rustyfarian-esp-idf-network` 0.5.0; the code is unchanged in the unreleased 0.5.1 on `main` (`4082d70`).
- Reported from the first hardware run of the September 2026 stack on an ESP32-C6-DevKitC-1 by `rustyfarian-rgb-clock`, whose main task has `CONFIG_ESP_MAIN_TASK_STACK_SIZE=8000`.
- Chip-independent: the overflow is a function of the caller's stack budget, not the SoC.

## Expected Behaviour
- `run_wifi_mqtt_portal` returns `PortalOutcome::JustProvisioned` and the caller's next log line prints.
- A committed config costs the caller no more stack than the enum it asked for.

## Actual Behaviour
- The stack pointer ends 40 bytes below the main task's bound and the device reboots with `rst:0xc (SW_CPU)`.
- Because the config was persisted before the clone, the next boot takes the provisioned path and looks healthy, so the consumer only notices a missing "Provisioning committed" line and one extra reboot.
- The panic's stack dump printed the submitted Wi-Fi and MQTT passwords in clear text.

## Reproduction Steps
1. Build a consumer that calls `run_wifi_mqtt_portal` from the ESP-IDF main task with `CONFIG_ESP_MAIN_TASK_STACK_SIZE=8000` in its `sdkconfig.defaults`.
2. Flash, join the SoftAP, and submit a valid form with a few extra fields.
3. Watch `Provisioning event: Committed` followed by the stack-protection panic instead of the caller's next line.
4. Symbolise the backtrace with `riscv32-esp-elf-addr2line` against the release ELF to see the two `clone` frames.

## Suggested Fix Area
- In `wait_outcome`, stop cloning: `run_wifi_mqtt_portal` discards the payload, so `SessionWait::Committed` can carry nothing and the consumer's path allocates nothing.
- In `wait_committed`, move the config out with `guard.committed.take()` instead of `clone()`.
- A bare `take()` is not enough on its own: `set_committed` (`mod.rs` line 241) never sets `state = ProvisioningState::Committed`, and `resolve_wait` keys on `committed.is_some()`, so a second waiter after `take()` would block forever.
  Apply the `Committed` state transition in `set_committed` and derive the waiter's signal from the state.
- Document the caller's minimum stack budget on `run_wifi_mqtt_portal` and `wait_committed`, since both block the caller for the portal's whole lifetime.
- Secret hygiene is a separate item: `ProvisioningConfig` has no `Drop` scrub and `juggler` does not depend on `zeroize`.
  Scrubbing on drop would not have prevented this dump, because the panic printed live frames, but fewer stack copies means fewer places for the passwords to sit.
- Downstream workaround until the fix ships: raise `CONFIG_ESP_MAIN_TASK_STACK_SIZE` and run `just clean-idf`.

## Owner
Unassigned.

## Links
- Source report: `review-queue/archive/rustyfarian-network-wait-committed-stack-clone.md`.
- Related lore: `docs/project-lore.md` "SoftAP Provisioning".
- Related ADR: `docs/adr/014-wifi-mqtt-provisioning-profile.md` (the profile whose optional groups and extras set the struct size).
- Follow-up: `docs/bugs/archive/003-provisioning-config-no-drop-scrub-2026-09-27.md` (secret scrubbing on drop, split out of this report).

## Session Log
- 2026-09-27 — Captured as a defect from the review-queue report; both clone sites and the `run_wifi_mqtt_portal` call path verified against source, struct size estimated from the `heapless` bounds.
- 2026-09-27 — Fix implemented on `scouting-2026-09-27`: `SessionWait::Committed` carries no payload, `wait_committed` moves the config out with `take()`, `set_committed` applies `ProvisioningState::Committed`, and `juggler::provisioning::resolve_wait` keys on the state alone (host test added first, red then green). Caller stack budget documented on `run_wifi_mqtt_portal` and `wait_committed`. Left open: hardware re-check on an 8 KB main task, and the separate `zeroize` follow-up.
- 2026-09-27 — Secret-hygiene follow-up filed as bug 003; this report now covers the stack clone only. Still open pending the hardware re-check.
- 2026-09-28 — Bench on an ESP32-C3 Super Mini with `idf_c3_provision_mqtt` and `CONFIG_ESP_MAIN_TASK_STACK_SIZE=8000`: the pre-fix build (`4962f6a`) committed and restarted cleanly, so the example's shallow `main` cannot reproduce the overflow; a 6144-byte attempt was abandoned after the sdkconfig overlay was lost between terminal tabs.
- 2026-09-28 — Closed as fixed by construction: `SessionWait::Committed` carries no payload, `wait_committed` moves the config with `take()`, and the second-waiter contract is locked by the juggler host test. Field confirmation is deferred to the `rustyfarian-rgb-clock` bump to 0.5.1, where its main task stack can return from the 16384 workaround to 8000. Closed via /bug and archived.
- 2026-09-28 — Late review: `run_wifi_mqtt_portal`'s committed path had a publication race — `apply(PersistOk)` released the lock with `state = Committed` before the payload was stored, so a `wait_committed` waiter waking in that gap returned `None`; state and payload are now published under one lock (`SharedState::commit`, since moved to `juggler::provisioning::SessionState::commit` with host-run waiter regression tests in `session.rs`). The field check is still open and must measure, not just pass: on `rustyfarian-rgb-clock` at `CONFIG_ESP_MAIN_TASK_STACK_SIZE=8000`, log `uxTaskGetStackHighWaterMark(NULL)` right after `run_wifi_mqtt_portal` returns and record the remaining headroom here — removing the clone does not by itself establish the caller's full stack budget.
- 2026-09-28 — Branch review: the atomic `commit` returned nothing, so `/save` reported success (page + `Committed` event) after `commit` rejected a post-reset or double submission, and the 0.5.0 rule "a commit after a reset request still wins" had silently flipped. Now `SessionState::{apply, apply_and_notify, commit}` return `Result`; `/save` is refused with `409` before NVS is touched once the session is terminal or `Persisting`, `/factory-reset` likewise after a commit; the first terminal event stands, locked by host tests in `session.rs`. `wait_committed` is documented single-consumer and warns when a waiter finds the config already taken.
