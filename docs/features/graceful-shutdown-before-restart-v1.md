# Feature: Graceful MQTT and Wi-Fi shutdown before a planned restart v1

Requested by `rustyfarian-rgb-clock` (2026-10-08, seen during the 0.5.1 release-candidate hardware test on an ESP32-C3).
A planned restart — after an OTA image was activated, or the health policy's `NoteAndRestart` — tears the network down under an open MQTT session.
The broker sees a dropped TCP connection instead of an MQTT `DISCONNECT`, and the device logs errors for an event that is working as designed.

## Problem & evidence

Serial log of the restart after `swap_pending` (rgb-clock on `2a3ac3f`):

```text
I (192255) wifi:pm stop, total sleep time: 0 us / 188605273 us
E (192265) transport_base: tcp_read error, errno=Software caused connection abort
E (192265) mqtt_client: esp_mqtt_handle_transport_read_error: transport_read() error: errno=113
E (192275) rustyfarian_esp_idf_network::mqtt: [mqtt] error: ESP_FAIL (error code -1)
E (192285) mqtt_client: mqtt_process_receive: mqtt_message_receive() returned -2
I (192295) rustyfarian_esp_idf_network::mqtt: [mqtt] disconnected
```

| Where          | Behaviour                                                                                                                           | Evidence                                                               |
|:---------------|:------------------------------------------------------------------------------------------------------------------------------------|:-----------------------------------------------------------------------|
| OTA worker     | Sleeps `restart_grace`, then calls the app's `RestartFn` with MQTT and Wi-Fi still up                                               | `crates/rustyfarian-esp-idf-network/src/ota/runtime/worker.rs:256-257` |
| Health policy  | Calls the same `RestartFn` for `DeadlineAction::NoteAndRestart`                                                                     | `ota/runtime/health.rs:84`                                             |
| `MqttHandle`   | Owns the client as `Arc<Mutex<SubscribableClient>>`; no `disconnect`/`stop`, the client is only destroyed when the last clone drops | `mqtt/mod.rs:1418-1428`                                                |
| Auto-reconnect | Enabled since `2a3ac3f` (bug 005): during `restart_grace` the client may already try to reconnect after a drop                      | `mqtt/mod.rs:521`, `:1072`                                             |
| `WiFiManager`  | No `disconnect`, although `EspWifi::disconnect` exists in esp-idf-svc 0.53                                                          | `wifi/mod.rs`; esp-idf-svc `src/wifi.rs:682`                           |
| Consumer       | Can only restart; it holds no handle that could disconnect cleanly                                                                  | rgb-clock `src/ota/mod.rs:65`                                          |

Consequences:

- The broker keeps the session until its keep-alive expires, and would publish a last-will message if one were configured, although nothing failed.
- Error-level log lines on every successful update make real transport failures harder to spot.

## Proposed change

1. **`MqttHandle::disconnect(&self, timeout: Duration) -> Result<(), DisconnectError>`**
   - sends MQTT `DISCONNECT` and waits (bounded) until the event loop reports `Disconnected`;
   - turns auto-reconnect off for this handle, so the client stays down until the restart;
   - is refused from an event-loop callback (`WrongThread`), like `publish_acked` (ADR 017);
   - logs the planned disconnect at `info`, not as a transport error.
2. **`WiFiManager::disconnect(&self)`** wrapping `EspWifi::disconnect`, logged as a planned disconnect.
3. **Runtime integration (optional):** a `prepare_restart` step in `OtaConfig` (or a default) that the worker and the health policy run before the `RestartFn`: flush the last status publish, `MqttHandle::disconnect`, then the app's `RestartFn`.
   Failures in the shutdown never block the restart; they are logged and the restart proceeds.

## Decisions

|                                           Decision | Reason                                                                     | Rejected Alternative                                   |
|---------------------------------------------------:|:---------------------------------------------------------------------------|:-------------------------------------------------------|
|              Disconnect is bounded and best-effort | A restart must never hang on an unreachable broker                         | Unbounded wait for the broker                          |
| Auto-reconnect is disabled by a planned disconnect | Otherwise the client reconnects inside `restart_grace` after bug 005's fix | Leave reconnect on and accept a second session briefly |
|                  Consumer keeps owning `RestartFn` | Apps decide how to restart (log flush, LED state)                          | Runtime calls `esp_restart` itself                     |

## Constraints

- Additive: existing `MqttHandle` and `WiFiManager` methods keep their behaviour.
- `disconnect` must be safe while other clones of the handle (OTA worker, reporter) still exist: later publishes fail with a clear error instead of reconnecting.
- The OTA `swap_pending` status must still reach the broker before the disconnect; the shutdown runs after `restart_grace`, not instead of it.

## Open Questions

- [ ] esp-idf-svc 0.53's `EspMqttClient` exposes no disconnect; does this need `esp_mqtt_client_disconnect` on the raw handle (not public in esp-idf-svc), an upstream esp-idf-svc change, or `esp_mqtt_client_stop` via a wrapper that owns the raw client?
- [ ] Does `esp_mqtt_client_stop` on ESP-IDF v5.3 send `DISCONNECT`, or only close the transport?
- [ ] Should `prepare_restart` be a default of the runtime, or opt-in per consumer?
- [ ] Same treatment for the esp-hal tier?

## State

- [ ] Design approved
- [ ] Core implementation
- [ ] Tests passing
- [ ] Documentation updated

## Session Log

- 2026-10-08 — Feature doc created from the `rustyfarian-rgb-clock` request (error-level MQTT transport logs on every planned OTA restart, seen during the 0.5.1 release-candidate hardware test).
