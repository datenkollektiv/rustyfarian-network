//! The reporter thread: publishes rejections and delivers the `rolled_back` report.
//!
//! It never publishes from an MQTT callback and never holds the store lock while publishing.
//! Its wait is always bounded by the retry interval, so a report armed later (by a repair, or after boot) is picked up and a closed runtime is noticed.
//!
//! A rejection waits behind at most one `publish_acked`; `publish_acked` takes the client mutex and can block in `enqueue` on esp-mqtt's `api_lock` for a whole reconnect (seconds, not only the acknowledgement timeout), and while it does other threads' `try_publish_with` return `WouldBlock`.
//! Those callers retry (see `publish_status`), so rejections and statuses survive normal contention.

use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::Arc;
use std::time::Instant;

use juggler::ota::runtime::{
    ack_delivered, rejects_delta, retry_completion, CompletionRetry, CompletionRetryConfig,
    CompletionTry, DeliveryStep, PublishOutcome, PublishStep, ReporterConfig, ReporterMachine,
    ViewStep,
};
use juggler::ota::{FailReason, OtaStatus, PendingReportView};

use super::publish::publish_status;
use super::shared::Shared;
use super::stack::sample;
use super::{OtaConfig, Records};
use crate::mqtt::{MqttHandle, PublishAckError};

pub(super) struct ReporterCtx {
    pub(super) cfg: Arc<OtaConfig>,
    pub(super) shared: Arc<Shared>,
    pub(super) records: Records,
    pub(super) mqtt: MqttHandle,
}

impl ReporterCtx {
    fn publish_rejection(&self, reason: FailReason) {
        publish_status(
            &self.mqtt,
            &self.cfg.settings.status_topic,
            &OtaStatus::Failed { reason },
            &self.cfg.settings.timings,
        );
    }

    fn drain_rejections(&self, rejections: &Receiver<FailReason>) {
        for _ in 0..self.cfg.settings.timings.reject_queue_depth {
            match rejections.try_recv() {
                Ok(reason) => self.publish_rejection(reason),
                Err(_) => break,
            }
        }
    }
}

pub(super) fn run(ctx: ReporterCtx, rejections: Receiver<FailReason>) {
    let origin = Instant::now();
    let mut machine = ReporterMachine::new(ReporterConfig::from(&ctx.cfg.settings.timings));
    let mut completion =
        CompletionRetry::new(CompletionRetryConfig::from(&ctx.cfg.settings.timings));
    let mut last_seen = 0u32;
    loop {
        let now = origin.elapsed();
        let wait = completion
            .next_wait(now)
            .map_or_else(|| machine.next_wait(now), |w| w.min(machine.next_wait(now)));
        match rejections.recv_timeout(wait) {
            Ok(reason) => {
                ctx.publish_rejection(reason);
                ctx.drain_rejections(&rejections);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                log::info!("[ota] the rejection channel closed; the reporter exits");
                return;
            }
        }
        if ctx.shared.is_closed() {
            ctx.drain_rejections(&rejections);
            log::info!("[ota] the runtime is closed; the reporter exits");
            return;
        }

        let total = ctx.shared.rejects_total();
        let dropped = rejects_delta(total, last_seen);
        if dropped > 0 {
            log::warn!("[ota] {dropped} rejection(s) could not be reported");
            last_seen = total;
        }

        if machine.report_due(origin.elapsed()) {
            service_report(&ctx, &mut machine, origin);
        }
        service_completion(&ctx, &mut completion, origin);
        sample(&ctx.shared.reporter_free, "reporter");
    }
}

/// Retries `complete_attempt` for the running version while the health policy asked for it; the store lock is held for one call only, never across the backoff.
fn service_completion(ctx: &ReporterCtx, retry: &mut CompletionRetry, origin: Instant) {
    if !retry.is_armed() && ctx.shared.completion_retry_requested() {
        retry.arm(origin.elapsed());
    }
    if !retry.due(origin.elapsed()) {
        return;
    }
    let running = ctx.cfg.settings.running_version;
    let result = ctx.records.with(|store| retry_completion(store, running));
    match result {
        CompletionTry::Completed => {
            log::info!("[ota] the attempt record is cleared; updates are admitted again");
        }
        CompletionTry::Gone => log::info!("[ota] no attempt record remains; the retry stops"),
        CompletionTry::StillFailing => log::warn!("[ota] the attempt record is still not cleared"),
    }
    if !retry.on_result(origin.elapsed(), result) {
        ctx.shared.clear_completion_retry();
    }
}

fn service_report(ctx: &ReporterCtx, machine: &mut ReporterMachine, origin: Instant) {
    let timings = &ctx.cfg.settings.timings;
    loop {
        if !ctx.mqtt.is_connected() {
            machine.on_disconnected(origin.elapsed());
            return;
        }
        let report = match ctx.records.with(|store| store.pending_report()) {
            Ok(PendingReportView::Ready(report)) => match machine.on_view(origin.elapsed(), true) {
                ViewStep::Publish => report,
                ViewStep::Idle => return,
            },
            Ok(PendingReportView::None | PendingReportView::AwaitingBootReconcile) => {
                machine.on_view(origin.elapsed(), false);
                return;
            }
            Err(e) => {
                log::warn!("[ota] the pending report is unreadable: {e}");
                machine.on_store_error(origin.elapsed());
                return;
            }
        };

        let status = OtaStatus::RolledBack {
            reason: report.reason(),
            attempt_id: report.attempt_id,
            epoch: report.epoch,
        };
        let json = match status.to_json() {
            Ok(json) => json,
            Err(e) => {
                log::warn!("[ota] the report was not serialised: {e}");
                machine.on_store_error(origin.elapsed());
                return;
            }
        };
        let outcome = match ctx.mqtt.publish_acked(
            &ctx.cfg.settings.status_topic,
            json.as_bytes(),
            false,
            timings.publish_ack_timeout,
        ) {
            Ok(()) => PublishOutcome::Acked,
            Err(PublishAckError::Timeout) => PublishOutcome::Timeout,
            Err(PublishAckError::Disconnected) => PublishOutcome::Disconnected,
            Err(PublishAckError::WrongThread) => PublishOutcome::WrongThread,
            Err(PublishAckError::Other(e)) => {
                log::warn!("[ota] the report publish failed: {e:#}");
                PublishOutcome::Other
            }
        };

        match machine.on_publish(origin.elapsed(), outcome) {
            PublishStep::RetryLater { loud } => {
                if loud {
                    log::error!(
                        "[ota] the report was published from a callback thread: {outcome:?}"
                    );
                } else {
                    log::info!(
                        "[ota] the report {} is kept and retried in {} s: {outcome:?}",
                        report.attempt_id,
                        timings.report_retry.as_secs()
                    );
                }
                return;
            }
            PublishStep::MarkDelivered => {
                match ctx
                    .records
                    .with(|store| ack_delivered(store, report.attempt_id))
                {
                    Ok(delivery) => {
                        log::info!(
                            "[ota] the report {} is delivered: {delivery:?}",
                            report.attempt_id
                        );
                        match machine.on_delivery(origin.elapsed(), delivery) {
                            DeliveryStep::Done => return,
                            DeliveryStep::PublishNext => continue,
                        }
                    }
                    Err(e) => {
                        log::warn!("[ota] the delivery was not recorded: {e}");
                        machine.on_store_error(origin.elapsed());
                        return;
                    }
                }
            }
        }
    }
}
