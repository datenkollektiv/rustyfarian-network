//! Join-failure classification: what a boot whose Wi-Fi join failed says about
//! the stored network (see `docs/features/wifi-portal-fallback-v1.md`, part 1).
//!
//! [`JoinFailure::of`] classifies one disconnect reason; [`JoinAttempts`] collects
//! the reasons of one boot's connect attempts and gives the boot's verdict, which
//! the provisioning fallback counts across boots.
//! Both are pure, so the two tiers share one rule.

use super::DisconnectReason;

/// How many `AUTH_EXPIRE` (2) reasons one boot's attempts must contain before
/// [`JoinAttempts::class`] reads them as a credential failure.
///
/// A single `AUTH_EXPIRE` is common on a busy or roaming access point; a wrong
/// passphrase repeats it on every attempt.
/// The occurrences need not be consecutive: other reasons in between (another AP
/// of the same SSID answering `4WAY_HANDSHAKE_TIMEOUT`) do not reset the tally.
pub const AUTH_EXPIRE_REPEAT: u16 = 2;

/// The class of a failed Wi-Fi join.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum JoinFailure {
    /// The access point answered but refused the passphrase:
    /// `4WAY_HANDSHAKE_TIMEOUT`, `HANDSHAKE_TIMEOUT`, `AUTH_FAIL`, `MIC_FAILURE`,
    /// or a repeated `AUTH_EXPIRE` ([`AUTH_EXPIRE_REPEAT`]).
    Credentials,
    /// The SSID was not found (`NO_AP_FOUND`): renamed, or the router is off.
    NetworkAbsent,
    /// Anything else (beacon loss, association leave, DHCP timeout, no reason
    /// at all); says nothing about the stored network.
    Transient,
}

impl JoinFailure {
    /// The class of one disconnect reason.
    ///
    /// `AUTH_EXPIRE` alone is [`Transient`](Self::Transient); only
    /// [`JoinAttempts`] sees whether it repeats.
    pub const fn of(reason: DisconnectReason) -> Self {
        match reason {
            DisconnectReason::FourWayHandshakeTimeout
            | DisconnectReason::HandshakeTimeout
            | DisconnectReason::AuthFail
            | DisconnectReason::MicFailure => Self::Credentials,
            DisconnectReason::NoApFound => Self::NetworkAbsent,
            _ => Self::Transient,
        }
    }

    /// A stable lower-case name for log lines: `credentials`, `network_absent`
    /// or `transient`.
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::Credentials => "credentials",
            Self::NetworkAbsent => "network_absent",
            Self::Transient => "transient",
        }
    }
}

/// The disconnect reasons of one boot's connect attempts, reduced to the boot's
/// [`JoinFailure`] class.
///
/// The boot helper [`record`](Self::record)s the reason of every failed attempt
/// and, when the boot gives up on the join, asks for [`class`](Self::class).
/// Tallies saturate, so an unbounded retry loop cannot overflow them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JoinAttempts {
    credentials: u16,
    network_absent: u16,
    auth_expire: u16,
    transient: u16,
}

impl JoinAttempts {
    /// No attempts recorded.
    pub const fn new() -> Self {
        Self {
            credentials: 0,
            network_absent: 0,
            auth_expire: 0,
            transient: 0,
        }
    }

    /// Records the disconnect reason of one failed attempt.
    pub fn record(&mut self, reason: DisconnectReason) {
        let tally = match (reason, JoinFailure::of(reason)) {
            (DisconnectReason::AuthExpire, _) => &mut self.auth_expire,
            (_, JoinFailure::Credentials) => &mut self.credentials,
            (_, JoinFailure::NetworkAbsent) => &mut self.network_absent,
            (_, JoinFailure::Transient) => &mut self.transient,
        };
        *tally = tally.saturating_add(1);
    }

    /// Attempts recorded so far (saturating).
    pub const fn attempts(&self) -> u16 {
        self.credentials
            .saturating_add(self.network_absent)
            .saturating_add(self.auth_expire)
            .saturating_add(self.transient)
    }

