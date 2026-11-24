//! Shared body of the bare-metal DDI OTA examples (`hal_c3_ddi`, `hal_c6_ddi`), the esp-hal twin of the ESP-IDF tier's `examples/common/ddi_app.rs`.
//!
//! Each example includes this file with `#[path = "common/ddi_app.rs"] mod ddi_app;` next to `#[path = "common/env.rs"] mod env;` (the shared Wi-Fi and MQTT settings), sets up the heap and the scheduler, names its board and calls [`run`]; the chip comes from the build target (`TARGET_CHIP`).
//! The configuration is compiled in through `option_env!` (the table is in each example's module docs).
//! No credential is ever logged: the token and the passwords are read where they are used and never stored in a struct that is logged, and the base URL is logged without userinfo, query or fragment.
//!
//! A start that fails, a missing or invalid value included, logs one `[app] refusing to start` line and restarts the device after 5 s through the restart callback.
//! On a new image that still awaits its health check, the bootloader then rolls back to the previous one, so a broken or misconfigured image never sits idle; an image flashed over serial restarts until it is fixed.
//! Wi-Fi gets until the health policy's failure deadline (counted from boot) to come up.
//!
//! Hook order of the DDI runtime, as on the ESP-IDF tier: `DdiRuntime::new` (validates every DDI setting before anything touches the store or the radio), `open_records` before Wi-Fi, `DdiRuntime::start` once Wi-Fi is up (its `DdiRunner` spawned as a task), `run_health_policy` on the main task.
//! The app owns the rest: it subscribes the nudge topic through `MqttHandle::subscribe` and hands every inbound topic to `DdiNudge::on_message` from its receive loop, supplies the health predicate (Wi-Fi ready and MQTT connected) and the restart callback, the only place the device is ever reset.
//! This tier has no wall clock (no SNTP), so feedback carries no timestamp.

use alloc::format;
use alloc::string::String;
use core::convert::Infallible;

use embassy_executor::Spawner;
use embassy_time::{with_timeout, Duration, Instant, Timer};
use esp_radio::wifi::Interface;
use juggler::ota::UrlForLog;
use rustyfarian_esp_hal_network::mqtt::{MqttBuilder, MqttHandle, MqttRunner, QoS};
use rustyfarian_esp_hal_network::ota::ddi::{
    open_records, DdiNudge, DdiRunner, DdiRuntime, DdiSettings, DdiToken, DdiTransportConfig,
    RestartFn,
};
use rustyfarian_esp_hal_network::ota::{SharedFlash, Version, TARGET_CHIP};
use rustyfarian_esp_hal_network::wifi::{
    link_task, AsyncWifiHandle, WiFiConfig, WiFiConfigExt, WiFiManager,
};

/// Period of the main loop's status line.
const HEARTBEAT: Duration = Duration::from_secs(30);

/// One wait for the link (associated with an IP); the waits repeat until the health policy's failure deadline.
const WIFI_WAIT: Duration = Duration::from_secs(10);

/// Wait between the refusal line and the restart, so the line reaches the console.
const RESTART_DELAY: Duration = Duration::from_secs(5);

static LINK: WiFiManager = WiFiManager::new();

/// The non-secret build-time settings.
///
/// No credential lives here, so every field may be logged.
struct BuildConfig {
    ssid: &'static str,
    mqtt_host: &'static str,
    mqtt_port: u16,
    base_url: &'static str,
    controller_id: &'static str,
    version: Version,
}

impl BuildConfig {
    /// Reads and checks every non-secret value; the first missing or invalid one is the error.
    fn from_build() -> Result<Self, String> {
        let ssid = required("WIFI_SSID", Some(crate::env::WIFI_SSID))?;
        let mqtt_host = crate::env::mqtt_host().map_err(String::from)?;
        let mqtt_port = crate::env::mqtt_port();
        let base_url = required("DDI_BASE_URL", option_env!("DDI_BASE_URL"))?;
        let controller_id = required("DDI_CONTROLLER_ID", option_env!("DDI_CONTROLLER_ID"))?;
        let version_text = required("FIRMWARE_VERSION", option_env!("FIRMWARE_VERSION"))?;
        let version = Version::parse(version_text)
            .map_err(|e| format!("FIRMWARE_VERSION is not MAJOR.MINOR.PATCH: {e}"))?;
        Ok(Self {
            ssid,
            mqtt_host,
            mqtt_port,
            base_url,
            controller_id,
            version,
        })
    }
}

