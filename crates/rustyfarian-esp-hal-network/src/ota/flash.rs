//! [`SharedFlash`]: the one `FlashStorage` of a DDI firmware, shared by the OTA manager, the record store and the boot-fact reads.
//!
//! esp-storage hands out one `FlashStorage` per `FLASH` peripheral, and every user here needs it at different times: the record store commits between two chunks of a download, the boot facts read the otadata while the runtime runs.
//! Each user borrows the storage for ONE synchronous flash operation inside a critical section and never across an `.await`.
//! esp-storage masks interrupts for every flash operation anyway (its `critical-section` feature), so the borrow adds no masked time of its own.

use core::cell::{Cell, RefCell};

use critical_section::Mutex;

/// A value set once per boot and lent out for one synchronous call at a time.
///
/// A second set, a call before the set and a nested call all fail softly instead of panicking.
pub(crate) struct OnceShared<T>(Mutex<RefCell<Option<T>>>);

impl<T> OnceShared<T> {
    /// An empty cell.
    pub(crate) const fn new() -> Self {
        Self(Mutex::new(RefCell::new(None)))
    }

    /// Stores the value `make` builds, unless a value is stored already; `make` then never runs.
    ///
    /// Returns whether the value was stored.
    pub(crate) fn init_with(&self, make: impl FnOnce() -> T) -> bool {
        critical_section::with(|cs| {
            let Ok(mut slot) = self.0.borrow(cs).try_borrow_mut() else {
                return false;
            };
            if slot.is_some() {
                return false;
            }
            *slot = Some(make());
            true
        })
    }

    /// Whether a value is stored.
    pub(crate) fn is_set(&self) -> bool {
        critical_section::with(|cs| {
            self.0
                .borrow(cs)
                .try_borrow()
                .map_or(true, |slot| slot.is_some())
        })
    }

    /// Runs `f` on the value inside a critical section.
    ///
    /// `None` when no value is stored or the value is already lent out (a nested call).
    /// `f` must be short: it runs with interrupts masked.
    pub(crate) fn with<R>(&self, f: impl FnOnce(&mut T) -> R) -> Option<R> {
        critical_section::with(|cs| {
            let mut slot = self.0.borrow(cs).try_borrow_mut().ok()?;
            slot.as_mut().map(f)
        })
    }
}

/// A flag that at most one holder owns at a time, released when the holder drops.
///
/// Held by a download from the choice of its target slot to the end of its activation, so two downloads never erase and write the same slot between their awaits.
/// A `critical_section` and a `Cell`, not an atomic: the ESP32-C3 (`riscv32imc`) has no atomic read-modify-write.
pub(crate) struct LeaseCell(Mutex<Cell<bool>>);

impl LeaseCell {
    /// A free cell.
    pub(crate) const fn new() -> Self {
        Self(Mutex::new(Cell::new(false)))
    }

    /// Takes the lease; `None` when it is taken already.
    pub(crate) fn try_take(&self) -> Option<Lease<'_>> {
        critical_section::with(|cs| {
            let taken = self.0.borrow(cs);
            if taken.get() {
                None
            } else {
                taken.set(true);
                Some(Lease(self))
            }
        })
    }
}

/// The lease of a [`LeaseCell`]; dropping it, with the future that holds it, frees the cell.
#[must_use = "the lease is released when it is dropped"]
pub(crate) struct Lease<'a>(&'a LeaseCell);

impl Lease<'_> {
    /// Keeps the cell taken for the rest of the boot: the lease is never released.
    ///
    /// Called once a download has selected its slot for the next boot, so no later download erases the image the bootloader is about to start.
    #[cfg_attr(
        not(test),
        allow(
            dead_code,
            reason = "only the chip build's shared-flash download keeps a lease"
        )
    )]
    pub(crate) fn keep(self) {
        core::mem::forget(self);
    }
}

impl Drop for Lease<'_> {
    fn drop(&mut self) {
        critical_section::with(|cs| self.0 .0.borrow(cs).set(false));
    }
}

