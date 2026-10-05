//! Best-effort status publishing for the worker, the reporter and the health policy.

use std::thread;

use esp_idf_svc::mqtt::client::QoS;
use juggler::ota::{OtaStatus, OtaTimings};

use crate::mqtt::{MqttHandle, TryPublishError};

/// Publishes `status` without blocking on the client for long.
///
/// The `rolled_back` report holds the client mutex for a whole acknowledged publish, which can span an MQTT reconnect, so another thread's `try_publish_with` may see `WouldBlock` for seconds.
/// A busy client is retried `status_publish_retries` times, `status_publish_delay` apart; after that the status is dropped and logged.
/// That bound covers only contention on the client mutex: once the mutex is won, `enqueue` can block on esp-mqtt's `api_lock` for one reconnect handshake, so a status published before a restart is best-effort in both directions (it can be dropped, or it can delay the restart).
/// This is not a deadlock: the event loop never waits for the publisher, it only hands commands to a channel.
/// Blocking `publish_with` is deliberately not used: it could stall the worker on a dead link.
pub(super) fn publish_status(
    mqtt: &MqttHandle,
    topic: &str,
    status: &OtaStatus<'_>,
    timings: &OtaTimings,
) {
    let json = match status.to_json() {
        Ok(json) => json,
        Err(e) => {
            log::warn!("[ota] status not serialised: {e}");
            return;
        }
    };
    for attempt in 0..=timings.status_publish_retries {
        match mqtt.try_publish_with(topic, json.as_bytes(), QoS::AtLeastOnce, false) {
            Ok(()) => return,
            Err(TryPublishError::WouldBlock) => {
                if attempt < timings.status_publish_retries {
                    thread::sleep(timings.status_publish_delay);
                }
            }
            Err(TryPublishError::Other(e)) => {
                log::warn!("[ota] status publish failed: {e:#}");
                return;
            }
        }
    }
    log::warn!("[ota] status publish dropped: the MQTT client stayed busy");
}