/// A build-time value that must be set and non-empty.
fn required(name: &str, value: Option<&'static str>) -> Result<&'static str, String> {
    match value {
        Some(text) if !text.is_empty() => Ok(text),
        _ => Err(format!(
            "{name} is not set; put it in .env (just loads it at build time) and rebuild"
        )),
    }
}

/// MQTT password of the device user; `None` when `MQTT_PASS` is unset or empty, which sends the username alone.
fn mqtt_pass() -> Option<&'static str> {
    crate::env::mqtt_pass().filter(|pass| !pass.is_empty())
}

/// The DDI target token text; empty when `DDI_TOKEN` is unset, which `DdiToken::parse` refuses.
fn ddi_token() -> &'static str {
    option_env!("DDI_TOKEN").unwrap_or_default()
}

/// Reads `DDI_DEMO_UNHEALTHY`: unset or empty is off, `1` is on, anything else is a startup error.
fn demo_unhealthy() -> Result<bool, String> {
    match option_env!("DDI_DEMO_UNHEALTHY") {
        None | Some("") => Ok(false),
        Some("1") => Ok(true),
        Some(other) => Err(format!(
            "DDI_DEMO_UNHEALTHY must be 1 or unset, got '{other}'"
        )),
    }
}

/// The app's restart callback, the only place the device is ever reset.
fn restart_device() {
    esp_hal::system::software_reset();
}

/// The whole example: `boot` is the `Instant` captured right after the scheduler started, `board` the name the configData reports.
///
/// It never returns: a failed start logs the refusal and restarts the device (see the module docs).
pub async fn run(
    spawner: Spawner,
    boot: Instant,
    board: &'static str,
    wifi: esp_hal::peripherals::WIFI<'static>,
    flash: esp_hal::peripherals::FLASH<'static>,
) -> ! {
    let restart: RestartFn = restart_device;
    let error = match app(spawner, boot, board, wifi, flash, restart).await {
        Ok(never) => match never {},
        Err(error) => error,
    };
    log::error!(
        "[app] refusing to start: {error}; restarting in {} s",
        RESTART_DELAY.as_secs()
    );
    Timer::after(RESTART_DELAY).await;
    (restart)();
    loop {
        Timer::after(HEARTBEAT).await;
    }
}

