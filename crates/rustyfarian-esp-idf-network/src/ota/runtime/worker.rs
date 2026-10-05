//! The worker thread: runs one command at a time.
//!
//! Every decision is made by the pure functions of `juggler::ota::runtime`; this file only sequences them with the effects (hardware reads, HTTP, flash, store calls, publishes).
//! The store lock is taken only through `Records::with` closures that contain store calls and nothing else: never across HTTP, `OtaSession`, a hardware read, a sleep, a publish or a channel operation.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::thread;

use juggler::ota::runtime::{
    begin_failure, blocked_report, early_update_block, failed_download, plan_offer, plan_repair,
    plan_rollback, plan_update_gate, refresh_after_repair, refused_or_absent, repair_all,
    retained_due, rollback_arm, rollback_undo, FailedSettle, OfferPlan, Probe, Proceed, RepairGate,
    Retention,
};
use juggler::ota::{
    FailReason, FailedDownload, Manifest, OtaCommand, OtaStatus, OtaTimings, Version,
};

use super::manifest::{fetch_manifest, ManifestLimits};
use super::publish::publish_status;
use super::shared::{OtaFlags, Shared};
use super::stack::sample;
use super::{OtaConfig, Records};
use crate::mqtt::MqttHandle;
use crate::ota::{
    read_hardware_facts, url_for_log, BusyRetry, OtaSession, OtaSessionConfig, TARGET_CHIP,
};

pub(super) struct WorkerCtx {
    pub(super) cfg: Arc<OtaConfig>,
    pub(super) shared: Arc<Shared>,
    pub(super) flags: OtaFlags,
    pub(super) records: Records,
    pub(super) mqtt: MqttHandle,
}

impl WorkerCtx {
    fn timings(&self) -> &OtaTimings {
        &self.cfg.settings.timings
    }

    fn running(&self) -> Version {
        self.cfg.settings.running_version
    }

    fn publish(&self, status: &OtaStatus<'_>) {
        publish_status(
            &self.mqtt,
            &self.cfg.settings.status_topic,
            status,
            self.timings(),
        );
    }

    fn fail(&self, reason: FailReason) {
        log::warn!("[ota] command failed: {reason}");
        self.publish(&OtaStatus::Failed { reason });
    }

    fn session(&self) -> Result<OtaSession, juggler::ota::OtaError> {
        OtaSession::new(OtaSessionConfig {
            timeout_secs: self.timings().firmware_timeout_secs,
        })
    }

    /// The hardware read used by every gate: `1 + session_busy_retries` tries, `session_busy_delay` apart.
    ///
    /// `OtaSession::new` never touches the OTA handle, so the busy window is covered here, right before the gate decides.
    fn busy_retry(&self) -> BusyRetry {
        BusyRetry {
            tries: u8::try_from(self.timings().session_busy_retries.saturating_add(1))
                .unwrap_or(u8::MAX),
            delay: self.timings().session_busy_delay,
        }
    }

    fn retained_ready(&self, retention: &Retention) -> bool {
        retention.has_offer()
            && retained_due(
                true,
                self.flags.admission_open(),
                self.records.with(|store| store.admission()),
            )
    }
}

/// The worker loop.
///
/// With a retained offer the wait is `retained_poll` long so the offer is re-evaluated silently when admission reopens; without one the worker sleeps until a command arrives.
pub(super) fn run(ctx: WorkerCtx, commands: Receiver<OtaCommand>) {
    let mut retention = Retention::new();
    loop {
        if ctx.retained_ready(&retention) && ctx.shared.claim_busy() {
            if let Some(offer) = retention.take() {
                log::info!("[ota] admission reopened, re-evaluating the retained offer");
                execute(&ctx, &mut retention, offer, true);
            }
            ctx.shared.release_busy();
            sample(&ctx.shared.worker_free, "worker");
            if retention.has_offer() {
                thread::sleep(ctx.timings().retained_poll);
            }
            continue;
        }
        let received = if retention.has_offer() {
            commands.recv_timeout(ctx.timings().retained_poll)
        } else {
            commands.recv().map_err(|_| RecvTimeoutError::Disconnected)
        };
        match received {
            Ok(command) => {
                retention.on_command(&command);
                execute(&ctx, &mut retention, command, false);
                ctx.shared.release_busy();
                sample(&ctx.shared.worker_free, "worker");
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                log::error!("[ota] the command channel closed; the worker exits");
                ctx.shared.close();
                return;
            }
        }
    }
}

