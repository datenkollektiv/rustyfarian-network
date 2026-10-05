//! The OTA consumer runtime for the ESP-IDF tier: MQTT command intake, the update worker, the `rolled_back` reporter and the health policy.
//!
//! Every decision lives in the pure state machines of `juggler::ota::runtime` (host-tested); this module owns the threads, channels, the store lock, HTTP, flash and MQTT around them.
//!
//! # Hook points
//!
//! 1. [`open_records`], early in `main`, before Wi-Fi: opens the record store and records the boot slot, so the note survives a crash later in that boot.
//! 2. [`channel`], before the MQTT client is built: the subscribe callback needs the [`OtaSubmitter`].
//! 3. [`OtaRuntime::start`], once the MQTT client exists: reconciles the boot records and starts the reporter and worker threads.
//! 4. [`OtaHandle::run_health_policy`], on the app's main thread, instead of parking.
//!
//! Capture `let boot = Instant::now();` as the first statement of `main` and pass it to hook 4.
//!
//! # Threads
//!
//! - `ota-reporter` (started first) publishes rejections and delivers the `rolled_back` report with acknowledged publishes; it keeps making progress while a download runs.
//! - `ota-worker` runs one command at a time (update, operator rollback, repair); a command that arrives while another is queued or running is answered `busy`.
//! - The health policy runs on the app thread.
//!
//! The MQTT callback only parses, flips atomics and `try_send`s (ADR 017); it never calls the client.
//!
//! # Reboots
//!
//! The runtime never restarts the device itself.
//! After a successful download the worker waits `restart_grace`, then calls the app's [`RestartFn`]; the health policy calls it only for `DeadlineAction::NoteAndRestart`.
//! The only other reboot is `OtaSession::rollback`, used for an operator rollback, for a rollback demanded at boot, and at the health deadline.
//!
//! # Fail closed
//!
//! If `open_records` or `start` fails there is no health policy, a pending image is not marked valid and the bootloader aborts it at the next reset.
//!
//! # Commands are never retained
//!
//! Publish commands without the retain flag: a retained command would run again on every reconnect and boot.
//!
//! # Store lock
//!
//! The record store sits behind one mutex that is only taken through short closures that contain store calls and nothing else; never across HTTP, `OtaSession`, hardware reads, sleeps, publishes or channel operations.

mod health;
mod intake;
mod manifest;
mod publish;
mod reporter;
mod shared;
mod stack;
mod worker;

use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;

use anyhow::Context;
use esp_idf_svc::nvs::EspDefaultNvsPartition;
use juggler::mqtt::validate_publish_topic;
use juggler::ota::runtime::initial_admission;
use juggler::ota::{
    reconcile_boot, BootDisposition, BootFault, FailReason, OtaCommand, OtaStore, StoreError,
    Version,
};

pub use intake::OtaSubmitter;
pub use juggler::ota::{
    ConfigError, DeadlineAction, HealthVerdict, OtaSettings, OtaStacks, OtaTimings,
    MIN_STACK_BYTES, MIN_WORKER_STACK_BYTES,
};
pub use shared::OtaFlags;
pub use stack::StackMarks;

use self::reporter::ReporterCtx;
use self::shared::Shared;
use self::worker::WorkerCtx;
use super::{
    note_boot_slot_idf, open_store, read_hardware_facts, slot_evidence_line, BusyRetry, EspNvsKv,
    OtaSession, OtaSessionConfig,
};
use crate::mqtt::MqttHandle;

/// Called to restart the device.
///
/// The app decides how: the worker calls it after a successful download, the health policy only for `DeadlineAction::NoteAndRestart`.
/// It normally never returns; if it does the worker parks (every later command answers `busy`) and the health policy returns `HealthVerdict::RestartReturned`.
pub type RestartFn = Arc<dyn Fn() + Send + Sync>;

/// Runtime configuration: the platform-neutral [`OtaSettings`] plus the restart callback.
#[derive(Clone)]
pub struct OtaConfig {
    /// Topics, version, timings and stack sizes.
    pub settings: OtaSettings,
    /// How the device restarts.
    pub restart: RestartFn,
}

impl OtaConfig {
    /// A configuration with default timings and stacks.
    ///
    /// `command_topic` is subscribed to; `status_topic` receives statuses.
    /// Neither may contain a wildcard, and they must differ.
    ///
    /// # Errors
    ///
    /// Any [`ConfigError`] of [`OtaSettings::validate`].
    pub fn new(
        command_topic: impl Into<String>,
        status_topic: impl Into<String>,
        running_version: Version,
        restart: RestartFn,
    ) -> Result<Self, ConfigError> {
        Ok(Self {
            settings: OtaSettings::new(command_topic, status_topic, running_version)?,
            restart,
        })
    }

