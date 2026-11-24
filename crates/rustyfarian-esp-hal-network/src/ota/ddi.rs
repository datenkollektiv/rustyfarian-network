//! The esp-hal DDI (hawkBit DDI v1) OTA client (ADR 027), with the names of the ESP-IDF tier's `ota::ddi` module (ADR 022).
//!
//! [`DdiTransport`] is the credential-bound transport: one `async fn` per DDI endpoint, with the ESP-IDF tier's names, arguments and results; only [`DdiTransport::new`] also takes the station `Stack`.
//! It takes the base URL, the controller id and the [`DdiToken`] once and builds every request itself; a server link ([`Href`]) is requested only after [`DdiEndpoint::check_href`] found it on the configured origin, at the [`CheckedUrl`] rebuilt from the checked parts.
//! Redirects are never followed, plain `http://` needs the `ota-ddi-insecure-http` lab feature, and `https://` is refused until the TLS round lands, on both tiers.
//! The transport only performs I/O: it hands back the status, the raw `Retry-After` and the body ([`DdiResponse`]), and the DDI machine in `juggler::ota::ddi` decides what they mean.
//!
//! The record store opener ([`open_records`], [`Records`]) and the shared types of the runtime ([`DdiSettings`], [`DdiStackMarks`], [`HealthConfig`], ...) are here as on the ESP-IDF tier.
//!
//! [`DdiRuntime`] is the runtime with the ESP-IDF tier's hook points: `DdiRuntime::new` (validates everything, before the store or the radio), `open_records` before Wi-Fi, `DdiRuntime::start` once the station `Stack` exists, and `DdiHandle::run_health_policy` on the app's main task.
//! `start` also returns the [`DdiRunner`]: the firmware spawns `DdiRunner::run` in its own task (the ESP-IDF tier spawns its threads itself).
//! The app subscribes [`DdiNudge::topic`] with `MqttHandle::subscribe` and hands every inbound topic to [`DdiNudge::on_message`]; the restart callback ([`RestartFn`]) is the only place the device is ever reset.
//! The runtime composes the machines of `juggler::ota::ddi` (`DdiWorker`, `DownloadFlow`, `HealthFlow`, the boot plan functions) and only performs their actions (ADR 023).
//!
//! Gaps of this tier: no TLS (as on the ESP-IDF tier), no IPv6 server address, no wall clock (a `Retry-After` date is ignored and feedback carries no timestamp), and no stack high-water marks ([`DdiHandle::stack_high_water`] is always `None`).

mod error;
#[cfg(all(
    feature = "ota-ddi",
    any(
        feature = "esp32c3",
        feature = "esp32c6",
        feature = "esp32",
        feature = "esp32s3"
    )
))]
mod handle;
#[cfg(all(
    feature = "ota-ddi",
    any(
        feature = "esp32c3",
        feature = "esp32c6",
        feature = "esp32",
        feature = "esp32s3"
    )
))]
mod health;
mod nudge;
pub(crate) mod plan;
#[cfg(all(
    feature = "ota-ddi",
    any(
        feature = "esp32c3",
        feature = "esp32c6",
        feature = "esp32",
        feature = "esp32s3"
    )
))]
pub(crate) mod resolve;
#[cfg(all(
    feature = "ota-ddi",
    any(
        feature = "esp32c3",
        feature = "esp32c6",
        feature = "esp32",
        feature = "esp32s3"
    )
))]
mod runner;
#[cfg(all(
    feature = "ota-ddi",
    any(
        feature = "esp32c3",
        feature = "esp32c6",
        feature = "esp32",
        feature = "esp32s3"
    )
))]
mod transport;

pub use juggler::ota::ddi::wire::{
    ActionId, Board, Chip, ConfigData, Execution, Feedback, FeedbackError, Finished, Href,
    ProfileCode, MAX_RESPONSE_BYTES,
};
pub use juggler::ota::ddi::{
    clock_is_set, nudge_topic, DdiSettings, DdiSettingsError, DdiStackMarks, DdiStacks, DdiTimings,
    DdiTransportConfig, DdiTransportError, DDI_MIN_STACK_BYTES, DEFAULT_DEADLINE, DEFAULT_TIMEOUT,
    WALL_CLOCK_FLOOR_SECS,
};
pub use juggler::ota::ddi::{
    BodyFailure, CheckedUrl, DdiEndpoint, DdiResponse, DdiToken, DdiTokenError, RequestOutcome,
    RetryAfter, UrlRefusal, MAX_REQUEST_BYTES,
};
pub use juggler::ota::health::HealthConfig;
pub use juggler::ota::{DeadlineAction, HealthVerdict};

pub use crate::ota::flags::OtaFlags;
pub use error::DdiRuntimeError;
pub use nudge::DdiNudge;

/// Experimental: API may change before 1.0.
///
/// The app's restart callback: the only way the DDI runtime ever resets the device (after the new image is selected, after a rollback, at the health deadline with `DeadlineAction::NoteAndRestart`).
///
/// A plain `fn()` on this tier, typically one that calls the esp-hal system reset; the ESP-IDF tier takes an `Arc<dyn Fn() + Send + Sync>`, since the ESP32-C3 has no `Arc` here.
pub type RestartFn = fn();

