//! The DDI runtime builder, its boot sequence and the handle of a started runtime.

use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Instant;

use anyhow::Context;
use juggler::ota::ddi::wire::Chip;
use juggler::ota::ddi::{nudge_topic, ControllerId};
use juggler::ota::health::{refresh_after_repair, repair_all, repair_report_owed, RuntimeFlags};
use juggler::ota::{reconcile_boot, Admission, BlockedBy, HealthVerdict, ImageChip, Version};

use super::nudge::Waker;
use super::worker::{self, WorkerCtx};
use super::{
    DdiNudge, DdiSettings, DdiSettingsError, DdiTransport, DdiTransportConfig, DdiTransportError,
};
use crate::ota::flags::OtaFlags;
use crate::ota::flasher::FirmwareFlasher;
use crate::ota::health_driver::HealthDriver;
use crate::ota::records::{Records, RestartFn};
use crate::ota::stack;
use crate::ota::{
    read_hardware_facts, slot_evidence_line, BusyRetry, OtaSession, OtaSessionConfig, TARGET_CHIP,
};

const _: fn() = || {
    fn shared<T: Send + Sync>() {}
    shared::<DdiTransport>();
};

const _: () = assert!(Chip::is_valid(TARGET_CHIP));

/// Repair passes of the boot repair; the store has only a handful of record groups, one per pass.
const BOOT_REPAIR_PASSES: u32 = 32;

/// Experimental: API may change before 1.0.
///
/// Why the DDI runtime could not be built.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DdiRuntimeError {
    /// The settings were refused.
    Settings(DdiSettingsError),
    /// The transport could not be built.
    Transport(DdiTransportError),
    /// `TARGET_CHIP` is not a profile chip name; cannot happen for a known chip.
    InvalidChip,
}

impl fmt::Display for DdiRuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DdiRuntimeError::Settings(e) => write!(f, "invalid DDI settings: {e}"),
            DdiRuntimeError::Transport(e) => write!(f, "invalid DDI transport: {e}"),
            DdiRuntimeError::InvalidChip => f.write_str("the target chip is not a profile chip"),
        }
    }
}

impl std::error::Error for DdiRuntimeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            DdiRuntimeError::Settings(e) => Some(e),
            DdiRuntimeError::Transport(e) => Some(e),
            DdiRuntimeError::InvalidChip => None,
        }
    }
}

impl From<DdiSettingsError> for DdiRuntimeError {
    fn from(e: DdiSettingsError) -> Self {
        DdiRuntimeError::Settings(e)
    }
}

impl From<DdiTransportError> for DdiRuntimeError {
    fn from(e: DdiTransportError) -> Self {
        DdiRuntimeError::Transport(e)
    }
}

/// Experimental: API may change before 1.0.
///
/// The least free stack bytes the DDI runtime threads have seen, in bytes; `None` until the thread has sampled once.
///
/// The health policy runs on the app's own thread and is not tracked here.
/// The `ota-download` thread is sampled when a download ends, and exists only while an attempt does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DdiStackMarks {
    /// Least free bytes of the `ota-worker` thread.
    pub worker_min_free_bytes: Option<u32>,
    /// Least free bytes of the `ota-download` threads.
    pub download_min_free_bytes: Option<u32>,
}

/// What the runtime threads and the handle share besides the flags: the stack marks and the completion-retry request.
#[derive(Debug)]
pub(super) struct DdiShared {
    pub(super) worker_free: AtomicU32,
    pub(super) download_free: AtomicU32,
    /// The health policy ended with `ApplyNotRecorded`: the worker keeps retrying `complete_attempt` until the record is cleared.
    pub(super) completion_retry: AtomicBool,
}

impl DdiShared {
    fn new() -> Self {
        Self {
            worker_free: AtomicU32::new(u32::MAX),
            download_free: AtomicU32::new(u32::MAX),
            completion_retry: AtomicBool::new(false),
        }
    }
}

