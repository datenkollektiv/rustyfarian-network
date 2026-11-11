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
- ADR 022 holds: additive or deprecating changes only, and the traits keep one name per role; the approved 0.7.0 exceptions are the IDF `with_startup_message` signature and the required trait methods `MqttClient::subscribe` and `NetworkLink::wait_suspended`, all recorded as BREAKING in the CHANGELOG.
- ADR 021 holds: libraries select no executor, console, panic handler, critical section or scheduler.
- The sync `suspend` and `resume` stay; `wait_suspended` is additive.
- Hal DNS and runtime subscribe stay behind the existing `mqtt` feature and add no default dependency.
- Each item ends with `just fmt`, `just verify`, and `just clippy-hal` or the touched crate's recipe.
- The feature completes only when its bench items pass in the release 0.7.0 hardware run.
- The stale "MQTT reconnect after link-up" row is removed first; the Step 9 doc already does so.

## Open Questions
- [ ] Who files the esp-radio request for `WifiController::{stop, start}`?
- [ ] Who files the esp-idf-svc request to expose `retain` and `qos` in `EventPayload::Received`?
- [x] Runtime subscribe on hal: copy the topic into `heapless` or require `&'static str` like the builder?
  Answered 2026-11-05: the topic is copied into a bounded `heapless` buffer (64 bytes), and a filter that is too long or invalid is refused with an error.
- [x] Does `wait_suspended` need its own timeout, or is the hal's bounded supervisor transition enough?
  Answered 2026-11-05: `wait_suspended` has no timeout of its own, and the hal supervisor's bounded transition (500 ms) is the only bound.
- [x] Do the four items ship in one 0.x minor or as separate releases?
  Answered 2026-11-05: all four ship in 0.7.0.

## State
- [x] Design approved
- [x] Corrected capability table in both READMEs
- [x] IDF `with_startup_message(topic, payload, qos, retain)`
- [x] `NetworkLink::wait_suspended` on both tiers
- [x] Hal runtime `MqttHandle::subscribe`
- [x] Hal DNS resolution

Hardware: covered by the release 0.7.0 hardware run, see ../runbooks/release-0.7.0-hardware-test.md

## Session Log
- 2026-11-03 — Feature doc created from the Step 9 capability-gap review.
- 2026-11-05 — All four items landed in code, checked by host tests, `just verify`, `just clippy-hal` and example builds; the IDF `wait_suspended` returns at once, and the hardware runs of S3-B2 and C6-H5 are open in the release 0.7.0 runbook.
- 2026-11-11 — Bench: C6-H5 passed 2026-11-10 (hostname broker and runtime subscribe); S3-B2 on both tiers has the startup message retained on the first connect and after a broker restart, and `wait_suspended` prompt in three suspend cycles; the Wi-Fi blip half and the S3 DNS line wait for the S3-H1 pause run.
- 2026-11-11 — esp-hal S3-B2 blip half passed in the S3-H1 run (`online` retained again after the router pause); only the S3 DNS line remains, and C6-H5 already ran the hostname broker on the C6.
- 2026-11-11 — ESP-IDF S3-B2 complete on the bench (`online` retained after the router pause as well); the IDF `with_startup_message` bench item is done.
