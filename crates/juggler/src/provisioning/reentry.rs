//! Provisioning re-entry hook: the consumer's once-per-boot answer, asked
//! before the stored record is read.
//!
//! The ordering lives here, as pure code, so it is host-tested without
//! ESP-IDF. The library only reports the answer; it never erases the record
//! and never restarts the device. Acting on `FactoryReset` is the consumer's
//! decision.

/// Experimental: API may change before 1.0.
///
/// The consumer's answer to the once-per-boot re-entry hook.
///
/// - [`None`](Self::None): boot normally; the stored record is classified as
///   usual.
/// - [`Portal`](Self::Portal): reopen the portal and keep the stored record,
///   which the portal prefills from.
/// - [`FactoryReset`](Self::FactoryReset): the consumer wants the record
///   erased. The library only reports this; the consumer erases.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum ReentryRequest {
    /// Boot normally.
    #[default]
    None,
    /// Reopen the portal, keeping the stored record.
    Portal,
    /// Erase the stored record (performed by the consumer, never the library).
    FactoryReset,
}

/// Experimental: API may change before 1.0.
///
/// The result of [`gate_reentry`].
#[derive(Debug, PartialEq, Eq)]
pub enum ReentryGate<T> {
    /// The hook answered [`ReentryRequest::Portal`]; the store was not opened.
    Portal,
    /// The hook answered [`ReentryRequest::FactoryReset`]; the store was not
    /// opened.
    FactoryReset,
    /// The hook answered [`ReentryRequest::None`]; the store was opened and is
    /// handed back for the normal load classification.
    Proceed(T),
}

/// Experimental: API may change before 1.0.
///
/// Asks `hook` exactly once, then opens the store only when the answer is
/// [`ReentryRequest::None`].
///
/// `open` is not called for `Portal` or `FactoryReset`, so those answers never
/// touch the stored record.
///
/// # Errors
///
/// Returns the error from `open` when it runs and fails.
pub fn gate_reentry<T, E>(
    hook: impl FnOnce() -> ReentryRequest,
    open: impl FnOnce() -> Result<T, E>,
) -> Result<ReentryGate<T>, E> {
    match hook() {
        ReentryRequest::Portal => Ok(ReentryGate::Portal),
        ReentryRequest::FactoryReset => Ok(ReentryGate::FactoryReset),
        ReentryRequest::None => open().map(ReentryGate::Proceed),
    }
}

#[cfg(all(test, feature = "std"))]
mod tests {
    use super::*;
    use crate::provisioning::{KvProvisioningStore, ProvisioningKv, SchemaProfile};
    use crate::test_support::test_psk;
    use core::cell::Cell;
    use std::collections::BTreeMap;