#[cfg(all(
    feature = "ota-ddi",
    any(
        feature = "esp32c3",
        feature = "esp32c6",
        feature = "esp32",
        feature = "esp32s3"
    )
))]
pub use crate::ota::records::{open_records, Records};
#[cfg(all(
    feature = "ota-ddi",
    any(
        feature = "esp32c3",
        feature = "esp32c6",
        feature = "esp32",
        feature = "esp32s3"
    )
))]
pub use handle::{DdiHandle, DdiRuntime};
#[cfg(all(
    feature = "ota-ddi",
    any(
        feature = "esp32c3",
        feature = "esp32c6",
        feature = "esp32",
        feature = "esp32s3"
    )
))]
pub use runner::DdiRunner;
#[cfg(all(
    feature = "ota-ddi",
    any(
        feature = "esp32c3",
        feature = "esp32c6",
        feature = "esp32",
        feature = "esp32s3"
    )
))]
pub use transport::DdiTransport;

// Parity guard: every public name of the ESP-IDF tier's `ota::ddi` module resolves here. The names
// that need a chip build (`DdiTransport`, `DdiRuntime`, `DdiHandle`, `open_records`, `Records`) are
// checked by the chip-build guard below; everything else runs on the host via `just test-hal`.
#[cfg(test)]
mod parity_guard {
    #[test]
    #[allow(unused_imports)]
    fn every_esp_idf_name_resolves_on_this_tier() {
        use super::{
            clock_is_set, nudge_topic, ActionId, Board, BodyFailure, CheckedUrl, Chip, ConfigData,
            DdiEndpoint, DdiNudge, DdiResponse, DdiRuntimeError, DdiSettings, DdiSettingsError,
            DdiStackMarks, DdiStacks, DdiTimings, DdiToken, DdiTokenError, DdiTransportConfig,
            DdiTransportError, DeadlineAction, Execution, Feedback, FeedbackError, Finished,
            HealthConfig, HealthVerdict, Href, OtaFlags, ProfileCode, RequestOutcome, RestartFn,
            RetryAfter, UrlRefusal, DDI_MIN_STACK_BYTES, DEFAULT_DEADLINE, DEFAULT_TIMEOUT,
            MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, WALL_CLOCK_FLOOR_SECS,
        };
        let same = |_: juggler::ota::ddi::DdiTransportError| {};
        same(DdiTransportError::Timeout);
        assert_eq!(
            DdiTransportError::ServerUnreachable.into_outcome(),
            Some(RequestOutcome::Unreachable)
        );
        assert_eq!(DEFAULT_TIMEOUT, juggler::ota::ddi::DEFAULT_TIMEOUT);
        assert_eq!(DEFAULT_DEADLINE, juggler::ota::ddi::DEFAULT_DEADLINE);
        let _: fn(DdiTransportConfig) -> juggler::ota::ddi::DdiTransportConfig = |c| c;
        let marks: juggler::ota::ddi::DdiStackMarks = DdiStackMarks {
            worker_min_free_bytes: None,
            download_min_free_bytes: None,
        };
        assert_eq!(marks.worker_min_free_bytes, None);
        let _: fn(DdiSettingsError) -> DdiRuntimeError = DdiRuntimeError::from;
        let _: fn(&DdiNudge, &str) -> bool = DdiNudge::on_message;
        let _: fn(&DdiNudge) -> &str = DdiNudge::topic;
        let _: [fn(&OtaFlags) -> bool; 4] = [
            OtaFlags::admission_open,
            OtaFlags::refuse_mark_valid,
            OtaFlags::health_settled,
            OtaFlags::records_suspect,
        ];
        fn reset_device() {}
        let _: RestartFn = reset_device;
    }
}

// The chip-build half of the parity guard: the names that need a chip feature, with the ESP-IDF
// tier's signatures where the tiers agree (`start` also takes the stack and returns the runner).
#[cfg(all(
    feature = "ota-ddi",
    any(
        feature = "esp32c3",
        feature = "esp32c6",
        feature = "esp32",
        feature = "esp32s3"
    )
))]
#[allow(
    clippy::type_complexity,
    reason = "the signatures are spelled out on purpose"
)]
const _: fn() = || {
    let _: fn(DdiSettings, DdiTransportConfig, RestartFn) -> Result<DdiRuntime, DdiRuntimeError> =
        DdiRuntime::new;
    let _: fn(&DdiRuntime) -> DdiNudge = DdiRuntime::nudge;
    let _: fn(&DdiRuntime) -> &str = DdiRuntime::nudge_topic;
    let _: fn(
        DdiRuntime,
        Records,
        embassy_net::Stack<'static>,
    ) -> Result<(DdiHandle, DdiRunner), DdiRuntimeError> = DdiRuntime::start;
    let _: fn(&DdiHandle) -> &OtaFlags = DdiHandle::flags;
    let _: fn(&DdiHandle) -> DdiNudge = DdiHandle::nudge;
    let _: fn(&DdiHandle) -> &str = DdiHandle::nudge_topic;
    let _: fn(&DdiHandle) -> DdiStackMarks = DdiHandle::stack_high_water;
    let _: fn(&'static crate::ota::SharedFlash) -> Result<Records, juggler::ota::StoreError> =
        open_records;
    let _: fn(
        DdiTransportConfig,
        embassy_net::Stack<'static>,
    ) -> Result<DdiTransport, DdiTransportError> = DdiTransport::new;
};
