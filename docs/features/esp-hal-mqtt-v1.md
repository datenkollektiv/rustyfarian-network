# Feature: MQTT on the esp-hal tier v1

Requested by the rgb-puzzle brain (2026-10-12, planning a bare-metal variant of its ESP32-C6 firmware, which bridges the puzzle to a game-master dashboard over MQTT).
`docs/ROADMAP.md:59` lists "bare-metal MQTT domain in rustyfarian-esp-hal-network — minimq-based (after async MQTT ADR)" as long term, and ADR 015 notes `rustyfarian-rgb-clock`'s bare-metal milestone needs it too.
This document turns that roadmap line into a concrete request with three downstream consumers.
`rustyfarian-knob` (ESP32-S3, request U2b, 2026-10-14) joined as the third consumer and makes `esp32s3` a first-class chip of the domain.
ADR 019 (`docs/adr/019-esp-hal-async-mqtt-client.md`) holds the client decision; this doc holds the plan and the sign-off.

## Consumers

| Consumer                | Chip     | Needs                                                                                                     |
|:------------------------|:---------|:----------------------------------------------------------------------------------------------------------|
| rgb-puzzle brain        | ESP32-C6 | LWT, retained online flag, resubscribe; shares `esp-radio` with ESP-NOW; needs an IP for the broker in v1 |
| `rustyfarian-rgb-clock` | ESP32-C3 | `WifiMqttDevice` profile from provisioning (ADR 015)                                                      |
| `rustyfarian-knob`      | ESP32-S3 | status on the round display, encoder/app-mode publishes, fits beside Wi-Fi in internal RAM                |

## Problem & evidence

| Where                                | Behaviour                                                                                                       | Evidence                                                               |
|:-------------------------------------|:----------------------------------------------------------------------------------------------------------------|:-----------------------------------------------------------------------|
| `crates/rustyfarian-esp-hal-network` | No `mqtt` feature or module                                                                                     | `Cargo.toml`, `src/lib.rs`                                             |
| `docs/adr/`                          | The gating "async MQTT decision" ADR does not exist yet (ADRs end at 018)                                       | `docs/ROADMAP.md` long term; `docs/adr/015-esp-hal-provisioning.md:38` |
| `juggler::mqtt`                      | `QoS`, `SubscribeClient`, `PendingAcks`, `ConnectionEpoch`, `CallbackScope` are `std`-gated                     | `crates/juggler/src/mqtt/mod.rs:298-313`                               |
| hal `embassy` feature                | Enables `dhcpv4`, `proto-ipv4`, `udp`, `tcp` but not `embassy-net/dns`, so a broker hostname cannot be resolved | workspace and hal `Cargo.toml`                                         |
| Ecosystem                            | No sibling repo has adopted a bare-metal MQTT client; `minimq` is only named in docs                            | grep across `scm-repos`                                                |

## How the consumer uses MQTT today (idf tier)

- `MqttConfig::new(host, port, client_id).with_keep_alive(30).with_lwt(LwtConfig::new(topic, b"false", QoS::AtLeastOnce, true))`.
- `MqttBuilder` with `on_connect` (retained `online=true` publish on every connect), `on_disconnect` (log), `on_message`, `subscribe`.
- `MqttHandle::try_publish` in the main loop, treating `TryPublishError::WouldBlock` as "skip this tick"; retained state publishes.
- Auto-reconnect with resubscribe (fixed in 0.6.0, bug 005).
- `MQTT_HOST` comes from `env!()` and may be a hostname.

## Proposed change

1. ADR 019 picks `minimq =0.13.5`; evidence for `embassy-sync =0.8.0`, `no_std` + no-alloc and the embassy-net `TcpSocket` is in the ADR.
2. New feature `mqtt` on `rustyfarian-esp-hal-network` providing a builder and a handle running over `AsyncWifiHandle.stack`, with a background runner for keep-alive and auto-reconnect; every chip arm covers `esp32c3`, `esp32c6` and `esp32s3`.
3. Behavioural parity with the idf tier, not closure-API parity: LWT, resubscribe and an "on (re)connect" hook that can publish retained state; events may arrive through a channel or stream instead of callbacks.
4. Non-blocking publish with a `WouldBlock`-equivalent error (queue full or session not connected), plus retained publish.
5. Move `QoS`, `LwtConfig`/`MqttConfig` and a `TryPublishError`-shaped error into juggler without the `std` gate, so both tiers share one config surface.
6. Broker address is an IPv4 literal, validated at build time; `embassy-net/dns` is an additive follow-up (ADR 019 §2).
7. Examples `hal_c6_mqtt_lwt` and its S3 twin `hal_esp32s3_mqtt_lwt` (connect, LWT, retained online flag, reconnect after broker restart), plus `just clippy-hal` lines for `mqtt` on C3, C6 and S3.
8. Record S3 heap headroom after connect in `docs/project-lore.md` (knob acceptance).

