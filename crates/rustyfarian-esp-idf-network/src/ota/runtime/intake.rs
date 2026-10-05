//! Command intake for the MQTT `on_message` callback.
//!
//! The callback runs on the MQTT event loop under esp-mqtt's `api_lock` (ADR 017), so everything here is non-blocking: parse, a few atomics, `try_send`.
//! It never calls the MQTT client, never takes the store lock and never logs a payload.

use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::Arc;

use juggler::ota::runtime::{decide_intake, IntakeDecision, SendResult};
use juggler::ota::{FailReason, OtaCommand};

use super::shared::Shared;

/// Hands incoming command payloads to the worker; cheap to clone into the MQTT callback.
#[derive(Clone)]
pub struct OtaSubmitter {
    inner: Arc<SubmitterInner>,
}

struct SubmitterInner {
    command_topic: String,
    shared: Arc<Shared>,
    commands: SyncSender<OtaCommand>,
    rejections: SyncSender<FailReason>,
}

impl OtaSubmitter {
    pub(super) fn new(
        command_topic: String,
        shared: Arc<Shared>,
        commands: SyncSender<OtaCommand>,
        rejections: SyncSender<FailReason>,
    ) -> Self {
        Self {
            inner: Arc::new(SubmitterInner {
                command_topic,
                shared,
                commands,
                rejections,
            }),
        }
    }

    /// The topic to subscribe to (`MqttBuilder::subscribe`).
    ///
    /// Commands must never be published retained: a retained command would run again on every reconnect and boot.
    pub fn command_topic(&self) -> &str {
        &self.inner.command_topic
    }

    /// For the `on_message` callback: takes the message if it is on the command topic and returns `true`, otherwise returns `false` untouched.
    pub fn handle(&self, topic: &str, payload: &[u8]) -> bool {
        if topic != self.inner.command_topic {
            return false;
        }
        self.submit(payload);
        true
    }

    /// Parses one command payload and queues it, or queues the rejection for the reporter.
    ///
    /// Non-blocking; safe to call from the MQTT callback.
    /// A rejection that cannot be queued (the reporter is behind or gone) is counted in [`OtaSubmitter::rejects_dropped`].
    pub fn submit(&self, payload: &[u8]) {
        let shared = &self.inner.shared;
        let decision = decide_intake(
            OtaCommand::parse(payload),
            shared.is_closed(),
            || shared.claim_busy(),
            |command| match self.inner.commands.try_send(command) {
                Ok(()) => SendResult::Sent,
                Err(TrySendError::Full(_)) => SendResult::Full,
                Err(TrySendError::Disconnected(_)) => SendResult::Disconnected,
            },
        );
        if let IntakeDecision::Rejected {
            reason,
            release_busy,
            set_closed,
        } = decision
        {
            if release_busy {
                shared.release_busy();
            }
            if set_closed {
                shared.close();
            }
            log::debug!("[ota] command rejected: {reason}");
            if self.inner.rejections.try_send(reason).is_err() {
                shared.count_dropped_reject();
            }
        }
    }

    /// How many rejections were dropped over the lifetime of the runtime because the reporter queue was full or the reporter was gone.
    ///
    /// A monotonic counter that wraps at `u32::MAX`; never reset.
    pub fn rejects_dropped(&self) -> u32 {
        self.inner.shared.rejects_total()
    }
}
