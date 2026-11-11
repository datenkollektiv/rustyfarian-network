//! The MQTT log lines both tiers print, one text per event.
//!
//! Each type renders a whole line, prefix included, through [`Display`].
//! The tier name is not part of any line; examples print it once in their boot line.
//! The host tests at the bottom pin every text.

use core::fmt::{self, Display};

/// `[mqtt] waiting for network`
pub struct WaitingForNetwork;

impl Display for WaitingForNetwork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[mqtt] waiting for network")
    }
}

/// A connection attempt starts: `[mqtt] connecting to 192.168.1.5:1883`.
pub struct Connecting<H> {
    /// Broker host name or address.
    pub host: H,
    /// Broker port.
    pub port: u16,
}

impl<H: Display> Display for Connecting<H> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[mqtt] connecting to {}:{}", self.host, self.port)
    }
}

/// The broker accepted the session:
/// `[mqtt] connected (session present: no, 2 subscriptions)`.
pub struct Connected {
    /// The broker kept the previous session.
    pub session_present: bool,
    /// Subscriptions configured on the builder.
    pub subscriptions: usize,
}

impl Display for Connected {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "[mqtt] connected (session present: {}, {} subscriptions)",
            if self.session_present { "yes" } else { "no" },
            self.subscriptions
        )
    }
}

/// The session ended: `[mqtt] disconnected — tcp connect timed out`, or just
/// `[mqtt] disconnected` where the transport reports no cause.
pub struct Disconnected<'a> {
    /// Why it ended, when the tier knows.
    pub cause: Option<&'a str>,
}

impl Display for Disconnected<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[mqtt] disconnected")?;
        if let Some(cause) = self.cause {
            write!(f, " — {cause}")?;
        }
        Ok(())
    }
}

/// A pause before the next attempt: `[mqtt] reconnecting in 10000 ms`.
pub struct Reconnecting {
    /// Pause in milliseconds.
    pub ms: u64,
}

impl Display for Reconnecting {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[mqtt] reconnecting in {} ms", self.ms)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texts_are_pinned() {
        assert_eq!(
            alloc::format!("{}", WaitingForNetwork),
            "[mqtt] waiting for network"
        );
        assert_eq!(
            alloc::format!(
                "{}",
                Connecting {
                    host: "192.168.1.5",
                    port: 1883
                }
            ),
            "[mqtt] connecting to 192.168.1.5:1883"
        );
        assert_eq!(
            alloc::format!(
                "{}",
                Connected {
                    session_present: false,
                    subscriptions: 2
                }
            ),
            "[mqtt] connected (session present: no, 2 subscriptions)"
        );
        assert_eq!(
            alloc::format!(
                "{}",
                Connected {
                    session_present: true,
                    subscriptions: 0
                }
            ),
            "[mqtt] connected (session present: yes, 0 subscriptions)"
        );
        assert_eq!(
            alloc::format!(
                "{}",
                Disconnected {
                    cause: Some("tcp connect timed out")
                }
            ),
            "[mqtt] disconnected — tcp connect timed out"
        );
        assert_eq!(
            alloc::format!("{}", Disconnected { cause: None }),
            "[mqtt] disconnected"
        );
        assert_eq!(
            alloc::format!("{}", Reconnecting { ms: 10_000 }),
            "[mqtt] reconnecting in 10000 ms"
        );
    }
}
