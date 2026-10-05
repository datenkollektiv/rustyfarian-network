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
/// Whether the device may admit a new firmware offer right now.
///
/// Build it with [`Admission::from_records`] from two facts the consumer reads
/// from its own storage, and pass it to [`decide_offer`].
///
/// Admission is [`Blocked`](Admission::Blocked) while either condition holds:
///
/// - An attempt record exists (see [`AttemptRecord`](super::AttemptRecord)).
///   Overwriting it with a new attempt destroys unresolved evidence of what
///   happened to the previous one (see the [`reconcile`](super::reconcile)
///   module docs).
/// - A report is undelivered.
///   v1 keeps a single report record, so a second report must not overwrite
///   it.
///   This includes consumer-originated rollback reports (for example an
///   operator rollback after `mark_valid`) if the consumer stores them in the
///   same record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// No attempt record and no undelivered report: a new offer may start.
    Open,
    /// An attempt record or an undelivered report exists: do not start a new
    /// attempt yet.
    Blocked,
}

impl Admission {
    /// Experimental: API may change before 1.0.
    ///
    /// Derive the admission state: [`Admission::Blocked`] if an attempt record
    /// exists or a report is undelivered, otherwise [`Admission::Open`].
    ///
    /// # Example
    ///
    /// ```
    /// use juggler::ota::Admission;
    ///
    /// assert_eq!(Admission::from_records(false, false), Admission::Open);
    /// assert_eq!(Admission::from_records(true, false), Admission::Blocked);
    /// assert_eq!(Admission::from_records(false, true), Admission::Blocked);
    /// ```
    #[must_use]
    pub fn from_records(attempt_exists: bool, report_undelivered: bool) -> Self {
        if attempt_exists || report_undelivered {
            Self::Blocked
        } else {
            Self::Open
        }
    }
}

/// Experimental: API may change before 1.0.
///
/// The outcome of [`decide_offer`]: [`UpdateDecision`] plus a loop guard for
/// versions the device previously rolled back from, and the admission gate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OfferDecision {
    /// The offered version is strictly newer, has not been refused, and
    /// admission is open.
    Apply,
    /// The offered version is identical to the running one.
    Skip,
    /// The offered version is older than the running one — no downgrade path.
    Reject,
    /// The offered version would be applied, but the device already rolled
    /// back from exactly this version and must not retry it.
    Refused,
    /// The offered version would be applied, but admission is
    /// [`Blocked`](Admission::Blocked).
    /// Keep the offer and re-evaluate it once admission reopens.
    Blocked,
}

