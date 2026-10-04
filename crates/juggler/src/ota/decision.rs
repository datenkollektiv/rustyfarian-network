//! Update decision policy: compare a running and offered [`Version`].

use core::cmp::Ordering;

use super::Version;

/// Experimental: API may change before 1.0.
///
/// The outcome of comparing a running firmware [`Version`] against an offered
/// one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateDecision {
    /// The offered version is strictly newer than the running one.
    Apply,
    /// The offered version is identical to the running one.
    Skip,
    /// The offered version is older than the running one — no downgrade path.
    Reject,
}

/// Experimental: API may change before 1.0.
///
/// Decide whether to apply an offered firmware update given the currently
/// running [`Version`].
///
/// Returns [`UpdateDecision::Apply`] when `offered` is strictly newer than
/// `running`, [`UpdateDecision::Skip`] when the two are equal, and
/// [`UpdateDecision::Reject`] when `offered` is older — there is no downgrade
/// path.
///
/// # Comparison contract
///
/// Ordering is lexicographic over `MAJOR.MINOR.PATCH` (see [`Version`]'s
/// `Ord`): major decides first, then minor, then patch — `1.3.0` is newer than
/// `1.2.9`. [`Version`] has no pre-release or build-metadata component, so
/// there is nothing else to compare; [`Version::parse`] rejects inputs such as
/// `"1.2.3-rc.1"`.
///
/// [`UpdateDecision::Reject`] reflects the current MVP policy, not a permanent
/// guarantee: a future release may add an explicit, opt-in downgrade path.
///
/// # Example
///
/// ```
/// use juggler::ota::{decide_update, UpdateDecision, Version};
///
/// let running = Version::new(1, 2, 9);
/// let offered = Version::parse("1.3.0").unwrap();
///
/// let action = match decide_update(running, offered) {
///     UpdateDecision::Apply => "download, verify, flash",
///     UpdateDecision::Skip => "already up to date",
///     UpdateDecision::Reject => "refuse the downgrade",
/// };
/// assert_eq!(action, "download, verify, flash");
/// ```
pub fn decide_update(running: Version, offered: Version) -> UpdateDecision {
    match offered.cmp(&running) {
        Ordering::Greater => UpdateDecision::Apply,
        Ordering::Equal => UpdateDecision::Skip,
        Ordering::Less => UpdateDecision::Reject,
    }
}

/// Experimental: API may change before 1.0.
///
/// The outcome of [`decide_offer`]: [`UpdateDecision`] plus a loop guard for
/// versions the device previously rolled back from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfferDecision {
    /// The offered version is strictly newer and has not been refused.
    Apply,
    /// The offered version is identical to the running one.
    Skip,
    /// The offered version is older than the running one — no downgrade path.
    Reject,
    /// The offered version would be applied, but the device already rolled
    /// back from exactly this version and must not retry it.
    Refused,
}

