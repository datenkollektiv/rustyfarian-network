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
}
