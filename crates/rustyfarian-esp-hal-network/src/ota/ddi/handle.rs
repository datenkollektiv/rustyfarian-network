//! The esp-hal DDI runtime builder, its boot sequence and the handle of a started runtime, with the ESP-IDF tier's names (ADR 022).
//!
//! The boot sequence is the ESP-IDF tier's order over the flat plan functions of `juggler::ota::ddi` ([`plan_boot`], [`repair_at_boot`], [`after_repair`], [`unbound_report_blocks`]); the hardware is read between them, outside any store borrow.
//! What the ESP-IDF tier keeps behind `Arc`s (the flags, the wake-up path, the nudge topic) is allocated once per [`DdiRuntime::new`] and kept for the whole program: the ESP32-C3 has no atomic compare-and-swap, so this tier has no `Arc`.

use alloc::boxed::Box;
use core::cell::Cell;
use core::fmt;

use critical_section::Mutex;
use embassy_net::Stack;
use embassy_time::Instant;
use juggler::ota::ddi::wire::Chip;
use juggler::ota::ddi::{
    after_repair, nudge_topic, plan_boot, repair_at_boot, unbound_report_blocks, ControllerId,
    DdiSettings, DdiStackMarks, DdiTransportConfig, DdiTransportError, DdiWorker, Wake,
    WorkerConfig,
};
use juggler::ota::health::RuntimeFlags;
use juggler::ota::{BusyRetry, HealthVerdict, KvError, OtaError, StoreError, Version};

use super::health::HealthDriver;
use super::nudge::{DdiNudge, Waker};
use super::plan::{self, Accepted};
use super::runner::{Core, DdiRunner, Link};
use super::transport::DdiTransport;
use super::{DdiRuntimeError, RestartFn};
use crate::ota::boot_facts::update_partition_capacity;
use crate::ota::flags::{FlagCells, OtaFlags};
use crate::ota::flash::SharedFlash;
use crate::ota::records::{Records, Store};
use crate::ota::{
    read_hardware_facts, slot_evidence_line, EspHalOtaManager, OtaManagerConfig, TARGET_CHIP,
};

const _: () = assert!(Chip::is_valid(TARGET_CHIP));

/// What the runner, the handle and the nudge share for the whole program: the wake-up path, the flags and the guard of the health policy.
pub(super) struct DdiShared {
    pub(super) waker: Waker,
    pub(super) flags: FlagCells,
    health_started: Mutex<Cell<bool>>,
}

impl DdiShared {
    const fn new() -> Self {
        Self {
            waker: Waker::new(),
            flags: FlagCells::new(),
            health_started: Mutex::new(Cell::new(false)),
        }
    }
}

/// Runs `f` on the store; a store that is lent out (no correct caller nests) fails the start instead of panicking.
fn with_store<R>(records: Records, f: impl FnOnce(&mut Store) -> R) -> Result<R, DdiRuntimeError> {
    records
        .with(f)
        .ok_or(DdiRuntimeError::Store(StoreError::Kv(KvError::new(
            KvError::CODE_ALREADY_OPEN,
        ))))
}

/// Experimental: API may change before 1.0.
///
/// The DDI OTA runtime before it is started (hook 2): the validated settings, the accepted transport configuration and the nudge hook.
///
/// The same calls as on the ESP-IDF tier; [`start`](Self::start) also takes the station `Stack` and returns the [`DdiRunner`] the firmware spawns, next to the [`DdiHandle`].
///
/// ```ignore
/// let flash = SharedFlash::new(peripherals.FLASH)?;
/// let records = open_records(flash)?;
/// let settings = DdiSettings::new(Version::new(1, 0, 0), "c3-supermini")?;
/// let token = DdiToken::parse(token_hex)?;
/// let transport = DdiTransportConfig::new("http://192.168.1.10:8080/DEFAULT", "dev-1", token);
/// let runtime = DdiRuntime::new(settings, transport, restart_device)?;
/// let nudge = runtime.nudge();
/// // ... Wi-Fi up, MQTT subscribed to runtime.nudge_topic() ...
/// let (handle, runner) = runtime.start(records, stack)?;
/// spawner.spawn(ddi_task(runner).unwrap());
/// let _verdict = handle.run_health_policy(boot, || true).await;
/// ```
pub struct DdiRuntime {
    settings: DdiSettings,
    restart: RestartFn,
    accepted: Accepted,
    chip: Chip,
    shared: &'static DdiShared,
    nudge: DdiNudge,
}

