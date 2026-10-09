# Feature: Re-enter the provisioning portal after repeated Wi-Fi failures v1

Requested by `rustyfarian-rgb-clock` (2026-10-08, found during the OTA release-candidate hardware test on an ESP32-C3).
A provisioned device whose stored Wi-Fi credentials stop working — a mistyped password on the portal, or a router whose SSID or passphrase changed — must recover without a cable.
Today it cannot: the only recovery is a full chip erase and reflash.

## Problem & evidence

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

| Where                          | Behaviour                                                                          | Evidence                                                                           |
|:-------------------------------|:-----------------------------------------------------------------------------------|:-----------------------------------------------------------------------------------|
| `WiFiManager::new`             | Connects blocking inside the constructor and returns `Err` after the timeout       | `crates/rustyfarian-esp-idf-network/src/wifi/mod.rs` `connect_with_led` (~332-365) |
| Disconnect reasons             | Mapped and logged only in non-blocking mode; a blocking caller gets a bare timeout | `wifi/mod.rs` ~264-275 (`wifi_disconnect_reason_name`)                             |
| `WifiMqttBoot::load`           | Returns `Ready` for any stored config; nothing records that it never connected     | `provisioning/boot.rs` ~113                                                        |
| `ProvisioningStore::erase_all` | Exists, but no boot path uses it on failure                                        | `provisioning/store.rs` ~476                                                       |
| Consumer                       | Propagates the error with `?`, `app_main` returns                                  | rgb-clock `src/main.rs` ~148-154                                                   |

A consumer can wrap the error and restart, but it cannot decide well on its own: it does not see the disconnect reason, and every consumer would re-implement the same counter and portal hand-off.

## Proposed change

A boot-level fallback in `rustyfarian-esp-idf-network::provisioning`, opt-in through `BootConfig` (or a new policy struct), so the `WifiMqttDevice` profile gets it with one setting:

1. **Classify each failed connect** from the disconnect reasons the driver already reports (a pure, host-testable classifier next to `juggler::wifi::wifi_disconnect_reason_name`, `crates/juggler/src/wifi/mod.rs:401`):
   - *credential failure*: `4WAY_HANDSHAKE_TIMEOUT`, `HANDSHAKE_TIMEOUT`, `AUTH_FAIL`, `MIC_FAILURE`, repeated `AUTH_EXPIRE`;
   - *network absent*: `NO_AP_FOUND` (SSID renamed, or router off);
   - *transient*: everything else (beacon timeout, association leave, DHCP timeout).
2. **Count consecutive failed boots** in NVS (survives power cycles; reset on the first successful association and IP).
3. **Fall back to the portal** when the count reaches a configurable threshold (e.g. 3 credential failures, or N network-absent boots), instead of returning an error:
   - prefill the form from the stored config, never the password;
   - keep the stored config until a new one is committed, so an aborted or timed-out portal changes nothing;
   - leave the portal after a configurable window without a submission and retry the stored config (a router that was merely off comes back without user action);
   - signal the state on the status LED, distinct from first-boot provisioning.
4. **Surface the outcome** to the consumer (e.g. `WifiMqttLoadOutcome::FallbackPortal { reason, failures }` or a `PortalOutcome` variant), so it can log it, skip its OTA and MQTT start-up, and keep its own UI consistent.
5. **Never end the boot on a Wi-Fi timeout** in this profile: below the threshold, back off and retry (or restart) rather than returning an error that ends `app_main`.

## Decisions

|                                     Decision | Reason                                                                      | Rejected Alternative                                       |
|---------------------------------------------:|:----------------------------------------------------------------------------|:-----------------------------------------------------------|
|      Fallback lives in the provisioning boot | Every Wi-Fi/MQTT device needs the same counter, reason mapping and hand-off | Each consumer wraps `WiFiManager::new` and restarts itself |
|        Stored config kept until a new commit | A router outage must not erase working credentials                          | Erase the config when the fallback triggers                |
| Threshold and portal window are configurable | Battery devices and always-on clocks tolerate different retry costs         | Fixed constants                                            |

## Constraints

- Opt-in or default-on for `WifiMqttDevice` only; other profiles keep today's behaviour.
- No change for first-boot provisioning (unprovisioned device).
- The portal fallback must not run while an OTA image is pending verification; the consumer's health deadline decides that image first (rgb-clock's OTA runtime rolls back an image that never reaches the network).

## Security

The fallback re-opens the open SoftAP portal on a device that was already provisioned, which widens rgb-clock's accepted first-boot exposure (`docs/features/wifi-softap-provisioning-v1.md`, "Security stance / threat model" there).
Anyone in radio range who can make the device's Wi-Fi fail (deauthentication, jamming) could reach the portal and re-provision it, including the MQTT broker — and the broker controls OTA, whose images are not yet signed.
Options to weigh:

- [ ] Allow only the Wi-Fi fields in fallback mode; the MQTT broker stays as stored (changing it still needs a factory reset).
- [ ] Protect the fallback portal with a WPA2 passphrase derived from the device (printed on a label).
- [ ] Require physical presence (BOOT button held during power-up) to unlock broker changes.
- [ ] Count only credential failures toward the threshold; network-absent boots never open the portal.

## Open Questions

- [ ] Default threshold and portal window for the `WifiMqttDevice` profile?
- [ ] Should a repeated `NO_AP_FOUND` (renamed SSID) open the portal at all, or only credential failures?
- [ ] Does the esp-hal tier need the same fallback now, or is it tracked separately?
- [ ] Should the non-blocking connect path become the default for this profile, so reasons are available without a second code path?

## State

- [ ] Design approved
- [ ] Core implementation
- [ ] Tests passing
- [ ] Documentation updated

## Session Log

- 2026-10-08 — Feature doc created from the `rustyfarian-rgb-clock` request (mistyped password during the OTA release-candidate hardware test left the C3 dark until a full reflash).
