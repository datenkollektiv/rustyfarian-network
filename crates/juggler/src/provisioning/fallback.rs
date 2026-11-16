//! Portal fallback after repeated failed Wi-Fi joins: the pure counter policy
//! (see `docs/features/wifi-portal-fallback-v1.md`, parts 2 and 3).
//!
//! Two counters persist across boots, one per [`JoinFailure`] class that can
//! open the portal: credential failures and network-absent boots.
//! A transient boot changes neither.
//! The policy works on plain [`FailureCounts`], so it is storage-agnostic; the
//! `rf_prov` keys that hold the counts belong to `KvProvisioningStore`
//! (`std` feature).
//!
//! Clear rules: the counts return to [`FailureCounts::ZERO`] on a successful
//! join (the boot helper), on a commit (`KvProvisioningStore::save` does it), and
//! when the fallback portal opens ([`FallbackPolicy::record_failed_boot`] hands
//! back zero counts with the decision).
//! Counting never erases the stored record, and the library never restarts: the
//! consumer acts on [`FallbackDecision::FallbackPortal`].
//!
//! A tier that must restart before it can open the portal (the radio is still
//! held by the station) uses [`JoinRecorder`] instead: it keeps the counts at
//! the threshold, so the next boot's load decides with
//! [`FallbackPolicy::decide`], and the fallback portal clears them when it opens.
//!
//! A boot that ends without a join records its class once:
//!
//! ```
//! use juggler::provisioning::{FailureCounts, FallbackDecision, FallbackPolicy, FallbackReason};
//! use juggler::wifi::JoinFailure;
//!
//! let policy = FallbackPolicy::new();
//! let mut stored = FailureCounts::ZERO;
//! for _ in 0..4 {
//!     let step = policy.record_failed_boot(stored, JoinFailure::Credentials);
//!     assert_eq!(step.decision, FallbackDecision::CarryOn);
//!     stored = step.counts;
//! }
//! let step = policy.record_failed_boot(stored, JoinFailure::Credentials);
//! assert_eq!(
//!     step.decision,
//!     FallbackDecision::FallbackPortal { reason: FallbackReason::Credentials, failures: 5 }
//! );
//! assert_eq!(step.counts, FailureCounts::ZERO);
//! ```

use core::time::Duration;

use crate::wifi::JoinFailure;

/// Experimental: API may change before 1.0.
///
/// Default credential-failure threshold: the fallback portal opens after this
/// many failed boots classified [`JoinFailure::Credentials`].
pub const CREDENTIAL_THRESHOLD_DEFAULT: u8 = 5;

/// Experimental: API may change before 1.0.
///
/// Default network-absent threshold: the fallback portal opens after this many
/// failed boots classified [`JoinFailure::NetworkAbsent`].
///
/// It counts boots, not time: a boot counts once, and a join clears the count.
/// Whether a router switched off overnight leaves the access point closed
/// depends on the consumer's restart cadence; a consumer that restarts after
/// every 30 s join window reaches 20 in about ten minutes.
pub const NETWORK_ABSENT_THRESHOLD_DEFAULT: u8 = 20;

/// Experimental: API may change before 1.0.
///
/// Default window of the fallback portal: with no submission within it the
/// portal closes and the stored config is retried, so a router that was merely
/// switched off comes back without anyone touching the device.
pub const FALLBACK_PORTAL_WINDOW_DEFAULT: Duration = Duration::from_secs(10 * 60);

/// Experimental: API may change before 1.0.
///
/// The persisted failure counters, one per class that can open the portal.
///
/// Each counts failed boots since the last clear; a boot of the other class or
/// a transient boot does not reset it, so "consecutive" means "with no
/// successful join, commit or fallback portal in between".
/// Counts saturate at `u8::MAX`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct FailureCounts {
    /// Failed boots classified [`JoinFailure::Credentials`].
    pub credentials: u8,
    /// Failed boots classified [`JoinFailure::NetworkAbsent`].
    pub network_absent: u8,
}

