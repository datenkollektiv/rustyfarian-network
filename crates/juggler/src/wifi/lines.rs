//! The Wi-Fi log lines both tiers print, one text per event.
//!
//! Each type renders a whole line, prefix included, through [`Display`](core::fmt::Display),
//! so a tier logs it with `log::info!("{}", line)` and cannot word it differently
//! from the other tier. The tier name is not part of any line; examples print it once
//! in their boot line.
//!
//! The host tests at the bottom pin every text.

use core::fmt::{self, Debug, Display};
use core::net::Ipv4Addr;

use super::JoinedAp;

/// The station associated: `[wifi] link up #3` (plus ` (adopted)` for a link the
/// supervisor found already up when it started, and ` — bssid …, ch N, rssi -NN dBm`
/// when the driver reports the AP).
pub struct LinkUp {
    /// Associations since boot (`LinkMachine::connect_count`).
    pub count: u32,
    /// The supervisor adopted an association that predates it.
    pub adopted: bool,
    /// The AP joined, when the driver reports it.
    pub ap: Option<JoinedAp>,
}

impl Display for LinkUp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[wifi] link up #{}", self.count)?;
        if self.adopted {
            f.write_str(" (adopted)")?;
        }
        if let Some(ap) = &self.ap {
            write!(f, " — {ap}")?;
        }
        Ok(())
    }
}

/// Association and IPv4 configuration are both in place:
/// `[wifi] network ready — ip 192.168.1.20`.
pub struct NetworkReady {
    /// The station's address, when the tier knows it.
    pub ip: Option<Ipv4Addr>,
}

impl Display for NetworkReady {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[wifi] network ready")?;
        if let Some(ip) = self.ip {
            write!(f, " — ip {ip}")?;
        }
        Ok(())
    }
}

/// The link dropped: `[wifi] disconnected — <reason><hint>`.
///
/// The reason text is the tier's own (ESP-IDF numeric `reason 8 (ASSOC_LEAVE)`,
/// esp-radio's enum name); the hint is the shared [`CREDENTIALS_HINT`](super::CREDENTIALS_HINT)
/// or empty.
pub struct Disconnected<R> {
    /// What the driver reported.
    pub reason: R,
    /// `""` or [`CREDENTIALS_HINT`](super::CREDENTIALS_HINT).
    pub hint: &'static str,
}

impl<R: Display> Display for Disconnected<R> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[wifi] disconnected — {}{}", self.reason, self.hint)
    }
}

/// Renders a value with its [`Debug`] form where a [`Display`] is required,
/// for drivers that report a named enum instead of a code.
pub struct ByDebug<T>(pub T);

impl<T: Debug> Display for ByDebug<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.0)
    }
}

/// A back-off before the next association attempt:
/// `[wifi] reconnecting in 2000 ms (attempt 2)`.
pub struct Reconnecting {
    /// Pause in milliseconds.
    pub ms: u64,
    /// Consecutive failures so far (`LinkMachine::failed_attempts`).
    pub attempt: u32,
}

impl Display for Reconnecting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[wifi] reconnecting in {} ms (attempt {})",
            self.ms, self.attempt
        )
    }
}

/// Suspend and resume milestones both tiers pass through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lifecycle {
    /// `suspend()` was called and acted on.
    SuspendRequested,
    /// The link is down and parked.
    Suspended,
    /// `resume()` was called and acted on.
    ResumeRequested,
}

impl Display for Lifecycle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::SuspendRequested => "[wifi] suspend requested",
            Self::Suspended => "[wifi] suspended",
            Self::ResumeRequested => "[wifi] resume requested",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wifi::{credentials_hint, DisconnectReason};

    #[test]
    fn link_up_text_is_pinned() {
        let up = |count, adopted, ap| alloc::format!("{}", LinkUp { count, adopted, ap });
        let ap = JoinedAp {
            bssid: [0x72, 0xd7, 0x9a, 0xca, 0xb9, 0x44],
            channel: 11,
            rssi: -42,
        };
        assert_eq!(up(1, false, None), "[wifi] link up #1");
        assert_eq!(up(4, true, None), "[wifi] link up #4 (adopted)");
        assert_eq!(
            up(1, false, Some(ap)),
            "[wifi] link up #1 — bssid 72:d7:9a:ca:b9:44, ch 11, rssi -42 dBm"
        );
        assert_eq!(
            up(2, true, Some(ap)),
            "[wifi] link up #2 (adopted) — bssid 72:d7:9a:ca:b9:44, ch 11, rssi -42 dBm"
        );
    }

    #[test]
    fn network_ready_text_is_pinned() {
        let ip = Some(Ipv4Addr::new(192, 168, 1, 20));
        assert_eq!(
            alloc::format!("{}", NetworkReady { ip }),
            "[wifi] network ready — ip 192.168.1.20"
        );
        assert_eq!(
            alloc::format!("{}", NetworkReady { ip: None }),
            "[wifi] network ready"
        );
    }

    #[test]
    fn disconnected_text_is_pinned() {
        assert_eq!(
            alloc::format!(
                "{}",
                Disconnected {
                    reason: DisconnectReason(8),
                    hint: ""
                }
            ),
            "[wifi] disconnected — reason 8 (ASSOC_LEAVE)"
        );
        assert_eq!(
            alloc::format!(
                "{}",
                Disconnected {
                    reason: ByDebug(Some(1)),
                    hint: credentials_hint(202)
                }
            ),
            "[wifi] disconnected — Some(1) — check SSID/password and network availability"
        );
    }

    #[test]
    fn reconnecting_text_is_pinned() {
        assert_eq!(
            alloc::format!(
                "{}",
                Reconnecting {
                    ms: 2000,
                    attempt: 2
                }
            ),
            "[wifi] reconnecting in 2000 ms (attempt 2)"
        );
    }

    #[test]
    fn lifecycle_text_is_pinned() {
        assert_eq!(
            alloc::format!("{}", Lifecycle::SuspendRequested),
            "[wifi] suspend requested"
        );
        assert_eq!(
            alloc::format!("{}", Lifecycle::Suspended),
            "[wifi] suspended"
        );
        assert_eq!(
            alloc::format!("{}", Lifecycle::ResumeRequested),
            "[wifi] resume requested"
        );
    }
}
