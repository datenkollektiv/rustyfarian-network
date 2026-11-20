//! Shared body of the ESP-IDF DDI OTA examples (`idf_c3_ddi`, `idf_c6_ddi`).
//!
//! Each example includes this file with `#[path = "common/ddi_app.rs"] mod ddi_app;` next to `#[path = "common/env.rs"] mod env;` (the shared Wi-Fi and MQTT settings), names its board and calls [`run`]; the chip comes from the build target (`TARGET_CHIP`).
//! The configuration is compiled in through `option_env!` (the table is in each example's module docs).
//! No credential is ever logged: the token and the passwords are read where they are used and never stored in a struct that is logged, and the base URL is logged without userinfo, query or fragment.
//!
//! A start that fails, a missing or invalid value included, logs one `[app] refusing to start` line and restarts the device after 5 s through the restart callback.
//! On a new image that still awaits its health check, the bootloader then rolls back to the previous one, so a broken or misconfigured image never sits idle; an image flashed over serial restarts until it is fixed.
//! Wi-Fi gets until the health policy's failure deadline (counted from boot) to come up.
//!
//! Hook order of the DDI runtime, as its module docs require: `DdiRuntime::new` (validates every DDI setting before anything touches the store or the radio), `open_records` before Wi-Fi, `DdiRuntime::start` once Wi-Fi is up, `run_health_policy` on the main thread.
//! The app owns the rest: it starts SNTP, subscribes the nudge topic through `MqttBuilder::subscribe` and hands every inbound topic to `DdiNudge::on_message`, supplies the health predicate (Wi-Fi ready and MQTT connected) and the restart callback, the only place the device is ever reset.

use std::convert::Infallible;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Context;
use esp_idf_svc::hal::peripherals::Peripherals;
use esp_idf_svc::sntp::EspSntp;
use esp_idf_svc::{eventloop::EspSystemEventLoop, nvs::EspDefaultNvsPartition};
use rustyfarian_esp_idf_network::mqtt::{MqttBuilder, MqttConfig, QoS};
use rustyfarian_esp_idf_network::ota::ddi::{
    open_records, DdiRuntime, DdiSettings, DdiToken, DdiTransportConfig, RestartFn,
};
use rustyfarian_esp_idf_network::ota::{url_for_log, Version, TARGET_CHIP};
use rustyfarian_esp_idf_network::wifi::{WiFiConfig, WiFiManager};

/// Period of the main loop's status line.
const HEARTBEAT: Duration = Duration::from_secs(30);

/// One wait for the station's IP address; the waits repeat until the health policy's failure deadline.
const WIFI_WAIT_MS: u64 = 10_000;

