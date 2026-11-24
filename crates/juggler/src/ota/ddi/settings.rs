//! The settings the ESP-IDF DDI runtime is built from: stack sizes, timings and the identity of the firmware, validated before any thread starts.
//!
//! It is the DDI twin of the MQTT runtime's `OtaSettings`, without anything MQTT-shaped: the server, the controller id and the token belong to the tier's transport configuration, and the restart callback is the tier's.
//! The wall-clock rule of the runtime ([`WALL_CLOCK_FLOOR_SECS`], [`clock_is_set`]) lives here so the host tests pin it.

use alloc::format;
use alloc::string::String;
use core::fmt;
use core::time::Duration;

use super::endpoint::ControllerId;
use super::wire::Board;
use crate::ota::health::HealthConfig;
use crate::ota::Version;

/// Experimental: API may change before 1.0.
///
/// The smallest accepted stack of either DDI runtime thread, in bytes; the same floor as the MQTT runtime's worker (`MIN_WORKER_STACK_BYTES`).
///
/// It covers the 4 KiB download buffer, the HTTP client, SHA-256 and logging frames.
pub const DDI_MIN_STACK_BYTES: usize = 12288;

#[cfg(all(feature = "ota-wire", feature = "ota-ddi"))]
#[allow(deprecated)]
const _: () = assert!(DDI_MIN_STACK_BYTES == crate::ota::runtime::MIN_WORKER_STACK_BYTES);

/// Experimental: API may change before 1.0.
///
/// Seconds since the Unix epoch of 2024-01-01T00:00:00Z; a wall clock at or after it counts as set.
///
/// ESP-IDF boots at 1970 until SNTP sets the clock, so any reading below this floor is not a date.
/// The TLS clock gate (S2) replaces this rule.
pub const WALL_CLOCK_FLOOR_SECS: u64 = 1_704_067_200;

/// Experimental: API may change before 1.0.
///
/// Whether a wall clock reading counts as set: at or after [`WALL_CLOCK_FLOOR_SECS`].
///
/// ```
/// use juggler::ota::ddi::{clock_is_set, WALL_CLOCK_FLOOR_SECS};
///
/// assert!(!clock_is_set(0));
/// assert!(clock_is_set(WALL_CLOCK_FLOOR_SECS));
/// ```
pub const fn clock_is_set(secs_since_epoch: u64) -> bool {
    secs_since_epoch >= WALL_CLOCK_FLOOR_SECS
}

/// Experimental: API may change before 1.0.
///
/// The exact MQTT topic of the poll-now nudge for `controller_id`: `rustyfarian/ota/<controller-id>/nudge` (fleet DDI profile, section 6).
///
/// The app subscribes to it with its MQTT builder on every connect.
/// A controller id matches `[a-z0-9-]`, so the topic needs no further MQTT validation.
///
/// ```
/// use juggler::ota::ddi::{nudge_topic, ControllerId};
///
/// let id = ControllerId::parse("dev-1").unwrap();
/// assert_eq!(nudge_topic(&id), "rustyfarian/ota/dev-1/nudge");
/// ```
pub fn nudge_topic(controller_id: &ControllerId) -> String {
    format!("rustyfarian/ota/{controller_id}/nudge")
}

/// Experimental: API may change before 1.0.
///
/// Stack sizes of the DDI runtime threads, in bytes; provisional until measured under TLS (S4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DdiStacks {
    /// The `ota-worker` thread: polls, admission, feedback and the cancel path.
    pub worker_bytes: usize,
    /// The `ota-download` thread of one attempt: the streaming download, verification and flash.
    pub download_bytes: usize,
}

impl Default for DdiStacks {
    /// 16 KiB for each thread, provisional until the high-water marks are measured under TLS.
    fn default() -> Self {
        Self {
            worker_bytes: 16384,
            download_bytes: 16384,
        }
    }
}

/// Experimental: API may change before 1.0.
///
/// Every time and count the DDI runtime uses; all fields are public so an app (or a demo build) can override single values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DdiTimings {
    /// Per network operation timeout of the firmware download, in seconds.
    pub firmware_timeout_secs: u64,
    /// Total time limit of one firmware download (the cooperative deadline of the OTA session).
    pub firmware_deadline: Duration,
    /// Pause between the pre-reboot `proceeding` feedback and the restart callback, so the request can leave the device.
    pub restart_grace: Duration,
    /// Extra attempts to read the hardware facts while another OTA handle is busy.
    pub session_busy_retries: u32,
    /// Pause between those attempts.
    pub session_busy_delay: Duration,
    /// The health policy of the running image.
    pub health: HealthConfig,
}

impl Default for DdiTimings {
    /// 60 s per network operation, 300 s per download, 1 s restart grace, 3 busy retries 500 ms apart and the [`HealthConfig`] defaults.
    fn default() -> Self {
        Self {
            firmware_timeout_secs: 60,
            firmware_deadline: Duration::from_secs(300),
            restart_grace: Duration::from_secs(1),
            session_busy_retries: 3,
            session_busy_delay: Duration::from_millis(500),
            health: HealthConfig::default(),
        }
    }
}

