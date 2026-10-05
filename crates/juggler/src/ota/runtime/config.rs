//! Runtime configuration: topics, timings and thread stack sizes, validated before any thread starts.

use alloc::string::String;
use core::fmt;
use core::time::Duration;

use crate::ota::{RollbackReason, Version};

/// The smallest accepted stack in bytes for the reporter thread.
///
/// The ESP-IDF pthread default (3 KiB) overflows with the default logger, so anything below this is refused.
/// The worker needs more: see [`MIN_WORKER_STACK_BYTES`].
pub const MIN_STACK_BYTES: usize = 4096;

/// The smallest accepted worker stack in bytes.
///
/// Covers the 4 KiB download buffer, the 1 KiB manifest buffer and the HTTP client, SHA-256 and logging frames.
pub const MIN_WORKER_STACK_BYTES: usize = 12288;

/// The largest accepted `reject_queue_depth`.
///
/// The depth sizes a preallocated `sync_channel` and bounds the reporter's drain loop, whose every entry may spend `status_publish_retries` publish attempts; 32 is eight times the default and plenty for the few distinct rejections a boot can accumulate, while keeping both the RAM and the worst-case drain time small.
pub const MAX_REJECT_QUEUE_DEPTH: usize = 32;

/// The largest accepted `repair_max_iterations`.
///
/// A repair repairs one record group per iteration and the store has only a handful of groups (the default of 32 is already generous); 256 stays far above any real need while a corrupted or mistaken value can no longer turn one command into an effectively unbounded flash-write loop.
pub const MAX_REPAIR_ITERATIONS: u32 = 256;

/// The longest accepted topic in bytes.
const MAX_TOPIC_BYTES: usize = 256;

/// What the health policy does when the failure deadline passes with the image still pending verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeadlineAction {
    /// Note `health_deadline`, then roll back with bounded retries (the production behaviour).
    Rollback,
    /// Note the given reason, then call the app's restart callback and let the bootloader abort the unverified image.
    ///
    /// The unhealthy demo build uses this with [`RollbackReason::Unhealthy`].
    NoteAndRestart(RollbackReason),
}

/// Stack sizes of the runtime threads, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OtaStacks {
    /// The worker thread: manifest fetch, download, flash.
    pub worker_bytes: usize,
    /// The reporter thread: rejections and the `rolled_back` report.
    pub reporter_bytes: usize,
}

impl Default for OtaStacks {
    /// 16 KiB worker, 8 KiB reporter (raised from 6 KiB until hardware high-water marks justify less).
    fn default() -> Self {
        Self {
            worker_bytes: 16384,
            reporter_bytes: 8192,
        }
    }
}

/// Every time and count the runtime uses; all fields are public so an app (or a demo build) can override single values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OtaTimings {
    /// Per network operation timeout of the manifest fetch (`esp_http_client`).
    pub manifest_per_op: Duration,
    /// Total time limit of the whole manifest fetch.
    pub manifest_total: Duration,
    /// Per network operation timeout of the firmware download, in seconds.
    pub firmware_timeout_secs: u64,
    /// Total time limit of one firmware download (the cooperative deadline of `OtaSession::with_deadline`).
    pub firmware_deadline: Duration,
    /// How long one `rolled_back` publish waits for the broker acknowledgement.
    pub publish_ack_timeout: Duration,
    /// Interval between report attempts and the longest idle wait of the reporter.
    pub report_retry: Duration,
    /// How often a reporter that finds the broker disconnected looks again.
    pub reporter_connect_poll: Duration,
    /// Extra attempts of a best-effort status publish that found the client busy.
    pub status_publish_retries: u32,
    /// Pause between those attempts.
    pub status_publish_delay: Duration,
    /// Pause between `swap_pending` and the restart callback, so the status can leave the device.
    pub restart_grace: Duration,
    /// How often the worker re-evaluates a retained blocked offer.
    pub retained_poll: Duration,
    /// Capacity of the rejection queue to the reporter.
    pub reject_queue_depth: usize,
    /// Extra attempts to create the OTA session while another handle is busy.
    pub session_busy_retries: u32,
    /// Pause between those attempts.
    pub session_busy_delay: Duration,
    /// Time since boot before a healthy image is marked valid.
    pub healthy_dwell: Duration,
    /// Time since boot after which an unverified image that is still unhealthy gives up.
    pub failure_deadline: Duration,
    /// Health poll interval.
    pub poll_interval: Duration,
    /// Interval between reads of the running slot at policy start.
    pub slot_retry: Duration,
    /// Failed slot reads (not counting "busy") before the deadline may end verification.
    pub slot_min_attempts: u32,
    /// Failed slot reads between two warnings.
    pub slot_warn_every: u32,
    /// Attempts to record the completed attempt after the image was marked valid.
    pub complete_attempts: u32,
    /// Pause between those attempts.
    pub complete_retry: Duration,
    /// Rollback attempts at the failure deadline.
    pub rollback_attempts: u32,
    /// Pause between those attempts.
    pub rollback_retry: Duration,
    /// The most record groups one repair command may repair.
    pub repair_max_iterations: u32,
    /// What happens at the failure deadline.
    pub deadline_action: DeadlineAction,
}