    /// Checks the settings again (the fields are public).
    ///
    /// # Errors
    ///
    /// Any [`ConfigError`] of [`OtaSettings::validate`].
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.settings.validate()
    }
}

/// The record store, shared by every runtime thread behind one mutex.
///
/// Clone it freely; the app only passes it from [`open_records`] to [`OtaRuntime::start`].
#[derive(Clone)]
pub struct Records(Arc<Mutex<OtaStore<EspNvsKv>>>);

impl Records {
    /// Runs `f` with the store locked; a poisoned lock is ignored (the store holds no invariant across a panic: every record write is one commit).
    ///
    /// `f` must contain store calls only.
    pub(crate) fn with<R>(&self, f: impl FnOnce(&mut OtaStore<EspNvsKv>) -> R) -> R {
        let mut guard = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        f(&mut guard)
    }
}

/// Hook 1, early in `main` before Wi-Fi: opens the record store and records the boot slot.
///
/// A failure of the boot-slot note is only logged; boot reconciliation in [`OtaRuntime::start`] is the gate.
///
/// # Errors
///
/// [`StoreError`] when the store cannot be opened.
pub fn open_records(nvs: EspDefaultNvsPartition) -> Result<Records, StoreError> {
    let mut store = open_store(nvs)?;
    match note_boot_slot_idf(&mut store) {
        Ok(note) => log::info!("[ota] boot slot noted: {note:?}"),
        Err(e) => log::warn!("[ota] the boot slot was not noted: {e}"),
    }
    Ok(Records(Arc::new(Mutex::new(store))))
}

struct Parts {
    config: OtaConfig,
    flags: OtaFlags,
    commands: Receiver<OtaCommand>,
    rejections: Receiver<FailReason>,
}

/// The runtime before it is started; created by [`channel`].
///
/// Dropping it without calling [`OtaRuntime::start`] closes the command path: later commands are answered `worker_unavailable`.
pub struct OtaRuntime {
    shared: Arc<Shared>,
    parts: Option<Parts>,
}

impl Drop for OtaRuntime {
    fn drop(&mut self) {
        if self.parts.is_some() {
            self.shared.close();
        }
    }
}

/// Hook 2: creates the command channel before the MQTT client is built.
///
/// Subscribe to [`OtaSubmitter::command_topic`] and call [`OtaSubmitter::handle`] from `on_message`.
/// `config` is validated again by [`OtaRuntime::start`].
pub fn channel(config: OtaConfig) -> (OtaSubmitter, OtaRuntime) {
    let shared = Arc::new(Shared::new());
    let (command_tx, commands) = mpsc::sync_channel(1);
    let depth = config.settings.timings.reject_queue_depth.max(1);
    let (rejection_tx, rejections) = mpsc::sync_channel(depth);
    let submitter = OtaSubmitter::new(
        config.settings.command_topic.clone(),
        Arc::clone(&shared),
        command_tx,
        rejection_tx,
    );
    let runtime = OtaRuntime {
        shared,
        parts: Some(Parts {
            config,
            flags: OtaFlags::new(),
            commands,
            rejections,
        }),
    };
    (submitter, runtime)
}

impl OtaRuntime {
    /// Hook 3: reconciles the boot records, then starts the reporter and the worker.
    ///
    /// Runs the boot reconciliation on the calling thread; if it demands an immediate rollback (the running image is not the promised version) that rollback is started here, outside any store lock.
    /// On a boot whose running slot is already valid (and not refused or failed closed) admission is open at once; otherwise it stays closed until [`OtaHandle::run_health_policy`] has read the running slot.
    ///
    /// # Errors
    ///
    /// The configuration is invalid or a thread cannot be spawned; the command path is then closed (later commands are answered `worker_unavailable`).
    pub fn start(mut self, mqtt: MqttHandle, records: Records) -> anyhow::Result<OtaHandle> {
        let shared = Arc::clone(&self.shared);
        let Some(parts) = self.parts.take() else {
            shared.close();
            anyhow::bail!("the OTA runtime was already started");
        };
        match launch(parts, Arc::clone(&shared), mqtt, records) {
            Ok(handle) => Ok(handle),
            Err(e) => {
                shared.close();
                Err(e)
            }
        }
    }
}