fn execute(ctx: &WorkerCtx, retention: &mut Retention, command: OtaCommand, silent: bool) {
    match &command {
        OtaCommand::Update {
            manifest_url,
            sig_present,
        } => run_update(ctx, retention, &command, manifest_url, *sig_present, silent),
        OtaCommand::Rollback { from } => run_rollback(ctx, *from),
        OtaCommand::Repair => run_repair(ctx),
    }
}

fn run_update(
    ctx: &WorkerCtx,
    retention: &mut Retention,
    command: &OtaCommand,
    url: &str,
    sig_present: bool,
    silent: bool,
) {
    let timings = ctx.timings();
    log::info!("[ota] update requested from {}", url_for_log(url));
    if sig_present {
        log::info!("[ota] signature received, not verified");
    }

    if let Some(OfferPlan::Blocked(reason)) =
        early_update_block(ctx.flags.admission_open(), ctx.flags.health_settled())
    {
        log::info!("[ota] the health policy has not read the slot yet; the offer is retained");
        return blocked(ctx, retention, command, silent, reason);
    }

    let retry = ctx.busy_retry();
    let hardware = match plan_update_gate(ctx.flags.admission_open(), || read_hardware_facts(retry))
    {
        Ok(hardware) => hardware,
        Err(reason) => return ctx.fail(reason),
    };

    let limits = ManifestLimits {
        per_op: timings.manifest_per_op,
        total: timings.manifest_total,
    };
    let manifest = match fetch_manifest(url, limits) {
        Ok(manifest) => manifest,
        Err(e) => {
            log::warn!("[ota] manifest from {} rejected: {e}", url_for_log(url));
            return ctx.fail(e.reason());
        }
    };

    let (admission, refused) = ctx
        .records
        .with(|store| (store.admission(), store.refused()));
    let refused = refused_or_absent(refused);

    match plan_offer(
        ctx.running(),
        &manifest,
        TARGET_CHIP,
        refused,
        admission,
        &hardware,
    ) {
        OfferPlan::Fail(reason) => ctx.fail(reason),
        OfferPlan::Blocked(reason) => blocked(ctx, retention, command, silent, reason),
        OfferPlan::Proceed(plan) => apply(ctx, retention, command, silent, &manifest, plan),
    }
}

fn blocked(
    ctx: &WorkerCtx,
    retention: &mut Retention,
    command: &OtaCommand,
    silent: bool,
    reason: FailReason,
) {
    retention.retain(command.clone());
    match blocked_report(silent, reason) {
        Some(reason) => ctx.fail(reason),
        None => log::debug!("[ota] the retained offer is still blocked: {reason}"),
    }
}

fn apply(
    ctx: &WorkerCtx,
    retention: &mut Retention,
    command: &OtaCommand,
    silent: bool,
    manifest: &Manifest,
    plan: Proceed,
) {
    let timings = ctx.timings();
    let begun = ctx
        .records
        .with(|store| store.begin_attempt(plan.metadata.version, plan.slot, plan.slot_was_invalid));
    match begun {
        Ok(id) => log::info!("[ota] attempt {id} begun"),
        Err(e) => {
            log::warn!("[ota] attempt not begun: {e}");
            return match begin_failure(&e) {
                OfferPlan::Blocked(reason) => blocked(ctx, retention, command, silent, reason),
                OfferPlan::Fail(reason) => ctx.fail(reason),
                OfferPlan::Proceed(_) => ctx.fail(FailReason::AttemptNotPersisted),
            };
        }
    }

    ctx.publish(&OtaStatus::Downloading);
    let mut session = match ctx.session() {
        Ok(session) => session.with_deadline(timings.firmware_deadline),
        Err(e) => {
            settle_failed(ctx);
            return ctx.fail(FailReason::Ota(e));
        }
    };
    if let Err(e) = session.fetch_and_apply(&manifest.url, &plan.metadata.sha256) {
        log::error!("[ota] download failed: {}", e.code());
        sample(&ctx.shared.worker_free, "worker");
        settle_failed(ctx);
        return ctx.fail(FailReason::Ota(e));
    }

    if let Err(e) = ctx.records.with(|store| store.mark_boot_selected()) {
        log::error!("[ota] the boot selection was not recorded: {e}; restarting anyway");
    }
    ctx.publish(&OtaStatus::SwapPending);
    sample(&ctx.shared.worker_free, "worker");
    thread::sleep(timings.restart_grace);
    (ctx.cfg.restart)();
    log::error!(
        "[ota] the restart callback returned; the worker parks and every command answers busy"
    );
    park_forever()
}