impl Default for OtaTimings {
    fn default() -> Self {
        Self {
            manifest_per_op: Duration::from_secs(5),
            manifest_total: Duration::from_secs(10),
            firmware_timeout_secs: 60,
            firmware_deadline: Duration::from_secs(300),
            publish_ack_timeout: Duration::from_secs(5),
            report_retry: Duration::from_secs(10),
            reporter_connect_poll: Duration::from_secs(1),
            status_publish_retries: 5,
            status_publish_delay: Duration::from_millis(200),
            restart_grace: Duration::from_millis(1000),
            retained_poll: Duration::from_secs(1),
            reject_queue_depth: 4,
            session_busy_retries: 3,
            session_busy_delay: Duration::from_millis(500),
            healthy_dwell: Duration::from_secs(30),
            failure_deadline: Duration::from_secs(120),
            poll_interval: Duration::from_secs(1),
            slot_retry: Duration::from_millis(200),
            slot_min_attempts: 5,
            slot_warn_every: 25,
            complete_attempts: 3,
            complete_retry: Duration::from_secs(1),
            rollback_attempts: 3,
            rollback_retry: Duration::from_secs(5),
            repair_max_iterations: 32,
            deadline_action: DeadlineAction::Rollback,
        }
    }
}

/// Why a configuration was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigError {
    /// A duration that must not be zero is zero; carries the field name.
    ZeroDuration(&'static str),
    /// A count that must not be zero is zero; carries the field name.
    ZeroCount(&'static str),
    /// A stack is below its floor ([`MIN_WORKER_STACK_BYTES`] for the worker, [`MIN_STACK_BYTES`] for the reporter).
    StackTooSmall {
        /// The thread whose stack is too small.
        thread: &'static str,
        /// The configured size.
        bytes: usize,
    },
    /// A topic is empty, too long, contains a NUL byte or a wildcard; carries which topic.
    BadTopic(&'static str),
    /// A count exceeds its documented maximum ([`MAX_REJECT_QUEUE_DEPTH`], [`MAX_REPAIR_ITERATIONS`]); carries the field name.
    TooLarge(&'static str),
    /// The command and status topics are the same (the status would be taken for a command).
    SameTopic,
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::ZeroDuration(name) => write!(f, "OTA timing `{name}` must not be zero"),
            ConfigError::ZeroCount(name) => write!(f, "OTA setting `{name}` must not be zero"),
            ConfigError::StackTooSmall { thread, bytes } => write!(
                f,
                "OTA {thread} stack of {bytes} bytes is below {}",
                stack_floor(thread)
            ),
            ConfigError::TooLarge(name) => write!(f, "OTA setting `{name}` exceeds its maximum"),
            ConfigError::BadTopic(which) => write!(f, "OTA {which} topic is invalid"),
            ConfigError::SameTopic => f.write_str("OTA command and status topics must differ"),
        }
    }
}

impl core::error::Error for ConfigError {}

fn stack_floor(thread: &str) -> usize {
    if thread == "worker" {
        MIN_WORKER_STACK_BYTES
    } else {
        MIN_STACK_BYTES
    }
}

fn topic_ok(topic: &str) -> bool {
    !topic.is_empty() && topic.len() <= MAX_TOPIC_BYTES && !topic.contains(['+', '#', '\0'])
}

/// The platform-neutral part of the runtime configuration.
///
/// The ESP-IDF tier wraps it with the restart callback in `OtaConfig`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OtaSettings {
    /// The topic the runtime subscribes to for commands; no wildcard.
    pub command_topic: String,
    /// The topic statuses are published to; no wildcard.
    pub status_topic: String,
    /// The version of the running firmware.
    pub running_version: Version,
    /// Times and counts.
    pub timings: OtaTimings,
    /// Thread stack sizes.
    pub stacks: OtaStacks,
}

impl OtaSettings {
    /// Settings with default timings and stacks.
    ///
    /// # Errors
    ///
    /// Any [`ConfigError`] of [`OtaSettings::validate`].
    pub fn new(
        command_topic: impl Into<String>,
        status_topic: impl Into<String>,
        running_version: Version,
    ) -> Result<Self, ConfigError> {
        let settings = Self {
            command_topic: command_topic.into(),
            status_topic: status_topic.into(),
            running_version,
            timings: OtaTimings::default(),
            stacks: OtaStacks::default(),
        };
        settings.validate()?;
        Ok(settings)
    }

