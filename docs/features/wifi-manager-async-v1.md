# Feature: WiFiManager Async Companion v1

Add an async API to `rustyfarian-esp-hal-wifi` that lives alongside the existing blocking `WiFiManager`.
Replaces the manual smoltcp DHCP polling loop with `embassy-net` and exposes components suitable for embassy task spawning.

Depends on `embassy-feature-flag-v1`.
Consumed by `hal-c3-connect-async-example-v1`.

Source: `docs/embassy-integration-research.md` — Option B "blocking + async companion".

## Decisions

| Decision                                                                                      | Reason                                                                                                            | Rejected Alternative                                                                                                                                    |
|:----------------------------------------------------------------------------------------------|:------------------------------------------------------------------------------------------------------------------|:--------------------------------------------------------------------------------------------------------------------------------------------------------|
| New `WiFiManagerAsync` type alongside `WiFiManager`, gated on `embassy` feature               | Option B from research — incremental, non-breaking, lets blocking users continue unchanged                        | Option A (full replacement) — breaks working C3 blocking path; Option C (async-first + block_on) — block_on is fragile in embedded multi-task scenarios |
| `init_async()` returns an `AsyncWifiHandle { controller, stack, runner }`                     | Matches embassy-net canonical pattern; users spawn `wifi_task(controller)` and `net_task(runner)` themselves      | Hiding spawning inside the library — would require the library to own the `Spawner`, infectious `'static` requirements                                  |
| Refactor shared init into a private `init_inner()` used by both blocking and async paths      | Avoids duplicating heap region setup, `esp_rtos::start`, `esp_radio::init`, and `wifi::new` across two code paths | Copy-paste — two places to fix bugs, guaranteed drift                                                                                                   |
| `embassy-net` replaces the manual `smoltcp::Interface` + `dhcpv4::Socket` polling             | Eliminates the blocking poll loop; DHCP becomes automatic via `Config::dhcpv4()`                                  | Keeping the manual smoltcp path — duplicates logic embassy-net already provides correctly                                                               |
| `wait_for_ip().await` convenience on the handle for the common "wait until online" case       | Every async user needs this; makes the simple case a one-liner mirroring blocking `wait_connected()`              | Forcing users to poll `stack.config_v4()` themselves — boilerplate in every example                                                                     |
| `StackResources<N>` sized for N=3 sockets by default; exposed via builder if needed           | DHCP + one TCP + one UDP is the baseline; matches embassy-net examples                                            | Hard-coding N=1 — can't run DHCP and user sockets concurrently                                                                                          |
| Feature requires `alloc` (already present); `Stack` is stored via `StaticCell` / `mk_static!` | `embassy-net` `Stack` needs `'static` lifetime; `StaticCell` is the idiomatic no_std pattern                      | `Box::leak` — works but `static_cell` is safer and clearer                                                                                              |
| `WifiDriver` trait in `wifi-pure` stays synchronous for this feature                          | Async trait extensions are an open design question (see feature 5); don't block this work on that decision        | Adding `connect_async` to the trait now — premature, ties `wifi-pure` to a specific async runtime                                                       |
| Blocking `WiFiManager::wait_connected()` remains unchanged and deprecated-free                | Research explicitly recommends keeping the working blocking path; it is validated on hardware                     | Removing or deprecating the blocking API — breaks `hal_c3_connect` example and existing users                                                           |

## Constraints

- No changes to the blocking `WiFiManager` public API — existing users must compile and run unchanged
- `just verify` with default features must continue to pass (blocking-only build)
- Async path compiles and type-checks for ESP32-C3 and ESP32-C6 targets
- Hardware validation is deferred to `hal-c3-connect-async-example-v1` (this feature lands behind the feature flag without a working example in the same PR is acceptable as long as types check)
- `AsyncWifiHandle` components must be `'static` (required by embassy tasks) — enforced by the API shape
- Must not require users to enable `esp-rtos/embassy` manually — handled by `embassy` feature activation from `embassy-feature-flag-v1`

## API sketch

```rust
#[cfg(feature = "embassy")]
pub struct AsyncWifiHandle {
    pub controller: WifiController<'static>,
    pub stack: embassy_net::Stack<'static>,
    pub runner: embassy_net::Runner<'static, WifiDevice<'static>>,
}

#[cfg(feature = "embassy")]
impl WiFiManager {
    pub fn init_async(config: HalWifiConfig<'_>) -> Result<AsyncWifiHandle, WifiError>;
}

#[cfg(feature = "embassy")]
impl AsyncWifiHandle {
    pub async fn wait_for_ip(&self) -> embassy_net::StaticConfigV4;
}
```

## Open Questions

- [x] Should `init_async()` be a method on `WiFiManager` or a free function / separate builder? — Kept as `WiFiManager::init_async` on `WiFiManager<'static, NoLed>`. Discoverable alongside the blocking `init`; the different return type (`AsyncWifiHandle`) is self-documenting
- [x] Where should the `StaticCell` live — inside the library or pushed to the caller via a macro? — Inside `into_async_handle()` as a function-local `static`. Simplest API, caller needs zero boilerplate. `init_async` is implicitly one-shot per boot, which matches hardware reality; a second call panics at `RESOURCES.init()`
- [x] Does `embassy-net 0.7` still expose `Stack` as a generic over the device? — No: `Stack<'d>` is lifetime-only in 0.7.1 (the device is erased behind `DriverAdapter`). `Runner<'d, D: Driver>` still carries the driver type. Updated struct shape to `Stack<'static>` + `Runner<'static, WifiDevice<'static>>`
- [x] Should `wait_for_ip().await` have a timeout variant? — No. Users who need a timeout can wrap the call in `embassy_time::with_timeout(..)`; a second variant would add API surface with no new capability

## State

- [x] Design approved
- [x] `embassy-feature-flag-v1` landed (blocker)
- [x] `init_inner()` refactor — blocking path still works (no refactor was actually needed; the existing `init_inner` already produces everything `init_async` consumes via a new `into_async_handle()` method)
- [x] `init_async()` + `AsyncWifiHandle` implemented
- [x] `wait_for_ip()` helper implemented
- [x] Type-check passes for C3 and C6 (`just check-wifi-hal-embassy` — both bare-metal targets clean)
- [x] CHANGELOG entry

## Session Log

- 2026-04-08 — Feature doc created from `docs/embassy-integration-research.md`
- 2026-04-08 — Implemented: `AsyncWifiHandle` struct added to the driver module, `WiFiManager::init_async` + private `into_async_handle()` method, `AsyncWifiHandle::wait_for_ip()` helper. `WifiDevice` already implements `embassy_net_driver::Driver` unconditionally via `esp-radio/wifi`, so no feature bridging was needed. `StackResources<3>` baseline (DHCP + 1 TCP + 1 UDP) wired via a function-local `StaticCell`. Seeded embassy-net's RNG from `esp_hal::time::Instant::now().duration_since_epoch().as_micros()`. `just fmt`, `just verify`, and `just check-wifi-hal-embassy` all pass clean on ESP32-C6 and ESP32-C3.
