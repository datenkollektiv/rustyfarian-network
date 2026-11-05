//! Activation order for a freshly written OTA slot.
//!
//! `OtaUpdater::activate_next_partition` writes only the new `ota_seq` and keeps
//! the `ota_state` of the otadata sector it reuses, which can be `Undefined`
//! (the bootloader treats it as valid), `Valid`, or even `Invalid`.
//! The new image must instead read `New`, so the bootloader turns it into
//! `PendingVerify` on its first boot and rolls back if it never reaches
//! `mark_valid`.
//! Kept free of ESP types so the order is checked on the host.

/// The two otadata writes an activation needs.
pub(super) trait SlotControl {
    /// The error of a failed write.
    type Error;

    /// Selects the next OTA slot for boot.
    fn activate_next(&mut self) -> Result<(), Self::Error>;

    /// Sets the state of the slot selected by [`activate_next`](Self::activate_next) to `New`.
    fn mark_activated_new(&mut self) -> Result<(), Self::Error>;
}

/// Selects the next slot and arms the bootloader's rollback for it.
pub(super) fn activate_as_new<C: SlotControl>(control: &mut C) -> Result<(), C::Error> {
    control.activate_next()?;
    control.mark_activated_new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Mock {
        calls: [&'static str; 2],
        count: usize,
        fail_activate: bool,
    }

    impl Mock {
        fn log(&mut self, call: &'static str) {
            self.calls[self.count] = call;
            self.count += 1;
        }
    }

    impl SlotControl for Mock {
        type Error = ();

        fn activate_next(&mut self) -> Result<(), ()> {
            self.log("activate");
            if self.fail_activate {
                Err(())
            } else {
                Ok(())
            }
        }

        fn mark_activated_new(&mut self) -> Result<(), ()> {
            self.log("new");
            Ok(())
        }
    }

    #[test]
    fn activated_slot_is_marked_new_after_activation() {
        let mut mock = Mock::default();
        assert_eq!(activate_as_new(&mut mock), Ok(()));
        assert_eq!(mock.calls[..mock.count], ["activate", "new"]);
    }

    #[test]
    fn failed_activation_leaves_the_state_untouched() {
        let mut mock = Mock {
            fail_activate: true,
            ..Mock::default()
        };
        assert_eq!(activate_as_new(&mut mock), Err(()));
        assert_eq!(mock.calls[..mock.count], ["activate"]);
    }
}