    #[derive(Default)]
    struct CountingKv<'a> {
        strs: BTreeMap<String, String>,
        u8s: BTreeMap<String, u8>,
        reads: Option<&'a Cell<u32>>,
    }

    impl ProvisioningKv for CountingKv<'_> {
        type Error = core::convert::Infallible;

        fn get_u8(&self, key: &str) -> Result<Option<u8>, Self::Error> {
            if let Some(r) = self.reads {
                r.set(r.get() + 1);
            }
            Ok(self.u8s.get(key).copied())
        }
        fn set_u8(&mut self, key: &str, value: u8) -> Result<(), Self::Error> {
            self.u8s.insert(key.to_owned(), value);
            Ok(())
        }
        fn get_str(&self, key: &str) -> Result<Option<String>, Self::Error> {
            if let Some(r) = self.reads {
                r.set(r.get() + 1);
            }
            Ok(self.strs.get(key).cloned())
        }
        fn set_str(&mut self, key: &str, value: &str) -> Result<(), Self::Error> {
            self.strs.insert(key.to_owned(), value.to_owned());
            Ok(())
        }
        fn remove(&mut self, key: &str) -> Result<(), Self::Error> {
            self.strs.remove(key);
            self.u8s.remove(key);
            Ok(())
        }
    }

    fn provisioned<'a>(reads: &'a Cell<u32>, profile: &str) -> KvProvisioningStore<CountingKv<'a>> {
        let mut kv = CountingKv {
            reads: Some(reads),
            ..Default::default()
        };
        kv.set_str("wifi_ssid", "FieldNet").unwrap();
        kv.set_str("wifi_pass", test_psk()).unwrap();
        kv.set_str("dev_name", "clock-07").unwrap();
        kv.set_str("profile", profile).unwrap();
        if profile == "wifi_mqtt" {
            kv.set_str("mqtt_host", "broker.local").unwrap();
            kv.set_str("mqtt_port", "1883").unwrap();
        }
        kv.set_u8("schema_ver", 2).unwrap();
        KvProvisioningStore::new(kv)
    }

    /// Runs the gate and, on `Proceed`, the load classification; returns the
    /// gate tag plus the loaded config (if any).
    fn run(
        answer: ReentryRequest,
        reads: &Cell<u32>,
        profile: Option<&str>,
        hook_calls: &Cell<u32>,
        open_calls: &Cell<u32>,
    ) -> (
        &'static str,
        Option<crate::provisioning::ProvisioningConfig>,
    ) {
        let gate = gate_reentry(
            || {
                hook_calls.set(hook_calls.get() + 1);
                answer
            },
            || {
                open_calls.set(open_calls.get() + 1);
                Ok::<_, core::convert::Infallible>(match profile {
                    Some(p) => provisioned(reads, p),
                    None => KvProvisioningStore::new(CountingKv {
                        reads: Some(reads),
                        ..Default::default()
                    }),
                })
            },
        )
        .unwrap();
        match gate {
            ReentryGate::Portal => ("portal", None),
            ReentryGate::FactoryReset => ("reset", None),
            ReentryGate::Proceed(store) => ("proceed", store.load().unwrap()),
        }
    }

    #[test]
    fn portal_and_factory_reset_never_touch_the_store() {
        for (answer, tag) in [
            (ReentryRequest::Portal, "portal"),
            (ReentryRequest::FactoryReset, "reset"),
        ] {
            let (reads, hook, open) = (Cell::new(0), Cell::new(0), Cell::new(0));
            let (got, cfg) = run(answer, &reads, Some("wifi_mqtt"), &hook, &open);
            assert_eq!(got, tag);
            assert!(cfg.is_none());
            assert_eq!(hook.get(), 1, "hook asked exactly once");
            assert_eq!(open.get(), 0, "store not opened");
            assert_eq!(reads.get(), 0, "store not read");
        }
    }

    #[test]
    fn none_reproduces_not_provisioned() {
        let (reads, hook, open) = (Cell::new(0), Cell::new(0), Cell::new(0));
        let (got, cfg) = run(ReentryRequest::None, &reads, None, &hook, &open);
        assert_eq!(got, "proceed");
        assert!(cfg.is_none());
        assert_eq!((hook.get(), open.get()), (1, 1));
    }

    #[test]
    fn none_reproduces_ready_record() {
        let (reads, hook, open) = (Cell::new(0), Cell::new(0), Cell::new(0));
        let (_, cfg) = run(
            ReentryRequest::None,
            &reads,
            Some("wifi_mqtt"),
            &hook,
            &open,
        );
        let cfg = cfg.expect("provisioned");
        assert_eq!(cfg.profile(), SchemaProfile::WifiMqttDevice);
        assert!(cfg.mqtt().is_some());
        assert!(reads.get() > 0);
    }

    #[test]
    fn open_error_propagates_only_for_none() {
        let r: Result<ReentryGate<()>, &str> = gate_reentry(|| ReentryRequest::None, || Err("x"));
        assert_eq!(r, Err("x"));
        let r: Result<ReentryGate<()>, &str> = gate_reentry(|| ReentryRequest::Portal, || Err("x"));
        assert_eq!(r, Ok(ReentryGate::Portal));
    }

    #[test]
    fn default_is_none() {
        assert_eq!(ReentryRequest::default(), ReentryRequest::None);
    }
}
