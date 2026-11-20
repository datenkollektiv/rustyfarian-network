//! The `ota-download` thread: one per admitted attempt, with its own OTA session and its own cancel switch.
//!
//! It streams the artifact through [`DdiTransport::download`], and on failure settles the attempt (`settle_failed_ddi_download`) before it exits, so the `ota-worker` thread that joins it knows the thread returned and settled.
//! On success it records the boot selection, sends the pre-reboot `proceeding`, waits `restart_grace` and calls the app's restart callback.
//! The store lock is taken only through `Records::with` closures that contain store calls and nothing else; the hardware is read, and the session dropped, outside it.

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use juggler::ota::ddi::wire::ProfileCode;
use juggler::ota::ddi::{decide_download_retry, AdmittedOffer, DownloadRetry, RetryAfter};
use juggler::ota::{BusyRetry, FailReason, FailedDownload, ImageChip, OtaError, Version};

use super::clock;
use super::handle::DdiShared;
use super::nudge::Waker;
use super::worker::send_proceeding;
use super::{DdiSettings, DdiTransport};
use crate::ota::records::{Records, RestartFn};
use crate::ota::stack::sample;
use crate::ota::{read_hardware_facts, OtaCancel, OtaSession, OtaSessionConfig};

/// How often the wait between two attempts looks at the cancel switch.
const CANCEL_POLL: Duration = Duration::from_millis(250);

pub(super) struct DownloadCtx {
    pub(super) transport: Arc<DdiTransport>,
    pub(super) records: Records,
    pub(super) settings: Arc<DdiSettings>,
    pub(super) restart: RestartFn,
    pub(super) image_chip: Option<ImageChip>,
    pub(super) cancel: Arc<OtaCancel>,
    pub(super) waker: Arc<Waker>,
    pub(super) shared: Arc<DdiShared>,
    pub(super) offer: AdmittedOffer,
}

/// The thread body: download, then settle the failure or restart into the new image.
pub(super) fn run(ctx: DownloadCtx) {
    match download(&ctx) {
        Ok(()) => restart_into_new_image(&ctx),
        Err(e) => failed(&ctx, e),
    }
}

/// The download with its retries: a transient error is tried again after a back-off until the one `firmware_deadline` passes (`juggler::ota::ddi::decide_download_retry`, the last wait clamped to the time left); the last real error is the reason when the policy gives up or the deadline has passed after a wait.
///
/// Each attempt has its own session, dropped before the wait, so the next one re-erases the slot.
/// A cancel during the wait ends it as [`OtaError::Cancelled`].
/// One cancel switch spans all attempts and stays open until the download gives up, where `close` decides atomically whether a cancel came first.
fn download(ctx: &DownloadCtx) -> Result<(), OtaError> {
    let deadline = ctx.settings.timings.firmware_deadline;
    let start = Instant::now();
    let mut attempts = 0_u32;
    loop {
        attempts += 1;
        let mut retry_after = None;
        let e = match attempt(
            ctx,
            deadline.saturating_sub(start.elapsed()),
            &mut retry_after,
        ) {
            Ok(()) => return Ok(()),
            Err(e) => e,
        };
        let asked = retry_after.and_then(|r: RetryAfter| r.delay(clock::secs()));
        let wait = match decide_download_retry(&e, attempts, start.elapsed(), deadline, asked) {
            DownloadRetry::Retry { wait } => wait,
            _ => return Err(give_up(ctx, e)),
        };
        log::warn!(
            "[ota] download of action {} failed ({}), attempt {attempts}; retrying in {} s",
            ctx.offer.action,
            e.code(),
            wait.as_secs()
        );
        if let Err(cancelled) = wait_or_cancel(&ctx.cancel, wait) {
            return Err(give_up(ctx, cancelled));
        }
        if start.elapsed() >= deadline {
            return Err(give_up(ctx, e));
        }
    }
}

