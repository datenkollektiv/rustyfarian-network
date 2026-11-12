---
gate: none
desk-work: available
---

# Feature: Shared disconnect-reason vocabulary v1

## Decisions
| Decision                                                                         | Reason                                                                    | Rejected Alternative                               |
|----------------------------------------------------------------------------------|---------------------------------------------------------------------------|----------------------------------------------------|
| `juggler::wifi::DisconnectReason` enum with `name()` and `code() -> Option<u16>` | One vocabulary both tiers and consumer code match on (ADR 022 decision 5) | Tier-specific names; the `u16` formatter only      |
| Named kinds carry their ESP-IDF code, so esp-hal logs `reason 8` too             | esp-hal cannot observe the code, but the kind identifies it               | Leaving `code()` empty on esp-hal                  |
| `code()` is `None` only where the tier cannot tell: esp-hal's catch-all          | esp-radio folds code 1 and unknown codes into `Unspecified`               | Pretending a code on both tiers                    |
| esp-hal maps esp-radio variants through a local table in the hal crate           | Parity now; esp-radio exposes no numeric accessor                         | Waiting for an upstream `code()`; `u16`-only table |
| Replace `wifi_disconnect_reason_name(u16)` and `DisconnectReason(u16)` outright  | One way to name a reason; unreleased 0.x, nothing depends on it           | A deprecation window                               |
| `suggests_credentials()` becomes a method of the kind                            | Keeps the credentials-hint policy in one place instead of a free function            | Two parallel classifiers                           |
| Adopt the `[wifi]`/`[mqtt]` prefix convention in the IDF wifi module             | Both tiers then log alike; newer code already does it                     | Keeping prose on one tier                          |

## Constraints
- `no_std` in `juggler`; the enum is `#[non_exhaustive]`.
- Kind names spell as `wifi_err_reason_t` (`ASSOC_LEAVE`, `4WAY_HANDSHAKE_TIMEOUT`).
- Naming and log text only: link machine, MQTT gate and connect behaviour must not change.
- The hal table is target-gated, so it has no host test; keep it as small as the named kinds.
- The prefix change also touches runbooks, READMEs, lore and a `CLAUDE.md` row that quote the old lines.
- ADR 021 holds: neither tier selects a console, executor or provider.

## Open Questions
- [ ] Does `Unspecified` cover esp-radio's catch-all, or does it need a distinct `Other`?
- [ ] Should the enum own `Display` (the whole log line, hint included) or do the tiers format?
- [ ] Can the hal mapping be host-tested by mirroring the variants, or is review enough?
- [ ] Do the existing `reason_suggests_credentials` / `credentials_hint` stay public after folding?

## State
- [x] Design approved
- [ ] Core implementation
- [ ] Tests passing
- [ ] Documentation updated

## Session Log
- 2026-11-02 — Feature doc created via /feature dialog; direction set by a review nit on the disconnect-log fix