fn park_forever() -> ! {
    loop {
        thread::park();
    }
}

/// After a failed `fetch_and_apply`: lets the decision core reconcile the stalled attempt.
///
/// The hardware is read outside the store lock.
fn settle_failed(ctx: &WorkerCtx) {
    let hardware = read_hardware_facts(BusyRetry::default());
    let running = ctx.running();
    match ctx
        .records
        .with(|store| failed_download(store, hardware, Some(running)))
    {
        FailedSettle::Settled(FailedDownload::Cleared) => {
            log::info!("[ota] the failed attempt was cleared");
        }
        FailedSettle::Settled(other) => {
            log::info!("[ota] the failed attempt is kept for the next boot: {other:?}");
        }
        FailedSettle::HardwareUnreadable(e) => {
            log::warn!("[ota] the failed attempt is kept, hardware unreadable: {e}");
        }
        FailedSettle::Store(e) => {
            log::warn!("[ota] the failed attempt is kept, records unreadable: {e}");
        }
    }
}

fn run_rollback(ctx: &WorkerCtx, from: Version) {
    let running = ctx.running();
    let retry = ctx.busy_retry();
    let slot = match plan_rollback(from, running, ctx.flags.admission_open(), || {
        read_hardware_facts(retry)
    }) {
        Ok(slot) => slot,
        Err(reason) => return ctx.fail(reason),
    };
    let armed = match ctx.records.with(|store| rollback_arm(store, slot, running)) {
        Ok(armed) => armed,
        Err(reason) => return ctx.fail(reason),
    };
    log::info!("[ota] operator rollback armed; rolling back");

    let result = ctx.session().and_then(|mut session| session.rollback());
    match result {
        Err(e) => {
            log::error!("[ota] the rollback did not start: {}", e.code());
            let undone = ctx.records.with(|store| rollback_undo(store, armed));
            if !undone.is_clean() {
                log::warn!("[ota] the rollback request could not be fully withdrawn: {undone:?}");
            }
            ctx.fail(FailReason::RollbackUnavailable);
        }
        Ok(()) => {
            log::warn!("[ota] the rollback returned without a restart; the request stays armed");
            let _armed = armed;
        }
    }
}

fn run_repair(ctx: &WorkerCtx) {
    let timings = ctx.timings();
    let running = ctx.running();
    let settled = ctx.flags.health_settled();
    let probe = Probe::from_store(ctx.records.with(|store| store.corrupt_present()));
    let gate = RepairGate::new(settled, ctx.flags.records_suspect(), probe);
    if let Err(reason) = plan_repair(&gate) {
        return ctx.fail(reason);
    }

    let repaired = match ctx
        .records
        .with(|store| repair_all(store, timings.repair_max_iterations))
    {
        Ok(repaired) => repaired,
        Err(e) => {
            log::warn!("[ota] repair failed: {e:?}");
            return ctx.fail(e.reason());
        }
    };

    let hardware = read_hardware_facts(BusyRetry::default());
    let roll_back_now = ctx.flags.roll_back_now_at_boot();
    let refreshed = ctx
        .records
        .with(|store| refresh_after_repair(store, hardware, Some(running), roll_back_now));
    match refreshed {
        Some(refreshed) => {
            ctx.flags.set_records_suspect(refreshed.records_suspect);
            ctx.flags.set_refuse_mark_valid(refreshed.refuse_mark_valid);
            if refreshed.outcome.roll_back_now {
                log::warn!("[ota] after the repair a rollback would be demanded; it is left to the health policy");
            }
            log::info!(
                "[ota] boot reconciliation re-run after the repair: {:?}",
                refreshed.outcome.disposition
            );
        }
        None => log::warn!("[ota] the hardware was unreadable; the flags are unchanged"),
    }

    if repaired == 0 {
        return ctx.fail(FailReason::RepairNotNeeded);
    }
    log::info!("[ota] repaired {repaired} record group(s)");
    ctx.publish(&OtaStatus::Repaired { records: repaired });
}
