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
| Disconnect reasons | Mapped and logged only when non-blocking; a blocking caller gets a bare timeout | `wifi/mod.rs` ~264-275 (`wifi_disconnect_reason_name`, since replaced by `juggler::wifi::DisconnectReason`) |
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

1. **Classify each failed connect** as credential failure (`4WAY_HANDSHAKE_TIMEOUT`, `HANDSHAKE_TIMEOUT`, `AUTH_FAIL`, `MIC_FAILURE`, repeated `AUTH_EXPIRE`), network absent (`NO_AP_FOUND`: SSID renamed, or router off), or transient (everything else, e.g. beacon timeout, association leave, DHCP timeout), using a pure host-testable classifier in `juggler::wifi` that maps the `DisconnectReason` kinds of the [disconnect-reason vocabulary feature](./disconnect-reason-vocabulary-v1.md).
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
5. **Hardware re-entry trigger**: an optional hook the consumer answers once per boot, before the store is read; see [`provisioning-reentry-hook-v1.md`](./archive/provisioning-reentry-hook-v1.md).
   The library reports the answer as an outcome; the consumer owns the GPIO, the hold time and the erase.

Only the consumer erases, on an explicit request (a `FactoryReset` hook answer or the portal's factory-reset outcome); the counter-driven path never calls `erase_all`.

<details><summary>Spookhouse benched counter (evidence)</summary>

Spookhouse benched a counter: failed joins are classified and counted in NVS (`sh_net`), limit 5, restart into a fallback portal that keeps and pre-fills the record, 10 min timeout, count cleared on a BOOT erase, a commit, and when the portal opens.
Spookhouse migrates to the library counter and drops its own once it lands; rgb-clock remains the first consumer.
Source: spookhouse's reply of 2026-10-28.

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
| Non-blocking connect for the `WifiMqttDevice` bundle from `load_with_fallback` | The classifier needs each attempt's reason; the blocking path ends in a bare timeout | Blocking connect with the last reason carried in the timeout error |
| Classifier builds on `juggler::wifi::DisconnectReason` | The vocabulary lands first and replaces `wifi_disconnect_reason_name(u16)` for both tiers | Classify raw `u16` reason codes |
| `wait_for_join(&wifi, window)` reads the reasons the link worker recorded; no new `WiFiManager` API | Keeps the load modem-free and adds no tier-parity surface on `WiFiManager` | Explicit `record_join_failure` / `record_join_success` methods, which would expose reasons on the public API |
| The fallback portal opens on the boot after the threshold, not in the failing boot | The station holds the modem in the failing boot, so the portal runs after a restart | Opening the portal in the failing boot |
| A consumer that stays up after `NotJoined` calls `wait_for_join` again until it returns `Joined`; only that call clears the counters after a late join | No NVS write on the event-loop task, and no new `WiFiManager` API | A clear hook in the link worker's `DhcpIpAssigned` handler |
| The network-absent threshold counts boots, so the consumer's restart cadence decides whether a router off overnight opens the AP | A restart after every 30 s window reaches 20 in about 10 min; `idf_c3_provision_mqtt` stays up longer per network-absent boot (about 14 h to 20) | A time-based network-absent threshold |
| A repeated `AUTH_EXPIRE` counts as credential evidence from its second occurrence in one boot (`AUTH_EXPIRE_REPEAT` = 2) | A single `AUTH_EXPIRE` stays transient; a repeat within one boot is the credential signal | Every `AUTH_EXPIRE` counts as credentials |
| The fallback commit replaces only the Wi-Fi pair through a journal (backups, `wifi_pending` marker, commit by removing the marker) | A failed write at any step keeps the old record readable, and a retry keeps the first attempt's backups | Roll back by re-saving the old record on error: a restore write can fail under the same persistent NVS fault, so the replacement is not atomic |
| A transient window leaves the boot uncounted, and a network-absent count moves to credentials when a later window of the same boot shows credential failures | Credential evidence in a later `wait_for_join` window still counts, and a boot still counts at most once | First expired window is final: a transient first window used up the boot's count, so later credential failures never counted |

## Constraints

- Opt-in or default-on for `WifiMqttDevice` only; other profiles keep today's behaviour.
- No change for first-boot provisioning (unprovisioned device).
- The portal fallback must not run while an OTA image is pending verification, because the consumer's health deadline decides that image first (rgb-clock's OTA runtime rolls back an image that never reaches the network).
- The counter keys join `CANONICAL_KEYS` in `crates/juggler/src/provisioning/kv_store.rs`, so `erase_all` removes them.
- The new reserved keys need an ADR 014 §4 amendment.
- 0.6.x firmware ignores the new keys, so a downgrade still boots the stored record.
- The classifier and counter policy live in `juggler` and are shared by both tiers; the esp-hal boot wiring is a recorded ADR 022 tier-parity gap and follows as a separate feature.

## Security

The fallback reopens the open SoftAP portal on a provisioned node; the options below decide the exposure.

<details><summary>Security prose</summary>