/// Wait between the refusal line and the restart, so the line reaches the console.
const RESTART_DELAY: Duration = Duration::from_secs(5);

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
    fn from_build() -> anyhow::Result<Self> {
        let ssid = required("WIFI_SSID", Some(crate::env::WIFI_SSID))?;
        let mqtt_host = crate::env::mqtt_host()?;
        let mqtt_port = crate::env::mqtt_port();
        let base_url = required("DDI_BASE_URL", option_env!("DDI_BASE_URL"))?;
        let controller_id = required("DDI_CONTROLLER_ID", option_env!("DDI_CONTROLLER_ID"))?;
        let version_text = required("FIRMWARE_VERSION", option_env!("FIRMWARE_VERSION"))?;
        let version = Version::parse(version_text)
            .map_err(|e| anyhow::anyhow!("FIRMWARE_VERSION is not MAJOR.MINOR.PATCH: {e}"))?;
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
fn required(name: &str, value: Option<&'static str>) -> anyhow::Result<&'static str> {
    match value {
        Some(text) if !text.is_empty() => Ok(text),
        _ => anyhow::bail!(
            "{name} is not set; put it in .env (just loads it at build time) and rebuild"
        ),
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
fn demo_unhealthy() -> anyhow::Result<bool> {
    match option_env!("DDI_DEMO_UNHEALTHY") {
        None | Some("") => Ok(false),
        Some("1") => Ok(true),
        Some(other) => anyhow::bail!("DDI_DEMO_UNHEALTHY must be 1 or unset, got '{other}'"),
    }
}

/// Free heap in bytes, for the status line.
fn free_heap() -> u32 {
    unsafe { esp_idf_svc::sys::esp_get_free_heap_size() }
}

/// The whole example: `boot` is the `Instant` captured as the first statement of `main`, `board` the name the configData reports.
///
/// It never returns: a failed start logs the refusal and restarts the device (see the module docs).
pub fn run(boot: Instant, board: &'static str) -> ! {
    esp_idf_svc::sys::link_patches();
    esp_idf_svc::log::EspLogger::initialize_default();
    log::info!("[app] start {} (esp-idf)", env!("CARGO_CRATE_NAME"));

    let restart: RestartFn = Arc::new(|| {
        esp_idf_svc::hal::reset::restart();
    });
    let error = match app(boot, board, Arc::clone(&restart)) {
        Ok(never) => match never {},
        Err(error) => error,
    };
    log::error!(
        "[app] refusing to start: {error:#}; restarting in {} s",
        RESTART_DELAY.as_secs()
    );
    std::thread::sleep(RESTART_DELAY);
    restart();
    loop {
        std::thread::sleep(HEARTBEAT);
    }
}

/// The start and the main loop; returns only with the error that stopped the start.
fn app(boot: Instant, board: &'static str, restart: RestartFn) -> anyhow::Result<Infallible> {
    let config = BuildConfig::from_build()?;
    let unhealthy = demo_unhealthy()?;
    log::info!(
        "DDI example {} starting on {board} ({TARGET_CHIP}): ssid={} mqtt={}:{} controller_id={} base_url={}",
        config.version,
        config.ssid,
        config.mqtt_host,
        config.mqtt_port,
        config.controller_id,
        url_for_log(config.base_url)
    );

    // Every check that can refuse the build-time configuration runs before the boot is recorded.
    let settings = DdiSettings::new(config.version, board)
        .map_err(|e| anyhow::anyhow!("DDI settings refused: {e}"))?;
    let failure_deadline = settings.timings.health.failure_deadline;
    let token = DdiToken::parse(ddi_token()).map_err(|e| {
        anyhow::anyhow!("DDI_TOKEN refused: {e} (32 to 64 lowercase hex characters)")
    })?;
    let transport = DdiTransportConfig::new(config.base_url, config.controller_id, token);
    let runtime = DdiRuntime::new(settings, transport, restart)
        .map_err(|e| anyhow::anyhow!("DDI runtime refused: {e}"))?;
    let nudge = runtime.nudge();
    let nudge_topic = runtime.nudge_topic().to_owned();

    let peripherals = Peripherals::take()?;
    let sys_loop = EspSystemEventLoop::take()?;
    let nvs = EspDefaultNvsPartition::take()?;

    let records = open_records(nvs.clone()).context("the OTA record store cannot be opened")?;

    let wifi_config = WiFiConfig::new(config.ssid, crate::env::wifi_pass());
    let wifi = WiFiManager::new_without_led(peripherals.modem, sys_loop, Some(nvs), wifi_config)?;
    // The manager keeps reconnecting in the background; a slow router must not end the start of a new image early.
    let ip = loop {
        match wifi.wait_connected_timeout(WIFI_WAIT_MS) {
            Ok(ip) => break ip,
            Err(_) if boot.elapsed() < failure_deadline => {
                log::warn!(
                    "[app] Wi-Fi not up {} s after boot; still waiting",
                    boot.elapsed().as_secs()
                );
            }
            Err(e) => {
                return Err(e.context("Wi-Fi did not come up before the health failure deadline"))
            }
        }
    };
    log::info!("Wi-Fi connected — IP: {ip}");

    let _sntp = EspSntp::new_default().context("SNTP could not be started")?;
    log::info!("[app] SNTP started");

    let mqtt_config = MqttConfig::new(config.mqtt_host, config.mqtt_port, config.controller_id);
    let mqtt_config = match mqtt_pass() {
        Some(pass) => mqtt_config.with_auth(config.controller_id, pass),
        None => mqtt_config.with_username_only(config.controller_id),
    };
    let mqtt = MqttBuilder::new(mqtt_config)
        .subscribe(nudge_topic.clone(), QoS::AtMostOnce)
        .on_message(move |topic, _payload| {
            if nudge.on_message(topic) {
                log::info!("[app] nudge received");
            }
        })
        .build(&wifi)?;
    log::info!("[app] nudge topic {nudge_topic} registered");

    let handle = runtime.start(records)?;
    if unhealthy {
        log::error!(
            "*** DDI_DEMO_UNHEALTHY=1: the health predicate always fails; this image will be rolled back at the failure deadline ({}s) ***",
            failure_deadline.as_secs()
        );
    }

    let verdict = handle.run_health_policy(boot, || {
        !unhealthy && wifi.is_ready() && mqtt.is_connected()
    });
    log::info!("[app] running after the health policy ({verdict:?})");

    loop {
        std::thread::sleep(HEARTBEAT);
        let marks = handle.stack_high_water();
        log::info!(
            "[app] DDI: admission open {}, health settled {}, mqtt connected {}, worker stack min free {:?}, download stack min free {:?}, heap free {} B",
            handle.flags().admission_open(),
            handle.flags().health_settled(),
            mqtt.is_connected(),
            marks.worker_min_free_bytes,
            marks.download_min_free_bytes,
            free_heap()
        );
    }
}
