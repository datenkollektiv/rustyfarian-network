---
gate: none
desk-work: available
---

# Feature: Re-enter the provisioning portal after repeated Wi-Fi failures v1

Requested by `rustyfarian-rgb-clock` (2026-10-08, found during the OTA release-candidate hardware test on an ESP32-C3).
A provisioned device whose stored credentials stop working (mistyped password, or a router whose SSID or passphrase changed) must recover without a cable.
Today the only recovery is a full chip erase and reflash.

## Problem & evidence

The C3 stayed dark after provisioning with a mistyped password.
<details><summary>Evidence: boot log, disconnect reasons, code locations</summary>

Observed on the C3 (rgb-clock firmware on rustyfarian-network `2a3ac3f`) after provisioning with a mistyped password:

```text
I (3595) wifi:state: assoc -> run (0x10)
I (6645) wifi:state: run -> init (0xf00)
I (9745) wifi:state: init -> auth (0xb0)
I (10755) wifi:state: auth -> init (0x200)
E (30915) rustyfarian_esp_idf_network::wifi: WiFi connection timeout after 30 seconds
Error: Wi-Fi connection timeout after 30 seconds
I (31955) main_task: Returned from app_main()
```

Disconnect reason 15 (`4WAY_HANDSHAKE_TIMEOUT`) on two access points of the same SSID, then reason 2 (`AUTH_EXPIRE`): the passphrase is wrong, and the device stays dark until someone attaches a cable.

| Where | Behaviour | Evidence |
|:--|:--|:--|
| `WiFiManager::new` (`connect_with_led`) | Connects blocking inside the constructor and returns `Err` after the timeout | `crates/rustyfarian-esp-idf-network/src/wifi/mod.rs` ~332-365 |
| Disconnect reasons | Mapped and logged only when non-blocking; a blocking caller gets a bare timeout | `wifi/mod.rs` ~264-275 (`wifi_disconnect_reason_name`) |
| `WifiMqttBoot::load` | Returns `Ready` for any stored config; nothing records that it never connected | `provisioning/boot.rs` ~113 |
| `ProvisioningStore::erase_all` | Exists, but no boot path uses it on failure | `provisioning/store.rs` ~476 |
| Consumer | Propagates the error with `?`, `app_main` returns | rgb-clock `src/main.rs` ~148-154 |

</details>

A consumer can wrap the error and restart, but it does not see the disconnect reason, and every consumer would re-implement the same counter and portal hand-off.
A second ESP32-C3 consumer (filed 2026-10-26) needs a failed-join counter and a re-entry trigger.
Its need is design-level, not an observed failure, because its join is not written and its portal step is not yet flashed.
The trigger holds BOOT (GPIO9) for 3 s, sampled after boot, then erases `rf_prov` and restarts into the portal.

## Proposed change

A boot-level fallback in `rustyfarian-esp-idf-network::provisioning`, opt-in through `BootConfig` (or a new policy struct), so the `WifiMqttDevice` profile gets it with one setting.
Since commit `9604a9d` (2026-10-27), `WifiMqttBoot::load` hands out juggler's `ProvisioningConfig`.

1. **Classify each failed connect** as credential failure (`4WAY_HANDSHAKE_TIMEOUT`, `HANDSHAKE_TIMEOUT`, `AUTH_FAIL`, `MIC_FAILURE`, repeated `AUTH_EXPIRE`), network absent (`NO_AP_FOUND`: SSID renamed, or router off), or transient (everything else, e.g. beacon timeout, association leave, DHCP timeout), using a pure host-testable classifier next to `juggler::wifi::wifi_disconnect_reason_name` (`crates/juggler/src/wifi/mod.rs:401`).
   The second consumer needs the same three classes.
2. **Keep two counters** in NVS, one per class: credential failures open the portal at 5 consecutive failed boots (configurable), and network-absent boots use their own higher threshold (20 proposed; 0 means never).
   Counting never erases, and the counts survive power cycles.
   They live in library-owned reserved keys in `rf_prov` and clear on a successful join, on a commit, and when the fallback portal opens.
3. **Return `WifiMqttLoadOutcome::FallbackPortal { reason, failures }`** at the threshold instead of an error; the library never restarts, and the consumer restarts into the portal or continues.
   The portal prefills the form from the stored `ProvisioningConfig` (never the password) and keeps that config until a new one is committed, so an aborted or timed-out portal changes nothing.
   The portal closes after a 10 min window (configurable) without a submission, and the stored config is retried, so a router that was merely off returns without user action.
   The status LED signals the state, distinct from first-boot provisioning.
   The outcome lets the consumer log it, skip OTA and MQTT start-up, and keep its UI consistent.
   The second consumer needs a distinct variant for its log line and OLED state.
4. **Never end the boot on a Wi-Fi timeout** in this profile: below the threshold, back off and retry (or restart) rather than returning an error that ends `app_main`.
   The second consumer needs a load failure to keep the node running, with no `?` on the join path.
5. **Hardware re-entry trigger**: an optional hook the consumer answers once per boot, before the store is read; see [`provisioning-reentry-hook-v1.md`](./provisioning-reentry-hook-v1.md).
   The library reports the answer as an outcome; the consumer owns the GPIO, the hold time and the erase.