The fallback re-opens the open SoftAP portal on a device that was already provisioned, which widens rgb-clock's accepted first-boot exposure (`docs/features/wifi-softap-provisioning-v1.md`, "Security stance / threat model" there).
Anyone in radio range who can make the device's Wi-Fi fail (deauthentication, jamming) could reach the portal and re-provision it, including the MQTT broker — and the broker controls OTA, whose images are not yet signed.
The second consumer runs `portal_timeout: None` with an open AP unless `PROVISION_AP_PSK` is set (decided 2026-10-26).
A fallback portal on a deployed node is therefore open by default unless the consumer sets a PSK.

</details>

Options to weigh:

- [x] Allow only the Wi-Fi fields in fallback mode; the MQTT broker stays as stored (changing it still needs a factory reset). (rgb-clock: wanted, removes the OTA takeover path)
- [ ] Protect the fallback portal with a WPA2 passphrase derived from the device (printed on a label). (rgb-clock: not needed, its clocks have no label)
- [ ] Require physical presence, BOOT held ≥3 s and sampled after boot, to unlock broker changes. (rgb-clock: not wanted, the factory reset covers broker changes)
  - On ESP32-C3 GPIO9 is a strapping pin, so BOOT held at reset enters download mode instead of the portal.
- [x] Count only credential failures toward the threshold; network-absent boots never open the portal (network-absent has its own threshold; 0 = never).

## Open Questions

- [x] Default threshold and portal window for the `WifiMqttDevice` profile (5 credential failures, 10 min).
- [x] Should a repeated `NO_AP_FOUND` (renamed SSID) open the portal at all, or only credential failures? (yes, on its own higher threshold)
- [x] Default network-absent threshold — 20 boots proposed, 0 disables. (rgb-clock: 20; a router off overnight must not open the AP)
- [x] Does the esp-hal tier need the same fallback now, or is it tracked separately? (tracked separately as a tier-parity gap (ADR 022) since rgb-clock's request of 2026-11-16, which adds esp-hal demand from its clock; the classifier and counter policy live in `juggler` and are shared by both tiers; the esp-hal boot wiring follows as its own feature when rgb-clock files that request)
- [x] Should the non-blocking connect path become the default for this profile, so reasons are available without a second code path? (yes: the `WifiMqttDevice` profile connects non-blocking, the classifier reads each failed attempt's disconnect reason from the link worker, and part 4 follows from it without a second code path)
- [x] Re-entry hook vs consumer-owned trigger: hook with outcomes, consumer erases (2026-10-27).
- [ ] Acceptance: a host test or bench run shows `ProvisioningStore::load` returns the pre-fallback record after a fallback portal times out with no commit (open: `failed_boots_open_the_fallback_portal_and_keep_the_record` in `kv_store` covers the counting path only, and no host test covers the timeout path).
  The failed-commit path is host-tested by `a_failed_wifi_replacement_keeps_the_old_record`, but that does not close this item.
- [x] Acceptance: no `erase_all` call on the counter-driven fallback path, checkable in review (`provisioning/boot.rs` names `erase_all` only in doc comments; the examples call it only on an explicit factory-reset answer).
- [ ] Acceptance (rgb-clock): a mistyped password on the IDF clock opens the portal after 5 boots without anyone touching it, and the stored broker is unchanged afterwards.
- [x] A consumer may ignore `FallbackPortal` while an OTA image is pending verification, since the library cannot know the OTA state (rgb-clock enforces this).
- [ ] Maintainer to confirm: reasons 210, 211 and 212 (`NO_AP_FOUND` with compatible security, authmode or RSSI threshold) and `802_1X_AUTH_FAILED` (23) classify as transient, so a device that only sees them never reaches a threshold.
- [ ] Maintainer to confirm: a credential threshold of 0 also means never, as the network-absent threshold does.

## State

- [x] Design approved
- [x] Core implementation
- [x] Tests passing
- [x] Documentation updated

## Session Log

- 2026-10-08 — Created from the `rustyfarian-rgb-clock` request (mistyped password left the C3 dark).
- 2026-10-27 — A second consumer recorded, BOOT-at-reset option reworded, re-entry hook decided (option a, consumer erases) and split into `archive/provisioning-reentry-hook-v1.md`.
- 2026-10-28 — Updated: counter phase planned — separate credential and network-absent counters, threshold 5, FallbackPortal outcome, 10 min window, counts in rf_prov.
- 2026-11-12 — Updated: rgb-clock's reply of 2026-11-01 folded in (Wi-Fi-only fallback wanted, threshold 20, esp-hal later); it adopts the re-entry hook with 0.7.0
- 2026-11-16 — Design approved (parts 1–4): non-blocking connect is the profile default, the classifier builds on the disconnect-reason vocabulary, esp-hal recorded as a parity gap after rgb-clock's change of demand; Ready-band roadmap row added.
- 2026-11-16 — Core (juggler classifier, counter policy, reserved keys), IDF wiring (`load_with_fallback`, `wait_for_join`, Wi-Fi-only fallback portal) implemented; esp-hal not started (ADR 022 gap); rgb-clock hardware acceptance still open.
- 2026-11-16 — Review: a late join clears the counters only through another `wait_for_join` call (now documented); `idf_c3_provision_mqtt` restarts at once on a credential failure and otherwise stays up, its stay doubling per network-absent boot.
- 2026-11-17 — Review fixes: journaled Wi-Fi replacement for the fallback commit, transient windows no longer use up a boot's count.
