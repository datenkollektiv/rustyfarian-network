# Wi-Fi suspend, resume and deep sleep

Both tiers can park the station and bring it back within the same boot, which is what a deep-sleep firmware needs when a sleep attempt can be aborted.
Suspend is a state of the running stack: nothing is dropped or re-created, and the reconnect supervisor (`juggler::wifi::LinkMachine`) pauses until resume.
Design and decisions: `docs/features/archive/wifi-reconnect-and-suspend-v1.md`; hardware protocol: runbook R3 in `docs/runbooks/wifi-reconnect-hardware-test.md`.

## API

> **The tiers differ in what suspend does to the radio.**
> ESP-IDF stops the radio (`esp_wifi_stop`); esp-hal only disconnects the station and the radio stays powered, because esp-radio has no stop.
> On esp-hal only the reset of deep sleep powers the radio down; a firmware that stays awake while suspended keeps the radio's idle draw.

| Tier    | Suspend                       | Resume                       | Radio while suspended       | State                        |
|:--------|:------------------------------|:-----------------------------|:----------------------------|:-----------------------------|
| ESP-IDF | `WiFiManager::suspend(&self)` | `WiFiManager::resume(&self)` | stopped                     | `link_state()` → `LinkState` |
| esp-hal | `WiFiManager::suspend(&self)` | `WiFiManager::resume(&self)` | on, station disconnected    | `state()`, `is_suspended()`  |

- Both calls are idempotent; resume is a no-op unless suspended.
- ESP-IDF runs `esp_wifi_disconnect` + `esp_wifi_stop` (resume: `esp_wifi_start` + `esp_wifi_connect`) on the caller's thread; the radio is stopped when `suspend()` returns, within a few milliseconds, and `resume()` never waits; driver events queued before the stop are dropped by the link worker, however late they arrive.
- esp-hal only signals the supervisor task and never blocks; the task disconnects within `SUSPEND_DISCONNECT_TIMEOUT_MS` (500 ms) and `is_suspended()` turns `true` when that disconnect finished or gave up.
- `state()` / `link_state()` report `LinkState::Suspended` from the moment `suspend()` is called.
- A `resume()` right after `suspend()` (even within 50 ms) leaves one connect in flight, never two, and uses a fresh back-off.

## Deep sleep on ESP-IDF

Call `suspend()` right before `esp_deep_sleep_start`; if the sleep attempt is aborted, call `resume()`.

```rust
wifi.suspend()?;
if sleep_still_wanted() {
    unsafe { esp_idf_svc::sys::esp_deep_sleep_start() };
}
wifi.resume()?;
```

## Deep sleep on esp-hal

Call `suspend()`, wait up to 500 ms for `is_suspended()`, then sleep; the reset on wake re-initialises the radio, so no resume is needed on that path.
`suspend()` and `resume()` return `Result<(), Infallible>` on esp-hal, the same shape as on ESP-IDF.

```rust
let _ = LINK.suspend();
let deadline = Instant::now() + Duration::from_millis(SUSPEND_DISCONNECT_TIMEOUT_MS);
while !LINK.is_suspended() && Instant::now() < deadline {
    Timer::after_millis(10).await;
}
if sleep_still_wanted() {
    rtc.sleep_deep(&[&wake_source]);
}
let _ = LINK.resume();
```

## MQTT while suspended

Neither tier has an MQTT suspend call: `esp_mqtt_client_stop` sends DISCONNECT, which would cancel the will.

| Aspect           | ESP-IDF (esp-mqtt)                                                    | esp-hal (minimq runner)                    |
|:-----------------|:----------------------------------------------------------------------|:-------------------------------------------|
| State            | `Disconnected` once the aborted socket is noticed (expected < 1 s)    | session ends within 500 ms (network watch) |
| `try_publish`    | `WouldBlock` after that; a message queued just before may go out late | `WouldBlock`                               |
| Retry cadence    | one transport error log per `reconnect_timeout` (10 s default)        | none; the runner waits for the network     |
| After `resume()` | next esp-mqtt retry after link up (up to `reconnect_timeout`)         | as soon as link and DHCP are up            |
| Reconnect signal | `MqttHandle::connect_count()` bumps once `is_connected()` is `true`   | `MqttHandle::connect_count()` bumps        |

- The broker publishes the will 1.5 × keep-alive after the last packet, because the client never sent DISCONNECT.
- A suspend shorter than that ends in a client-id takeover on reconnect: the broker publishes the will as it drops the old session, then the new session's startup message, so a retained `offline` is followed by `online` within a second.
- A firmware that wants a clean `offline` instead publishes it with an acknowledged publish before `suspend()`.
- The same holds for a last value that must arrive: esp-hal drops the message queued at the suspend instant (at most one), so publish it acknowledged before `suspend()`.
- The ESP-IDF retry churn is harmless log noise; a longer `MqttConfig::with_reconnect_timeout` quiets it but delays the reconnect after resume by up to the same value.

## Validation status

- Validated: host tests of the suspend semantics in `LinkMachine`, of the esp-hal handover (`LinkHandover`), and of the ESP-IDF supervisor (`LinkSupervisor`) under delayed events, rapid cycles and concurrent callers with a mock radio; builds of both S3 examples with `MQTT_SUSPEND_DEMO`.
- Not yet validated on hardware: the R3 suspend/resume cycle on the CrowPanel S3 (both tiers), pending the knob maintainer's run.
- Not covered in this repository: entering and waking from deep sleep after `suspend()`; that path is exercised in the `rustyfarian-knob` firmware only.