#[cfg(all(
    feature = "ota-ddi",
    any(
        feature = "esp32c3",
        feature = "esp32c6",
        feature = "esp32",
        feature = "esp32s3"
    )
))]
mod shared {
    use embedded_storage::nor_flash::{ErrorType, NorFlash, ReadNorFlash};
    use esp_storage::{FlashStorage, FlashStorageError};

    use super::{Lease, LeaseCell, OnceShared};

    /// `FlashStorageError::Other` code of a shared-flash operation that found the storage lent out.
    const BUSY: i32 = -0x7102;

    /// Experimental: API may change before 1.0.
    ///
    /// The firmware's one `FlashStorage`, shared by [`EspHalOtaManager::new_shared`](super::super::EspHalOtaManager::new_shared), [`open_records`](super::super::open_records) and [`read_hardware_facts`](super::super::read_hardware_facts).
    ///
    /// Create it once, early in `main`, from the `FLASH` peripheral; it lives for the whole program.
    /// `&SharedFlash` implements `NorFlash`: every call borrows the storage for that one operation inside a critical section, so no borrow ever spans an `.await`.
    /// The ESP-IDF tier needs no such handle (its flash driver is global).
    ///
    /// ```ignore
    /// let peripherals = esp_hal::init(esp_hal::Config::default());
    /// let flash = SharedFlash::new(peripherals.FLASH)?;
    /// let records = open_records(flash)?;
    /// ```
    pub struct SharedFlash(OnceShared<FlashStorage<'static>>, LeaseCell);

    /// The only instance; empty until [`SharedFlash::new`].
    static SHARED_FLASH: SharedFlash = SharedFlash(OnceShared::new(), LeaseCell::new());

    /// Experimental: API may change before 1.0.
    ///
    /// [`SharedFlash::new`] was called before in this boot.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct SharedFlashTaken;

    impl core::fmt::Display for SharedFlashTaken {
        fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
            f.write_str("the shared flash handle was already created in this boot")
        }
    }

    impl core::error::Error for SharedFlashTaken {}

    impl SharedFlash {
        /// Experimental: API may change before 1.0.
        ///
        /// Takes the `FLASH` peripheral and returns the program-long shared handle.
        ///
        /// # Errors
        ///
        /// [`SharedFlashTaken`] when a handle was created before in this boot (the peripheral is dropped; nothing panics).
        pub fn new(
            flash: esp_hal::peripherals::FLASH<'static>,
        ) -> Result<&'static SharedFlash, SharedFlashTaken> {
            if SHARED_FLASH.0.init_with(|| FlashStorage::new(flash)) {
                Ok(&SHARED_FLASH)
            } else {
                Err(SharedFlashTaken)
            }
        }

        /// The handle, once [`SharedFlash::new`] ran.
        pub(crate) fn get() -> Option<&'static SharedFlash> {
            SHARED_FLASH.0.is_set().then_some(&SHARED_FLASH)
        }

        /// The exclusive OTA lease: held by one download from its target selection to the end of its activation, so no second download erases or writes the same slot in between.
        /// A download that selected its slot for the next boot keeps it until the reset, so the selected image is never erased.
        ///
        /// `None` when another download holds it, or one has selected its slot in this boot.
        /// The record store and the boot-fact reads never take it; they run between two chunks of a download.
        pub(crate) fn lease_ota(&self) -> Option<Lease<'_>> {
            self.1.try_take()
        }

        /// Runs `f` on the storage inside a critical section; `None` when it is lent out already.
        pub(crate) fn with<R>(&self, f: impl FnOnce(&mut FlashStorage<'static>) -> R) -> Option<R> {
            self.0.with(f)
        }
    }

    impl ErrorType for &SharedFlash {
        type Error = FlashStorageError;
    }

    impl ReadNorFlash for &SharedFlash {
        const READ_SIZE: usize = <FlashStorage<'static> as ReadNorFlash>::READ_SIZE;

        fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
            self.with(|flash| ReadNorFlash::read(flash, offset, bytes))
                .unwrap_or(Err(FlashStorageError::Other(BUSY)))
        }

        fn capacity(&self) -> usize {
            self.with(|flash| ReadNorFlash::capacity(flash))
                .unwrap_or(0)
        }
    }

    impl NorFlash for &SharedFlash {
        const WRITE_SIZE: usize = <FlashStorage<'static> as NorFlash>::WRITE_SIZE;
        const ERASE_SIZE: usize = <FlashStorage<'static> as NorFlash>::ERASE_SIZE;

        fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
            self.with(|flash| NorFlash::erase(flash, from, to))
                .unwrap_or(Err(FlashStorageError::Other(BUSY)))
        }

        fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
            self.with(|flash| NorFlash::write(flash, offset, bytes))
                .unwrap_or(Err(FlashStorageError::Other(BUSY)))
        }
    }
}

