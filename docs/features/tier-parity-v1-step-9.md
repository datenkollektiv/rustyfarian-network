# Tier Parity Step 9 — One App, Two Tiers

Status: desk work, awaiting review (2026-11-02).
Program plan: [tier-parity-v1-plan.md](tier-parity-v1-plan.md), Step 9.
Feature: [tier-parity-v1.md](tier-parity-v1.md).

## Goal

Prove that one application body compiles and runs unchanged on both tiers.
`idf_esp32s3_mqtt_lwt` and `hal_esp32s3_mqtt_lwt` shrink to setup plus one shared `run`.
The shared module lives outside both crates and is bench-only.

## Non-goals

- No change to `juggler` traits or the tier crates' library code.
- No new example crate and no publishing of the shared module.
- No rgb-clock migration in this step; that is the post-Phase-6 acceptance test.

## Decisions from review (2026-11-02)

| Decision             | Choice                                                               | Reason                                                                                                                     |
|:---------------------|:---------------------------------------------------------------------|:---------------------------------------------------------------------------------------------------------------------------|
| Module sharing       | `#[path = "../../../examples-shared/lwt_app.rs"]` from both examples | Rust resolves `#[path]` relative to the declaring file; the same three `..` reach the workspace root from both crates      |
| Publishing           | Exclude the two LWT example files from their packages                | `cargo package` drops the matching `[[example]]` target with a warning and verifies cleanly; all other examples still ship |
| IDF executor         | `esp_idf_svc::hal::task::block_on`                                   | `idf_c3_async_mqtt` already runs `block_on(join(app, rx))` with `embassy_time::Timer` on hardware; no new dependency       |
| Shared app ownership | Bench harness only                                                   | App code stays out of the published crates; rgb-clock is the later customer proof                                          |
| `run` signature      | `run(link, mqtt, cfg)`                                               | Topics are runtime strings and the heap reporter is tier-specific                                                          |

## Why rgb-clock is not the vehicle yet

- Its esp-hal firmware enables `provisioning, embassy, rt` but not `mqtt`, so the Wi-Fi plus MQTT app cannot run there today.
- Its OTA path is ESP-IDF only, and provisioning still differs, so a full shared app is blocked on Phase 6.
- It targets the C3 and C6; the Step 9 hardware gate is the CrowPanel S3.
- The in-repo LWT pair is the only complete Wi-Fi plus MQTT slice that can run the S3 gate today.

## Files

| File                                                                  | Change                                               |
|:----------------------------------------------------------------------|:-----------------------------------------------------|
| `examples-shared/lwt_app.rs`                                          | New shared application body                          |
| `crates/rustyfarian-esp-idf-network/examples/idf_esp32s3_mqtt_lwt.rs` | Setup only, then `block_on(lwt_app::run(..))`        |
| `crates/rustyfarian-esp-hal-network/examples/hal_esp32s3_mqtt_lwt.rs` | Setup only, then `lwt_app::run(..).await`            |
| `crates/rustyfarian-esp-idf-network/Cargo.toml`                       | `heapless` dev-dependency; `exclude` the LWT example |
| `crates/rustyfarian-esp-hal-network/Cargo.toml`                       | `exclude` the LWT example                            |
| both crate `README.md`                                                | New "Tier capability differences" section            |
| `docs/features/tier-parity-v1-plan.md`                                | Step 9 signature and packaging amendment             |
| `docs/features/tier-parity-v1.md`                                     | Session log entry                                    |

## Shared module

The module is compiled by the esp-hal example, which is `#![no_std]`.
It therefore uses `core`, `heapless`, `embassy-time`, `embassy-futures`, `log` and `juggler`, and never `std`, `format!` or `anyhow`.
Both binaries install a `log` logger, so the `log` macros need no feature work.

`examples-shared/lwt_app.rs`:

```rust
//! One application for both tiers (ADR 022, Step 9).
//!
//! `idf_esp32s3_mqtt_lwt` and `hal_esp32s3_mqtt_lwt` include this file with
//! `#[path = "../../../examples-shared/lwt_app.rs"] mod lwt_app;`.
//! It is bench-only: both crates exclude their LWT example from the published package.
//!
//! The file compiles under the esp-hal example's `#![no_std]`, so it uses `core`,
//! `heapless`, `embassy-time` and `embassy-futures`, never `std`.

use core::fmt::Write as _;

use embassy_futures::join::join;
use embassy_time::{Duration, Timer};
use juggler::mqtt::{MqttClient, MqttMessage, TryPublishError};
use juggler::wifi::NetworkLink;