/// Experimental: API may change before 1.0.
///
/// Why the settings were refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DdiSettingsError {
    /// The board does not match `[a-z0-9._-]{1,32}`.
    InvalidBoard,
    /// A stack is below [`DDI_MIN_STACK_BYTES`].
    StackTooSmall {
        /// The thread whose stack is too small (`worker` or `download`).
        thread: &'static str,
        /// The configured size.
        bytes: usize,
    },
    /// A duration that must not be zero is zero; carries the field name.
    ZeroDuration(&'static str),
    /// A count that must not be zero is zero; carries the field name.
    ZeroCount(&'static str),
}

impl fmt::Display for DdiSettingsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DdiSettingsError::InvalidBoard => f.write_str("DDI board name invalid"),
            DdiSettingsError::StackTooSmall { thread, bytes } => write!(
                f,
                "DDI {thread} stack of {bytes} bytes is below {DDI_MIN_STACK_BYTES}"
            ),
            DdiSettingsError::ZeroDuration(name) => {
                write!(f, "DDI timing `{name}` must not be zero")
            }
            DdiSettingsError::ZeroCount(name) => {
                write!(f, "DDI setting `{name}` must not be zero")
            }
        }
    }
}

impl core::error::Error for DdiSettingsError {}

/// Experimental: API may change before 1.0.
///
/// The platform-neutral part of the DDI runtime configuration.
///
/// The ESP-IDF tier adds the transport configuration and the restart callback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DdiSettings {
    /// The version of the running firmware.
    pub running_version: Version,
    /// The lowest version the device accepts; an offer below it is refused (fleet DDI profile, `below_floor`).
    pub floor: Option<Version>,
    /// The board name the configData reports.
    pub board: Board,
    /// Times and counts.
    pub timings: DdiTimings,
    /// Thread stack sizes.
    pub stacks: DdiStacks,
}

impl DdiSettings {
    /// Experimental: API may change before 1.0.
    ///
    /// Settings with default timings and stacks and no floor.
    ///
    /// `board` must match `[a-z0-9._-]{1,32}`; it is fixed when the image is built and never provisioned.
    ///
    /// # Errors
    ///
    /// [`DdiSettingsError::InvalidBoard`] when `board` does not match.
    ///
    /// ```
    /// use juggler::ota::ddi::{DdiSettings, DdiSettingsError};
    /// use juggler::ota::Version;
    ///
    /// assert!(DdiSettings::new(Version::new(1, 0, 0), "c3-supermini").is_ok());
    /// assert_eq!(
    ///     DdiSettings::new(Version::new(1, 0, 0), "C3"),
    ///     Err(DdiSettingsError::InvalidBoard)
    /// );
    /// ```
    pub fn new(running_version: Version, board: &str) -> Result<Self, DdiSettingsError> {
        let board = Board::parse(board).map_err(|_| DdiSettingsError::InvalidBoard)?;
        let settings = Self {
            running_version,
            floor: None,
            board,
            timings: DdiTimings::default(),
            stacks: DdiStacks::default(),
        };
        settings.validate()?;
        Ok(settings)
    }

