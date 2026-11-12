---
gate: external
gate-note: a consumer asks for the helper; until then this doc is a reference
desk-work: available
---

# Feature: Provisioning boot-policy helper v1

Requested by spookhouse, a downstream project (2026-10-26, lower priority): three consumers hand-write `WifiMqttBoot::load`, the portal and the outcome match, and their matches differ.

| Decision                    | rgb-clock                  | spookhouse                      | `idf_c3_provision_mqtt`       |
|:----------------------------|:---------------------------|:--------------------------------|:------------------------------|
| `portal_timeout`            | `None`                     | `None`                          | `Some(600 s)`                 |
| `OtherProfile`              | Portal, no erase           | Portal, no erase                | Restart (loops every boot)    |
| Unexpected load outcome     | Portal                     | Keep running, no network        | Restart                       |
| `load` returns `Err`        | `main` returns `Err`       | Keep running, no network        | `main` returns `Err`          |
| Portal returns `Err`        | `main` returns `Err`       | Wait 10 s, then restart         | `main` returns `Err`          |
| `PortalExitedWithoutCommit` | Restart                    | Restart into the portal         | Restart                       |
| Erase fails                 | Log, restart               | Stay up, show ERASE FAILED      | Log, restart                  |
| Helper threads at restart   | Flag set, not joined       | BOOT thread keeps running       | Flagged and joined            |

## Decisions
| Decision | Reason | Rejected Alternative |
|:--|:--|:--|
| Two pure decision fns in `juggler` (load outcome, portal outcome) | Host-testable; the library never restarts or erases | A helper that restarts |
| Actions `Continue`, `OpenPortal`, `Restart`, `Halt`, `EraseThenRestart` | One named action per table cell | Free-form callbacks |
| A policy struct with one field per table row | Each consumer's choices become data | Hard-coded defaults |

## Constraints
- The consumer keeps the modem, so it is claimed only on the portal path.
- The consumer joins helper threads before it acts on a restart.
- `PortalOutcome` and `WifiMqttLoadOutcome` stay unchanged (both `#[non_exhaustive]`).
- `ProvisioningStore::erase_all` ignores every `remove` result, so an erase failure only shows as a failed `open`.

## Open Questions
- [ ] Should `erase_all` report `remove` failures first, so `on_erase_failure` can be decided?
- [ ] Should `idf_c3_provision_mqtt` stop restarting on `OtherProfile`, which repeats every boot?

## Acceptance
- Host tests map every table row to one named action.
- The three consumers' behaviour can be written as three policies whose outputs match the table.

## State
- [ ] Design approved
- [ ] Core implementation
- [ ] Tests passing
- [ ] Documentation updated

## Session Log
- 2026-11-12 — Feature doc created from the spookhouse request of 2026-10-26