/// Tick period; both tiers publish on this cadence.
pub const TICK_SECS: u64 = 5;
/// Free-heap reporting period.
pub const HEAP_LOG_SECS: u64 = 60;
/// Suspend demo: one cycle per minute.
pub const SUSPEND_EVERY_SECS: u64 = 60;
/// Suspend demo: twenty seconds suspended.
pub const SUSPEND_FOR_SECS: u64 = 20;
/// Poll period of the bounded wait for `is_suspended`.
const SUSPEND_POLL_MS: u64 = 10;
/// Stack buffer for the tick payload.
const PAYLOAD_CAPACITY: usize = 96;

/// Tier-supplied knobs the shared app cannot derive from the traits.
pub struct LwtAppConfig<'a> {
    /// Topic for the periodic tick.
    pub tick_topic: &'a str,
    /// Runs the suspend and resume demo once per minute.
    pub suspend_demo: bool,
    /// Upper bound on the wait for a completed suspend after `suspend()`.
    pub suspend_timeout_ms: u64,
    /// Tier heap reporter; the value is logged and sent in the tick payload.
    pub free_heap: fn() -> usize,
}

/// Runs the LWT application until the process ends.
pub async fn run(link: &impl NetworkLink, mqtt: &impl MqttClient, cfg: &LwtAppConfig<'_>) {
    mqtt.wait_connected().await;
    log::info!("[app] mqtt ready — connect #{}", mqtt.connect_count());
    join(tick_loop(link, mqtt, cfg), rx_loop(mqtt)).await;
}

async fn tick_loop(link: &impl NetworkLink, mqtt: &impl MqttClient, cfg: &LwtAppConfig<'_>) {
    let mut tick: u32 = 0;
    let mut last_link = link.state();
    let mut last_state = mqtt.state();
    let mut was_connected = false;
    let mut secs_since_heap_log: u64 = 0;

    loop {
        Timer::after(Duration::from_secs(TICK_SECS)).await;
        tick += 1;
        secs_since_heap_log += TICK_SECS;
        let uptime = tick as u64 * TICK_SECS;

        if cfg.suspend_demo && uptime % SUSPEND_EVERY_SECS == 0 {
            log::info!("[app] demo: suspend for {} s", SUSPEND_FOR_SECS);
            if let Err(e) = link.suspend() {
                log::warn!("[app] suspend failed: {:?}", e);
            }
            let mut waited_ms = 0;
            while !link.is_suspended() && waited_ms < cfg.suspend_timeout_ms {
                Timer::after(Duration::from_millis(SUSPEND_POLL_MS)).await;
                waited_ms += SUSPEND_POLL_MS;
            }
            log::info!("[app] demo: is_suspended = {}", link.is_suspended());
        } else if cfg.suspend_demo && uptime % SUSPEND_EVERY_SECS == SUSPEND_FOR_SECS {
            log::info!("[app] demo: resume");
            if let Err(e) = link.resume() {
                log::warn!("[app] resume failed: {:?}", e);
            }
        }

        let link_state = link.state();
        if link_state != last_link {
            log::info!("[app] wifi link {:?} -> {:?}", last_link, link_state);
            last_link = link_state;
        }

        let state = mqtt.state();
        if state != last_state {
            log::info!("[app] mqtt state {:?} -> {:?}", last_state, state);
            last_state = state;
        }

        if mqtt.is_connected() && !was_connected {
            log::info!(
                "[app] connect #{} free heap: {} B",
                mqtt.connect_count(),
                (cfg.free_heap)()
            );
        }
        was_connected = mqtt.is_connected();

        let mut payload = heapless::String::<PAYLOAD_CAPACITY>::new();
        let _ = write!(
            &mut payload,
            "{{\"tick\":{},\"uptime_s\":{},\"free_heap\":{}}}",
            tick,
            uptime,
            (cfg.free_heap)()
        );
        match mqtt.try_publish(cfg.tick_topic, payload.as_bytes()) {
            Ok(()) => log::info!("[app] tick #{} queued", tick),
            Err(TryPublishError::WouldBlock) => {
                log::info!("[app] tick #{} would block (mqtt {:?})", tick, state)
            }
            Err(TryPublishError::Other(e)) => {
                log::warn!("[app] tick #{} failed: {:?}", tick, e)
            }
        }

        if secs_since_heap_log >= HEAP_LOG_SECS {
            secs_since_heap_log = 0;
            log::info!(
                "[app] heap free: {} B, connects: {}",
                (cfg.free_heap)(),
                mqtt.connect_count()
            );
        }
    }
}