/// Experimental: API may change before 1.0.
///
/// The DDI OTA runtime before it is started (hook 2): the validated settings, the transport and the nudge hook.
///
/// ```rust,no_run
/// use std::sync::Arc;
/// use rustyfarian_esp_idf_network::ota::ddi::{
///     DdiRuntime, DdiSettings, DdiToken, DdiTransportConfig, open_records,
/// };
/// use rustyfarian_esp_idf_network::ota::Version;
///
/// # fn restart_device() {}
/// # fn demo(nvs: esp_idf_svc::nvs::EspDefaultNvsPartition) -> anyhow::Result<()> {
/// let records = open_records(nvs)?;
/// let settings = DdiSettings::new(Version::new(1, 0, 0), "c3-supermini")?;
/// let token = DdiToken::parse("0123456789abcdef0123456789abcdef").unwrap();
/// let transport = DdiTransportConfig::new("http://192.168.1.10:8080/DEFAULT", "dev-1", token);
/// let runtime = DdiRuntime::new(settings, transport, Arc::new(restart_device))?;
/// let _nudge = runtime.nudge();
/// let handle = runtime.start(records)?;
/// let _verdict = handle.run_health_policy(std::time::Instant::now(), || true);
/// # Ok(())
/// # }
/// ```
pub struct DdiRuntime {
    settings: Arc<DdiSettings>,
    restart: RestartFn,
    transport: Arc<DdiTransport>,
    chip: Chip,
    waker: Arc<Waker>,
    nudge: DdiNudge,
}

impl DdiRuntime {
    /// Experimental: API may change before 1.0.
    ///
    /// Validates the settings, builds the transport (the token is taken here) and the nudge topic; nothing touches the store or the hardware yet.
    ///
    /// # Errors
    ///
    /// [`DdiRuntimeError::Settings`] for settings that fail [`DdiSettings::validate`], [`DdiRuntimeError::Transport`] when [`DdiTransport::new`] refuses the configuration.
    pub fn new(
        settings: DdiSettings,
        transport: DdiTransportConfig,
        restart: RestartFn,
    ) -> Result<Self, DdiRuntimeError> {
        settings.validate()?;
        let controller_id = transport.controller_id.clone();
        let transport = Arc::new(DdiTransport::new(transport)?);
        let controller_id = ControllerId::parse(&controller_id)
            .map_err(|_| DdiTransportError::InvalidControllerId)?;
        let chip = Chip::parse(TARGET_CHIP).map_err(|_| DdiRuntimeError::InvalidChip)?;
        let waker = Arc::new(Waker::new());
        let nudge = DdiNudge::new(nudge_topic(&controller_id), Arc::clone(&waker));
        Ok(Self {
            settings: Arc::new(settings),
            restart,
            transport,
            chip,
            waker,
            nudge,
        })
    }

    /// Experimental: API may change before 1.0.
    ///
    /// A cheap handle for the app's `MqttBuilder::on_message` closure, available before [`start`](Self::start) so the MQTT builder can be built first.
    pub fn nudge(&self) -> DdiNudge {
        self.nudge.clone()
    }

