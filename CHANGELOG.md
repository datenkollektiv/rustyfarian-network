# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- `LwtConfig` struct with `new()` constructor for Last Will and Testament support
- `MqttConfig::with_lwt()` builder for configuring LWT messages
- `MqttConfig::with_auth()` builder for MQTT broker authentication
- `MqttManager::publish_with()` for publishing with explicit QoS and retain control
- Multi-topic subscription via `&[&str]` constructor parameter
- Topic-based dispatch: callback receives `(topic, payload)` instead of just `payload`

### Deprecated

- `MqttManager::send_startup_message()` — use `publish()` or `publish_with()` instead
- `MqttManager::send_shutdown_message()` — use `publish()` or `publish_with()` instead