async fn rx_loop(mqtt: &impl MqttClient) {
    loop {
        let msg = mqtt.receive().await;
        match core::str::from_utf8(msg.payload()) {
            Ok(text) => log::info!(
                "[app] rx topic={} payload={} retained={}",
                msg.topic(),
                text,
                msg.retained()
            ),
            Err(_) => log::info!(
                "[app] rx topic={} payload={} bytes (binary) retained={}",
                msg.topic(),
                msg.payload().len(),
                msg.retained()
            ),
        }
    }
}
```

The `[app]` prefix is what makes the "identical app logs" gate checkable.
Setup logs stay `[example]` and `[mqtt]` and may differ between tiers.
The status line is the state-transition log, not a publish: the retained `online` message is still published by each tier's setup.

`retained` is logged but `qos` is not: the IDF inbound type reports `retained = false` and `QoS::AtMostOnce` always (`crates/rustyfarian-esp-idf-network/src/mqtt/session.rs:40`).
The `cmd` topic is not retained, so both tiers log `retained=false` and the line is identical.
The `cmd` path logs the inbound message; it does not republish it.

## ESP-IDF example

The example keeps its setup and loses its loop.
It also loses the `on_disconnect` log and the module-doc sentence that claim esp-mqtt reconnects on its own; Step 8b-2 turned auto-reconnect off and the session machine paces retries.
`on_message` is removed so inbound messages reach `MqttHandle::receive`, which the shared app drains.
The module docs are rewritten around the shared app and the `block_on` executor.

Imports and constants become:

```rust
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::mqtt::client::EspMqttClient;
use esp_idf_svc::{eventloop::EspSystemEventLoop, nvs::EspDefaultNvsPartition};
use rustyfarian_esp_idf_network::mqtt::{to_esp_qos, LwtConfig, MqttBuilder, QoS};
use rustyfarian_esp_idf_network::wifi::{WiFiConfig, WiFiConfigExt, WiFiManager};

#[path = "../../../examples-shared/lwt_app.rs"]
mod lwt_app;
#[path = "common/env.rs"]
mod env;

const KEEP_ALIVE_SECS: u64 = 30;
const WIFI_FIRST_CONNECT_WAIT_MS: u64 = 30_000;
const SUSPEND_TIMEOUT_MS: u64 = 500;

fn free_heap() -> usize {
    // SAFETY: plain FFI query with no arguments or side effects.
    unsafe { esp_idf_svc::sys::esp_get_free_heap_size() as usize }
}
```

The builder keeps the retained `online` publish on connect and the `cmd` subscription:

```rust
let mqtt = MqttBuilder::new(mqtt_config)
    .subscribe(&cmd_topic, QoS::AtLeastOnce)
    .on_connect(
        move |client: &mut EspMqttClient<'_>, session_present: bool| {
            client.enqueue(&online_topic, to_esp_qos(QoS::AtLeastOnce), true, b"online")?;
            log::info!(
                "[mqtt] connected ({}) — published retained online",
                if session_present {
                    "session resumed"
                } else {
                    "clean session"
                }
            );
            Ok(())
        },
    )
    .build(&wifi)?;
```

The end of `main` becomes:

```rust
log::info!(
    "[example] topics: status={} cmd={} tick={}",
    status_topic,
    cmd_topic,
    tick_topic
);

let cfg = lwt_app::LwtAppConfig {
    tick_topic: &tick_topic,
    suspend_demo: env::mqtt_suspend_demo(),
    suspend_timeout_ms: SUSPEND_TIMEOUT_MS,
    free_heap,
};
esp_idf_svc::hal::task::block_on(lwt_app::run(&wifi, &mqtt, &cfg));
Ok(())
```

`block_on` needs the esp-idf-svc time driver and a timer queue.
Both are already dev-dependencies: `esp-idf-svc` with `embassy-time-driver`, `embassy-time`, and `embassy-time-queue-utils` with `generic-queue-16`.

## esp-hal example

The example keeps its setup and loses `rx_task` and the main loop.
`mqtt_task` and `net_task` stay.
The module docs are rewritten around the shared app.

Imports and constants become:

```rust
use embassy_executor::Spawner;
use esp_backtrace as _;
use esp_hal::timer::timg::TimerGroup;
use esp_radio::wifi::Interface;
use rustyfarian_esp_hal_network::mqtt::{LwtConfig, MqttBuilder, MqttRunner, QoS};
use rustyfarian_esp_hal_network::wifi::{
    link_task, AsyncWifiHandle, WiFiConfig, WiFiConfigExt, WiFiManager, WifiLink,
    SUSPEND_DISCONNECT_TIMEOUT_MS,
};
use static_cell::StaticCell;

#[path = "../../../examples-shared/lwt_app.rs"]
mod lwt_app;
#[path = "common/env.rs"]
mod env;

