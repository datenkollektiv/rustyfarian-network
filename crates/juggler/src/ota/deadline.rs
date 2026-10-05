//! Cooperative total-time deadline for OTA downloads.
//!
//! Pure and `no_std`: callers pass the elapsed time, there is no clock.
//! The tiers check the deadline at operation boundaries and take an
//! [`ActivationPermit`] immediately before selecting the new boot slot.

use core::time::Duration;

use super::error::OtaError;

/// Experimental: API may change before 1.0.
///
/// A total time limit for one OTA download, or no limit.
///
/// The deadline is expired when `elapsed >= total`, so a zero total expires
/// at elapsed zero.
/// Expiry is reported as [`OtaError::DownloadTimeout`].
///
/// ```
/// use core::time::Duration;
/// use juggler::ota::{Deadline, OtaError};
///
/// let d = Deadline::after(Duration::from_secs(10));
/// assert_eq!(d.check(Duration::from_secs(9)), Ok(()));
/// assert_eq!(d.check(Duration::from_secs(10)), Err(OtaError::DownloadTimeout));
/// assert_eq!(Deadline::none().check(Duration::MAX), Ok(()));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Deadline {
    total: Option<Duration>,
}

impl Deadline {
    /// No limit: every check passes.
    pub const fn none() -> Self {
        Self { total: None }
    }

    /// Limit the download to `total` elapsed time.
    pub const fn after(total: Duration) -> Self {
        Self { total: Some(total) }
    }

    /// Passes while `elapsed < total`; `Err(DownloadTimeout)` once `elapsed >= total`.
    pub fn check(&self, elapsed: Duration) -> Result<(), OtaError> {
        match self.total {
            Some(total) if elapsed >= total => Err(OtaError::DownloadTimeout),
            _ => Ok(()),
        }
    }

    /// The pre-activation check: on success returns the [`ActivationPermit`]
    /// that slot activation requires.
    ///
    /// Call it immediately before the call that selects the new boot slot.
    pub fn permit_activation(&self, elapsed: Duration) -> Result<ActivationPermit, OtaError> {
        self.check(elapsed)?;
        Ok(ActivationPermit(()))
    }

    /// Time left before expiry (saturating at zero), or `None` without a limit.
    pub fn remaining(&self, elapsed: Duration) -> Option<Duration> {
        self.total.map(|total| total.saturating_sub(elapsed))
    }

    /// The wait to use for one operation: `min(per_op, remaining)`, or `per_op`
    /// without a limit.
    pub fn clip(&self, per_op: Duration, elapsed: Duration) -> Duration {
        match self.remaining(elapsed) {
            Some(remaining) => per_op.min(remaining),
            None => per_op,
        }
    }
}

/// Experimental: API may change before 1.0.
///
/// Proof that the pre-activation deadline check passed.
///
/// Obtainable only from [`Deadline::permit_activation`]; the field is private,
/// so no code outside this module can construct one.
/// Each tier's slot activation takes a permit by value, so a timed-out
/// download cannot select the new boot slot.
///
/// ```compile_fail
/// use juggler::ota::ActivationPermit;
/// let _forged = ActivationPermit(());
/// ```
#[derive(Debug)]
#[must_use = "an ActivationPermit must be consumed by slot activation"]
pub struct ActivationPermit(());

#[cfg(test)]
mod tests {
    use super::*;

    const fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn no_deadline_never_expires() {
        let d = Deadline::none();
        assert_eq!(d.check(Duration::ZERO), Ok(()));
        assert_eq!(d.check(Duration::MAX), Ok(()));
        assert!(d.permit_activation(Duration::MAX).is_ok());
        assert_eq!(d.remaining(Duration::MAX), None);
        assert_eq!(Deadline::default(), Deadline::none());
    }

    #[test]
    fn zero_duration_expires_immediately() {
        let d = Deadline::after(Duration::ZERO);
        assert_eq!(d.check(Duration::ZERO), Err(OtaError::DownloadTimeout));
        assert_eq!(
            d.permit_activation(Duration::ZERO).unwrap_err(),
            OtaError::DownloadTimeout
        );
    }

    #[test]
    fn exact_boundary_expires() {
        let d = Deadline::after(ms(100));
        assert_eq!(d.check(ms(99)), Ok(()));
        assert_eq!(d.check(ms(100)), Err(OtaError::DownloadTimeout));
        assert_eq!(d.check(ms(101)), Err(OtaError::DownloadTimeout));
    }

    #[test]
    fn permit_refused_after_expiry_and_granted_before() {
        let d = Deadline::after(ms(100));
        assert!(d.permit_activation(ms(99)).is_ok());
        assert_eq!(
            d.permit_activation(ms(100)).unwrap_err(),
            OtaError::DownloadTimeout
        );
        assert_eq!(
            d.permit_activation(ms(5_000)).unwrap_err(),
            OtaError::DownloadTimeout
        );
    }