/// The start and the main loop; returns only with the error that stopped the start.
async fn app(
    spawner: Spawner,
    boot: Instant,
    board: &'static str,
    wifi: esp_hal::peripherals::WIFI<'static>,
    flash: esp_hal::peripherals::FLASH<'static>,
    restart: RestartFn,
) -> Result<Infallible, String> {
    let config = BuildConfig::from_build()?;
    let unhealthy = demo_unhealthy()?;
    log::info!(
        "DDI example {} starting on {board} ({TARGET_CHIP}): ssid={} mqtt={}:{} controller_id={} base_url={}",
        config.version,
        config.ssid,
        config.mqtt_host,
        config.mqtt_port,
        config.controller_id,
        UrlForLog(config.base_url)
    );

    // Every check that can refuse the build-time configuration runs before the boot is recorded.
    let settings = DdiSettings::new(config.version, board)
        .map_err(|e| format!("DDI settings refused: {e}"))?;
    let failure_deadline = settings.timings.health.failure_deadline;
    let token = DdiToken::parse(ddi_token())
        .map_err(|e| format!("DDI_TOKEN refused: {e} (32 to 64 lowercase hex characters)"))?;
    let transport = DdiTransportConfig::new(config.base_url, config.controller_id, token);
    let runtime = DdiRuntime::new(settings, transport, restart)
        .map_err(|e| format!("DDI runtime refused: {e}"))?;
    let nudge = runtime.nudge();

    let flash = SharedFlash::new(flash).map_err(|e| format!("{e}"))?;
    let records =
        open_records(flash).map_err(|e| format!("the OTA record store cannot be opened: {e}"))?;

    let wifi_config =
        WiFiConfig::new(config.ssid, crate::env::wifi_pass()).with_wifi_scheduler_running(wifi);
    let AsyncWifiHandle {
        controller,
        stack,
        runner,
    } = WiFiManager::init_async(wifi_config).map_err(|e| format!("Wi-Fi init failed: {e}"))?;
    spawner.spawn(link_task(&LINK, controller, stack).unwrap());
    spawner.spawn(net_task(runner).unwrap());
    // The supervisor keeps reconnecting in the background; a slow router must not end the start of a new image early.
    loop {
        match with_timeout(WIFI_WAIT, LINK.wait_connected()).await {
            Ok(()) => break,
            Err(_) if since(boot) < failure_deadline => {
                log::warn!(
                    "[app] Wi-Fi not up {} s after boot; still waiting",
                    boot.elapsed().as_secs()
                );
            }
            Err(_) => {
                return Err(String::from(
                    "Wi-Fi did not come up before the health failure deadline",
                ))
            }
        }
    }
    if let Some(v4) = stack.config_v4() {
        log::info!("Wi-Fi connected — IP: {}", v4.address);
    }

    let mqtt_config = crate::env::mqtt_config(config.controller_id).map_err(String::from)?;
    let mqtt_config = match mqtt_pass() {
        Some(pass) => mqtt_config.with_auth(config.controller_id, pass),
        None => mqtt_config.with_username_only(config.controller_id),
    };
    let (mqtt, mqtt_runner) = MqttBuilder::new(mqtt_config)
        .build(stack)
        .map_err(|e| format!("MQTT build failed: {e}"))?;
    spawner.spawn(mqtt_task(mqtt_runner).unwrap());
    mqtt.subscribe(nudge.topic(), QoS::AtMostOnce)
        .map_err(|e| format!("the nudge topic cannot be subscribed: {e}"))?;
    spawner.spawn(nudge_task(mqtt, nudge).unwrap());
    log::info!("[app] nudge topic {} registered", nudge.topic());

    let (handle, ddi_runner) = runtime
        .start(records, stack)
        .map_err(|e| format!("DDI runtime start failed: {e}"))?;
    spawner.spawn(ddi_task(ddi_runner).unwrap());
    if unhealthy {
        log::error!(
            "*** DDI_DEMO_UNHEALTHY=1: the health predicate always fails; this image will be rolled back at the failure deadline ({}s) ***",
            failure_deadline.as_secs()
        );
    }

    let verdict = handle
        .run_health_policy(boot, || {
            !unhealthy && LINK.is_ready() && mqtt.is_connected()
        })
        .await;
    log::info!("[app] running after the health policy ({verdict:?})");

    loop {
        Timer::after(HEARTBEAT).await;
        let marks = handle.stack_high_water();
        log::info!(
            "[app] DDI: admission open {}, health settled {}, mqtt connected {}, worker stack min free {:?}, download stack min free {:?}, heap free {} B",
            handle.flags().admission_open(),
            handle.flags().health_settled(),
            mqtt.is_connected(),
            marks.worker_min_free_bytes,
            marks.download_min_free_bytes,
            esp_alloc::HEAP.free()
        );
    }
}

/// The time since `boot` as a `core` duration, the unit of the DDI settings.
fn since(boot: Instant) -> core::time::Duration {
    core::time::Duration::from_micros(boot.elapsed().as_micros())
}

/// Hands every inbound topic to the nudge hook; the payload is never read.
#[embassy_executor::task]
async fn nudge_task(mqtt: MqttHandle, nudge: DdiNudge) -> ! {
    loop {
        let message = mqtt.receive().await;
        if nudge.on_message(message.topic()) {
            log::info!("[app] nudge received");
        }
    }
}

#[embassy_executor::task]
async fn ddi_task(runner: DdiRunner) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn mqtt_task(runner: MqttRunner) -> ! {
    runner.run().await
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, Interface>) -> ! {
    runner.run().await
}
