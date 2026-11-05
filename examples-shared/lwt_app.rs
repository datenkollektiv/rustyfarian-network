//! One application for both tiers (ADR 022, Step 9).
//!
//! `idf_esp32s3_mqtt_lwt`, `hal_esp32s3_mqtt_lwt`, `hal_c3_mqtt_lwt` and `hal_esp32_mqtt_lwt`
//! include this file with
//! `#[path = "../../../examples-shared/lwt_app.rs"] mod lwt_app;`.
//! It is bench-only: both crates exclude their LWT example from the published package.
//!
//! The file compiles under the esp-hal example's `#![no_std]`, so it uses `core`,
//! `heapless`, `embassy-time` and `embassy-futures`, never `std`.

use core::fmt::Write as _;

use embassy_futures::join::join3;
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
///
/// Every loop starts at once: the first-connect marker must not hold back the tick
/// loop, the receive loop or the suspend demo when the broker is unreachable at boot.
pub async fn run(link: &impl NetworkLink, mqtt: &impl MqttClient, cfg: &LwtAppConfig<'_>) {
    join3(
        ready_marker(mqtt),
        tick_loop(link, mqtt, cfg),
        rx_loop(mqtt),
    )
    .await;
}

/// Logs the first broker session; it completes after one connect and the other loops run on.
async fn ready_marker(mqtt: &impl MqttClient) {
    mqtt.wait_connected().await;
    log::info!("[app] mqtt ready — connect #{}", mqtt.connect_count());
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