    /// Checks topics, stacks, durations and counts.
    ///
    /// Commands must never be published retained: a retained command would run again on every reconnect and boot.
    /// That is a publisher rule the runtime cannot check.
    ///
    /// # Errors
    ///
    /// The first [`ConfigError`] found: topics, then stacks, then durations, then counts, then maxima.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !topic_ok(&self.command_topic) {
            return Err(ConfigError::BadTopic("command"));
        }
        if !topic_ok(&self.status_topic) {
            return Err(ConfigError::BadTopic("status"));
        }
        if self.command_topic == self.status_topic {
            return Err(ConfigError::SameTopic);
        }
        for (thread, bytes) in [
            ("worker", self.stacks.worker_bytes),
            ("reporter", self.stacks.reporter_bytes),
        ] {
            if bytes < stack_floor(thread) {
                return Err(ConfigError::StackTooSmall { thread, bytes });
            }
        }
        let t = &self.timings;
        for (name, value) in [
            ("manifest_per_op", t.manifest_per_op),
            ("manifest_total", t.manifest_total),
            ("firmware_deadline", t.firmware_deadline),
            ("publish_ack_timeout", t.publish_ack_timeout),
            ("report_retry", t.report_retry),
            ("reporter_connect_poll", t.reporter_connect_poll),
            ("status_publish_delay", t.status_publish_delay),
            ("retained_poll", t.retained_poll),
            ("session_busy_delay", t.session_busy_delay),
            ("healthy_dwell", t.healthy_dwell),
            ("failure_deadline", t.failure_deadline),
            ("poll_interval", t.poll_interval),
            ("slot_retry", t.slot_retry),
            ("complete_retry", t.complete_retry),
            ("rollback_retry", t.rollback_retry),
        ] {
            if value.is_zero() {
                return Err(ConfigError::ZeroDuration(name));
            }
        }
        if t.firmware_timeout_secs == 0 {
            return Err(ConfigError::ZeroDuration("firmware_timeout_secs"));
        }
        for (name, value) in [
            ("reject_queue_depth", t.reject_queue_depth as u64),
            ("slot_min_attempts", u64::from(t.slot_min_attempts)),
            ("slot_warn_every", u64::from(t.slot_warn_every)),
            ("complete_attempts", u64::from(t.complete_attempts)),
            ("rollback_attempts", u64::from(t.rollback_attempts)),
            ("repair_max_iterations", u64::from(t.repair_max_iterations)),
        ] {
            if value == 0 {
                return Err(ConfigError::ZeroCount(name));
            }
        }
        if t.reject_queue_depth > MAX_REJECT_QUEUE_DEPTH {
            return Err(ConfigError::TooLarge("reject_queue_depth"));
        }
        if t.repair_max_iterations > MAX_REPAIR_ITERATIONS {
            return Err(ConfigError::TooLarge("repair_max_iterations"));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok() -> OtaSettings {
        OtaSettings::new("dev/ota/command", "dev/ota/status", Version::new(1, 0, 0)).unwrap()
    }

    #[test]
    fn defaults_are_the_documented_values() {
        let s = ok();
        let t = s.timings;
        assert_eq!(t.manifest_per_op, Duration::from_secs(5));
        assert_eq!(t.manifest_total, Duration::from_secs(10));
        assert_eq!(t.firmware_timeout_secs, 60);
        assert_eq!(t.firmware_deadline, Duration::from_secs(300));
        assert_eq!(t.publish_ack_timeout, Duration::from_secs(5));
        assert_eq!(t.report_retry, Duration::from_secs(10));
        assert_eq!(t.status_publish_retries, 5);
        assert_eq!(t.status_publish_delay, Duration::from_millis(200));
        assert_eq!(t.restart_grace, Duration::from_millis(1000));
        assert_eq!(t.retained_poll, Duration::from_secs(1));
        assert_eq!(t.reject_queue_depth, 4);
        assert_eq!(t.healthy_dwell, Duration::from_secs(30));
        assert_eq!(t.failure_deadline, Duration::from_secs(120));
        assert_eq!(t.poll_interval, Duration::from_secs(1));
        assert_eq!(t.slot_retry, Duration::from_millis(200));
        assert_eq!((t.slot_min_attempts, t.slot_warn_every), (5, 25));
        assert_eq!(
            (t.complete_attempts, t.complete_retry),
            (3, Duration::from_secs(1))
        );
        assert_eq!(
            (t.rollback_attempts, t.rollback_retry),
            (3, Duration::from_secs(5))
        );
        assert_eq!(t.repair_max_iterations, 32);
        assert_eq!(t.deadline_action, DeadlineAction::Rollback);
        assert_eq!(s.stacks.worker_bytes, 16384);
        assert_eq!(s.stacks.reporter_bytes, 8192);
    }

    #[test]
    fn topics_are_validated() {
        let v = Version::new(1, 0, 0);
        for bad in ["", "a/+/c", "a/#", "a\0b"] {
            assert_eq!(
                OtaSettings::new(bad, "s", v),
                Err(ConfigError::BadTopic("command")),
                "{bad:?}"
            );
            assert_eq!(
                OtaSettings::new("c", bad, v),
                Err(ConfigError::BadTopic("status")),
                "{bad:?}"
            );
        }
        let long = "a".repeat(MAX_TOPIC_BYTES + 1);
        assert_eq!(
            OtaSettings::new(long, "s", v),
            Err(ConfigError::BadTopic("command"))
        );
        assert_eq!(OtaSettings::new("t", "t", v), Err(ConfigError::SameTopic));
        assert!(OtaSettings::new("rustyfarian/watchtower/dev-1/ota/command", "s", v).is_ok());
    }

    #[test]
    fn the_stack_floors_are_4096_reporter_and_12288_worker() {
        let mut s = ok();
        s.stacks.worker_bytes = 12288;
        s.stacks.reporter_bytes = 4096;
        assert_eq!(s.validate(), Ok(()));
        s.stacks.reporter_bytes = 4095;
        assert_eq!(
            s.validate(),
            Err(ConfigError::StackTooSmall {
                thread: "reporter",
                bytes: 4095
            })
        );
        s.stacks.reporter_bytes = 4096;
        s.stacks.worker_bytes = 12287;
        assert_eq!(
            s.validate(),
            Err(ConfigError::StackTooSmall {
                thread: "worker",
                bytes: 12287
            })
        );
    }

    #[test]
    fn zero_durations_and_counts_are_refused() {
        let mut s = ok();
        s.timings.manifest_total = Duration::ZERO;
        assert_eq!(
            s.validate(),
            Err(ConfigError::ZeroDuration("manifest_total"))
        );
        let mut s = ok();
        s.timings.firmware_timeout_secs = 0;
        assert_eq!(
            s.validate(),
            Err(ConfigError::ZeroDuration("firmware_timeout_secs"))
        );
        let mut s = ok();
        s.timings.rollback_attempts = 0;
        assert_eq!(
            s.validate(),
            Err(ConfigError::ZeroCount("rollback_attempts"))
        );
        let mut s = ok();
        s.timings.reject_queue_depth = 0;
        assert_eq!(
            s.validate(),
            Err(ConfigError::ZeroCount("reject_queue_depth"))
        );
    }

    #[test]
    fn counts_have_documented_maxima() {
        let mut s = ok();
        s.timings.reject_queue_depth = MAX_REJECT_QUEUE_DEPTH;
        s.timings.repair_max_iterations = MAX_REPAIR_ITERATIONS;
        assert_eq!(s.validate(), Ok(()));
        s.timings.reject_queue_depth = MAX_REJECT_QUEUE_DEPTH + 1;
        assert_eq!(
            s.validate(),
            Err(ConfigError::TooLarge("reject_queue_depth"))
        );
        s.timings.reject_queue_depth = usize::MAX;
        assert_eq!(
            s.validate(),
            Err(ConfigError::TooLarge("reject_queue_depth"))
        );
        s.timings.reject_queue_depth = 1;
        s.timings.repair_max_iterations = MAX_REPAIR_ITERATIONS + 1;
        assert_eq!(
            s.validate(),
            Err(ConfigError::TooLarge("repair_max_iterations"))
        );
        s.timings.repair_max_iterations = u32::MAX;
        assert_eq!(
            s.validate(),
            Err(ConfigError::TooLarge("repair_max_iterations"))
        );
        assert!(alloc::format!("{}", ConfigError::TooLarge("x")).contains('x'));
    }

    #[test]
    fn a_zero_restart_grace_and_zero_retries_are_allowed() {
        let mut s = ok();
        s.timings.restart_grace = Duration::ZERO;
        s.timings.status_publish_retries = 0;
        s.timings.session_busy_retries = 0;
        assert_eq!(s.validate(), Ok(()));
    }

    #[test]
    fn the_unhealthy_demo_timings_validate() {
        let mut s = ok();
        s.timings.failure_deadline = Duration::from_secs(15);
        s.timings.deadline_action = DeadlineAction::NoteAndRestart(RollbackReason::Unhealthy);
        assert_eq!(s.validate(), Ok(()));
    }

    #[test]
    fn errors_name_the_field_and_carry_no_topic_text() {
        let shown = alloc::format!("{}", ConfigError::BadTopic("command"));
        assert!(shown.contains("command"));
        assert!(
            alloc::format!("{}", ConfigError::ZeroCount("slot_min_attempts"))
                .contains("slot_min_attempts")
        );
    }
}
