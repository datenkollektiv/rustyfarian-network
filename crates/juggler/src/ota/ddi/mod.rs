//! The Eclipse hawkBit DDI v1 profile client of the fleet.
//!
//! The JSON codec is in [`wire`]: the poll, deployment and cancel documents, the feedback encoder and the profile codes.
//! The per-device target token is [`DdiToken`], validated as `[0-9a-f]{32,64}` and held, like the `Authorization` value built from it, in a zeroizing, redacting [`Secret`].
//! The pure rules a tier's DDI transport applies are here too: [`DdiEndpoint`] builds every request URL from the configured base and [`ControllerId`] and checks server links for the configured origin, and [`DdiResponse`], [`RetryAfter`] and [`ResponseBody`] bound what a response may carry (ADR 027).
//! [`PollMachine`] is the sans-IO poll loop (ADR 023 rung 3): it times each poll from `config.polling.sleep`, `Retry-After` and a jittered back-off ([`clamp_wait`], [`backoff_delay`]), judges the status of each answer, and picks the one link of a poll the device acts on.
//! A tier feeds it each answer as a [`RequestOutcome`]; the deployment admission, feedback and cancel parts plug into its actions, and [`PollMachine::nudge`] is the MQTT nudge's early poll.
//! [`admit_retrieved`] is the admission step after [`PollAction::RetrieveDeployment`]: document validity, skip, chip and version gates in the profile's order, ending in a [`DeploymentVerdict`].
//! [`VerdictSender`] delivers the persisted terminal verdict (30 s pacing, the 200/404/410 rule, the consecutive-401 rule), [`CancelMachine`] answers a `cancelAction` on the cancel path, and [`FeedbackPost`] builds every feedback, the `proceeding` progress included.
//! [`decide_download_retry`] is the retry policy of a failed artifact download: transient errors back off ([`download_retry_backoff`]) inside one deadline, the rest are final.
//! [`DdiSettings`] is the platform-neutral configuration of a tier's DDI runtime (stacks, timings, the board name), [`WALL_CLOCK_FLOOR_SECS`] the rule that decides when a wall clock counts as set and [`nudge_topic`] the exact MQTT topic of the nudge.
//!
//! Enabled by the `ota-ddi` feature (implies `ota`, needs `alloc`).

mod admission;
mod cancel;
mod endpoint;
mod feedback;
mod poller;
mod response;
mod retry;
#[cfg(test)]
mod scenarios;
mod schedule;
mod settings;
mod token;
pub mod wire;

pub use admission::{
    admit_deployment, admit_retrieved, AdmissionFacts, AdmittedOffer, DeploymentVerdict, Refusal,
};
pub use cancel::{CancelInput, CancelMachine, CancelResult, CancelStep, CancelTarget};
pub use endpoint::{
    CheckedUrl, ControllerId, DdiEndpoint, EndpointError, Scheme, UrlRefusal,
    CONTROLLER_ID_MAX_LEN, CONTROLLER_ID_MIN_LEN, MAX_BASE_URL_BYTES,
};
pub use feedback::{
    feedback_hand_back, FeedbackPath, FeedbackPost, SendOutcome, VerdictSender, VerdictStep,
    FEEDBACK_RETRY_INTERVAL, MAX_UNAUTHORIZED_SENDS,
};
pub use poller::{
    Nudge, OfferGate, PollAction, PollAnswer, PollInput, PollMachine, PollVerdict, Transient,
    WorkOutcome, NUDGE_MIN_INTERVAL,
};
pub use response::{
    is_redirect, BodyError, BodyFailure, DdiResponse, RequestOutcome, ResponseBody, RetryAfter,
    MAX_REQUEST_BYTES, MAX_RETRY_AFTER_BYTES,
};
pub use retry::{
    decide_download_retry, download_retry_backoff, is_transient_download_error, DownloadRetry,
    RETRY_INITIAL_BACKOFF, RETRY_MAX_BACKOFF,
};
pub use schedule::{
    backoff_delay, clamp_wait, parse_http_date, DEFAULT_POLL_INTERVAL, MAX_POLL_INTERVAL,
    MIN_POLL_INTERVAL,
};
pub use settings::{
    clock_is_set, nudge_topic, DdiSettings, DdiSettingsError, DdiStacks, DdiTimings,
    DDI_MIN_STACK_BYTES, WALL_CLOCK_FLOOR_SECS,
};
pub use token::{
    DdiToken, DdiTokenError, Secret, AUTHORIZATION_MAX_LEN, TOKEN_MAX_LEN, TOKEN_MIN_LEN,
};