/// Experimental: API may change before 1.0.
///
/// Decide whether to apply an offered firmware version, honouring a
/// previously refused version and the admission gate.
///
/// Delegates to [`decide_update`]: `Skip` and `Reject` are returned
/// unchanged.
/// An otherwise-`Apply` becomes [`OfferDecision::Refused`] if and only if
/// `refused == Some(offered)`, and otherwise [`OfferDecision::Blocked`] if
/// `admission` is [`Admission::Blocked`].
/// `Refused` wins over `Blocked`: a refused version is never worth retrying.
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
/// Build `admission` with [`Admission::from_records`]: it is blocked while an
/// attempt record exists (overwriting it destroys unresolved evidence, see the
/// [`reconcile`](super::reconcile) module docs) or a report is undelivered
/// (v1 keeps a single report record).
///
/// On [`OfferDecision::Blocked`] the offer is not dropped by the core, and
/// nothing re-triggers it: a retained MQTT command is not redelivered without
/// resubscribing.
/// The consumer must keep the offer (for example in RAM) and call
/// `decide_offer` again once admission reopens, that is after
/// `ClearAttempt`, `CompleteAttempt`, or delivery of the pending report.
/// Otherwise the operator must republish the offer.
///
/// # Example
///
/// ```
/// use juggler::ota::{decide_offer, Admission, OfferDecision, Version};
///
/// let running = Version::new(1, 2, 0);
/// let offered = Version::new(1, 3, 0);
///
/// assert_eq!(
///     decide_offer(running, offered, None, Admission::Open),
///     OfferDecision::Apply
/// );
/// assert_eq!(
///     decide_offer(running, offered, Some(offered), Admission::Open),
///     OfferDecision::Refused
/// );
/// assert_eq!(
///     decide_offer(running, offered, None, Admission::from_records(true, false)),
///     OfferDecision::Blocked
/// );
/// ```
pub fn decide_offer(
    running: Version,
    offered: Version,
    refused: Option<Version>,
    admission: Admission,
) -> OfferDecision {
    match decide_update(running, offered) {
        UpdateDecision::Skip => OfferDecision::Skip,
        UpdateDecision::Reject => OfferDecision::Reject,
        UpdateDecision::Apply if refused == Some(offered) => OfferDecision::Refused,
        UpdateDecision::Apply if admission == Admission::Blocked => OfferDecision::Blocked,
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
    fn admission_from_records_table() {
        assert_eq!(Admission::from_records(false, false), Admission::Open);
        assert_eq!(Admission::from_records(true, false), Admission::Blocked);
        assert_eq!(Admission::from_records(false, true), Admission::Blocked);
        assert_eq!(Admission::from_records(true, true), Admission::Blocked);
    }

    #[test]
    fn decide_offer_table() {
        let v = Version::new;
        let open = Admission::Open;
        let blocked = Admission::Blocked;
        let cases = [
            // (running, offered, refused, admission, expected)
            (v(1, 0, 0), v(2, 0, 0), None, open, OfferDecision::Apply),
            (
                v(1, 0, 0),
                v(2, 0, 0),
                Some(v(2, 0, 0)),
                open,
                OfferDecision::Refused,
            ),
            (
                v(1, 0, 0),
                v(2, 0, 0),
                Some(v(1, 5, 0)),
                open,
                OfferDecision::Apply,
            ),
            // refused == running: still Skip.
            (
                v(1, 0, 0),
                v(1, 0, 0),
                Some(v(1, 0, 0)),
                open,
                OfferDecision::Skip,
            ),
            (v(1, 0, 0), v(1, 0, 0), None, open, OfferDecision::Skip),
            // refused older offered: still Reject.
            (
                v(2, 0, 0),
                v(1, 0, 0),
                Some(v(1, 0, 0)),
                open,
                OfferDecision::Reject,
            ),
            (v(2, 0, 0), v(1, 0, 0), None, open, OfferDecision::Reject),
            (
                v(2, 0, 0),
                v(1, 0, 0),
                Some(v(3, 0, 0)),
                open,
                OfferDecision::Reject,
            ),
            // refused differs from offered: Apply.
            (
                v(1, 0, 0),
                v(1, 1, 0),
                Some(v(1, 2, 0)),
                open,
                OfferDecision::Apply,
            ),
            // Blocked turns only an otherwise-Apply into Blocked.
            (
                v(1, 0, 0),
                v(2, 0, 0),
                None,
                blocked,
                OfferDecision::Blocked,
            ),
            (
                v(1, 0, 0),
                v(2, 0, 0),
                Some(v(1, 5, 0)),
                blocked,
                OfferDecision::Blocked,
            ),
            // Refused beats Blocked.
            (
                v(1, 0, 0),
                v(2, 0, 0),
                Some(v(2, 0, 0)),
                blocked,
                OfferDecision::Refused,
            ),
            // Skip and Reject are unaffected by Blocked.
            (v(1, 0, 0), v(1, 0, 0), None, blocked, OfferDecision::Skip),
            (v(2, 0, 0), v(1, 0, 0), None, blocked, OfferDecision::Reject),
        ];
        for (running, offered, refused, admission, expected) in cases {
            assert_eq!(
                decide_offer(running, offered, refused, admission),
                expected,
                "running={running:?} offered={offered:?} refused={refused:?} admission={admission:?}"
            );
        }
    }
}
