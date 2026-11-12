---
gate: hardware
gate-note: a C3 peer a few cm from the coordinator finds it with the capped burst power
desk-work: available
---

# Feature: Configurable ESP-NOW scan-burst TX power v1

Requested by a downstream escape-room project (2026-10-31, dense-room bench).
`scan_for_peer` raises TX power to `TxPowerLevel::Max` (about 19.5 dBm) for the scan burst on both tiers (`burst_tx_power` in the IDF worker, `boost_scan_power` in the esp-hal driver).
A peer a few cm from the coordinator found it on no channel, and slightly further away it found it on the first try.
This matches the near-field saturation behind the shared 13 dBm ESP-NOW default (`docs/project-lore.md` "ESP-NOW").

The other two items of that request are done: `juggler::wifi::ApSelection` (`Strongest` by default, both tiers) and the probe reply that carries the coordinator's channel (ADR 025).
The requested `ApSelection::Bssid([u8; 6])` variant was not added; it needs its own feature doc if a consumer asks again.

## Decisions
| Decision | Reason | Rejected Alternative |
|:--|:--|:--|
| A burst TX power field in `juggler::espnow::ScanConfig`, honoured by both tiers | One setting, tier parity (ADR 022) | A tier-specific setter |

## Constraints
- A scan restores the previous TX power afterwards, also when cancelled (unchanged).
- The default must still find a coordinator across a room.

## Open Questions
- [ ] Default: 13 dBm, the owned-radio level, or no boost at all?
- [ ] Is `None` ("no boost") a separate value, or the same as the current level?

## State
- [ ] Design approved
- [ ] Core implementation
- [ ] Tests passing
- [ ] Documentation updated

## Session Log
- 2026-11-12 — Feature doc created from the escape-room project's dense-room request of 2026-10-31