impl fmt::Debug for DdiRuntime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DdiRuntime")
            .field("settings", &self.settings)
            .field("endpoint", &self.accepted.endpoint)
            .field("nudge", &self.nudge)
            .finish_non_exhaustive()
    }
}

impl DdiRuntime {
    /// Experimental: API may change before 1.0.
    ///
    /// Validates the settings and the transport configuration (the token is taken here) and builds the nudge topic; nothing touches the store, the flash or the network yet.
    ///
    /// Each successful call keeps a little memory for the whole program (the nudge topic, the flags and the wake-up path, under 100 bytes); a firmware builds one runtime per boot.
    ///
    /// # Errors
    ///
    /// [`DdiRuntimeError::Settings`] for settings that fail `DdiSettings::validate`, [`DdiRuntimeError::Transport`] when the configuration is refused as `DdiTransport::new` would refuse it.
    pub fn new(
        settings: DdiSettings,
        transport: DdiTransportConfig,
        restart: RestartFn,
    ) -> Result<Self, DdiRuntimeError> {
        settings.validate()?;
        let controller_id = ControllerId::parse(&transport.controller_id)
            .map_err(|_| DdiTransportError::InvalidControllerId)?;
        let accepted = plan::accept(transport, cfg!(feature = "ota-ddi-insecure-http"))?;
        let chip = Chip::parse(TARGET_CHIP).map_err(|_| DdiRuntimeError::InvalidChip)?;
        let topic: &'static str = nudge_topic(&controller_id).leak();
        let shared: &'static DdiShared = Box::leak(Box::new(DdiShared::new()));
        let nudge = DdiNudge::new(topic, &shared.waker);
        Ok(Self {
            settings,
            restart,
            accepted,
            chip,
            shared,
            nudge,
        })
    }

    /// Experimental: API may change before 1.0.
    ///
    /// The nudge hook for the app's MQTT receive loop, available before [`start`](Self::start) so MQTT can be set up first.
    pub fn nudge(&self) -> DdiNudge {
        self.nudge
    }

