# ADR 003: Non-Blocking Wi-Fi Connect Option

## Status

Accepted

## Context

`WiFiManager::new` takes two connection paths based on whether an LED driver is supplied:

- **With LED** — calls `wifi.wifi_mut().connect()` (the non-blocking `EspWifi` variant), then polls `is_connected()` in a loop that respects a caller-visible timeout.
- **Without LED** — calls `wifi.connect()` (the `BlockingWifi` wrapper), which suspends the thread on an ESP-IDF event wait until association succeeds or the driver times out (~15–17 s).

The no-LED path gives callers no control over how long `new()` blocks.
On hardware where the AP is not immediately reachable (a device boots before the router, or the AP is temporarily out of range), the thread is suspended for the full timeout and then exits with `ESP_ERR_TIMEOUT`, dropping all peripheral drivers.

The standalone firmware (for example, an ESP32-S3 CrowPanel knob board) must remain interactive from the first millisecond of boot.
The display, LED ring, and rotary encoder must respond to user input even before Wi-Fi is available.
With the current no-LED path, nothing is rendered and the encoder is unresponsive until `WiFiManager::new` times out.

The non-blocking technique already exists in `connect_with_led`, which calls `wifi.wifi_mut().connect()`.
The gap is that this path is unreachable without a hardware LED.

## Decision

Replace the existing `connection_timeout_secs: Option<u64>` field and the proposed `connect_nonblocking: bool` flag with a single `ConnectMode` enum on `WiFiConfig`.
The timeout field only appears inside the `Blocking` variant, so it cannot be set in a context where it would have no effect.

```rust
pub enum ConnectMode {
    /// Block `WiFiManager::new` until connected or the timeout expires.
    Blocking { timeout_secs: u64 },
    /// Initiate association and return immediately.
    /// The ESP-IDF event loop drives the connection in the background.
    NonBlocking,
}
```

`ConnectMode` defaults to `Blocking { timeout_secs: 10 }`, preserving current behaviour for callers that do not opt in.
The existing `.with_timeout(secs)` builder method sets `Blocking { timeout_secs: secs }`.
A new `.connect_nonblocking()` builder method sets `NonBlocking`.

In `WiFiManager::new`, the no-LED branch matches on `ConnectMode`:

- `Blocking { timeout_secs }` — existing path: `wifi.connect()?` then `wifi.wait_netif_up()?`
- `NonBlocking` — new path: `wifi.wifi_mut().connect().context("WiFi connect initiation failed")?`, return immediately on success

The `connect_with_led` path is unaffected; it already uses the non-blocking variant internally.

## Consequences

### Positive

- Firmware can render a UI, service peripherals, and start background tasks while Wi-Fi association proceeds in the background
- No breaking change: callers that do not call `.connect_nonblocking()` observe identical behaviour
- Timeout and connection mode cannot be set in a contradictory combination — the API is self-consistent by construction
- Aligns the no-LED path with how `connect_with_led` already works internally
- `get_ip()` and `is_connected()` — both already polling — provide a natural readiness signal

### Negative

- Immediate `EspWifi::connect()` failures (e.g. driver not ready, invalid state) are propagated as errors from `new()`.
  Errors that only surface later during background association (e.g. wrong password, AP out of range) are not visible until the caller polls `get_ip()` or `is_connected()`