    /// Experimental: API may change before 1.0.
    ///
    /// The exact nudge topic `rustyfarian/ota/<controller-id>/nudge`, the one to pass to `MqttBuilder::subscribe`.
    pub fn nudge_topic(&self) -> &str {
        self.nudge.topic()
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Hook 3, once Wi-Fi is up: reconciles the boot records on the calling thread, repairs unreadable records, reads the update partition size, then starts the `ota-worker` thread.
    ///
    /// If the boot reconciliation demands an immediate rollback (the running image is not the version it promised) that rollback is started here, outside any store lock.
    /// On a boot whose running slot is already valid (and not refused or failed closed) admission is open at once; otherwise it stays closed until the health policy has read the running slot.
    /// Unreadable records are repaired here, and when the repair dropped records or could not finish, the next offered action is reported `closed` + `failure` + 1005 `records_unreadable` instead of being applied.
    /// A boot fault that was only a transient read error repairs nothing and owes no report.
    ///
    /// # Errors
    ///
    /// The update partition cannot be found or a thread cannot be spawned.
    /// With no handle there is no health policy: a pending image is not marked valid and the bootloader aborts it at the next reset.
    pub fn start(self, records: Records) -> anyhow::Result<DdiHandle> {
        let Self {
            settings,
            restart,
            transport,
            chip,
            waker,
            nudge,
        } = self;
        let running = settings.running_version;

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
        let mut boot = RuntimeFlags::from_boot(&outcome);
        let mut unreported_corruption = false;
        if outcome.records_suspect() {
            unreported_corruption = repair_at_boot(&records, running, &mut boot);
        }

        let flags = OtaFlags::new();
        flags.set_admission_open(boot.admission_open);
        flags.set_health_settled(boot.health_settled);
        flags.set_refuse_mark_valid(boot.refuse_mark_valid);
        flags.set_roll_back_now_at_boot(boot.roll_back_now_at_boot);
        flags.set_records_suspect(boot.records_suspect);

        if boot.roll_back_now_at_boot {
            roll_back_at_boot(settings.timings.firmware_timeout_secs);
        }

        warn_about_unbound_report(&records);

        let partition_capacity = FirmwareFlasher::new()
            .and_then(|flasher| flasher.update_partition_capacity())
            .map(|capacity| capacity as u64)
            .map_err(|e| anyhow::anyhow!("no OTA update partition: {}", e.code()))?;

        let shared = Arc::new(DdiShared::new());
        let worker_bytes = settings.stacks.worker_bytes;
        let download_bytes = settings.stacks.download_bytes;
        let ctx = WorkerCtx {
            settings: Arc::clone(&settings),
            restart: Arc::clone(&restart),
            transport,
            chip,
            image_chip: ImageChip::from_target(TARGET_CHIP),
            records: records.clone(),
            flags: flags.clone(),
            shared: Arc::clone(&shared),
            waker: Arc::clone(&waker),
            origin: Instant::now(),
            partition_capacity,
            unreported_corruption,
        };
        thread::Builder::new()
            .name("ota-worker".into())
            .stack_size(worker_bytes)
            .spawn(move || worker::run(ctx))
            .context("cannot spawn the DDI OTA worker thread")?;

        log::info!(
            "[ota] DDI runtime started: worker stack {worker_bytes} bytes, download stack {download_bytes} bytes (provisional until measured)"
        );
        Ok(DdiHandle {
            flags,
            shared,
            settings,
            restart,
            records,
            waker,
            nudge,
            health_started: AtomicBool::new(false),
        })
    }
}

/// The boot reconciliation failed closed on a store fault: repairs the unreadable records and re-runs the reconciliation.
///
/// Runs on the caller's thread; each store call is one short lock and the hardware is read outside it.
///
/// Returns whether a report is owed ([`repair_report_owed`]): the repair dropped a group, could not finish, or the records still fail closed.
/// A boot fault that was only a transient read error repairs nothing and owes none, so a valid action is not failed with 1005 for one flaky read.
fn repair_at_boot(records: &Records, running: Version, boot: &mut RuntimeFlags) -> bool {
    log::warn!("[ota] the OTA records are unreadable; repairing");
    let repaired = records.with(|store| repair_all(store, BOOT_REPAIR_PASSES));
    match repaired {
        Ok(groups) => log::info!("[ota] repaired {groups} record group(s)"),
        Err(e) => log::warn!("[ota] repair failed: {e:?}"),
    }
    let hardware = read_hardware_facts(BusyRetry::default());
    let roll_back_now = boot.roll_back_now_at_boot;
    let refreshed =
        records.with(|store| refresh_after_repair(store, hardware, Some(running), roll_back_now));
    match refreshed {
        Some(refreshed) => {
            boot.records_suspect = refreshed.records_suspect;
            boot.refuse_mark_valid = refreshed.refuse_mark_valid;
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
    let owed = repair_report_owed(repaired, boot.records_suspect);
    if !owed {
        log::info!("[ota] nothing was corrupt (a transient read error); no 1005 report is owed");
    }
    owed
}

/// A `rolled_back` report of the MQTT OTA runtime that no DDI verdict binds blocks every DDI offer; nothing is written, the operator is told.
fn warn_about_unbound_report(records: &Records) {
    let (admission, pending) = records.with(|store| (store.admission(), store.pending_verdict()));
    if admission == Admission::Blocked(BlockedBy::ReportPending) && matches!(pending, Ok(None)) {
        log::warn!("[ota] a rolled_back report of the MQTT OTA runtime awaits delivery and blocks DDI offers until an MQTT OTA image delivers it");
    }
}

/// The boot reconciliation demanded a rollback: the running image is not the version it promised.
///
/// Runs on the caller's thread with no store lock held.
/// On success the device restarts; on failure the image stays unmarked (`refuse_mark_valid`) and the health policy rolls it back at the deadline.
fn roll_back_at_boot(timeout_secs: u64) {
    log::error!("[ota] the running image is not the promised version; rolling back");
    let session = OtaSession::new(OtaSessionConfig { timeout_secs });
    match session.and_then(|mut session| session.rollback()) {
        Ok(()) => log::warn!("[ota] the rollback returned without a restart"),
        Err(e) => log::error!("[ota] the boot rollback failed: {}", e.code()),
    }
}

/// Experimental: API may change before 1.0.
///
/// A started DDI runtime.
///
/// Read the library-owned flags with [`DdiHandle::flags`], hook the nudge into the MQTT builder with [`DdiHandle::nudge`] and run the health policy of the running image with [`DdiHandle::run_health_policy`].
pub struct DdiHandle {
    flags: OtaFlags,
    shared: Arc<DdiShared>,
    settings: Arc<DdiSettings>,
    restart: RestartFn,
    records: Records,
    waker: Arc<Waker>,
    nudge: DdiNudge,
    health_started: AtomicBool,
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
    /// A cheap handle for the app's `MqttBuilder::on_message` closure.
    pub fn nudge(&self) -> DdiNudge {
        self.nudge.clone()
    }

    /// Experimental: API may change before 1.0.
    ///
    /// The exact nudge topic `rustyfarian/ota/<controller-id>/nudge`, the one to pass to `MqttBuilder::subscribe`.
    pub fn nudge_topic(&self) -> &str {
        self.nudge.topic()
    }

    /// Experimental: API may change before 1.0.
    ///
    /// The least free stack bytes the runtime threads have seen, in bytes.
    pub fn stack_high_water(&self) -> DdiStackMarks {
        DdiStackMarks {
            worker_min_free_bytes: stack::read(&self.shared.worker_free),
            download_min_free_bytes: stack::read(&self.shared.download_free),
        }
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Hook 4: runs the health policy of the running image on the calling thread and returns its verdict.
    ///
    /// Call it once from the app's main thread after the runtime is started, where the app would otherwise park.
    /// `boot` MUST be the [`Instant`] captured at the very start of `main`, before Wi-Fi: the dwell and the failure deadline are measured from it, not from association.
    /// `healthy` is the app's predicate (for example a fresh tick, an IPv4 address, the MQTT client connected); the library adds the minimum dwell, the refusal flag and the deadline.
    ///
    /// The policy:
    /// - reads the running slot every `slot_retry`; it gives up (`SlotUnreadable`) only after `slot_min_attempts` reads AND the failure deadline, touching nothing (failed reads count for a failed read, failed and busy reads together for a busy one);
    /// - does nothing for a slot that is not pending verification (`NothingToVerify`) and opens update admission;
    /// - for a pending slot polls every `poll_interval`: once the dwell has passed, the refusal flag is clear and `healthy()` is true, it marks the image valid and records the completed attempt (retried `complete_attempts` times);
    /// - at the failure deadline notes the rollback reason, then rolls back with bounded retries (`DeadlineAction::Rollback`), or notes the reason and calls the app's restart callback (`DeadlineAction::NoteAndRestart`).
    ///
    /// A second call returns [`HealthVerdict::AlreadyRunning`] without doing anything.
    ///
    /// Unlike the MQTT runtime nothing is published here.
    /// Recording the completed attempt (`complete_attempt`) writes the success verdict of the bound action into the store before it clears the attempt, and the `ota-worker` thread is signalled: it asks the poll machine for an early poll and sends the verdict with the next one.
    /// If the record still cannot be cleared (`ApplyNotRecorded`) the worker keeps retrying with backoff until it is, and the verdict goes out after that.
    /// The rollback at the deadline writes no verdict now: the next boot records it as 1004, and the image that then runs delivers it with its first accepted poll.
    ///
    /// If `open_records` or [`DdiRuntime::start`] failed there is no handle and no health policy: a pending image is then NOT marked valid and the bootloader aborts it at the next reset.
    /// That is deliberate; a degraded policy that marked the image valid without attempt bookkeeping would hide the failure.
    pub fn run_health_policy<F: FnMut() -> bool>(
        &self,
        boot: Instant,
        healthy: F,
    ) -> HealthVerdict {
        if self.health_started.swap(true, Ordering::AcqRel) {
            return HealthVerdict::AlreadyRunning;
        }
        let timings = &self.settings.timings;
        let verdict = HealthDriver {
            config: timings.health,
            records: &self.records,
            flags: &self.flags,
            restart: &self.restart,
            running: self.settings.running_version,
            timeout_secs: timings.firmware_timeout_secs,
        }
        .run(boot, healthy);
        self.settle(verdict);
        verdict
    }

    /// Applies a terminal verdict: arms the background completion retry if the attempt record was not cleared, opens admission, marks the policy settled and wakes the worker.
    ///
    /// `publishes_applied` is deliberately not consulted: the success verdict is already in the store, and the worker sends it.
    fn settle(&self, verdict: HealthVerdict) {
        log::info!("[ota] health policy finished: {verdict:?}");
        if verdict.arms_completion_retry() {
            log::warn!(
                "[ota] the attempt record was not cleared; the worker keeps retrying in the background"
            );
            self.shared.completion_retry.store(true, Ordering::Release);
        }
        if verdict.opens_admission() {
            self.flags.set_admission_open(true);
        }
        self.flags.set_health_settled(true);
        self.waker.notify_health();
    }
}
