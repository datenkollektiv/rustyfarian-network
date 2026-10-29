---
gate: upstream
gate-note: hal-provisioning-ap-ownership-v1 must land so the esp-hal portal holds the real AP MAC
desk-work: available
---

# Feature: Per-device name pre-fill v1

Source: review-queue request `rustyfarian-network-per-device-name-prefill-v1.md` (spookhouse bug 009, rgb-clock), filed 2026-10-27.
Today `PortalConfig::device_name` is one fixed string per firmware, so every device saved unchanged shares a name, an MQTT client ID and a topic tree.
Done when two devices running one firmware, saved without touching `dev_name`, store different names and connect with different client IDs.

## Decisions
| Decision                                                                | Reason                                                | Rejected Alternative                                   |
|:------------------------------------------------------------------------|:------------------------------------------------------|:-------------------------------------------------------|
| Pure `juggler::provisioning::derive_device_name(prefix, &mac)`          | One host-tested derivation shared by both tiers       | Per-tier derivation (drifts)                           |
| Return `heapless::String<DEVICE_NAME_MAX_LEN>` (24 bytes)               | Matches the stored-name limit, alloc-free             | `String` (needs alloc)                                 |
| Truncate the prefix on a char boundary, keep `-xxxx`                    | Same contract as `derive_softap_ssid`                 | Truncating the suffix (loses uniqueness)               |
| Suffix from AP MAC bytes 4..5, lowercase hex                            | Reads well in topics; same digits as the SSID `-1A2B` | Uppercase (byte-identical to SSID)                     |
| `device_name: DeviceName::{Fixed, MacSuffixed}` on both `PortalConfig`s | Explicit intent at the call site                      | `device_name_mac_suffix: bool`; helper-only first step |
| Ship both tiers together                                                | Same `PortalConfig` API on idf and hal in one release | IDF first; hal fallback to STA MAC (suffix ≠ SSID)     |

## Constraints
- The derived name is only the pre-fill; the user can edit it before saving.
- A stored name always wins over the derived one on re-provisioning.
- The suffix must equal the SSID suffix of the same device, case aside.
- `DeviceName` breaks consumer struct literals, so it ships in the next minor with a CHANGELOG migration note.
- The portal supplies the AP MAC itself; consumers never read it.
- The esp-hal portal has no AP MAC until `hal-provisioning-ap-ownership-v1` lands.

## Open Questions
- [ ] Does `DeviceName::Fixed` stay the default, or should the examples' fixed names all move to `MacSuffixed`?
- [ ] Should `derive_device_name` reject a prefix that is empty after trimming, or return just `xxxx`?

## State
- [x] Design approved
- [ ] `derive_device_name` + host tests (24-byte truncation with a multi-byte prefix)
- [ ] IDF portal: `DeviceName` + pre-fill from AP MAC
- [ ] esp-hal portal: `DeviceName` + pre-fill from AP MAC (after AP ownership)
- [ ] Doc notes on `PortalConfig::device_name` and `resolve_client_id`; examples use `MacSuffixed`
- [ ] Tests passing
- [ ] Two-device hardware check: distinct names and client IDs

## Session Log
- 2026-10-29 — Feature doc created via /feature dialog from the review-queue request