    /// Experimental: API may change before 1.0.
    ///
    /// The exact nudge topic `rustyfarian/ota/<controller-id>/nudge`, the one to pass to `MqttHandle::subscribe`.
    pub fn nudge_topic(&self) -> &str {
        self.nudge.topic()
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Hook 3, once Wi-Fi is up: reconciles the boot records on the calling task, repairs unreadable records, reads the update partition size, then builds the transport on `stack` and returns the handle and the runner.
    ///
    /// The order is the ESP-IDF tier's: the hardware facts and the slot evidence line, the reconciliation and the runtime flags, the repair (the hardware read again outside the store borrow), the flags, the rollback at boot, the unbound-report warning, the update partition size.
    /// If the boot reconciliation demands an immediate rollback (the running image is not the version it promised) it is written here and the app's restart callback is called; if that fails the image stays unmarked and the health policy rolls it back at the deadline.
    /// Nothing runs until the firmware spawns [`DdiRunner::run`].
    ///
    /// # Errors
    ///
    /// [`DdiRuntimeError::Flash`] when no `SharedFlash` exists or the update partition cannot be found, [`DdiRuntimeError::Store`] when the record store is lent out.
    /// With no handle there is no health policy: a pending image is not marked valid and the bootloader rolls it back at the next reset.
    pub fn start(
        self,
        records: Records,
        stack: Stack<'static>,
    ) -> Result<(DdiHandle, DdiRunner), DdiRuntimeError> {
        let Self {
            settings,
            restart,
            accepted,
            chip,
            shared,
            nudge,
        } = self;
        let flash =
            SharedFlash::get().ok_or(DdiRuntimeError::Flash(OtaError::PartitionNotFound))?;
        let running = settings.running_version;

        let hardware = read_hardware_facts(BusyRetry::default());
        log::info!("{}", slot_evidence_line(&hardware, BusyRetry::default()));
        let plan = with_store(records, |store| plan_boot(store, hardware, running))?;
        let outcome = &plan.outcome;
        log::info!(
            "[ota] boot reconciliation: {:?}, slot released: {}, report pending: {}",
            outcome.disposition,
            outcome.slot_released,
            outcome.report_pending
        );
        if outcome.warnings != Default::default() {
            log::warn!("[ota] boot bookkeeping warnings: {:?}", outcome.warnings);
        }
        let mut boot = plan.flags;
        let report_owed = if plan.needs_repair() {
            repair(records, running, &mut boot)?
        } else {
            false
        };

        let flags = OtaFlags::new(&shared.flags);
        flags.set_admission_open(boot.admission_open);
        flags.set_health_settled(boot.health_settled);
        flags.set_refuse_mark_valid(boot.refuse_mark_valid);
        flags.set_roll_back_now_at_boot(boot.roll_back_now_at_boot);
        flags.set_records_suspect(boot.records_suspect);

        if boot.roll_back_now_at_boot {
            roll_back_at_boot(flash, settings.timings.firmware_timeout_secs, restart);
        }

        if with_store(records, |store| unbound_report_blocks(store))? {
            log::warn!("[ota] a rolled_back report of the MQTT OTA runtime awaits delivery and blocks DDI offers until an MQTT OTA image delivers it");
        }

        let partition_capacity = update_partition_capacity().map_err(DdiRuntimeError::Flash)?;

        let config = WorkerConfig::new(&settings, chip, partition_capacity, &boot, report_owed);
        let worker = DdiWorker::new(accepted.endpoint.clone(), config);
        let transport = DdiTransport::from_accepted(accepted, stack);
        log::info!("[ota] DDI runtime started: run the DdiRunner in its own task");

        let handle = DdiHandle {
            flags,
            shared,
            settings: settings.clone(),
            restart,
            records,
            flash,
            nudge,
        };
        let runner = DdiRunner {
            worker,
            core: Core {
                transport,
                settings,
                records,
                flash,
                shared,
                restart,
                origin: Instant::now(),
                link: Link::new(),
            },
        };
        Ok((handle, runner))
    }
}

/// The boot reconciliation failed closed on a store fault: repairs the unreadable records and re-runs the reconciliation; returns whether a 1005 report is owed.
fn repair(
    records: Records,
    running: Version,
    boot: &mut RuntimeFlags,
) -> Result<bool, DdiRuntimeError> {
    log::warn!("[ota] the OTA records are unreadable; repairing");
    let repaired = with_store(records, repair_at_boot)?;
    match repaired {
        Ok(groups) => log::info!("[ota] repaired {groups} record group(s)"),
        Err(e) => log::warn!("[ota] repair failed: {e:?}"),
    }
    let hardware = read_hardware_facts(BusyRetry::default());
    let after = with_store(records, |store| {
        after_repair(store, repaired, hardware, running, boot)
    })?;
    match &after.refreshed {
        Some(refreshed) => {
            log::info!(
                "[ota] boot reconciliation re-run after the repair: {:?}",
                refreshed.outcome.disposition
            );
            if refreshed.outcome.roll_back_now {
                log::warn!("[ota] after the repair a rollback would be demanded; it is left to the health policy");
            }
        }
        None => log::warn!("[ota] the hardware was unreadable; the flags are unchanged"),
    }
    if !after.report_owed {
        log::info!("[ota] nothing was corrupt (a transient read error); no 1005 report is owed");
    }
    Ok(after.report_owed)
}

/// The boot reconciliation demanded a rollback: writes it, then calls the app's restart callback.
///
/// On failure the image stays unmarked (`refuse_mark_valid`) and the health policy rolls it back at the deadline.
fn roll_back_at_boot(flash: &'static SharedFlash, timeout_secs: u64, restart: RestartFn) {
    log::error!("[ota] the running image is not the promised version; rolling back");
    let manager = EspHalOtaManager::new_shared(OtaManagerConfig { timeout_secs }, flash);
    match manager.and_then(|mut manager| manager.rollback()) {
        Ok(()) => {
            log::warn!("[ota] the rollback is written; calling the restart callback");
            (restart)();
            log::warn!("[ota] the rollback returned without a restart");
        }
        Err(e) => log::error!("[ota] the boot rollback failed: {}", e.code()),
    }
}

/// Experimental: API may change before 1.0.
///
/// A started DDI runtime, with the methods of the ESP-IDF tier's `DdiHandle`.
///
/// Read the library-owned flags with [`DdiHandle::flags`], feed the nudge from the app's MQTT receive loop with [`DdiHandle::nudge`] and run the health policy of the running image with [`DdiHandle::run_health_policy`].
pub struct DdiHandle {
    flags: OtaFlags,
    shared: &'static DdiShared,
    settings: DdiSettings,
    restart: RestartFn,
    records: Records,
    flash: &'static SharedFlash,
    nudge: DdiNudge,
}

impl fmt::Debug for DdiHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DdiHandle")
            .field("flags", &self.flags)
            .field("nudge", &self.nudge)
            .finish_non_exhaustive()
    }
}