impl FailureCounts {
    /// Both counters cleared; also what a store without the counter keys
    /// (a 0.6.x record) reads as.
    pub const ZERO: Self = Self {
        credentials: 0,
        network_absent: 0,
    };

    /// `true` when both counters are zero.
    pub const fn is_zero(&self) -> bool {
        self.credentials == 0 && self.network_absent == 0
    }

    /// The counts after one more failed boot of `class`: the matching counter
    /// grows by one (saturating), a transient boot changes nothing.
    pub const fn after_failed_boot(self, class: JoinFailure) -> Self {
        match class {
            JoinFailure::Credentials => Self {
                credentials: self.credentials.saturating_add(1),
                ..self
            },
            JoinFailure::NetworkAbsent => Self {
                network_absent: self.network_absent.saturating_add(1),
                ..self
            },
            JoinFailure::Transient => self,
        }
    }
}

/// Experimental: API may change before 1.0.
///
/// Why the fallback portal opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FallbackReason {
    /// The credential counter reached its threshold.
    Credentials,
    /// The network-absent counter reached its threshold.
    NetworkAbsent,
}

impl FallbackReason {
    /// A stable lower-case name for log lines: `credentials` or
    /// `network_absent` (the [`JoinFailure::as_str`] spelling).
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Credentials => JoinFailure::Credentials.as_str(),
            Self::NetworkAbsent => JoinFailure::NetworkAbsent.as_str(),
        }
    }
}

/// Experimental: API may change before 1.0.
///
/// What the boot helper does with the current counts.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum FallbackDecision {
    /// Below every threshold: keep the stored config, back off and retry.
    CarryOn,
    /// A counter reached its threshold: hand the consumer the fallback portal.
    FallbackPortal {
        /// The counter that reached its threshold.
        reason: FallbackReason,
        /// Its value, the failed boots that led here.
        failures: u8,
    },
}

/// Experimental: API may change before 1.0.
///
/// The result of [`FallbackPolicy::record_failed_boot`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FallbackStep {
    /// The counts to persist: the incremented counts, or [`FailureCounts::ZERO`]
    /// when the decision opens the fallback portal.
    pub counts: FailureCounts,
    /// What the boot helper does next.
    pub decision: FallbackDecision,
}

/// Experimental: API may change before 1.0.
///
/// The thresholds of the portal fallback; `0` disables a counter's fallback.
///
/// Defaults: [`CREDENTIAL_THRESHOLD_DEFAULT`] and
/// [`NETWORK_ABSENT_THRESHOLD_DEFAULT`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FallbackPolicy {
    credential_threshold: u8,
    network_absent_threshold: u8,
}

impl Default for FallbackPolicy {
    fn default() -> Self {
        Self::new()
    }
}

impl FallbackPolicy {
    /// The default thresholds.
    pub const fn new() -> Self {
        Self {
            credential_threshold: CREDENTIAL_THRESHOLD_DEFAULT,
            network_absent_threshold: NETWORK_ABSENT_THRESHOLD_DEFAULT,
        }
    }

    /// Sets the credential-failure threshold; `0` means never.
    pub const fn with_credential_threshold(mut self, threshold: u8) -> Self {
        self.credential_threshold = threshold;
        self
    }

    /// Sets the network-absent threshold; `0` means never (a credential-only
    /// fallback).
    pub const fn with_network_absent_threshold(mut self, threshold: u8) -> Self {
        self.network_absent_threshold = threshold;
        self
    }

    /// The credential-failure threshold; `0` means never.
    pub const fn credential_threshold(&self) -> u8 {
        self.credential_threshold
    }

    /// The network-absent threshold; `0` means never.
    pub const fn network_absent_threshold(&self) -> u8 {
        self.network_absent_threshold
    }