/// Experimental: API may change before 1.0.
///
/// Decide whether to apply an offered firmware version, honouring a
/// previously refused version.
///
/// Delegates to [`decide_update`]: `Skip` and `Reject` are returned
/// unchanged, and `Apply` becomes [`OfferDecision::Refused`] if and only if
/// `refused == Some(offered)`.
///
/// The consumer persists `refused`: set it when a rollback leaves a version
/// (see [`ReconcileAction::ReportRollback`](super::ReconcileAction::ReportRollback)).
/// It is a loop guard against redelivered or retained offers (download,
/// deadline, rollback, repeat).
///
/// # Refusal lifetime
///
/// The refused version is kept until a DIFFERENT version passes the health
/// check (`mark_valid` succeeded; see
/// [`ReconcileAction::AwaitHealthCheck`](super::ReconcileAction::AwaitHealthCheck)).
/// It is NOT cleared when an offer is accepted or downloaded: otherwise a
/// failed download of B would re-admit the refused A.
/// v1 remembers a single refused version, so history is limited: after B
/// succeeds, A is admitted again.
///
/// # Admission
///
/// While an attempt record exists, do not call this to accept a new offer:
/// overwriting the record destroys unresolved evidence (see the
/// [`reconcile`](super::reconcile) module docs).
/// Admission is also blocked while an undelivered report exists: v1 keeps a
/// single report record, and a second rollback must not overwrite it.
/// Refuse the offer (and keep the retained command) until the report is
/// delivered.
///
/// # Example
///
/// ```
/// use juggler::ota::{decide_offer, OfferDecision, Version};
///
/// let running = Version::new(1, 2, 0);
/// let offered = Version::new(1, 3, 0);
///
/// assert_eq!(decide_offer(running, offered, None), OfferDecision::Apply);
/// assert_eq!(
///     decide_offer(running, offered, Some(offered)),
///     OfferDecision::Refused
/// );
/// ```
pub fn decide_offer(running: Version, offered: Version, refused: Option<Version>) -> OfferDecision {
    match decide_update(running, offered) {
        UpdateDecision::Skip => OfferDecision::Skip,
        UpdateDecision::Reject => OfferDecision::Reject,
        UpdateDecision::Apply if refused == Some(offered) => OfferDecision::Refused,
        UpdateDecision::Apply => OfferDecision::Apply,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decide_update_table() {
        let cases = [
            // (running, offered, expected)
            (
                Version::new(1, 0, 0),
                Version::new(2, 0, 0),
                UpdateDecision::Apply,
            ),
            (
                Version::new(2, 0, 0),
                Version::new(1, 0, 0),
                UpdateDecision::Reject,
            ),
            (
                Version::new(1, 1, 0),
                Version::new(1, 2, 0),
                UpdateDecision::Apply,
            ),
            (
                Version::new(1, 2, 0),
                Version::new(1, 1, 0),
                UpdateDecision::Reject,
            ),
            (
                Version::new(1, 0, 1),
                Version::new(1, 0, 2),
                UpdateDecision::Apply,
            ),
            (
                Version::new(1, 0, 2),
                Version::new(1, 0, 1),
                UpdateDecision::Reject,
            ),
            (
                Version::new(1, 2, 3),
                Version::new(1, 2, 3),
                UpdateDecision::Skip,
            ),
            // Higher-order component wins over larger lower-order ones.
            (
                Version::new(1, 2, 9),
                Version::new(1, 3, 0),
                UpdateDecision::Apply,
            ),
            (
                Version::new(2, 0, 0),
                Version::new(1, 65535, 65535),
                UpdateDecision::Reject,
            ),
            (
                Version::new(0, 0, 0),
                Version::new(0, 0, 0),
                UpdateDecision::Skip,
            ),
        ];

        for (running, offered, expected) in cases {
            assert_eq!(
                decide_update(running, offered),
                expected,
                "running={running:?} offered={offered:?}"
            );
        }
    }

    #[test]
    fn decide_update_equal_versions_skip() {
        let v = Version::new(1, 4, 0);
        assert_eq!(decide_update(v, v), UpdateDecision::Skip);
    }

    #[test]
    fn decide_offer_table() {
        let v = Version::new;
        let cases = [
            // (running, offered, refused, expected)
            (v(1, 0, 0), v(2, 0, 0), None, OfferDecision::Apply),
            (
                v(1, 0, 0),
                v(2, 0, 0),
                Some(v(2, 0, 0)),
                OfferDecision::Refused,
            ),
            (
                v(1, 0, 0),
                v(2, 0, 0),
                Some(v(1, 5, 0)),
                OfferDecision::Apply,
            ),
            // refused == running: still Skip.
            (
                v(1, 0, 0),
                v(1, 0, 0),
                Some(v(1, 0, 0)),
                OfferDecision::Skip,
            ),
            (v(1, 0, 0), v(1, 0, 0), None, OfferDecision::Skip),
            // refused older offered: still Reject.
            (
                v(2, 0, 0),
                v(1, 0, 0),
                Some(v(1, 0, 0)),
                OfferDecision::Reject,
            ),
            (v(2, 0, 0), v(1, 0, 0), None, OfferDecision::Reject),
            (
                v(2, 0, 0),
                v(1, 0, 0),
                Some(v(3, 0, 0)),
                OfferDecision::Reject,
            ),
            // refused differs from offered: Apply.
            (
                v(1, 0, 0),
                v(1, 1, 0),
                Some(v(1, 2, 0)),
                OfferDecision::Apply,
            ),
        ];
        for (running, offered, refused, expected) in cases {
            assert_eq!(
                decide_offer(running, offered, refused),
                expected,
                "running={running:?} offered={offered:?} refused={refused:?}"
            );
        }
    }
}
