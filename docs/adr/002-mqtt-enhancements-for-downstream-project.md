# ADR 002: MQTT Enhancements for Downstream IoT Project

## Status

Accepted

## Context

A downstream IoT platform needs advanced MQTT features beyond the original single-topic pub/sub API in `rustyfarian-esp-idf-mqtt`.
The platform requires:

- **Online/offline status** via Last Will and Testament (LWT)
- **Retained messages** so late-joining dashboards see the current state
- **Multi-topic subscription** for routing commands across subsystems
- **Topic-based dispatch** so the callback knows which topic fired
- **Authentication** for broker ACL enforcement
- **Custom lifecycle topics** instead of the fixed `iot/{id}/startup` prefix

The current API accepts a single topic string and a `Fn(&[u8])` callback, which is insufficient for these requirements.

## Decision

Accept all six enhancements as a single coordinated change:

| Enhancement                   | API change                                                                    |
|:------------------------------|:------------------------------------------------------------------------------|
| LWT support                   | `LwtConfig` struct with `new()` constructor; `MqttConfig::with_lwt()` builder |
| Authentication                | `MqttConfig::with_auth(username, password)` builder                           |
| Multi-topic subscription      | Constructor takes `&[&str]` instead of `impl Into<String>`                    |
| Topic-based dispatch          | Callback signature changes from `Fn(&[u8])` to `Fn(&str, &[u8])`              |
| Retained + QoS publish        | New `publish_with(topic, payload, qos, retain)` method                        |
| Configurable lifecycle topics | Deprecate `send_startup_message()` / `send_shutdown_message()`                |

### LWT and clean shutdown

The MQTT specification defines that the broker publishes the LWT message only on **unexpected** disconnect (network loss, crash, keep-alive timeout).
A clean `DISCONNECT` packet — sent by [`MqttManager::shutdown`] — suppresses the LWT.
This means callers that use LWT for online/offline status should also publish an explicit "offline" retained message during clean shutdown if they want consistent state.

## Consequences

### Positive

- Downstream project is unblocked without forking the crate
- Features are reusable by other ESP32 MQTT consumers
- Builder pattern keeps the API ergonomic despite new options
- Deprecation preserves backward compatibility for existing lifecycle callers

### Negative

- **Breaking API change**: constructor signature (`&[&str]` instead of a single topic) and callback signature (`Fn(&str, &[u8])` instead of `Fn(&[u8])`) require downstream migration
- Broader API surface to maintain