    /// The decision for stored `counts`, without recording a boot.
    ///
    /// A counter at or above its non-zero threshold opens the fallback portal;
    /// when both are, the credential counter wins, because new Wi-Fi fields fix
    /// both a wrong passphrase and a renamed SSID.
    pub const fn decide(&self, counts: FailureCounts) -> FallbackDecision {
        if reached(counts.credentials, self.credential_threshold) {
            FallbackDecision::FallbackPortal {
                reason: FallbackReason::Credentials,
                failures: counts.credentials,
            }
        } else if reached(counts.network_absent, self.network_absent_threshold) {
            FallbackDecision::FallbackPortal {
                reason: FallbackReason::NetworkAbsent,
                failures: counts.network_absent,
            }
        } else {
            FallbackDecision::CarryOn
        }
    }

    /// Records one failed boot of `class` on top of the stored `counts` and
    /// decides.
    ///
    /// The returned [`FallbackStep::counts`] are what the boot helper persists:
    /// zero when the fallback portal opens (so a portal that times out retries
    /// the stored config afresh), the incremented counts otherwise.
    pub const fn record_failed_boot(
        &self,
        counts: FailureCounts,
        class: JoinFailure,
    ) -> FallbackStep {
        let counts = counts.after_failed_boot(class);
        let decision = self.decide(counts);
        let counts = match decision {
            FallbackDecision::FallbackPortal { .. } => FailureCounts::ZERO,
            FallbackDecision::CarryOn => counts,
        };
        FallbackStep { counts, decision }
    }
}

/// Experimental: API may change before 1.0.
///
/// One boot's bookkeeping of the failure counters, for a boot helper that
/// loads the config before it connects.
///
/// Built from the counts read at load; the helper then reports the join
/// result.
/// Each call returns the counts to persist, or `None` when the stored counts
/// are already right, so a healthy boot with zero counts writes nothing.
/// A boot counts at most once, however often its join window expires; a
/// boot counted as network absent moves to credentials when a later window
/// of the same boot shows credential failures.
///
/// Unlike [`FallbackPolicy::record_failed_boot`], a failed boot that reaches a
/// threshold keeps its counts: the station still holds the radio, so the
/// consumer restarts, the next boot's load sees the counts and decides with
/// [`FallbackPolicy::decide`], and the fallback portal clears them when it opens.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JoinRecorder {
    policy: FallbackPolicy,
    stored: FailureCounts,
    counted: Option<JoinFailure>,
}

impl JoinRecorder {
    /// A recorder for one boot, from the policy and the counts read at load.
    pub const fn new(policy: FallbackPolicy, stored: FailureCounts) -> Self {
        Self {
            policy,
            stored,
            counted: None,
        }
    }

    /// The policy this boot decides with.
    pub const fn policy(&self) -> FallbackPolicy {
        self.policy
    }

    /// The counts as persisted after the last call.
    pub const fn stored(&self) -> FailureCounts {
        self.stored
    }

    /// The station joined: returns [`FailureCounts::ZERO`] to persist when
    /// any count was set, `None` otherwise.
    pub fn joined(&mut self) -> Option<FailureCounts> {
        if self.stored.is_zero() {
            return None;
        }
        self.stored = FailureCounts::ZERO;
        Some(self.stored)
    }

    /// The join window expired with no join, the boot's attempts classified
    /// `class`.
    ///
    /// Returns the counts to persist (`None` for a transient boot, or when
    /// this boot was already counted) and the decision for the stored counts:
    /// [`FallbackDecision::FallbackPortal`] tells the consumer to restart, so
    /// the next boot opens the fallback portal.
    ///
    /// A transient window does not use up the boot's count, so credential or
    /// network-absent evidence from a later window of the same boot still
    /// counts; credentials found after a network-absent count move that count
    /// to credentials, as they win within one window.
    pub fn not_joined(&mut self, class: JoinFailure) -> (Option<FailureCounts>, FallbackDecision) {
        let mut write = None;
        let counts = match (self.counted, class) {
            (_, JoinFailure::Transient) => None,
            (None, _) => Some(self.stored.after_failed_boot(class)),
            (Some(JoinFailure::NetworkAbsent), JoinFailure::Credentials) => Some(FailureCounts {
                network_absent: self.stored.network_absent.saturating_sub(1),
                ..self.stored.after_failed_boot(class)
            }),
            _ => None,
        };
        if let Some(counts) = counts {
            self.counted = Some(class);
            if counts != self.stored {
                self.stored = counts;
                write = Some(counts);
            }
        }
        (write, self.policy.decide(self.stored))
    }
}

