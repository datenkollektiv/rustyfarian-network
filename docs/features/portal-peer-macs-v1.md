---
gate: none
desk-work: available
---

# Feature: ESP-NOW peer MACs in the provisioning portal v1

Requested by a downstream escape-room project (2026-10-31): a replacement brain C6 had a new MAC, so its matrix and nunchuk peers needed a rebuild.
The consumer now sets peer MACs at build time (`env!("BRAIN_MAC")`), which still means one build per fleet.

## Requirements
- Collect ESP-NOW peer MACs in the captive portal and store them with the provisioning record, on both tiers.
- Accept `AA:BB:CC:DD:EE:FF` with `:` or `-`.
- Reject broadcast, multicast and the device's own MAC.
- Pre-fill from consumer defaults, like `PortalDefaults`, and let a stored value win.
- Offer a portal profile with no Wi-Fi or MQTT fields, for boards that only use ESP-NOW.
- Work with the re-entry hook, so a BOOT press after power-on reopens the portal.

## Decisions
| Decision | Reason | Rejected Alternative |
|:--|:--|:--|
| | | |

## Constraints
- Today's `ExtraField` stores up to `EXTRA_FIELDS_MAX` (8) opaque, secret-redacted pairs, with no label and no type.
- The canonical schema assumes a Wi-Fi join, so an ESP-NOW-only device has no SSID to store.

## Open Questions
- [ ] Typed MAC fields in `juggler`, or opaque extras plus a `juggler` MAC validator?
- [ ] How does a consumer declare a custom form field (key, label, type, default)?
- [ ] Does SoftAP plus HTTP fit an ESP32-C3 esp-hal image next to `espnow`?
- [ ] Does the radio return cleanly to an unassociated station for `EspHalEspNow::new_owned` after the portal stops?

## State
- [ ] Design approved
- [ ] Core implementation
- [ ] Tests passing
- [ ] Documentation updated

## Session Log
- 2026-11-12 — Feature doc created from the escape-room project's request of 2026-10-31
