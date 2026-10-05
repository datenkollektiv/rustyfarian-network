//! The OTA consumer runtime as pure state machines: command intake, worker decisions, health policy, report delivery, manifest body limits and the store operations that need more than one record call.
//!
//! The platform crate (ESP-IDF) owns the threads, channels, locks, HTTP, flash and MQTT; this module owns every decision.
//! Time enters as `core::time::Duration` elapsed since a caller-chosen origin, and no type here needs atomics, so the module builds for `riscv32imc`.
//! Nothing here reboots, marks an image valid, rolls back, publishes or logs; each machine returns what the driver must do.
//!
//! | Module | Decides |
//! |:-------|:--------|
//! | [`config`] | topics, timings, stack sizes and their validation |
//! | [`intake`] | what the MQTT callback does with one payload (reject, or queue it) |
//! | [`worker`] | admission gates for update, rollback and repair, and offer retention |
//! | [`store_ops`] | the multi-call record sequences (rollback arm and undo, repair, delivery) |
//! | [`health`] | the health policy of the running image as a function of time and step outcomes |
//! | [`report`] | when and how the `rolled_back` report is published and marked delivered, and the background completion retry |
//! | [`fetch`] | the size and total-time limits of the manifest body |

pub mod config;
pub mod fetch;
pub mod health;
pub mod intake;
pub mod report;
pub mod store_ops;
pub mod worker;

pub use config::{
    ConfigError, DeadlineAction, OtaSettings, OtaStacks, OtaTimings, MAX_REJECT_QUEUE_DEPTH,
    MAX_REPAIR_ITERATIONS, MIN_STACK_BYTES, MIN_WORKER_STACK_BYTES,
};
pub use fetch::{Feed, FetchError, ManifestBody};
pub use health::{
    initial_admission, HealthAction, HealthConfig, HealthEvent, HealthMachine, HealthVerdict,
    SlotReading,
};
pub use intake::{decide_intake, IntakeDecision, SendResult};
pub use report::{
    rejects_delta, CompletionRetry, CompletionRetryConfig, DeliveryStep, PublishOutcome,
    PublishStep, ReporterConfig, ReporterMachine, ViewStep,
};
pub use store_ops::{
    ack_delivered, failed_download, repair_all, retry_completion, rollback_arm, rollback_undo,
    ArmedRollback, CompletionTry, FailedSettle, RepairError, Undone,
};
pub use worker::{
    begin_failure, blocked_report, early_update_block, plan_offer, plan_repair, plan_rollback,
    plan_update_gate, refresh_after_repair, refused_or_absent, retained_due, OfferPlan, Probe,
    Proceed, RefreshedFlags, RepairGate, Retention,
};