/// `true` when `count` reached a non-zero `threshold`.
const fn reached(count: u8, threshold: u8) -> bool {
    threshold != 0 && count >= threshold
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Runs `boots` failed boots of `class` from `start`, persisting each
    /// step's counts as the boot helper would; returns the boot index (1-based)
    /// and decision of the first fallback, if any, and the final counts.
    fn run_boots(
        policy: FallbackPolicy,
        start: FailureCounts,
        class: JoinFailure,
        boots: u32,
    ) -> (Option<(u32, FallbackDecision)>, FailureCounts) {
        let mut stored = start;
        for boot in 1..=boots {
            let step = policy.record_failed_boot(stored, class);
            stored = step.counts;
            if step.decision != FallbackDecision::CarryOn {
                return (Some((boot, step.decision)), stored);
            }
        }
        (None, stored)
    }

    #[test]
    fn defaults() {
        let policy = FallbackPolicy::default();
        assert_eq!(policy, FallbackPolicy::new());
        assert_eq!(policy.credential_threshold(), 5);
        assert_eq!(policy.network_absent_threshold(), 20);
        assert_eq!(FailureCounts::default(), FailureCounts::ZERO);
        assert!(FailureCounts::ZERO.is_zero());
    }

    #[test]
    fn credentials_open_the_portal_on_the_fifth_boot() {
        let (fallback, stored) = run_boots(
            FallbackPolicy::new(),
            FailureCounts::ZERO,
            JoinFailure::Credentials,
            10,
        );
        assert_eq!(
            fallback,
            Some((
                5,
                FallbackDecision::FallbackPortal {
                    reason: FallbackReason::Credentials,
                    failures: 5
                }
            ))
        );
        assert_eq!(stored, FailureCounts::ZERO, "cleared when the portal opens");
    }

    #[test]
    fn network_absent_opens_the_portal_on_the_twentieth_boot() {
        let (fallback, stored) = run_boots(
            FallbackPolicy::new(),
            FailureCounts::ZERO,
            JoinFailure::NetworkAbsent,
            30,
        );
        assert_eq!(
            fallback,
            Some((
                20,
                FallbackDecision::FallbackPortal {
                    reason: FallbackReason::NetworkAbsent,
                    failures: 20
                }
            ))
        );
        assert!(stored.is_zero());
    }

    #[test]
    fn thresholds_are_configurable() {
        let policy = FallbackPolicy::new()
            .with_credential_threshold(2)
            .with_network_absent_threshold(3);
        let (cred, _) = run_boots(policy, FailureCounts::ZERO, JoinFailure::Credentials, 9);
        assert_eq!(cred.map(|(boot, _)| boot), Some(2));
        let (noap, _) = run_boots(policy, FailureCounts::ZERO, JoinFailure::NetworkAbsent, 9);
        assert_eq!(noap.map(|(boot, _)| boot), Some(3));

        let one = FallbackPolicy::new().with_credential_threshold(1);
        let (first, _) = run_boots(one, FailureCounts::ZERO, JoinFailure::Credentials, 1);
        assert_eq!(first.map(|(boot, _)| boot), Some(1));
    }

    #[test]
    fn zero_threshold_means_never() {
        let policy = FallbackPolicy::new()
            .with_credential_threshold(0)
            .with_network_absent_threshold(0);
        for class in [JoinFailure::Credentials, JoinFailure::NetworkAbsent] {
            let (fallback, stored) = run_boots(policy, FailureCounts::ZERO, class, 300);
            assert_eq!(fallback, None, "{class:?}");
            assert!(!stored.is_zero(), "the counter still counts");
        }
        let saturated = FailureCounts {
            credentials: u8::MAX,
            network_absent: u8::MAX,
        };
        assert_eq!(policy.decide(saturated), FallbackDecision::CarryOn);
    }

    #[test]
    fn credential_only_fallback_ignores_absent_networks() {
        let policy = FallbackPolicy::new().with_network_absent_threshold(0);
        let (noap, stored) = run_boots(policy, FailureCounts::ZERO, JoinFailure::NetworkAbsent, 50);
        assert_eq!(noap, None);
        assert_eq!(stored.network_absent, 50);
        let (cred, _) = run_boots(policy, stored, JoinFailure::Credentials, 9);
        assert_eq!(cred.map(|(boot, _)| boot), Some(5));
    }

    #[test]
    fn transient_boots_neither_count_nor_reset() {
        let policy = FallbackPolicy::new();
        let start = FailureCounts {
            credentials: 4,
            network_absent: 19,
        };
        let (fallback, stored) = run_boots(policy, start, JoinFailure::Transient, 100);
        assert_eq!(fallback, None);
        assert_eq!(stored, start);
        let step = policy.record_failed_boot(stored, JoinFailure::Credentials);
        assert_eq!(
            step.decision,
            FallbackDecision::FallbackPortal {
                reason: FallbackReason::Credentials,
                failures: 5
            }
        );
    }

    #[test]
    fn classes_count_independently() {
        let policy = FallbackPolicy::new();
        let mut stored = FailureCounts::ZERO;
        for class in [
            JoinFailure::Credentials,
            JoinFailure::NetworkAbsent,
            JoinFailure::Credentials,
            JoinFailure::Transient,
            JoinFailure::NetworkAbsent,
        ] {
            stored = policy.record_failed_boot(stored, class).counts;
        }
        assert_eq!(
            stored,
            FailureCounts {
                credentials: 2,
                network_absent: 2
            }
        );
    }

    #[test]
    fn credentials_win_when_both_reach_their_threshold() {
        let policy = FallbackPolicy::new();
        let counts = FailureCounts {
            credentials: 5,
            network_absent: 20,
        };
        assert_eq!(
            policy.decide(counts),
            FallbackDecision::FallbackPortal {
                reason: FallbackReason::Credentials,
                failures: 5
            }
        );
        let noap_only = FallbackPolicy::new().with_credential_threshold(0);
        assert_eq!(
            noap_only.decide(counts),
            FallbackDecision::FallbackPortal {
                reason: FallbackReason::NetworkAbsent,
                failures: 20
            }
        );
    }

    /// Counts persisted above a threshold (for example after the consumer
    /// lowered it) open the portal on the next decision, even on a transient
    /// boot.
    #[test]
    fn counts_above_the_threshold_open_the_portal() {
        let policy = FallbackPolicy::new().with_credential_threshold(3);
        let stored = FailureCounts {
            credentials: 4,
            network_absent: 0,
        };
        assert_eq!(
            policy.decide(stored),
            FallbackDecision::FallbackPortal {
                reason: FallbackReason::Credentials,
                failures: 4
            }
        );
        let step = policy.record_failed_boot(stored, JoinFailure::Transient);
        assert!(step.counts.is_zero());
        assert_ne!(step.decision, FallbackDecision::CarryOn);
    }

    /// A successful join clears the counts; the next failing streak starts over.
    #[test]
    fn join_clear_restarts_the_streak() {
        let policy = FallbackPolicy::new();
        let (_, stored) = run_boots(policy, FailureCounts::ZERO, JoinFailure::Credentials, 4);
        assert_eq!(stored.credentials, 4);
        let stored = FailureCounts::ZERO;
        let (fallback, _) = run_boots(policy, stored, JoinFailure::Credentials, 4);
        assert_eq!(fallback, None);
    }

    /// A fallback portal that times out without a commit leaves zero counts, so
    /// the stored config is retried for a full streak before the portal reopens.
    #[test]
    fn portal_timeout_retries_a_full_streak() {
        let policy = FallbackPolicy::new();
        let (first, stored) = run_boots(policy, FailureCounts::ZERO, JoinFailure::Credentials, 9);
        assert_eq!(first.map(|(boot, _)| boot), Some(5));
        let (second, _) = run_boots(policy, stored, JoinFailure::Credentials, 9);
        assert_eq!(second.map(|(boot, _)| boot), Some(5));
    }

    #[test]
    fn counters_saturate() {
        let max = FailureCounts {
            credentials: u8::MAX,
            network_absent: u8::MAX,
        };
        assert_eq!(max.after_failed_boot(JoinFailure::Credentials), max);
        assert_eq!(max.after_failed_boot(JoinFailure::NetworkAbsent), max);
    }

    #[test]
    fn reason_names_are_stable() {
        assert_eq!(FallbackReason::Credentials.as_str(), "credentials");
        assert_eq!(FallbackReason::NetworkAbsent.as_str(), "network_absent");
    }

    // ── JoinRecorder: load, connect, restart ────────────────────────────

    /// Simulates boots as a restart-before-portal tier runs them: decide at
    /// load, then report a failed join window; returns the 1-based boot whose
    /// load opens the fallback portal, with its decision, and the counts then.
    fn run_restarting_boots(
        policy: FallbackPolicy,
        class: JoinFailure,
        boots: u32,
    ) -> (Option<(u32, FallbackDecision)>, FailureCounts) {
        let mut persisted = FailureCounts::ZERO;
        for boot in 1..=boots {
            let at_load = policy.decide(persisted);
            if at_load != FallbackDecision::CarryOn {
                return (Some((boot, at_load)), persisted);
            }
            let mut recorder = JoinRecorder::new(policy, persisted);
            if let (Some(write), _) = recorder.not_joined(class) {
                persisted = write;
            }
        }
        (None, persisted)
    }

    #[test]
    fn restarting_boots_open_the_portal_after_five_credential_failures() {
        let (fallback, persisted) =
            run_restarting_boots(FallbackPolicy::new(), JoinFailure::Credentials, 10);
        assert_eq!(
            fallback,
            Some((
                6,
                FallbackDecision::FallbackPortal {
                    reason: FallbackReason::Credentials,
                    failures: 5
                }
            )),
            "five failed boots, the sixth load opens the portal"
        );
        assert_eq!(persisted.credentials, 5, "kept until the portal opens");
    }

    #[test]
    fn restarting_boots_never_open_the_portal_on_transient_failures() {
        let (fallback, persisted) =
            run_restarting_boots(FallbackPolicy::new(), JoinFailure::Transient, 50);
        assert_eq!(fallback, None);
        assert!(persisted.is_zero());
    }

    #[test]
    fn fifth_failed_window_reports_the_fallback() {
        let stored = FailureCounts {
            credentials: 4,
            network_absent: 0,
        };
        let mut recorder = JoinRecorder::new(FallbackPolicy::new(), stored);
        let (write, decision) = recorder.not_joined(JoinFailure::Credentials);
        assert_eq!(
            write,
            Some(FailureCounts {
                credentials: 5,
                network_absent: 0
            })
        );
        assert_eq!(
            decision,
            FallbackDecision::FallbackPortal {
                reason: FallbackReason::Credentials,
                failures: 5
            }
        );
        assert_eq!(recorder.stored().credentials, 5);
    }

    #[test]
    fn a_boot_counts_once() {
        let mut recorder = JoinRecorder::new(FallbackPolicy::new(), FailureCounts::ZERO);
        let (first, _) = recorder.not_joined(JoinFailure::Credentials);
        assert_eq!(first.map(|c| c.credentials), Some(1));
        let (second, decision) = recorder.not_joined(JoinFailure::Credentials);
        assert_eq!(second, None);
        assert_eq!(decision, FallbackDecision::CarryOn);
        assert_eq!(recorder.stored().credentials, 1);
    }

    #[test]
    fn a_healthy_boot_writes_nothing() {
        let mut recorder = JoinRecorder::new(FallbackPolicy::new(), FailureCounts::ZERO);
        assert_eq!(recorder.joined(), None);
        let (write, decision) = recorder.not_joined(JoinFailure::Transient);
        assert_eq!(write, None);
        assert_eq!(decision, FallbackDecision::CarryOn);
    }

    #[test]
    fn a_join_clears_the_counts_once() {
        let stored = FailureCounts {
            credentials: 3,
            network_absent: 7,
        };
        let mut recorder = JoinRecorder::new(FallbackPolicy::new(), stored);
        assert_eq!(recorder.joined(), Some(FailureCounts::ZERO));
        assert_eq!(recorder.joined(), None);

        let mut late = JoinRecorder::new(FallbackPolicy::new(), FailureCounts::ZERO);
        let _ = late.not_joined(JoinFailure::NetworkAbsent);
        assert_eq!(
            late.joined(),
            Some(FailureCounts::ZERO),
            "joined after the window"
        );
        assert!(late.stored().is_zero());
    }

    #[test]
    fn a_transient_window_leaves_the_boot_open_for_later_evidence() {
        let mut recorder = JoinRecorder::new(FallbackPolicy::new(), FailureCounts::ZERO);
        assert_eq!(
            recorder.not_joined(JoinFailure::Transient),
            (None, FallbackDecision::CarryOn)
        );
        let credentials = FailureCounts {
            credentials: 1,
            network_absent: 0,
        };
        assert_eq!(
            recorder.not_joined(JoinFailure::Credentials).0,
            Some(credentials)
        );
        assert_eq!(
            recorder.not_joined(JoinFailure::Credentials).0,
            None,
            "one count per boot"
        );
        assert_eq!(recorder.stored(), credentials);
    }

    #[test]
    fn later_credential_failures_move_a_network_absent_count() {
        let stored = FailureCounts {
            credentials: 2,
            network_absent: 4,
        };
        let mut recorder = JoinRecorder::new(FallbackPolicy::new(), stored);
        let absent = FailureCounts {
            credentials: 2,
            network_absent: 5,
        };
        assert_eq!(
            recorder.not_joined(JoinFailure::NetworkAbsent).0,
            Some(absent)
        );
        let moved = FailureCounts {
            credentials: 3,
            network_absent: 4,
        };
        assert_eq!(recorder.not_joined(JoinFailure::Credentials).0, Some(moved));
        assert_eq!(recorder.not_joined(JoinFailure::NetworkAbsent).0, None);
        assert_eq!(recorder.not_joined(JoinFailure::Credentials).0, None);
        assert_eq!(recorder.stored(), moved);
    }

    #[test]
    fn disabled_thresholds_still_count() {
        let never = FallbackPolicy::new()
            .with_credential_threshold(0)
            .with_network_absent_threshold(0);
        let stored = FailureCounts {
            credentials: 5,
            network_absent: 0,
        };
        assert_eq!(never.decide(stored), FallbackDecision::CarryOn);
        let mut recorder = JoinRecorder::new(never, stored);
        assert_eq!(recorder.policy(), never);
        let (write, decision) = recorder.not_joined(JoinFailure::Credentials);
        assert_eq!(write.map(|c| c.credentials), Some(6));
        assert_eq!(decision, FallbackDecision::CarryOn);
    }

    #[test]
    fn fallback_window_default_is_ten_minutes() {
        assert_eq!(FALLBACK_PORTAL_WINDOW_DEFAULT.as_secs(), 600);
    }
}
