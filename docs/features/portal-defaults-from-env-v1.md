---
gate: none
desk-work: available
---

# Feature: `PortalDefaults` from the build-time environment v1

Requested by spookhouse, a downstream project (2026-10-26): rgb-clock, spookhouse and `idf_c3_provision_mqtt` each hand-copy the same `option_env!` block into `PortalDefaults`, and the copies disagree.

| Key              | rgb-clock                      | spookhouse                          | `idf_c3_provision_mqtt` |
|:-----------------|:-------------------------------|:------------------------------------|:------------------------|
| `MQTT_PORT`      | trimmed, blank becomes `1883`  | trimmed, blank becomes `1883`       | raw, unset becomes `""` |
| `MQTT_CLIENT_ID` | unset becomes the device name  | trimmed, blank becomes `spookhouse` | raw, unset becomes `""` |
| `OTA_URL`        | not read                       | raw                                 | raw                     |
| `build.rs` check | port trimmed, then parsed      | port trimmed, then parsed           | none                    |

`WIFI_SSID`, `MQTT_HOST` and `MQTT_USER` are read raw by all three.

## Decisions
| Decision | Reason | Rejected Alternative |
|:--|:--|:--|
| `const fn portal_defaults(...)` in `juggler`, one `Option<&'static str>` per key | `option_env!` expands in the caller's crate | A plain fn reading the env (sees the library's env) |
| Trim, and treat blank as unset, for every field | One rule instead of three | Trimming the port only |
| Invalid set `MQTT_PORT` panics in const evaluation | A typo fails the build, not a boot | Falling back to the default |
| Never takes `WIFI_PASS` or `MQTT_PASS` | `PortalDefaults` stays non-secret | A secrets-aware variant |

## Constraints
- Takes `mqtt_port_default` and `mqtt_client_default` as arguments.
- Consumers keep their `build.rs` `rerun-if-env-changed` lists.
- Where the three copies agree today, each consumer's behaviour is unchanged.

## Open Questions
- [ ] Is `str` trimming const on the pinned toolchain, or does it need a byte loop?
- [ ] Add a `#[macro_export]` wrapper in `rustyfarian-esp-idf-network` for a one-line call?
- [ ] Does cargo track `option_env!` reads without `rerun-if-env-changed`?
- [ ] Do the esp-hal examples `hal_c3_provision_mqtt` and `hal_c6_provision_mqtt` adopt it too?

## Acceptance
- `juggler` host tests: all unset gives empty fields; `" 1883 "` gives `"1883"`; a blank port gives the default.
- A host test reproduces rgb-clock's current `PortalDefaults` for the same inputs.
- `MQTT_PORT=abc` or `70000` fails the build.
- The `idf_c3_provision_mqtt` block reduces to one call.

## State
- [ ] Design approved
- [ ] Core implementation
- [ ] Tests passing
- [ ] Documentation updated

## Session Log
- 2026-11-12 — Feature doc created from the spookhouse request of 2026-10-26