impl DdiHandle {
    /// Experimental: API may change before 1.0.
    ///
    /// The library-owned flags, read-only.
    pub fn flags(&self) -> &OtaFlags {
        &self.flags
    }

    /// Experimental: API may change before 1.0.
    ///
    /// The nudge hook for the app's MQTT receive loop.
    pub fn nudge(&self) -> DdiNudge {
        self.nudge
    }

    /// Experimental: API may change before 1.0.
    ///
    /// The exact nudge topic `rustyfarian/ota/<controller-id>/nudge`, the one to pass to `MqttHandle::subscribe`.
    pub fn nudge_topic(&self) -> &str {
        self.nudge.topic()
    }

    /// Experimental: API may change before 1.0.
    ///
    /// The least free stack bytes of the runtime's worker and download; always `None` on this tier (a named gap).
    ///
    /// The ESP-IDF tier samples the FreeRTOS high-water marks of its two threads; here both run in the firmware's one task on the shared `esp-rtos` stack, which has no per-task mark.
    pub fn stack_high_water(&self) -> DdiStackMarks {
        DdiStackMarks {
            worker_min_free_bytes: None,
            download_min_free_bytes: None,
        }
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Hook 4: runs the health policy of the running image on the calling task and returns its verdict.
    ///
    /// Await it once from the app's main task after the runtime is started.
    /// `boot` MUST be an [`Instant`] captured at the very start of `main`, before Wi-Fi: the dwell and the failure deadline are measured from it, not from association.
    /// `healthy` is the app's predicate (for example Wi-Fi ready and the MQTT client connected); the library adds the minimum dwell, the refusal flag and the deadline.
    ///
    /// The policy is the ESP-IDF tier's ([`HealthFlow`](juggler::ota::health::HealthFlow), the same decisions in the same order): it reads the running slot, does nothing for a slot that is not pending verification, marks a pending image valid once the dwell passed and `healthy()` is true and records the completed attempt, and at the failure deadline notes the reason and rolls back (then calls the restart callback) or calls the restart callback.
    /// A second call returns [`HealthVerdict::AlreadyRunning`] without doing anything.
    ///
    /// Recording the completed attempt writes the success verdict of the bound action into the store, and the worker is woken: it asks for an early poll and sends the verdict with the next one.
    /// If the record cannot be cleared (`ApplyNotRecorded`) the worker keeps retrying in the background.
    pub async fn run_health_policy<F: FnMut() -> bool>(
        &self,
        boot: Instant,
        healthy: F,
    ) -> HealthVerdict {
        let already =
            critical_section::with(|cs| self.shared.health_started.borrow(cs).replace(true));
        if already {
            return HealthVerdict::AlreadyRunning;
        }
        let timings = &self.settings.timings;
        let verdict = HealthDriver {
            config: timings.health,
            records: self.records,
            flags: &self.flags,
            restart: self.restart,
            running: self.settings.running_version,
            timeout_secs: timings.firmware_timeout_secs,
            flash: self.flash,
        }
        .run(boot, healthy)
        .await;
        self.settle(verdict);
        verdict
    }

    /// Applies a terminal verdict: opens admission, marks the policy settled and wakes the worker with the verdict (which arms its completion retry after `ApplyNotRecorded`).
    fn settle(&self, verdict: HealthVerdict) {
        log::info!("[ota] health policy finished: {verdict:?}");
        if verdict.arms_completion_retry() {
            log::warn!(
                "[ota] the attempt record was not cleared; the worker keeps retrying in the background"
            );
        }
        if verdict.opens_admission() {
            self.flags.set_admission_open(true);
        }
        self.flags.set_health_settled(true);
        self.shared.waker.notify(Wake {
            health: Some(verdict),
            ..Wake::default()
        });
    }
}