#[cfg(all(
    feature = "ota-ddi",
    any(
        feature = "esp32c3",
        feature = "esp32c6",
        feature = "esp32",
        feature = "esp32s3"
    )
))]
pub use shared::{SharedFlash, SharedFlashTaken};

#[cfg(test)]
mod tests {
    use super::{LeaseCell, OnceShared};

    #[test]
    fn a_second_set_is_refused_without_running_its_constructor() {
        let cell = OnceShared::new();
        assert!(!cell.is_set());
        assert_eq!(cell.with(|v: &mut u32| *v), None);
        assert!(cell.init_with(|| 1u32));
        assert!(cell.is_set());
        let mut ran = false;
        assert!(!cell.init_with(|| {
            ran = true;
            2
        }));
        assert!(!ran);
        assert_eq!(cell.with(|v| *v), Some(1));
    }

    #[test]
    fn a_nested_borrow_fails_instead_of_panicking() {
        let cell = OnceShared::new();
        assert!(cell.init_with(|| 7u32));
        let nested = cell.with(|outer| {
            *outer += 1;
            cell.with(|inner| *inner)
        });
        assert_eq!(nested, Some(None));
        assert!(!cell.init_with(|| 9));
        assert_eq!(cell.with(|v| *v), Some(8));
    }

    #[test]
    fn a_taken_lease_is_refused_until_it_is_dropped() {
        let cell = LeaseCell::new();
        let first = cell.try_take();
        assert!(first.is_some());
        assert!(cell.try_take().is_none());
        assert!(cell.try_take().is_none(), "a refusal does not release it");
        drop(first);
        let again = cell.try_take();
        assert!(again.is_some());
        assert!(cell.try_take().is_none());
    }

    #[test]
    fn a_kept_lease_is_never_released() {
        let cell = LeaseCell::new();
        cell.try_take().expect("free").keep();
        assert!(cell.try_take().is_none());
        assert!(cell.try_take().is_none());
    }

    #[test]
    fn the_lease_is_released_on_an_early_return() {
        fn download(cell: &LeaseCell, fail: bool) -> Result<(), ()> {
            let _lease = cell.try_take().ok_or(())?;
            if fail {
                return Err(());
            }
            Ok(())
        }
        let cell = LeaseCell::new();
        assert_eq!(download(&cell, true), Err(()));
        assert_eq!(download(&cell, false), Ok(()));
        assert!(cell.try_take().is_some());
    }

    #[test]
    fn a_dropped_future_releases_the_lease() {
        use core::future::Future;
        use core::pin::pin;
        use core::task::{Context, Poll, Waker};

        struct Pending(bool);
        impl Future for Pending {
            type Output = ();
            fn poll(mut self: core::pin::Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
                if self.0 {
                    Poll::Ready(())
                } else {
                    self.0 = true;
                    Poll::Pending
                }
            }
        }

        let cell = LeaseCell::new();
        {
            let mut download = pin!(async {
                let _lease = cell.try_take();
                Pending(false).await;
            });
            let mut cx = Context::from_waker(Waker::noop());
            assert!(download.as_mut().poll(&mut cx).is_pending());
            assert!(cell.try_take().is_none(), "held across the await");
        }
        assert!(cell.try_take().is_some(), "free once the future is dropped");
    }
}
