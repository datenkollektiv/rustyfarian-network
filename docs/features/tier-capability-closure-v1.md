---
gate: none
desk-work: available
---

# Feature: Tier Capability Closure v1

Close the capability rows that remain after the ADR 022 Step 9 shared-app proof.

## Decisions
| Decision                                                          | Reason                                                               | Rejected Alternative                           |
|:------------------------------------------------------------------|:---------------------------------------------------------------------|:-----------------------------------------------|
| IDF gains `with_startup_message(topic, payload, qos, retain)`     | The shared connect work already publishes a startup message          | Keep the no-arg `iot/{client_id}/startup` flag |
| Add `async fn wait_suspended(&self)` to `NetworkLink`             | Both tiers then await a completed suspend; IDF returns at once       | Keep the app-side poll and timeout             |
| Add runtime `MqttHandle::subscribe(topic, qos)` on hal            | `minimq` exposes `Connection::subscribe`; the runner owns it         | Reconnect with an extended build-time list     |
| Resolve the broker host in the hal runner with `Stack::dns_query` | `embassy-net 0.8` has a DNS client; closes the address row           | Require an IPv4 literal forever                |
| Document the upstream blockers, do not implement them here        | esp-radio has no public stop/start; esp-idf-svc hides retain and QoS | Raw FFI stop/start; patching esp-idf-svc       |
| Keep the executor difference documented                           | ADR 021 leaves the executor to the binary; app code is identical     | Add an embassy executor to IDF for symmetry    |
| Four items, one doc, one PR each                                  | Shared ADR 022 goal and one bench campaign; items land independently | Four feature docs                              |

## Constraints
- ADR 022 holds: additive or deprecating changes only, and the traits keep one name per role.
- ADR 021 holds: libraries select no executor, console, panic handler, critical section or scheduler.
- The sync `suspend` and `resume` stay; `wait_suspended` is additive.
- Hal DNS and runtime subscribe stay behind the existing `mqtt` feature and add no default dependency.
- Each item ends with `just fmt`, `just verify`, and `just clippy-hal` or the touched crate's recipe.
- The feature completes only when its bench items pass in the release 0.7.0 hardware run.
- The stale "MQTT reconnect after link-up" row is removed first; the Step 9 doc already does so.

## Open Questions
- [ ] Who files the esp-radio request for `WifiController::{stop, start}`?
- [ ] Who files the esp-idf-svc request to expose `retain` and `qos` in `EventPayload::Received`?
- [ ] Runtime subscribe on hal: copy the topic into `heapless` or require `&'static str` like the builder?
- [ ] Does `wait_suspended` need its own timeout, or is the hal's bounded supervisor transition enough?
- [ ] Do the four items ship in one 0.x minor or as separate releases?

## State
- [x] Design approved
- [ ] Corrected capability table in both READMEs
- [ ] IDF `with_startup_message(topic, payload, qos, retain)`
- [ ] `NetworkLink::wait_suspended` on both tiers
- [ ] Hal runtime `MqttHandle::subscribe`
- [ ] Hal DNS resolution

Hardware: covered by the release 0.7.0 hardware run, see ../runbooks/release-0.7.0-hardware-test.md

## Session Log
- 2026-11-03 — Feature doc created from the Step 9 capability-gap review.