    /// The boot's class.
    ///
    /// Credential evidence wins over an absent network, which wins over a
    /// transient failure: an access point that rejected the passphrase even once
    /// was in range, while a single `NO_AP_FOUND` among handshake timeouts is
    /// one missed scan.
    /// `AUTH_EXPIRE` counts as credential evidence only from its
    /// [`AUTH_EXPIRE_REPEAT`]th occurrence on.
    /// A boot with no recorded reason (a timeout without a disconnect event) is
    /// [`Transient`](JoinFailure::Transient).
    pub const fn class(&self) -> JoinFailure {
        if self.credentials > 0 || self.auth_expire >= AUTH_EXPIRE_REPEAT {
            JoinFailure::Credentials
        } else if self.network_absent > 0 {
            JoinFailure::NetworkAbsent
        } else {
            JoinFailure::Transient
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every named code 1..=24 and 200..=212, plus an unmapped one.
    fn all_codes() -> impl Iterator<Item = u16> {
        (1..=24).chain(200..=212).chain([0, 77, 1000])
    }

    #[test]
    fn classifier_table() {
        for code in all_codes() {
            let reason = DisconnectReason::from_code(code);
            let expected = match code {
                15 | 204 | 202 | 14 => JoinFailure::Credentials,
                201 => JoinFailure::NetworkAbsent,
                _ => JoinFailure::Transient,
            };
            assert_eq!(JoinFailure::of(reason), expected, "{reason}");
        }
        assert_eq!(
            JoinFailure::of(DisconnectReason::Unspecified { code: None }),
            JoinFailure::Transient
        );
    }

    #[test]
    fn single_auth_expire_is_transient() {
        assert_eq!(
            JoinFailure::of(DisconnectReason::AuthExpire),
            JoinFailure::Transient
        );
        let mut boot = JoinAttempts::new();
        boot.record(DisconnectReason::AuthExpire);
        assert_eq!(boot.class(), JoinFailure::Transient);
    }

    #[test]
    fn repeated_auth_expire_is_credentials_even_when_interleaved() {
        let mut boot = JoinAttempts::new();
        boot.record(DisconnectReason::AuthExpire);
        boot.record(DisconnectReason::BeaconTimeout);
        boot.record(DisconnectReason::AuthExpire);
        assert_eq!(boot.class(), JoinFailure::Credentials);
        assert_eq!(boot.attempts(), 3);
    }

    /// The evidence log of the feature doc: reason 15 on two access points of the
    /// same SSID, then reason 2.
    #[test]
    fn mistyped_password_boot_is_credentials() {
        let mut boot = JoinAttempts::new();
        for code in [15, 15, 2] {
            boot.record(DisconnectReason::from_code(code));
        }
        assert_eq!(boot.class(), JoinFailure::Credentials);
    }

    #[test]
    fn renamed_ssid_boot_is_network_absent() {
        let mut boot = JoinAttempts::new();
        for _ in 0..4 {
            boot.record(DisconnectReason::NoApFound);
        }
        boot.record(DisconnectReason::AuthExpire);
        assert_eq!(boot.class(), JoinFailure::NetworkAbsent);
    }

    #[test]
    fn credentials_win_over_network_absent_over_transient() {
        let mut boot = JoinAttempts::new();
        boot.record(DisconnectReason::AssocLeave);
        assert_eq!(boot.class(), JoinFailure::Transient);
        boot.record(DisconnectReason::NoApFound);
        assert_eq!(boot.class(), JoinFailure::NetworkAbsent);
        boot.record(DisconnectReason::AuthFail);
        assert_eq!(boot.class(), JoinFailure::Credentials);
    }

    #[test]
    fn boot_without_reasons_is_transient() {
        let boot = JoinAttempts::new();
        assert_eq!(boot.attempts(), 0);
        assert_eq!(boot.class(), JoinFailure::Transient);
        assert_eq!(boot, JoinAttempts::default());
    }

    #[test]
    fn tallies_saturate() {
        let mut boot = JoinAttempts::new();
        for _ in 0..=u32::from(u16::MAX) {
            boot.record(DisconnectReason::BeaconTimeout);
        }
        boot.record(DisconnectReason::NoApFound);
        assert_eq!(boot.attempts(), u16::MAX);
        assert_eq!(boot.class(), JoinFailure::NetworkAbsent);
    }

    #[test]
    fn names_are_stable() {
        assert_eq!(JoinFailure::Credentials.as_str(), "credentials");
        assert_eq!(JoinFailure::NetworkAbsent.as_str(), "network_absent");
        assert_eq!(JoinFailure::Transient.as_str(), "transient");
    }
}
