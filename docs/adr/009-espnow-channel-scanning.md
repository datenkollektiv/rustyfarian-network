# ADR 009: ESP-NOW Channel Scanning

## Status

Accepted

## Context

When an ESP-NOW-only slave uses `init_with_radio()`, the Wi-Fi radio defaults to channel 1.
If the coordinator is connected to a Wi-Fi AP on a different channel, all ESP-NOW frames fail silently in both directions.
The slave has no way to discover the correct channel through the `rustyfarian-esp-idf-espnow` API.

Downstream firmware (RGB Puzzle) works around this by calling the ESP-IDF C API directly:

```rust
unsafe {
    esp_idf_svc::sys::esp_wifi_set_channel(channel, 0);
}
```

This bypasses the abstraction layer and requires `unsafe` in application code.

### Approaches evaluated

**Active probe scanning** (chosen): loop channels 1-13, call `esp_wifi_set_channel()` for each, send a unicast probe frame, and detect the MAC-layer ACK.
Used by Espressif's own `esp-now` C library (`ESPNOW_CHANNEL_ALL` in `espnow.c`).

**Passive AP scanning**: use `esp_wifi_scan_start()` to find the coordinator's AP by SSID and read its channel.
Used by Arduino libraries (`WifiEspNow`, `esp-now-RssiScannerAutoParing`).
Requires the slave to know the AP SSID, which it does not.

**No existing Rust crate** implements ESP-NOW channel scanning (confirmed via GitHub and crates.io search, 2026-03-27).

### Design decisions

**Method placement**: `scan_for_peer()` on the concrete `EspIdfEspNow` type, not on the `EspNowDriver` trait.
Channel scanning requires `esp_wifi_set_channel()` which is inherently platform-specific.
This follows the precedent set by ADR 008 where `init_with_radio()` was added to the concrete type.

**Guard against AP disruption**: `scan_for_peer()` returns an error if the driver was not created via `init_with_radio()`.
Calling `esp_wifi_set_channel()` while connected to an AP would disrupt the connection.

**ACK mechanism**: `esp-idf-svc::espnow::EspNow::send()` enqueues the frame; ACK/NAK is delivered asynchronously via the send callback.
`scan_for_peer()` registers a temporary `register_send_cb` handler, waits on a `Condvar` with timeout, and unregisters the callback after scanning.
This keeps ACK detection reliable without exposing raw callback wiring to application code.

**Pure types**: `ScanConfig` and `ScanResult` are defined in `espnow-pure` (no platform deps, `no_std`).
The scan execution logic lives in the ESP-IDF crate.

## Decision

Add `scan_for_peer()` and `init_with_radio_scanning()` to `EspIdfEspNow`.
Add `ScanConfig`, `ScanResult`, and `DEFAULT_SCAN_CHANNELS` to `espnow-pure`.

### Algorithm

1. Remove any stale peer registration from previous scans.
2. Register a temporary peer with `channel=0` (follows the current channel).
3. Register a temporary sent callback for MAC-layer ACK status.
4. For each channel in `ScanConfig::channels`:
   - `esp_wifi_set_channel(ch, WIFI_SECOND_CHAN_NONE)`
   - reset callback ACK state
   - `send(mac, probe_data)`
   - wait for callback ACK/NAK with timeout
   - on ACK: remove temporary peer, re-register peer with a discovered channel, return `Ok(ScanResult)`
5. On exhaustion: remove temporary peer and return `Err`.
6. Unregister send callback before returning.

### API

```rust
let espnow = EspIdfEspNow::init_with_radio(modem, sys_loop, nvs)?;
let result = espnow.scan_for_peer(&brain_mac, &ScanConfig::new(b"probe"))?;
log::info!("Found peer on channel {}", result.channel);
```

Or the convenience constructor:

```rust
let (espnow, result) = EspIdfEspNow::init_with_radio_scanning(
    modem, sys_loop, nvs, &brain_mac, &ScanConfig::new(b"probe"),
)?;
```

## Consequences

### Positive

- Slaves auto-discover the coordinator's channel without application-level `unsafe`
- No protocol changes are needed (MAC-layer ACK is the probe mechanism)
- Re-scanning is supported by calling `scan_for_peer()` again when the peer becomes unreachable
- Configurable channel list supports non-EU regions

### Negative

- The worst-case scan latency is ~1.3 s (13 channels x ~100 ms send timeout)
- `esp_wifi_set_channel()` is a global operation and not thread-safe; concurrent Wi-Fi operations during a scan would conflict

## References

- [ADR 008 — ESP-NOW Radio-Only Initialisation](008-espnow-radio-only-init.md)
- [Feature request: espnow-channel-scanning](../../review-queue/espnow-channel-scanning.md)
- Espressif `esp-now` C library: `ESPNOW_CHANNEL_ALL` in `espnow.c`