/// One attempt on a new session that holds what is left of the download deadline; the session is dropped when this returns.
fn attempt(
    ctx: &DownloadCtx,
    remaining: Duration,
    retry_after: &mut Option<RetryAfter>,
) -> Result<(), OtaError> {
    let mut session = OtaSession::new(OtaSessionConfig {
        timeout_secs: ctx.settings.timings.firmware_timeout_secs,
    })?
    .with_deadline(remaining);
    if let Some(chip) = ctx.image_chip {
        session = session.with_expected_chip(chip);
    }
    ctx.transport.download_capturing(
        &mut session,
        &ctx.offer.artifact,
        &ctx.offer.sha256,
        ctx.offer.size,
        &ctx.cancel,
        retry_after,
    )
}

/// The download gives up on `e`: the switch closes for good, so a later cancel is too late, and a cancel accepted before that wins over `e` (no verdict).
fn give_up(ctx: &DownloadCtx, e: OtaError) -> OtaError {
    if ctx.cancel.close() {
        OtaError::Cancelled
    } else {
        e
    }
}

/// Sleeps `wait` in short slices; an accepted cancel ends the wait with [`OtaError::Cancelled`].
fn wait_or_cancel(cancel: &OtaCancel, wait: Duration) -> Result<(), OtaError> {
    let end = Instant::now() + wait;
    loop {
        if cancel.is_requested() {
            return Err(OtaError::Cancelled);
        }
        let left = end.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(());
        }
        thread::sleep(left.min(CANCEL_POLL));
    }
}

fn failed(ctx: &DownloadCtx, e: OtaError) {
    log::error!(
        "[ota] download of action {} failed: {}",
        ctx.offer.action,
        e.code()
    );
    sample(&ctx.shared.download_free, "download");
    settle_failed(
        &ctx.records,
        ctx.settings.running_version,
        FailReason::Ota(e),
    );
    ctx.waker.notify_download();
}

/// After a failed download: lets the store reconcile the stalled attempt and write the failure verdict bound to it.
///
/// A cancel writes no verdict; a reason without a profile code clears the attempt and the action is offered again.
/// A settle that keeps the attempt is logged, not retried: the next boot reconciles it.
pub(super) fn settle_failed(records: &Records, running: Version, reason: FailReason) {
    let hardware = match read_hardware_facts(BusyRetry::default()) {
        Ok(hardware) => hardware,
        Err(e) => {
            log::warn!("[ota] the failed attempt is kept, hardware unreadable: {e}");
            return;
        }
    };
    match records.with(|store| store.settle_failed_ddi_download(hardware, Some(running), reason)) {
        Ok(FailedDownload::Cleared) => {
            if ProfileCode::for_reason(reason).is_some() {
                log::info!("[ota] the failed attempt was cleared; its verdict awaits delivery");
            } else {
                log::info!("[ota] the failed attempt was cleared");
            }
        }
        Ok(other) => {
            log::warn!("[ota] the failed attempt is kept for the next boot: {other:?}");
        }
        Err(e) => log::warn!("[ota] the failed attempt is kept, records unreadable: {e}"),
    }
}

/// The image is written and selected for the next boot: record it, tell the server, wait `restart_grace`, then restart through the app.
fn restart_into_new_image(ctx: &DownloadCtx) -> ! {
    if let Err(e) = ctx.records.with(|store| store.mark_boot_selected()) {
        log::error!("[ota] the boot selection was not recorded: {e}; restarting anyway");
    }
    send_proceeding(&ctx.transport, &ctx.offer.action);
    sample(&ctx.shared.download_free, "download");
    let grace = ctx.settings.timings.restart_grace;
    log::info!(
        "[ota] action {}: boot slot selected; restarting in {} ms",
        ctx.offer.action,
        grace.as_millis()
    );
    thread::sleep(grace);
    (ctx.restart)();
    log::error!("[ota] the restart callback returned; the download thread parks");
    loop {
        thread::park();
    }
}