    #[test]
    fn remaining_saturates_at_zero() {
        let d = Deadline::after(ms(100));
        assert_eq!(d.remaining(ms(30)), Some(ms(70)));
        assert_eq!(d.remaining(ms(100)), Some(Duration::ZERO));
        assert_eq!(d.remaining(ms(500)), Some(Duration::ZERO));
    }

    #[test]
    fn clip_is_min_of_per_op_and_remaining() {
        let d = Deadline::after(ms(100));
        assert_eq!(d.clip(ms(60), ms(10)), ms(60));
        assert_eq!(d.clip(ms(60), ms(70)), ms(30));
        assert_eq!(d.clip(ms(60), ms(100)), Duration::ZERO);
        assert_eq!(Deadline::none().clip(ms(60), ms(10_000)), ms(60));
    }

    /// One checkpoint in the tier download sequence.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum Step {
        Connected,
        Prepared,
        Write(usize),
        Verified,
        PreActivation,
    }

    /// Models the checkpoint order the tiers implement.
    ///
    /// At every boundary the operation runs first (`op`); only if it succeeded
    /// is the deadline consulted (`clock` gives the elapsed time at that
    /// boundary).
    /// This is a model of the tier control flow, not the tier code itself;
    /// the tier code is checked against it by review.
    fn walk(
        deadline: &Deadline,
        writes: usize,
        clock: impl Fn(Step) -> Duration,
        op: impl Fn(Step) -> Result<(), OtaError>,
    ) -> Result<ActivationPermit, (Step, OtaError)> {
        let boundary = |step: Step| -> Result<(), (Step, OtaError)> {
            op(step).map_err(|e| (step, e))?;
            deadline.check(clock(step)).map_err(|e| (step, e))
        };
        boundary(Step::Connected)?;
        boundary(Step::Prepared)?;
        for i in 0..writes {
            boundary(Step::Write(i))?;
        }
        boundary(Step::Verified)?;
        op(Step::PreActivation).map_err(|e| (Step::PreActivation, e))?;
        deadline
            .permit_activation(clock(Step::PreActivation))
            .map_err(|e| (Step::PreActivation, e))
    }

    /// Strictly increasing elapsed time per checkpoint (3 writes):
    /// Connected 10, Prepared 20, Write(0..3) 30/40/50, Verified 60,
    /// PreActivation 70 (milliseconds).
    fn clock(step: Step) -> Duration {
        match step {
            Step::Connected => ms(10),
            Step::Prepared => ms(20),
            Step::Write(i) => ms(30 + 10 * i as u64),
            Step::Verified => ms(60),
            Step::PreActivation => ms(70),
        }
    }

    fn all_ok(_: Step) -> Result<(), OtaError> {
        Ok(())
    }

    #[test]
    fn sequence_grants_permit_when_every_boundary_is_in_time() {
        assert!(walk(&Deadline::after(ms(71)), 3, clock, all_ok).is_ok());
        assert!(walk(&Deadline::none(), 3, clock, all_ok).is_ok());
    }

    #[test]
    fn sequence_zero_deadline_fails_at_first_boundary() {
        let err = walk(&Deadline::after(Duration::ZERO), 3, clock, all_ok).unwrap_err();
        assert_eq!(err, (Step::Connected, OtaError::DownloadTimeout));
    }

    #[test]
    fn sequence_expires_at_preparation_without_permit() {
        let err = walk(&Deadline::after(ms(20)), 3, clock, all_ok).unwrap_err();
        assert_eq!(err, (Step::Prepared, OtaError::DownloadTimeout));
    }

    #[test]
    fn sequence_expires_after_final_write_without_permit() {
        let err = walk(&Deadline::after(ms(50)), 3, clock, all_ok).unwrap_err();
        assert_eq!(err, (Step::Write(2), OtaError::DownloadTimeout));
    }

    #[test]
    fn sequence_expires_just_before_activation_without_permit() {
        let err = walk(&Deadline::after(ms(70)), 3, clock, all_ok).unwrap_err();
        assert_eq!(err, (Step::PreActivation, OtaError::DownloadTimeout));
    }

    #[test]
    fn sequence_operation_error_supersedes_expired_deadline() {
        let fail_at = |target: Step, err: OtaError| {
            move |step: Step| {
                if step == target {
                    Err(err)
                } else {
                    Ok(())
                }
            }
        };
        let expired = Deadline::after(ms(40));

        let err = walk(
            &expired,
            3,
            clock,
            fail_at(Step::Write(1), OtaError::FlashWriteFailed),
        )
        .unwrap_err();
        assert_eq!(err, (Step::Write(1), OtaError::FlashWriteFailed));

        let err = walk(
            &Deadline::after(ms(60)),
            3,
            clock,
            fail_at(Step::Verified, OtaError::ChecksumMismatch),
        )
        .unwrap_err();
        assert_eq!(err, (Step::Verified, OtaError::ChecksumMismatch));

        let err = walk(
            &Deadline::after(ms(20)),
            3,
            clock,
            fail_at(Step::Prepared, OtaError::InsufficientSpace),
        )
        .unwrap_err();
        assert_eq!(err, (Step::Prepared, OtaError::InsufficientSpace));
    }
}