    /// Experimental: API may change before 1.0.
    ///
    /// Checks the stacks, then the durations, then the counts (the fields are public, so call it again after overriding one).
    ///
    /// # Errors
    ///
    /// The first [`DdiSettingsError`] found: a stack below [`DDI_MIN_STACK_BYTES`] (thread `worker`, then `download`), a zero duration, a zero count.
    pub fn validate(&self) -> Result<(), DdiSettingsError> {
        for (thread, bytes) in [
            ("worker", self.stacks.worker_bytes),
            ("download", self.stacks.download_bytes),
        ] {
            if bytes < DDI_MIN_STACK_BYTES {
                return Err(DdiSettingsError::StackTooSmall { thread, bytes });
            }
        }
        let t = &self.timings;
        let h = &t.health;
        for (name, value) in [
            ("firmware_deadline", t.firmware_deadline),
            ("restart_grace", t.restart_grace),
            ("session_busy_delay", t.session_busy_delay),
            ("healthy_dwell", h.healthy_dwell),
            ("failure_deadline", h.failure_deadline),
            ("poll_interval", h.poll_interval),
            ("slot_retry", h.slot_retry),
            ("complete_retry", h.complete_retry),
            ("rollback_retry", h.rollback_retry),
        ] {
            if value.is_zero() {
                return Err(DdiSettingsError::ZeroDuration(name));
            }
        }
        if t.firmware_timeout_secs == 0 {
            return Err(DdiSettingsError::ZeroDuration("firmware_timeout_secs"));
        }
        for (name, value) in [
            ("slot_min_attempts", h.slot_min_attempts),
            ("slot_warn_every", h.slot_warn_every),
            ("complete_attempts", h.complete_attempts),
            ("rollback_attempts", h.rollback_attempts),
        ] {
            if value == 0 {
                return Err(DdiSettingsError::ZeroCount(name));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::ToString;

    fn ok() -> DdiSettings {
        DdiSettings::new(Version::new(1, 2, 3), "c3-supermini").unwrap()
    }

    #[test]
    fn defaults_are_the_documented_values() {
        let s = ok();
        assert_eq!(s.running_version, Version::new(1, 2, 3));
        assert_eq!(s.floor, None);
        assert_eq!(s.board.as_str(), "c3-supermini");
        assert_eq!(s.stacks.worker_bytes, 16384);
        assert_eq!(s.stacks.download_bytes, 16384);
        let t = s.timings;
        assert_eq!(t.firmware_timeout_secs, 60);
        assert_eq!(t.firmware_deadline, Duration::from_secs(300));
        assert_eq!(t.restart_grace, Duration::from_secs(1));
        assert_eq!(t.session_busy_retries, 3);
        assert_eq!(t.session_busy_delay, Duration::from_millis(500));
        assert_eq!(t.health, HealthConfig::default());
        assert_eq!(DDI_MIN_STACK_BYTES, 12288);
    }

    #[test]
    fn a_board_outside_the_grammar_is_refused() {
        assert_eq!(
            DdiSettings::new(Version::new(1, 0, 0), "C3"),
            Err(DdiSettingsError::InvalidBoard)
        );
        assert_eq!(
            DdiSettings::new(Version::new(1, 0, 0), ""),
            Err(DdiSettingsError::InvalidBoard)
        );
    }

    #[test]
    fn a_stack_below_the_floor_is_refused_with_its_thread() {
        let mut s = ok();
        s.stacks.worker_bytes = DDI_MIN_STACK_BYTES - 1;
        assert_eq!(
            s.validate(),
            Err(DdiSettingsError::StackTooSmall {
                thread: "worker",
                bytes: 12287
            })
        );
        let mut s = ok();
        s.stacks.download_bytes = DDI_MIN_STACK_BYTES - 1;
        assert_eq!(
            s.validate(),
            Err(DdiSettingsError::StackTooSmall {
                thread: "download",
                bytes: 12287
            })
        );
        let mut s = ok();
        s.stacks.worker_bytes = DDI_MIN_STACK_BYTES;
        s.stacks.download_bytes = DDI_MIN_STACK_BYTES;
        assert_eq!(s.validate(), Ok(()));
    }

    #[test]
    fn a_zero_duration_is_refused_with_its_name() {
        let mut s = ok();
        s.timings.restart_grace = Duration::ZERO;
        assert_eq!(
            s.validate(),
            Err(DdiSettingsError::ZeroDuration("restart_grace"))
        );
        let mut s = ok();
        s.timings.health.complete_retry = Duration::ZERO;
        assert_eq!(
            s.validate(),
            Err(DdiSettingsError::ZeroDuration("complete_retry"))
        );
        let mut s = ok();
        s.timings.firmware_timeout_secs = 0;
        assert_eq!(
            s.validate(),
            Err(DdiSettingsError::ZeroDuration("firmware_timeout_secs"))
        );
    }

    #[test]
    fn a_zero_count_is_refused_with_its_name() {
        let mut s = ok();
        s.timings.health.complete_attempts = 0;
        assert_eq!(
            s.validate(),
            Err(DdiSettingsError::ZeroCount("complete_attempts"))
        );
        let mut s = ok();
        s.timings.health.slot_min_attempts = 0;
        assert_eq!(
            s.validate(),
            Err(DdiSettingsError::ZeroCount("slot_min_attempts"))
        );
    }

    #[test]
    fn the_stack_check_comes_before_the_duration_check() {
        let mut s = ok();
        s.stacks.download_bytes = 0;
        s.timings.restart_grace = Duration::ZERO;
        assert!(matches!(
            s.validate(),
            Err(DdiSettingsError::StackTooSmall { .. })
        ));
    }

    #[test]
    fn the_wall_clock_counts_as_set_from_2024() {
        assert!(!clock_is_set(0));
        assert!(!clock_is_set(1_704_067_199));
        assert!(clock_is_set(1_704_067_200));
        assert!(clock_is_set(u64::MAX));
    }

    #[test]
    fn the_nudge_topic_is_the_profile_topic_of_the_controller() {
        assert_eq!(
            nudge_topic(&ControllerId::parse("dev-1").unwrap()),
            "rustyfarian/ota/dev-1/nudge"
        );
        assert_eq!(
            nudge_topic(&ControllerId::parse("c3-supermini-07").unwrap()),
            "rustyfarian/ota/c3-supermini-07/nudge"
        );
    }

    #[test]
    fn the_errors_name_what_is_wrong() {
        assert_eq!(
            DdiSettingsError::StackTooSmall {
                thread: "download",
                bytes: 100
            }
            .to_string(),
            "DDI download stack of 100 bytes is below 12288"
        );
        assert_eq!(
            DdiSettingsError::ZeroCount("slot_warn_every").to_string(),
            "DDI setting `slot_warn_every` must not be zero"
        );
    }
}