Only the consumer erases, on an explicit request (a `FactoryReset` hook answer or the portal's factory-reset outcome); the counter-driven path never calls `erase_all`.

<details><summary>Spookhouse benched counter (evidence)</summary>

Spookhouse benched a counter: failed joins are classified and counted in NVS (`sh_net`), limit 5, restart into a fallback portal that keeps and pre-fills the record, 10 min timeout, count cleared on a BOOT erase, a commit, and when the portal opens.
Spookhouse migrates to the library counter and drops its own once it lands; rgb-clock remains the first consumer.
Source: `review-queue/archive/rustyfarian-network-reentry-hook-reply-v1.md`.

</details>

## Decisions

| Decision | Reason | Rejected Alternative |
|:--|:--|:--|
| Fallback lives in the provisioning boot | Every Wi-Fi/MQTT device needs the same counter, reason mapping and hand-off | Each consumer wraps `WiFiManager::new` and restarts itself |
| Stored config kept until a new commit | A router outage must not erase working credentials | Erase the config when the fallback triggers |
| Two counters: credential and network-absent | A renamed SSID recovers, a brief router outage does not open the AP | One shared counter; or credential failures only |
| Credential threshold 5 consecutive boots, configurable | Spookhouse's benched `CREDENTIAL_STRIKE_LIMIT` | 3 consecutive failed boots |
| Network-absent threshold separate and higher, 0 = never | Spookhouse can stay credential-only with 0 | Same threshold as credentials |
| Return `WifiMqttLoadOutcome::FallbackPortal` at threshold | The library never restarts, as with the re-entry hook | Route the counter through `ReentryRequest::Portal` |
| Fallback portal window 10 min, configurable | Spookhouse's benched value | No timeout |
| Counts clear on join, commit, and portal open | A timed-out portal retries afresh instead of reopening at once | Keep the count across a portal timeout |
| Counts live in library-owned reserved keys in `rf_prov` | A consumer `erase_all` clears them with the record | A separate library NVS namespace |
| Re-entry trigger: pre-load hook returning an outcome | One entry point and outcome set; the library never erases | Library erases on a `FactoryReset` answer |

## Constraints

- Opt-in or default-on for `WifiMqttDevice` only; other profiles keep today's behaviour.
- No change for first-boot provisioning (unprovisioned device).
- The portal fallback must not run while an OTA image is pending verification, because the consumer's health deadline decides that image first (rgb-clock's OTA runtime rolls back an image that never reaches the network).
- The counter keys join `CANONICAL_KEYS` in `crates/juggler/src/provisioning/kv_store.rs`, so `erase_all` removes them.
- The new reserved keys need an ADR 014 §4 amendment.
- 0.6.x firmware ignores the new keys, so a downgrade still boots the stored record.

## Security

The fallback reopens the open SoftAP portal on a provisioned node; the options below decide the exposure.

<details><summary>Security prose</summary>

The fallback re-opens the open SoftAP portal on a device that was already provisioned, which widens rgb-clock's accepted first-boot exposure (`docs/features/wifi-softap-provisioning-v1.md`, "Security stance / threat model" there).
Anyone in radio range who can make the device's Wi-Fi fail (deauthentication, jamming) could reach the portal and re-provision it, including the MQTT broker — and the broker controls OTA, whose images are not yet signed.
The second consumer runs `portal_timeout: None` with an open AP unless `PROVISION_AP_PSK` is set (decided 2026-10-26).
A fallback portal on a deployed node is therefore open by default unless the consumer sets a PSK.

</details>

Options to weigh:

- [ ] Allow only the Wi-Fi fields in fallback mode; the MQTT broker stays as stored (changing it still needs a factory reset).
- [ ] Protect the fallback portal with a WPA2 passphrase derived from the device (printed on a label).
- [ ] Require physical presence, BOOT held ≥3 s and sampled after boot, to unlock broker changes.
  - On ESP32-C3 GPIO9 is a strapping pin, so BOOT held at reset enters download mode instead of the portal.
- [x] Count only credential failures toward the threshold; network-absent boots never open the portal (network-absent has its own threshold; 0 = never).

## Open Questions

- [x] Default threshold and portal window for the `WifiMqttDevice` profile (5 credential failures, 10 min).
- [x] Should a repeated `NO_AP_FOUND` (renamed SSID) open the portal at all, or only credential failures? (yes, on its own higher threshold)
- [ ] Default network-absent threshold — 20 boots proposed, 0 disables.
- [ ] Does the esp-hal tier need the same fallback now, or is it tracked separately? (spookhouse: no demand)
- [ ] Should the non-blocking connect path become the default for this profile, so reasons are available without a second code path?
- [x] Re-entry hook vs consumer-owned trigger: hook with outcomes, consumer erases (2026-10-27).
- [ ] Acceptance: a host test or bench run shows `ProvisioningStore::load` returns the pre-fallback record after a fallback portal times out with no commit.
- [ ] Acceptance: no `erase_all` call on the counter-driven fallback path, checkable in review.

## State

- [ ] Design approved
- [ ] Core implementation
- [ ] Tests passing
- [ ] Documentation updated

## Session Log

- 2026-10-08 — Created from the `rustyfarian-rgb-clock` request (mistyped password left the C3 dark); 2026-10-27 — a second consumer recorded, re-entry hook proposed, BOOT-at-reset option reworded.
- 2026-10-27 — Updated: re-entry hook decided (option a, consumer erases) and split into `provisioning-reentry-hook-v1.md`.
- 2026-10-28 — Updated: counter phase planned — separate credential and network-absent counters, threshold 5, FallbackPortal outcome, 10 min window, counts in rf_prov.
