---
gate: none
gate-note: two-device check closed 2026-10-30 on two ESP32-C3 boards (`c3-mqtt-demo-7b5d`, `c3-mqtt-demo-0619`)
desk-work: available
---

# Feature: Per-device name pre-fill v1

Source: review-queue request `rustyfarian-network-per-device-name-prefill-v1.md` (spookhouse bug 009, rgb-clock), filed 2026-10-27.
Today `PortalConfig::device_name` is one fixed string per firmware, so every device saved unchanged shares a name, an MQTT client ID and a topic tree.
Done when two devices running one firmware, saved without touching `dev_name`, store different names and connect with different client IDs.

## Decisions
| Decision                                                                               | Reason                                                                                                              | Rejected Alternative                                   |
|:---------------------------------------------------------------------------------------|:--------------------------------------------------------------------------------------------------------------------|:-------------------------------------------------------|
| Pure `juggler::provisioning::derive_device_name(prefix, &mac)`                         | One host-tested derivation shared by both tiers                                                                     | Per-tier derivation (drifts)                           |
| Return `heapless::String<DEVICE_NAME_MAX_LEN>` (24 bytes)                              | Matches the stored-name limit, alloc-free                                                                           | `String` (needs alloc)                                 |
| Truncate the prefix on a char boundary, keep `-xxxx`                                   | Same contract as `derive_softap_ssid`                                                                               | Truncating the suffix (loses uniqueness)               |
| Suffix from AP MAC bytes 4..5, lowercase hex                                           | Reads well in topics; same digits as the SSID `-1A2B`                                                               | Uppercase (byte-identical to SSID)                     |
| `device_name: DeviceName::{Fixed, MacSuffixed}` on both `PortalConfig`s                | Explicit intent at the call site                                                                                    | `device_name_mac_suffix: bool`; helper-only first step |
| Ship both tiers together                                                               | Same `PortalConfig` API on idf and hal in one release                                                               | IDF first; hal fallback to STA MAC (suffix ≠ SSID)     |
| `DeviceName::resolve` applies one policy on both tiers (`Fixed` truncated to 24 bytes) | ESP-IDF passed a long fixed name through, esp-hal truncated; rejecting would need a new `ProvisioningError` variant | Reject an over-long fixed name                         |

## Constraints
- The derived name is only the pre-fill; the user can edit it before saving.
- A stored name always wins over the derived one on re-provisioning.
- The suffix must equal the suffix of the derived SSID of the same device, case aside.
- An `ssid_override` removes that relation, because the AP then broadcasts the override verbatim.
- Uniqueness is limited to the last two AP-MAC bytes, so two devices whose last two bytes match get the same name.
- `mqtt_client` must be blank in the defaults and the form for the client ID to follow the device name.
- The provisioning MQTT examples therefore do not pre-fill `mqtt_client` from `MQTT_CLIENT_ID`, which the plain MQTT examples still read from the same dotenv file.
- A `Fixed` name longer than 24 bytes is truncated on a char boundary to 24 bytes on both tiers.
- Already-provisioned devices keep their stored name and need one edit-and-save or a factory reset to pick up the derived name.
- `DeviceName` breaks consumer struct literals, so it ships in the next minor with a CHANGELOG migration note.
- The portal supplies the AP MAC itself; consumers never read it.
- The esp-hal portal holds the AP MAC since `hal-provisioning-ap-ownership-v1` (`ProvisioningSession::ap_mac()`, read via `wifi::softap_mac()` before bring-up).

## Open Questions
- [x] Does `DeviceName::Fixed` stay the default, or should the examples' fixed names all move to `MacSuffixed`?
  Resolved: examples use `MacSuffixed`; `Fixed` stays for consumers that own uniqueness.
- [x] Should `derive_device_name` reject a prefix that is empty after trimming, or return just `xxxx`?
  Resolved: trimmed-empty prefix yields the bare four digits, infallible, mirroring `derive_softap_ssid`.

## State
- [x] Design approved
- [x] `derive_device_name` + host tests (24-byte truncation with a multi-byte prefix)
- [x] IDF portal: `DeviceName` + pre-fill from AP MAC
- [x] esp-hal portal: `DeviceName` + pre-fill from AP MAC (after AP ownership)
- [x] Doc notes on `PortalConfig::device_name` and `resolve_client_id`; examples use `MacSuffixed`
- [x] Tests passing
- [x] Review feedback 2026-10-30 applied (client-ID default caveat, uniqueness wording, one `Fixed` policy, migration note, integration test)
- [x] Two-device hardware check: distinct names and client IDs with `mqtt_client` blank; a shared `mqtt_client` default collides as documented (closed 2026-10-30)

## Session Log
- 2026-10-29 — Feature doc created via /feature dialog from the review-queue request
- 2026-10-30 — Upstream gate cleared: hal-provisioning-ap-ownership-v1 merged (PR #112), the esp-hal session exposes the AP MAC
- 2026-10-30 — Implementation landed uncommitted across juggler and both tiers via a workflow run; the four provisioning examples use `MacSuffixed`; the two-device hardware check is open for the user.
- 2026-10-30 — External review accepted: client-ID default caveat and warning, uniqueness qualified to the last two MAC bytes and the derived SSID, one `Fixed` truncation policy via `DeviceName::resolve`, migration note for stored names, host integration test over two MACs; step 1 of the hardware check passed on a C3 (SSID `Rustyfarian-7B5D`).
- 2026-10-30 — Single-board hardware check on an ESP32-C3 Mini-1 (base MAC ending `7b:5c`): under ESP-IDF the SSID was `Rustyfarian-7B5D` and the portal pre-filled `c3-mqtt-demo-7b5d`.
  After an unchanged save the stored name and a 17-byte client ID matched.
  A shared `MQTT_CLIENT_ID` default first produced a 13-byte client ID, and the new warning fired as documented.
  Under esp-hal the SSID was `Rustyfarian-7B5C` and the pre-fill `c3-provision-demo-7b5c` survived the reboot.
  The second-board run is still open.
- 2026-10-30 — Second ESP32-C3 (base MAC ending `06:18`) ran the same `idf_c3_provision_mqtt` firmware and broadcast `Rustyfarian-0619`.
  The portal pre-filled `c3-mqtt-demo-0619`.
  After an unchanged save the stored name was `c3-mqtt-demo-0619` and the client ID was 17 bytes.
  The two boards differ only in the four-digit suffix.
  The automatic restart after commit was also observed on `hal_c3_provision_mqtt`.
  The feature is complete and ready for roadmap hygiene and archiving.
- 2026-10-30 — Second external review accepted: its one actionable point, that examples promising per-device client IDs must leave the `mqtt_client` default blank, is applied by dropping the `MQTT_CLIENT_ID` pre-fill from `idf_c3_provision_mqtt`, `hal_c3_provision_mqtt` and `hal_c6_provision_mqtt`; the four-digit collision note was already documented.