const KEEP_ALIVE_SECS: u64 = 30;

fn free_heap() -> usize {
    esp_alloc::HEAP.free()
}
```

The end of `main` becomes:

```rust
spawner.spawn(mqtt_task(mqtt_runner).unwrap());

log::info!(
    "[example] topics: status={} cmd={} tick={}",
    status_topic,
    cmd_topic,
    tick_topic
);

let cfg = lwt_app::LwtAppConfig {
    tick_topic,
    suspend_demo: env::mqtt_suspend_demo(),
    suspend_timeout_ms: SUSPEND_DISCONNECT_TIMEOUT_MS,
    free_heap,
};
lwt_app::run(&LINK, &mqtt, &cfg).await;
```

`MqttHandle` is `Clone, Copy`, so the handle is still usable after the runner task is spawned.

## Manifests

In `crates/rustyfarian-esp-idf-network/Cargo.toml`, add to the existing `[package]` table:

```toml
exclude = ["examples/idf_esp32s3_mqtt_lwt.rs"]
```

In the same file, add to the existing `[dev-dependencies]` table:

```toml
heapless = { workspace = true }
```

In `crates/rustyfarian-esp-hal-network/Cargo.toml`, add to the existing `[package]` table:

```toml
exclude = ["examples/hal_esp32s3_mqtt_lwt.rs"]
```

`cargo package` prints `ignoring example ...` for each excluded file and drops the corresponding `[[example]]` target from the package.
The remaining examples and `examples/common/env.rs` still ship.

## Capability table

Add the same section before "Integration with juggler" in both READMEs.
Use the absolute GitHub link for the design doc, because crates.io cannot resolve relative links.

```markdown
## Tier capability differences

The tiers share one API ([ADR 022](https://github.com/datenkollektiv/rustyfarian-network/blob/main/docs/adr/022-tier-parity-one-async-api.md)).
The differences below are capabilities, not API differences.
Design detail: [Wi-Fi suspend, resume and deep sleep](https://github.com/datenkollektiv/rustyfarian-network/blob/main/docs/wifi-suspend-and-deep-sleep.md).

| Capability                   | `rustyfarian-esp-idf-network`                                  | `rustyfarian-esp-hal-network`                  |
|:-----------------------------|:---------------------------------------------------------------|:-----------------------------------------------|
| Radio while suspended        | Stopped (`esp_wifi_stop`)                                      | Powered, station disconnected                  |
| Broker address               | Hostname or IPv4 (DNS via lwIP)                                | IPv4 literal only (no DNS yet)                 |
| Subscriptions                | Build time and runtime                                         | Build time only                                |
| Inbound messages             | Callback and/or the `receive()` queue                          | `receive()` channel; no callback               |
| Inbound retain and QoS       | Not reported (retain is always false, QoS always `AtMostOnce`) | Reported                                       |
| Executor                     | std threads; `block_on` for the async app                      | `esp-rtos` plus the embassy executor           |
| Startup message              | `on_connect` closure plus `enqueue`                            | `with_startup_message`                         |
| Suspend call                 | Blocking; the radio is stopped when it returns                 | Non-blocking signal; wait for `is_suspended()` |
```

The Step 2 "MQTT reconnect after link-up" row is not in the table.
Step 8b-2's network gate makes link-up reconnection immediate on both tiers, and a session loss with the network up waits `MqttConfig::reconnect_timeout_ms` on both, with the same 10 s default.

## Validation

Run in this order:

1. `just fmt` (rustfmt follows the `#[path]` module and formats `examples-shared/lwt_app.rs`).
2. `just verify`.
3. `just build-example idf_esp32s3_mqtt_lwt`.
4. `just build-example hal_esp32s3_mqtt_lwt`.
5. `cargo package --list -p rustyfarian-esp-idf-network` and `cargo package --list -p rustyfarian-esp-hal-network`: the LWT example must be absent and `cargo package` must report the ignored example. The `release-dry-run` recipe already runs the list step.

## Hardware gate (not part of this desk work)

CrowPanel S3 runbook R1 and R3 on both tiers.
Both runs must show the same `[app]` lines: `mqtt ready`, `connect #N`, the per-tick lines, the `cmd` rx line, and the suspend demo when `MQTT_SUSPEND_DEMO=1`.
The same bench session also covers the open esp-hal R1 cell and T5a/T5b.

## Follow-up

Open a downstream rgb-clock task after its esp-hal firmware enables `mqtt` and Phase 6 closes provisioning and OTA parity.
Until then the in-repo LWT pair is the one-app proof.

## Suggested commit

`Share one LWT application between the two tiers`