fn launch(
    parts: Parts,
    shared: Arc<Shared>,
    mqtt: MqttHandle,
    records: Records,
) -> anyhow::Result<OtaHandle> {
    let Parts {
        config,
        flags,
        commands,
        rejections,
    } = parts;
    config.validate().context("invalid OTA configuration")?;
    validate_publish_topic(&config.settings.status_topic)
        .map_err(|e| anyhow::anyhow!("invalid OTA status topic: {e}"))?;
    let cfg = Arc::new(config);

    let running = cfg.settings.running_version;
    let hardware = read_hardware_facts(BusyRetry::default());
    log::info!("{}", slot_evidence_line(&hardware, BusyRetry::default()));
    let outcome = records.with(|store| reconcile_boot(store, hardware, Some(running)));
    log::info!(
        "[ota] boot reconciliation: {:?}, slot released: {}, report pending: {}",
        outcome.disposition,
        outcome.slot_released,
        outcome.report_pending
    );
    if outcome.warnings != Default::default() {
        log::warn!("[ota] boot bookkeeping warnings: {:?}", outcome.warnings);
    }
    let admit = initial_admission(&outcome);
    flags.set_admission_open(admit);
    flags.set_health_settled(admit);
    flags.set_refuse_mark_valid(outcome.refuse_mark_valid);
    flags.set_roll_back_now_at_boot(outcome.roll_back_now);
    flags.set_records_suspect(matches!(
        outcome.disposition,
        BootDisposition::FailedClosed(BootFault::Store(_))
    ));

    if outcome.roll_back_now {
        roll_back_at_boot(&cfg);
    }

    let reporter_ctx = ReporterCtx {
        cfg: Arc::clone(&cfg),
        shared: Arc::clone(&shared),
        records: records.clone(),
        mqtt: mqtt.clone(),
    };
    thread::Builder::new()
        .name("ota-reporter".into())
        .stack_size(cfg.settings.stacks.reporter_bytes)
        .spawn(move || reporter::run(reporter_ctx, rejections))
        .context("cannot spawn the OTA reporter thread")?;

    let worker_ctx = WorkerCtx {
        cfg: Arc::clone(&cfg),
        shared: Arc::clone(&shared),
        flags: flags.clone(),
        records: records.clone(),
        mqtt: mqtt.clone(),
    };
    thread::Builder::new()
        .name("ota-worker".into())
        .stack_size(cfg.settings.stacks.worker_bytes)
        .spawn(move || worker::run(worker_ctx, commands))
        .context("cannot spawn the OTA worker thread")?;

    log::info!(
        "[ota] runtime started: worker stack {} bytes, reporter stack {} bytes",
        cfg.settings.stacks.worker_bytes,
        cfg.settings.stacks.reporter_bytes
    );
    Ok(OtaHandle {
        cfg,
        flags,
        shared,
        records,
        mqtt,
        health_started: AtomicBool::new(false),
    })
}

/// The boot reconciliation demanded a rollback: the running image is not the version its manifest promised.
///
/// Runs on the caller's thread with no store lock held.
/// On success the device restarts; on failure the image stays unmarked (`refuse_mark_valid`) and the health policy rolls it back at the deadline.
fn roll_back_at_boot(cfg: &OtaConfig) {
    log::error!("[ota] the running image is not the promised version; rolling back");
    let session = OtaSession::new(OtaSessionConfig {
        timeout_secs: cfg.settings.timings.firmware_timeout_secs,
    });
    match session.and_then(|mut session| session.rollback()) {
        Ok(()) => log::warn!("[ota] the rollback returned without a restart"),
        Err(e) => log::error!("[ota] the boot rollback failed: {}", e.code()),
    }
}

/// A started runtime.
///
/// Run the health policy with [`OtaHandle::run_health_policy`]; read the library-owned flags with [`OtaHandle::flags`].
pub struct OtaHandle {
    cfg: Arc<OtaConfig>,
    flags: OtaFlags,
    shared: Arc<Shared>,
    records: Records,
    mqtt: MqttHandle,
    health_started: AtomicBool,
}

impl OtaHandle {
    /// The library-owned flags, read-only.
    pub fn flags(&self) -> &OtaFlags {
        &self.flags
    }

    /// The least free stack bytes the runtime threads have seen, in bytes.
    pub fn stack_high_water(&self) -> StackMarks {
        StackMarks {
            worker_min_free_bytes: stack::read(&self.shared.worker_free),
            reporter_min_free_bytes: stack::read(&self.shared.reporter_free),
        }
    }
}
