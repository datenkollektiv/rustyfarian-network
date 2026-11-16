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
- [x] Does `Unspecified` cover esp-radio's catch-all, or does it need a distinct `Other`? (`Unspecified` covers it, with no `Other`: `Unspecified { code: Option<u16> }` carries `Some(1)` on ESP-IDF and `None` for esp-hal's catch-all.)
- [x] Should the enum own `Display` (the whole log line, hint included) or do the tiers format? (The enum owns the reason text, `reason 8 (ASSOC_LEAVE)`, without the hint; `lines::Disconnected::new(reason)` adds the credentials hint, and both tiers log through it.)
- [x] Can the hal mapping be host-tested by mirroring the variants, or is review enough? (Review is enough: the esp-hal `shared_reason` table is target-gated and has no host mirror test.)
- [x] Do the existing `reason_suggests_credentials` / `credentials_hint` stay public after folding? (No: `wifi_disconnect_reason_name`, `reason_suggests_credentials` and `credentials_hint` are removed and no longer re-exported by either tier; `CREDENTIALS_HINT` stays public.)

## State
- [x] Design approved
- [x] Core implementation
- [x] Tests passing
- [x] Documentation updated

## Session Log
- 2026-11-02 — Feature doc created via /feature dialog; direction set by a review nit on the disconnect-log fix
- 2026-11-16 — Implemented: the enum replaces the `u16` newtype on both tiers, the four open questions answered by the maintainer, esp-hal logs `reason <code> (<NAME>)` like ESP-IDF
