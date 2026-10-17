# Feature Request: esp-hal MQTT state and delivery during Wi-Fi loss

| Field        | Value                                                                    |
|:-------------|:-------------------------------------------------------------------------|
| Requested by | `rustyfarian-knob`, esp-hal parity quest (U2b follow-up)                 |
| Target repo  | `rustyfarian-network`, crate `rustyfarian-esp-hal-network`, `mqtt`       |
| Follows      | `docs/features/esp-hal-mqtt-v1.md`; ADR 019; runbook T5 sign-off         |
| Filed        | 2026-10-16                                                               |
| Kind         | Findings from hardware test T5; behaviour fix and runbook clarification  |
| Status       | Fixed (unreleased), knob re-run pending                                  |

## Summary

During a 3-minute Wi-Fi outage the esp-hal MQTT client kept reporting `Connected`, silently lost three publishes it had reported as `queued`, and only recovered after a TCP reset that arrived 22 s after Wi-Fi returned.
No panic, no reboot and no `failed:` line, so T5's robustness criteria pass; the state and delivery semantics do not match the runbook.

## Setup

- CrowPanel 1.28" ESP32-S3, `hal_esp32s3_mqtt_lwt`, `main` @ `d9e3b62`, `minimq` 0.13.5, keep-alive 30 s.
- Outage made with a per-device Wi-Fi pause on the router: the board saw `NoAccessPointFound` from 19:33:44 to 19:36:49.

## Evidence (timestamps from the serial and broker logs)

| Time     | Source | Line                                                               |
|:---------|:-------|:-------------------------------------------------------------------|
| 19:33:42 | serial | `tick #45 queued`, then `Wi-Fi disconnected #1 — reconnecting...`  |
| 19:33:47 | serial | `tick #46 queued` (47–50 likewise, Wi-Fi already down)             |
| 19:34:12 | serial | `tick #51 would block (mqtt Connected)`, every tick until 19:37:07 |
| 19:36:49 | serial | `Wi-Fi link up #2`                                                 |
| 19:37:11 | serial | `[mqtt] session error: Transport(ConnectionReset)`                 |
| 19:37:15 | broker | `status offline` (first LWT of the outage)                         |
| 19:37:21 | serial | `[mqtt] connected`, `connect #2 free heap: 25880 B`                |
| 19:37:21 | broker | `tick 49`, `tick 50` (3.5 min late), then `tick 89`                |

Ticks 46, 47 and 48 never reached the broker.

## Findings

1. **State stays `Connected` while the link is dead.**
   With a 30 s keep-alive, a missing PINGRESP should move the client to `Disconnected` within about 45 s; it never did during 3 min 7 s.
2. **Publishes report `queued` and are then lost.**
   For about 30 s after the drop, `try_publish` succeeded; three of those messages were dropped, two were delivered after reconnect.
3. **Recovery waits for a TCP reset.**
   The session error came 22 s after Wi-Fi link-up; total recovery was 32 s after link-up.
4. **Broker LWT did not fire during the outage** (it fired after about 45 s in T3).
   Unverified hypothesis: the per-device router pause keeps something half-open; an AP power-off run would tell.

## Proposal

1. Enforce the keep-alive on the client side: no PINGRESP within 1.5 × keep-alive → `Disconnected` and reconnect.
2. React to the Wi-Fi link-down event: mark the session `Disconnected` (or `Connecting`) and stop accepting publishes as `queued`.
3. Document the delivery guarantee of `try_publish` (QoS 0 queued is best-effort) in the API docs and README.
4. Repeat T5 with the AP powered off and add the result to the runbook; clarify T5's expected states.

## Contribution

- The knob maintainer can re-run T3/T5 on the CrowPanel against a fix, with the same timestamped logs.
- Knob impact (M8): until fixed, the knob's status UI combines the Wi-Fi link state with the MQTT state.

## Acceptance / validation

- T5 on the S3: state leaves `Connected` within about 45 s of Wi-Fi loss, and no publish is reported `queued` while the link is down.
- Recovery reaches `connect #2` within about 15 s of Wi-Fi link-up.
- `just test-hal` and `just clippy-hal` stay green.