## Decisions
|                                   Decision | Reason                                                                                 | Rejected Alternative                        |
|-------------------------------------------:|:---------------------------------------------------------------------------------------|:--------------------------------------------|
|             Shared config types in juggler | One `MqttConfig`/`LwtConfig` across tiers keeps consumer config code tier-agnostic     | Duplicate types per tier                    |
|    Behavioural parity over callback parity | Callbacks on helper threads (ADR 017) do not translate to a single-core async executor | Emulating threads on embassy                |
|                 `minimq =0.13.5` (ADR 019) | Managed session, no alloc, binds the pinned `embassy-time`/`embedded-io-async`         | `rust-mqtt` (no connection mgmt), hand-roll |
| `esp32s3` in every `mqtt` arm from day one | Knob (U2b) ships S3 in lockstep; S3 Wi-Fi is validated on `main`                       | Add S3 after C6 validation                  |
|     Static `StaticCell` buffers, zero heap | S3 keeps ~26 KB heap after Wi-Fi; esp-radio allocates internal-only                    | Heap-allocated queues                       |
| Network watch (polled 500 ms) + socket timeout 1.5 × keep-alive + stall deadline of socket timeout + keep-alive; queue kept across reconnect (2026-10-16) | T5 found the runner parked in `TcpSocket::flush`; one `Stack` waker slot forbids `wait_*` in the runner | Socket timeout only; `Stack::wait_link_down`; drain the queue on session end |

## Constraints
- `embassy-sync` is locked at `=0.8.0` (`docs/project-lore.md:150`); most published async networking crates target 0.7.x.
- The brain runs MQTT and ESP-NOW on one `esp-radio` instance (see `esp-hal-espnow-v1.md`), so the runner must coexist with ESP-NOW traffic.
- TLS is out of scope for all three consumers (plain TCP on a local network).
- `minimq` speaks MQTT 5 only; brokers must accept v5 (mosquitto ≥ 1.6 does).
- The S3 validation runs on the knob maintainer's CrowPanel 1.28"; C6/C3 validation stays with the rgb-puzzle and rgb-clock consumers.

## Open Questions
- [x] Client: `minimq =0.13.5` (ADR 019); MQTT 5, not 3.1.1 — LWT and retain are covered.
- [x] DNS: deferred — IPv4 literals only (ADR 019 §2, maintainer decision 2026-10-14).
- [x] Event delivery: `embassy-sync` channels plus an atomic state (ADR 019 §3).
- [x] Does the client allocate? No — caller-provided buffers, all `StaticCell` (knob question).
- [x] S3 main-task stack needs no tuning: 13-minute run with broker restart, heap flat at 25 928 B (2026-10-14).
- [x] T3 (USB power cut → LWT): pass on the CrowPanel 2026-10-16, `status offline` 45–51 s after the cut.
- [ ] T5 (Wi-Fi loss): the 2026-10-16 run exposed three findings (state stayed `Connected`, `queued` publishes lost, 22 s TCP-reset recovery); fixed (unreleased), re-run by the knob maintainer pending (T5a AP off + T5b router pause).
- [ ] Should this land after `esp-hal-espnow-v1.md` so STA + ESP-NOW + MQTT coexistence is validated once?

## State
- [x] Design approved (ADR 019 accepted 2026-10-14)
- [x] Core implementation (`mqtt` feature, `minimq` runner, C3/C6/S3 arms, two examples)
- [x] Tests passing (`just test-hal` 192 host tests, `just clippy-hal` 14 targets)
- [x] Documentation updated (ADR 019, README, CHANGELOG, runbook `docs/runbooks/esp-hal-mqtt-hardware-test.md`)
- [x] S3 hardware: T1, T2, T3, T4, T6 pass (CrowPanel, 2026-10-14/16); T5 re-run with the network-loss fix pending
- [ ] Knob maintainer re-run of T5a/T5b with timestamped serial and broker logs against the fix
- [ ] C6 hardware: build + clippy only; validation with the rgb-puzzle consumer

## Session Log
- 2026-10-12 — Feature doc created from the rgb-puzzle brain esp-hal variant gap analysis; expands the long-term roadmap line into a request.
- 2026-10-14 — Folded knob request U2b (third consumer, `esp32s3`, S3 example); ADR 019 drafted, awaiting approval.
- 2026-10-14 — ADR 019 accepted (minimq, IPv4-only); feature implemented; S3 T1/T2/T4/T6 pass, heap 25 928 B after connect and after a 10-min soak.
- 2026-10-16 — Knob maintainer ran T1/T3/T5 on the CrowPanel (`d9e3b62`); T5 found the client stuck `Connected` through a 3 min Wi-Fi outage because `TcpSocket::flush` waits for a peer ACK that never comes and minimq's ping timeout never runs inside a flush; fix: a polled network watch (link + IP config, 500 ms) races the session, `Disconnected` is set before the socket abort, the post-abort flush is bounded, the network-lost path waits for the network instead of the 10 s pause, the socket gets a 1.5 × keep-alive timeout, and every session await is bounded by a stall deadline of socket timeout + keep-alive (outcome `transport stalled`) for a dead host whose TCP reset cannot be routed; `try_publish` now documents queued-not-delivered (unacknowledged in-flight messages are lost, one at most on a network drop); runbook T5 split into AP-off and router-pause variants.
